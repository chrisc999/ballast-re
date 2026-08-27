#![no_std]
//! DeFindex strategy adapter for the baUSD vault.
//!
//! Implements DeFindex's `DeFindexStrategyTrait` shape so a DeFindex vault can allocate
//! USDC to baUSD alongside its other strategies. Signatures are declared locally rather
//! than imported: Soroban contract types match structurally over XDR, and pinning a git
//! dependency on another team's contracts would couple our build to their branch.
//!
//! # The liquidity constraint, stated honestly
//!
//! DeFindex's `withdraw` is SYNCHRONOUS — it must hand back the underlying asset within the
//! same invocation. baUSD is deliberately not built for that: most capital sits in
//! reinsurance treaties, redemption carries a notice period, and the vault holds only a
//! liquidity sleeve.
//!
//! This adapter therefore serves a withdrawal only when the vault can genuinely settle one
//! atomically — notice period elapsed (in practice, zero) AND the sleeve covering the full
//! amount. It calls `request_redemption` and `claim_redemption` back to back in the same
//! transaction. When that cannot be honoured in full the invocation FAILS, which rolls the
//! request back with it, so no half-open escrow is ever left behind.
//!
//! Declining is a legitimate outcome, not a defect: `withdraw` returns `Result`, and an
//! illiquid strategy that reports its limits truthfully is safer than one that pretends.
//! A DeFindex vault holding baUSD should therefore be configured to keep it a bounded,
//! low-priority allocation — idle funds and liquid strategies absorbing ordinary flow —
//! rather than treating it as on-demand liquidity.

use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contracterror, contractimpl, contracttype, panic_with_error,
    token::TokenClient,
    vec, Address, Bytes, Env, IntoVal, Symbol, TryFromVal, Val, Vec,
};

/// The slice of the baUSD vault this adapter needs.
#[soroban_sdk::contractclient(name = "VaultClient")]
pub trait BaUsdVault {
    fn subscribe(env: Env, from: Address, amount: i128) -> i128;
    fn request_redemption(env: Env, from: Address, shares: i128);
    fn claim_redemption(env: Env, from: Address) -> i128;
    fn cancel_redemption(env: Env, from: Address);
    fn convert_to_assets(env: Env, shares: i128) -> i128;
    fn convert_to_shares_ceil(env: Env, assets: i128) -> i128;
    fn get_redemption(env: Env, who: Address) -> Option<RedemptionRequest>;
    fn get_token(env: Env) -> Address;
}

/// Mirrors the vault's `RedemptionRequest`.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct RedemptionRequest {
    pub shares: i128,
    pub claimable_at: u64,
}

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_LIFETIME_THRESHOLD: u32 = INSTANCE_BUMP_AMOUNT - DAY_IN_LEDGERS;
const POSITION_BUMP_AMOUNT: u32 = 90 * DAY_IN_LEDGERS;
const POSITION_LIFETIME_THRESHOLD: u32 = POSITION_BUMP_AMOUNT - DAY_IN_LEDGERS;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum StrategyError {
    NotInitialized = 1,
    InvalidAmount = 2,
    /// The depositor's position is smaller than the requested withdrawal.
    InsufficientBalance = 3,
    /// The vault could not settle the full amount atomically — the sleeve is short, or a
    /// notice period stands between request and claim. The caller should retry later or
    /// unwind a different strategy.
    InsufficientLiquidity = 4,
}

#[contracttype]
pub enum DataKey {
    Vault,
    Underlying,
    /// baUSD shares attributed to a depositor. PERSISTENT: per-user data.
    Shares(Address),
}

#[contract]
pub struct StrategyContract;

#[contractimpl]
impl StrategyContract {
    /// `asset` is the underlying (USDC). `init_args` carries the baUSD vault address, which
    /// is how DeFindex passes strategy-specific configuration.
    pub fn __constructor(e: &Env, asset: Address, init_args: Vec<Val>) {
        let raw = init_args
            .get(0)
            .unwrap_or_else(|| panic_with_error!(e, StrategyError::NotInitialized));
        let vault = Address::try_from_val(e, &raw)
            .unwrap_or_else(|_| panic_with_error!(e, StrategyError::NotInitialized));

        let s = e.storage().instance();
        s.set(&DataKey::Underlying, &asset);
        s.set(&DataKey::Vault, &vault);
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
    }

    // --- DeFindex strategy interface ---------------------------------------------------

    pub fn asset(e: &Env) -> Result<Address, StrategyError> {
        read_underlying(e)
    }

