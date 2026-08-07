#![no_std]
//! baUSD vault — appreciating-share accounting over an attested NAV.
//!
//! `share_price = total_assets / total_shares`, where `total_assets` is a BOOKKEPT,
//! attested NAV (not the vault's on-chain balance). This is deliberate: it closes the
//! classic ERC-4626 inflation attack, since tokens donated directly to the vault are not
//! counted.
//!
//! Sub-steps so far: storage/roles/init/share-math (1), `subscribe` + pause + token wiring
//! (2). `request_redemption`/`claim_redemption` (3), upgrade + events (4) follow.

use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype,
    panic_with_error, symbol_short, token::TokenClient, Address, BytesN, Env, Symbol,
};

/// Minimal client for baUSD's owner-only admin interface. Lets the vault call `mint`
/// (and, from sub-step 3, `burn`) on the deployed baUSD token by address, without
/// compiling the token crate into the vault's wasm.
#[contractclient(name = "BaUsdClient")]
pub trait BaUsdAdmin {
    fn mint(env: Env, to: Address, amount: i128);
}

// --- TTL / rent (Soroban state archival footgun) -----------------------------------
// Persistent and instance entries expire; bump on access. ~5s ledgers => ~17,280/day.
const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_LIFETIME_THRESHOLD: u32 = INSTANCE_BUMP_AMOUNT - DAY_IN_LEDGERS;
// Per-user redemption requests live in persistent storage; bump on access too.
const PERSISTENT_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
const PERSISTENT_LIFETIME_THRESHOLD: u32 = PERSISTENT_BUMP_AMOUNT - DAY_IN_LEDGERS;

// --- Share-math virtual offset (defense-in-depth) ----------------------------------
// The primary inflation vector is already closed by bookkept NAV (see module docs);
// the +1/+1 offset guards rounding games at tiny totals.
const VIRTUAL_ASSETS: i128 = 1;
const VIRTUAL_SHARES: i128 = 1;

/// Minimum first deposit (belt-and-suspenders with the virtual offset). 1.0 baUSD at
/// 7 decimals.
const MIN_INITIAL_DEPOSIT: i128 = 10_000_000;

/// Governance upgrade timelock. 24h default; the duration is a config parameter.
const UPGRADE_TIMELOCK_SECS: u64 = 86_400;

// --- Events (consumed by indexers and any monitoring dashboard) --------------------
#[contractevent]
pub struct Initialized {
    #[topic]
    pub admin: Address,
}
#[contractevent]
pub struct TokenBound {
    #[topic]
    pub token: Address,
}
#[contractevent]
pub struct Subscribed {
    #[topic]
    pub from: Address,
    pub amount: i128,
    pub shares: i128,
}
#[contractevent]
pub struct RedemptionRequested {
    #[topic]
    pub from: Address,
    pub shares: i128,
    pub claimable_at: u64,
}
#[contractevent]
pub struct RedemptionClaimed {
    #[topic]
    pub from: Address,
    pub shares: i128,
    pub assets: i128,
}
#[contractevent]
pub struct PauseSet {
    pub paused: bool,
}
#[contractevent]
pub struct AuthorityUpdated {
    #[topic]
    pub role: Symbol,
    pub new: Address,
}
#[contractevent]
pub struct UpgradeProposed {
    pub wasm_hash: BytesN<32>,
    pub eta: u64,
}
#[contractevent]
pub struct Upgraded {
    pub wasm_hash: BytesN<32>,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    MathOverflow = 2,
    Paused = 3,
    InvalidAmount = 4,
    BelowMinInitialDeposit = 5,
    ZeroShares = 6,
    TokenAlreadySet = 7,
    TokenNotSet = 8,
    RequestPending = 9,
    NoRequest = 10,
    NoticeNotElapsed = 11,
    InsufficientSleeve = 12,
    NoPendingUpgrade = 13,
    TimelockNotElapsed = 14,
}