    /// Take `amount` of USDC from `from` and subscribe it to the baUSD vault. The resulting
    /// shares are held by THIS contract and attributed to `from` internally, since the vault
    /// mints to whoever subscribed.
    pub fn deposit(e: &Env, amount: i128, from: Address) -> Result<i128, StrategyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(StrategyError::InvalidAmount);
        }

        let underlying = read_underlying(e)?;
        let vault = read_vault(e)?;
        let me = e.current_contract_address();

        // Pull the underlying in, then subscribe as ourselves.
        TokenClient::new(e, &underlying).transfer(&from, &me, &amount);

        // The vault will move OUR usdc into itself as part of `subscribe`. That transfer is
        // two frames deep, so being the caller is not enough — a contract only satisfies
        // `require_auth` for its immediate callee. Authorize the specific sub-invocation.
        authorize_token_transfer(e, &underlying, &me, &vault, amount);
        let minted = VaultClient::new(e, &vault).subscribe(&me, &amount);

        let held = read_shares(e, &from);
        write_shares(e, &from, held + minted);

        Self::balance(e, from)
    }

    /// `from`'s position, denominated in the UNDERLYING asset, per DeFindex's requirement
    /// that balances are reported in underlying rather than in shares or a derivative.
    ///
    /// Note this tracks attested NAV: it rises as treaty premium accrues and FALLS on a
    /// treaty loss. A DeFindex vault holding this strategy inherits that exposure.
    pub fn balance(e: &Env, from: Address) -> Result<i128, StrategyError> {
        let vault = read_vault(e)?;
        let shares = read_shares(e, &from);
        if shares == 0 {
            return Ok(0);
        }
        Ok(VaultClient::new(e, &vault).convert_to_assets(&shares))
    }

    /// Redeem enough baUSD to hand `to` exactly `amount` of USDC, within this transaction.
    ///
    /// Fails with `InsufficientLiquidity` when the vault cannot settle atomically — a short
    /// sleeve, or a notice period between request and claim. The failure rolls back the
    /// redemption request along with everything else, so no escrow is stranded.
    pub fn withdraw(
        e: &Env,
        amount: i128,
        from: Address,
        to: Address,
    ) -> Result<i128, StrategyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(StrategyError::InvalidAmount);
        }

        let underlying = read_underlying(e)?;
        let vault_addr = read_vault(e)?;
        let vault = VaultClient::new(e, &vault_addr);
        let me = e.current_contract_address();

        let held = read_shares(e, &from);
        if held <= 0 {
            return Err(StrategyError::InsufficientBalance);
        }

        // Round the required shares UP. The flooring conversion is right for pricing a
        // deposit and wrong here: it lands a stroop short of `amount` at any share price
        // other than exactly 1.0, so the claim underpays and the withdrawal fails despite
        // ample shares and sleeve. Rounding up costs the caller at most one share and
        // guarantees the payout covers the request.
        let mut needed = vault.convert_to_shares_ceil(&amount);
        if needed <= 0 {
            return Err(StrategyError::InvalidAmount);
        }
        if needed > held {
            // Withdrawing the whole position: rounding up can exceed the holding by a
            // share. Use everything they have and let the payout check below decide.
            needed = held;
        }

        // Defensive: clear any request left over from an earlier attempt. A reverted
        // transaction leaves none, but the adapter is a single address at the vault and
        // must never be wedged by a stale pending request.
        if vault.get_redemption(&me).is_some() {
            vault.cancel_redemption(&me);
        }

        // Same nesting problem as `deposit`: `request_redemption` escrows our baUSD by
        // transferring it into the vault, one frame below us.
        let baus = vault.get_token();
        authorize_token_transfer(e, &baus, &me, &vault_addr, needed);
        vault.request_redemption(&me, &needed);

        // The claim moves value the vault already holds, under the vault's own authority,
        // so it needs nothing from us.
        let paid = vault.claim_redemption(&me);

        // A partial fill cannot satisfy DeFindex's synchronous contract. Refuse, and let the
        // revert undo the request too.
        if paid < amount {
            return Err(StrategyError::InsufficientLiquidity);
        }

        write_shares(e, &from, held - needed);
        // Pay out everything the claim produced, not just `amount`. Rounding up the shares
        // can yield a stroop or so more; forwarding it keeps the adapter holding no
        // unattributed dust, and the burned shares match the value delivered exactly.
        TokenClient::new(e, &underlying).transfer(&me, &to, &paid);

        Self::balance(e, from)
    }

    /// No-op. baUSD yield accrues through attested NAV rather than claimable rewards, so
    /// there is nothing to harvest — the position simply revalues.
    pub fn harvest(_e: &Env, _from: Address, _data: Option<Bytes>) -> Result<(), StrategyError> {
        Ok(())
    }

    // --- helpers ------------------------------------------------------------------------

    pub fn vault(e: &Env) -> Result<Address, StrategyError> {
        read_vault(e)
    }

    /// baUSD shares this adapter holds for `from`.
    pub fn shares_of(e: &Env, from: Address) -> i128 {
        read_shares(e, &from)
    }
}

/// Authorize `token.transfer(from, to, amount)` as a sub-invocation of the call we are
/// about to make. Scoped to exactly one transfer with exactly these arguments — a blanket
/// authorization would let the callee move more than we intend.
fn authorize_token_transfer(e: &Env, token: &Address, from: &Address, to: &Address, amount: i128) {
    e.authorize_as_current_contract(vec![
        e,
        InvokerContractAuthEntry::Contract(SubContractInvocation {
            context: ContractContext {
                contract: token.clone(),
                fn_name: Symbol::new(e, "transfer"),
                args: (from.clone(), to.clone(), amount).into_val(e),
            },
            sub_invocations: vec![e],
        }),
    ]);
}

fn read_vault(e: &Env) -> Result<Address, StrategyError> {
    e.storage()
        .instance()
        .get(&DataKey::Vault)
        .ok_or(StrategyError::NotInitialized)
}

fn read_underlying(e: &Env) -> Result<Address, StrategyError> {
    e.storage()
        .instance()
        .get(&DataKey::Underlying)
        .ok_or(StrategyError::NotInitialized)
}

fn read_shares(e: &Env, who: &Address) -> i128 {
    let key = DataKey::Shares(who.clone());
    let p = e.storage().persistent();
    match p.get::<DataKey, i128>(&key) {
        Some(v) => {
            p.extend_ttl(&key, POSITION_LIFETIME_THRESHOLD, POSITION_BUMP_AMOUNT);
            v
        }
        None => 0,
    }
}

fn write_shares(e: &Env, who: &Address, shares: i128) {
    let key = DataKey::Shares(who.clone());
    let p = e.storage().persistent();
    if shares == 0 {
        p.remove(&key);
    } else {
        p.set(&key, &shares);
        p.extend_ttl(&key, POSITION_LIFETIME_THRESHOLD, POSITION_BUMP_AMOUNT);
    }
}

#[cfg(test)]
mod test;