/// Vault configuration and role addresses. Separation of duties: each authority can do
/// exactly one class of thing. Stored as swappable addresses so mainnet multisig drops in
/// later without code changes. (The token address lives in its own slot, set once via
/// `set_token` after the token is deployed with this vault as its owner.)
#[contracttype]
#[derive(Clone)]
pub struct Config {
    pub admin: Address,                 // governance: upgrade + authority setters
    pub guardian: Address,              // pause only
    pub attestation_authority: Address, // update_nav only
    pub compliance_authority: Address,  // allowlist only
    pub treasury: Address,              // sleeve in/out only
    pub usdc: Address,                  // USDC Stellar Asset Contract
    pub notice_period: u64,             // redemption notice, seconds (0 on testnet demo)
}

/// A pending redemption. Shares are escrowed in the vault at request time; the payout is
/// computed at CLAIM time (claim-time NAV), so NAV moves between request and claim accrue
/// correctly.
#[contracttype]
#[derive(Clone)]
pub struct RedemptionRequest {
    pub shares: i128,
    pub claimable_at: u64,
}

/// A queued governance upgrade: the target wasm hash and the earliest execution time.
#[contracttype]
#[derive(Clone)]
pub struct PendingUpgrade {
    pub wasm_hash: BytesN<32>,
    pub eta: u64,
}

#[contracttype]
pub enum DataKey {
    Config,
    Token, // baUSD token address (vault is its owner); set once via set_token
    TotalShares,
    TotalAssets,    // bookkept NAV
    NavLastUpdated, // freshness timestamp (used by update_nav)
    Paused,
    PendingUpgrade,
    Redemption(Address), // per-user pending redemption (persistent storage)
}

#[contract]
pub struct VaultContract;

#[contractimpl]
impl VaultContract {
    /// Deploy-time initialization. The baUSD token is deployed separately with THIS vault
    /// as its owner, then bound via `set_token` — so mint authority is the vault's alone.
    #[allow(clippy::too_many_arguments)]
    pub fn __constructor(
        e: &Env,
        admin: Address,
        guardian: Address,
        attestation_authority: Address,
        compliance_authority: Address,
        treasury: Address,
        usdc: Address,
        notice_period: u64,
    ) {
        let config = Config {
            admin,
            guardian,
            attestation_authority,
            compliance_authority,
            treasury,
            usdc,
            notice_period,
        };
        let s = e.storage().instance();
        s.set(&DataKey::Config, &config);
        s.set(&DataKey::TotalShares, &0i128);
        s.set(&DataKey::TotalAssets, &0i128);
        s.set(&DataKey::NavLastUpdated, &e.ledger().timestamp());
        s.set(&DataKey::Paused, &false);
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        Initialized {
            admin: config.admin.clone(),
        }
        .publish(e);
    }

    /// Bind the baUSD token address. Admin-gated and one-time (the token must already be
    /// owned by this vault). After this, the vault can mint/burn baUSD; nobody else can.
    pub fn set_token(e: &Env, token: Address) {
        read_config(e).admin.require_auth();
        let s = e.storage().instance();
        if s.has(&DataKey::Token) {
            panic_with_error!(e, Error::TokenAlreadySet);
        }
        s.set(&DataKey::Token, &token);
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        TokenBound { token }.publish(e);
    }

    /// Deposit `amount` of USDC and mint baUSD shares to `from`. Shares are computed at
    /// the current share price and rounded DOWN (the vault's favor).
    pub fn subscribe(e: &Env, from: Address, amount: i128) -> i128 {
        from.require_auth();
        ensure_not_paused(e);
        ensure_allowed(e, &from);
        if amount <= 0 {
            panic_with_error!(e, Error::InvalidAmount);
        }

        let config = read_config(e);
        let token_addr = read_token(e);
        let ts = read_i128(e, &DataKey::TotalShares);
        let ta = read_i128(e, &DataKey::TotalAssets);

        if ts == 0 && amount < MIN_INITIAL_DEPOSIT {
            panic_with_error!(e, Error::BelowMinInitialDeposit);
        }

        let shares = shares_for_deposit(amount, ts, ta)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
        // A positive deposit that rounds to zero shares would burn the depositor's funds.
        if shares <= 0 {
            panic_with_error!(e, Error::ZeroShares);
        }

        // Effects: update bookkept totals first (a later failed transfer reverts all of it).
        let s = e.storage().instance();
        s.set(&DataKey::TotalAssets, &(ta + amount));
        s.set(&DataKey::TotalShares, &(ts + shares));
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        // Interactions: pull USDC into the vault, then mint baUSD. USDC is a
        // non-reentrant SAC and baUSD is our token, so ordering is safe; any failure
        // reverts the whole invocation atomically.
        TokenClient::new(e, &config.usdc).transfer(&from, e.current_contract_address(), &amount);
        BaUsdClient::new(e, &token_addr).mint(&from, &shares);

        Subscribed {
            from,
            amount,
            shares,
        }
        .publish(e);
        shares
    }

    /// Guardian-only: halt state-changing user flows.
    pub fn pause(e: &Env) {
        read_config(e).guardian.require_auth();
        e.storage().instance().set(&DataKey::Paused, &true);
        PauseSet { paused: true }.publish(e);
    }

    /// Guardian-only: resume.
    pub fn unpause(e: &Env) {
        read_config(e).guardian.require_auth();
        e.storage().instance().set(&DataKey::Paused, &false);
        PauseSet { paused: false }.publish(e);
    }

    /// Request to redeem `shares`. The shares are escrowed in the vault immediately; the
    /// USDC payout is computed at claim time (claim-time NAV) after the notice period.
    /// One pending request per address (a full queue is planned).
    pub fn request_redemption(e: &Env, from: Address, shares: i128) {
        from.require_auth();
        ensure_not_paused(e);
        ensure_allowed(e, &from);
        if shares <= 0 {
            panic_with_error!(e, Error::InvalidAmount);
        }

        let key = DataKey::Redemption(from.clone());
        if e.storage().persistent().has(&key) {
            panic_with_error!(e, Error::RequestPending);
        }

        let config = read_config(e);
        let token_addr = read_token(e);

        // Escrow the redeemer's baUSD into the vault (reverts if their balance is short).
        TokenClient::new(e, &token_addr).transfer(&from, e.current_contract_address(), &shares);

        let req = RedemptionRequest {
            shares,
            claimable_at: e.ledger().timestamp() + config.notice_period,
        };
        let p = e.storage().persistent();
        p.set(&key, &req);
        p.extend_ttl(&key, PERSISTENT_LIFETIME_THRESHOLD, PERSISTENT_BUMP_AMOUNT);

        RedemptionRequested {
            from,
            shares,
            claimable_at: req.claimable_at,
        }
        .publish(e);
    }

    /// Claim a matured redemption: burn the escrowed baUSD and pay USDC at claim-time NAV.
    pub fn claim_redemption(e: &Env, from: Address) {
        from.require_auth();
        ensure_not_paused(e);

        let key = DataKey::Redemption(from.clone());
        let req: RedemptionRequest = e
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(e, Error::NoRequest));

        if e.ledger().timestamp() < req.claimable_at {
            panic_with_error!(e, Error::NoticeNotElapsed);
        }

        let config = read_config(e);
        let token_addr = read_token(e);
        let ts = read_i128(e, &DataKey::TotalShares);
        let ta = read_i128(e, &DataKey::TotalAssets);

        // Payout at CURRENT (claim-time) NAV, rounded DOWN.
        let assets = assets_for_shares(req.shares, ts, ta)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));

        // Sleeve liquidity: the vault must hold enough USDC to pay now (otherwise the
        // request would queue). While the vault holds all deposits, this always passes.
        let usdc = TokenClient::new(e, &config.usdc);
        let vault_addr = e.current_contract_address();
        if assets > usdc.balance(&vault_addr) {
            panic_with_error!(e, Error::InsufficientSleeve);
        }

        // Effects: shrink bookkept totals and drop the request.
        let s = e.storage().instance();
        s.set(&DataKey::TotalShares, &(ts - req.shares));
        s.set(&DataKey::TotalAssets, &(ta - assets));
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        e.storage().persistent().remove(&key);

        // Interactions: burn the escrowed baUSD (vault authorizes as holder), pay USDC out.
        TokenClient::new(e, &token_addr).burn(&vault_addr, &req.shares);
        usdc.transfer(&vault_addr, from.clone(), &assets);

        RedemptionClaimed {
            from,
            shares: req.shares,
            assets,
        }
        .publish(e);
    }

    /// The caller's pending redemption, if any.
    pub fn get_redemption(e: &Env, who: Address) -> Option<RedemptionRequest> {
        e.storage().persistent().get(&DataKey::Redemption(who))
    }

    // --- authority setters (governance/admin only; swap roles without redeploy) -----

    pub fn set_admin(e: &Env, new_admin: Address) {
        let mut c = read_config(e);
        c.admin.require_auth();
        c.admin = new_admin.clone();
        write_config(e, &c);
        AuthorityUpdated {
            role: symbol_short!("admin"),
            new: new_admin,
        }
        .publish(e);
    }

    pub fn set_guardian(e: &Env, new_guardian: Address) {
        let mut c = read_config(e);
        c.admin.require_auth();
        c.guardian = new_guardian.clone();
        write_config(e, &c);
        AuthorityUpdated {
            role: symbol_short!("guardian"),
            new: new_guardian,
        }
        .publish(e);
    }

    pub fn set_attestation_authority(e: &Env, new_authority: Address) {
        let mut c = read_config(e);
        c.admin.require_auth();
        c.attestation_authority = new_authority.clone();
        write_config(e, &c);
        AuthorityUpdated {
            role: symbol_short!("attest"),
            new: new_authority,
        }
        .publish(e);
    }

    pub fn set_compliance_authority(e: &Env, new_authority: Address) {
        let mut c = read_config(e);
        c.admin.require_auth();
        c.compliance_authority = new_authority.clone();
        write_config(e, &c);
        AuthorityUpdated {
            role: symbol_short!("comply"),
            new: new_authority,
        }
        .publish(e);
    }

    pub fn set_treasury(e: &Env, new_treasury: Address) {
        let mut c = read_config(e);
        c.admin.require_auth();
        c.treasury = new_treasury.clone();
        write_config(e, &c);
        AuthorityUpdated {
            role: symbol_short!("treasury"),
            new: new_treasury,
        }
        .publish(e);
    }

    // --- governance-gated upgrade with timelock -------------------------------------

    /// Queue an upgrade to `new_wasm_hash`, executable after the timelock. Admin-only.
    /// A fresh proposal replaces any pending one.
    pub fn propose_upgrade(e: &Env, new_wasm_hash: BytesN<32>) {
        read_config(e).admin.require_auth();
        let eta = e.ledger().timestamp() + UPGRADE_TIMELOCK_SECS;
        let s = e.storage().instance();
        s.set(
            &DataKey::PendingUpgrade,
            &PendingUpgrade {
                wasm_hash: new_wasm_hash.clone(),
                eta,
            },
        );
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        UpgradeProposed {
            wasm_hash: new_wasm_hash,
            eta,
        }
        .publish(e);
    }

    /// Execute a queued upgrade once its timelock has elapsed. Admin-only. The contract
    /// address is unchanged across the wasm swap.
    pub fn execute_upgrade(e: &Env) {
        read_config(e).admin.require_auth();
        let pending: PendingUpgrade = e
            .storage()
            .instance()
            .get(&DataKey::PendingUpgrade)
            .unwrap_or_else(|| panic_with_error!(e, Error::NoPendingUpgrade));
        if e.ledger().timestamp() < pending.eta {
            panic_with_error!(e, Error::TimelockNotElapsed);
        }
        e.storage().instance().remove(&DataKey::PendingUpgrade);
        e.deployer()
            .update_current_contract_wasm(pending.wasm_hash.clone());
        Upgraded {
            wasm_hash: pending.wasm_hash,
        }
        .publish(e);
    }

    pub fn get_pending_upgrade(e: &Env) -> Option<PendingUpgrade> {
        e.storage().instance().get(&DataKey::PendingUpgrade)
    }

    pub fn is_paused(e: &Env) -> bool {
        e.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn get_config(e: &Env) -> Config {
        read_config(e)
    }

    pub fn get_token(e: &Env) -> Address {
        read_token(e)
    }

    pub fn total_shares(e: &Env) -> i128 {
        read_i128(e, &DataKey::TotalShares)
    }

    pub fn total_assets(e: &Env) -> i128 {
        read_i128(e, &DataKey::TotalAssets)
    }

    /// Shares minted for depositing `assets` of USDC, rounded DOWN (the vault's favor).
    pub fn convert_to_shares(e: &Env, assets: i128) -> i128 {
        let ts = read_i128(e, &DataKey::TotalShares);
        let ta = read_i128(e, &DataKey::TotalAssets);
        shares_for_deposit(assets, ts, ta)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow))
    }

    /// USDC returned for redeeming `shares`, rounded DOWN (the vault's favor).
    pub fn convert_to_assets(e: &Env, shares: i128) -> i128 {
        let ts = read_i128(e, &DataKey::TotalShares);
        let ta = read_i128(e, &DataKey::TotalAssets);
        assets_for_shares(shares, ts, ta)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow))
    }
}

// --- internal helpers --------------------------------------------------------------

fn ensure_not_paused(e: &Env) {
    if e.storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false)
    {
        panic_with_error!(e, Error::Paused);
    }
}

/// Allowlist compliance gate — currently a no-op (everyone allowed). Populating and
/// enforcing the list here later keeps subscribe/redeem the same shape.
fn ensure_allowed(_e: &Env, _who: &Address) {}

fn read_config(e: &Env) -> Config {
    e.storage()
        .instance()
        .get(&DataKey::Config)
        .unwrap_or_else(|| panic_with_error!(e, Error::NotInitialized))
}

fn write_config(e: &Env, config: &Config) {
    let s = e.storage().instance();
    s.set(&DataKey::Config, config);
    s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
}

fn read_token(e: &Env) -> Address {
    e.storage()
        .instance()
        .get(&DataKey::Token)
        .unwrap_or_else(|| panic_with_error!(e, Error::TokenNotSet))
}

fn read_i128(e: &Env, key: &DataKey) -> i128 {
    let s = e.storage().instance();
    s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
    s.get(key)
        .unwrap_or_else(|| panic_with_error!(e, Error::NotInitialized))
}

// --- pure share math (multiply-before-divide, floor, virtual offset) ---------------
// Kept as free functions of plain integers so they can be exhaustively unit-tested
// without an Env. Round DOWN everywhere so rounding always favors the vault.

fn shares_for_deposit(assets_in: i128, total_shares: i128, total_assets: i128) -> Option<i128> {
    let num = assets_in.checked_mul(total_shares.checked_add(VIRTUAL_SHARES)?)?;
    let den = total_assets.checked_add(VIRTUAL_ASSETS)?;
    num.checked_div(den)
}

fn assets_for_shares(shares_in: i128, total_shares: i128, total_assets: i128) -> Option<i128> {
    let num = shares_in.checked_mul(total_assets.checked_add(VIRTUAL_ASSETS)?)?;
    let den = total_shares.checked_add(VIRTUAL_SHARES)?;
    num.checked_div(den)
}

#[cfg(test)]
mod test;
