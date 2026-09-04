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
    panic_with_error, symbol_short, token::TokenClient, Address, BytesN, Env, Symbol, Vec,
};

/// SEP-40 price feed types, redeclared here so the vault can call any SEP-40 oracle
/// (Reflector's, or any other) without importing it. Soroban contract types match
/// structurally over XDR, so a local declaration interops with the real thing.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum Asset {
    Stellar(Address),
    Other(Symbol),
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PriceData {
    pub price: i128,
    pub timestamp: u64,
}

#[contractclient(name = "PriceFeedClient")]
pub trait Sep40PriceFeed {
    fn lastprice(env: Env, asset: Asset) -> Option<PriceData>;
    fn decimals(env: Env) -> u32;
}

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
// Per-user redemption requests live in persistent storage; bump on access too. The
// threshold is derived per-entry in `bump_request_ttl`, since a request's window depends
// on the notice period it was created under.
const PERSISTENT_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;

// --- Share-math virtual offset (defense-in-depth) ----------------------------------
// The primary inflation vector is already closed by bookkept NAV (see module docs);
// the +1/+1 offset guards rounding games at tiny totals.
const VIRTUAL_ASSETS: i128 = 1;
const VIRTUAL_SHARES: i128 = 1;

/// Minimum first deposit (belt-and-suspenders with the virtual offset). 1.0 baUSD at
/// 7 decimals.
const MIN_INITIAL_DEPOSIT: i128 = 10_000_000;

/// Governance upgrade timelock, fixed at 24h at compile time. Deliberately NOT a config
/// parameter: an admin who can shorten the timelock has defeated it, so changing this
/// requires a contract upgrade, which is itself subject to the current timelock.
const UPGRADE_TIMELOCK_SECS: u64 = 86_400;

/// How far the settlement asset (USDC) may drift from $1 before new deposits are refused.
/// 2% — wide enough to ignore ordinary noise, tight enough that a real depeg stops the
/// mispricing described on `ensure_settlement_asset_pegged`.
const MAX_DEPEG_BPS: i128 = 200;

/// Oracle prices older than this are treated as unusable. Reflector's public feeds tick on
/// the order of minutes, so an hour is generous.
const ORACLE_MAX_AGE_SECS: u64 = 60 * 60;

/// Allowlist entries get a long window (90 days) and are bumped on every access. An
/// archived entry reads as "not allowed", so a KYC'd LP could otherwise be locked out of
/// their own position by rent expiry rather than by any compliance decision.
const ALLOWLIST_BUMP_AMOUNT: u32 = 90 * DAY_IN_LEDGERS;
const ALLOWLIST_LIFETIME_THRESHOLD: u32 = ALLOWLIST_BUMP_AMOUNT - DAY_IN_LEDGERS;

/// Soroban ledgers close on a ~5s cadence. Used to size redemption-request TTLs in
/// ledgers from a notice period expressed in seconds.
const SECS_PER_LEDGER: u64 = 5;

/// Basis-point denominator for the NAV delta bound.
const BPS_DENOM: i128 = 10_000;

/// Maximum NAV move per routine attestation, in basis points (2%). Reinsurance NAV moves
/// slowly in normal conditions; anything larger is either an error or a catastrophe, and
/// both deserve the extraordinary path rather than a routine update.
const MAX_NAV_DELTA_BPS: i128 = 200;

/// Default minimum spacing between routine NAV updates (~20h). Without a cadence floor the
/// delta cap is far weaker than it looks: repeated small updates could walk NAV a long way
/// in a short time. Governable via `set_nav_cadence` - see there for the trade-off.
const DEFAULT_NAV_INTERVAL_SECS: u64 = 20 * 60 * 60;

/// NAV older than this (48h - roughly two missed daily cycles) is stale. Stale NAV blocks
/// NEW DEPOSITS only; exits are never blocked, since an operational failure to attest is
/// the vault's fault and must not trap an LP's capital.
const MAX_NAV_AGE_SECS: u64 = 48 * 60 * 60;

/// Share price is reported scaled by 10^7, matching baUSD's decimals.
const PRICE_SCALE: i128 = 10_000_000;

/// Upper bound on the redemption notice period (90 days). Bounded so a request's storage
/// entry can always be kept alive past its own maturity - see `request_ttl_ledgers`.
const MAX_NOTICE_PERIOD_SECS: u64 = 90 * 24 * 60 * 60;

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
pub struct PriceOracleSet {
    #[topic]
    pub oracle: Option<Address>,
}
#[contractevent]
pub struct AllowlistUpdated {
    #[topic]
    pub who: Address,
    pub allowed: bool,
}
#[contractevent]
pub struct AllowlistEnabledSet {
    pub enabled: bool,
}
#[contractevent]
pub struct SleeveFunded {
    pub amount: i128,
}
#[contractevent]
pub struct CapitalDeployed {
    pub amount: i128,
}
#[contractevent]
pub struct RedemptionsSuspendedSet {
    pub suspended: bool,
}
#[contractevent]
pub struct RedemptionPartiallyFilled {
    #[topic]
    pub from: Address,
    pub shares_burned: i128,
    pub assets_paid: i128,
    pub shares_remaining: i128,
}
#[contractevent]
pub struct NavUpdated {
    pub old_total_assets: i128,
    pub new_total_assets: i128,
    #[topic]
    pub proof_ref: BytesN<32>,
    /// True when the move exceeded the routine delta cap and took the governance path.
    pub extraordinary: bool,
}
#[contractevent]
pub struct AttestorsUpdated {
    pub count: u32,
    pub threshold: u32,
}
#[contractevent]
pub struct NavCadenceUpdated {
    pub interval_secs: u64,
}
#[contractevent]
pub struct RedemptionCancelled {
    #[topic]
    pub from: Address,
    pub shares: i128,
}
#[contractevent]
pub struct AdminProposed {
    #[topic]
    pub new_admin: Address,
}
#[contractevent]
pub struct NoticePeriodUpdated {
    pub notice_period: u64,
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
    NoticeTooLong = 15,
    NoPendingAdmin = 16,
    NavStale = 17,
    NavDeltaTooLarge = 18,
    NavTooSoon = 19,
    InsufficientAttestations = 20,
    UnknownAttestor = 21,
    DuplicateAttestor = 22,
    InvalidThreshold = 23,
    NoSharesOutstanding = 24,
    RedemptionsSuspended = 25,
    NotAllowed = 26,
    CadenceTooLong = 29,
    SettlementAssetDepegged = 27,
    OraclePriceUnavailable = 28,
}

/// Vault configuration and role addresses. Separation of duties: each authority can do
/// exactly one class of thing. Stored as swappable addresses so mainnet multisig drops in
/// later without code changes. (The token address lives in its own slot, set once via
/// `set_token` after the token is deployed with this vault as its owner.)
#[contracttype]
#[derive(Clone)]
pub struct Config {
    pub admin: Address,                // governance: upgrade + authority setters
    pub guardian: Address,             // pause only
    pub compliance_authority: Address, // allowlist only
    pub treasury: Address,             // sleeve in/out only
    pub usdc: Address,                 // USDC Stellar Asset Contract
    pub notice_period: u64,            // redemption notice, seconds
                                       // NAV attestation is an m-of-n SET, not a single address - see DataKey::Attestors.
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
    NavBaseline,    // total_assets as of the last attestation; bounds the delta cap
    NavInterval,    // minimum spacing between routine attestations, seconds
    Paused,
    PriceOracle,          // optional SEP-40 feed used as a USDC depeg guard
    PriceOracleAsset,     // the Asset identifier this feed knows USDC by
    AllowlistEnabled,     // master switch for compliance gating
    Allowlist(Address),   // per-LP compliance flag (PERSISTENT: unbounded user data)
    RedemptionsSuspended, // deliberate economic gate, distinct from the guardian pause
    Attestors,            // Vec<Address> authorized to attest NAV
    AttestationThreshold, // m of n: how many of them must sign one update
    PendingAdmin,         // admin handover awaiting acceptance by the proposed address
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
        if notice_period > MAX_NOTICE_PERIOD_SECS {
            panic_with_error!(e, Error::NoticeTooLong);
        }
        let config = Config {
            admin,
            guardian,
            compliance_authority,
            treasury,
            usdc,
            notice_period,
        };
        let s = e.storage().instance();
        s.set(&DataKey::Config, &config);
        // Seed a 1-of-1 attestor set from the deploy-time authority. Governance widens it
        // to a real m-of-n quorum via `set_attestors` before any capital is at risk; the
        // code path is identical either way, so there is no special-case single-signer
        // branch to get wrong later.
        let mut seed = Vec::new(e);
        seed.push_back(attestation_authority);
        s.set(&DataKey::Attestors, &seed);
        s.set(&DataKey::AttestationThreshold, &1u32);
        s.set(&DataKey::TotalShares, &0i128);
        s.set(&DataKey::TotalAssets, &0i128);
        s.set(&DataKey::NavBaseline, &0i128);
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
        // Never sell shares at a price we cannot currently vouch for. Exits deliberately
        // carry no such gate. An EMPTY vault is exempt: with no shares outstanding there
        // is no price to vouch for and nobody to dilute — and `update_nav` refuses to
        // attest a shareless vault, so gating here would wedge deposits permanently once
        // everyone has redeemed and the last attestation has aged out.
        if read_i128(e, &DataKey::TotalShares) > 0 {
            ensure_nav_fresh(e);
        }
        ensure_settlement_asset_pegged(e);
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
        // Checked so an overflow surfaces as a typed MathOverflow the dApp can render,
        // rather than an opaque wasm trap.
        let new_ta = ta
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
        let new_ts = ts
            .checked_add(shares)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
        let s = e.storage().instance();
        s.set(&DataKey::TotalAssets, &new_ta);
        s.set(&DataKey::TotalShares, &new_ts);
        // The FIRST deposit establishes the NAV baseline the delta cap is measured against.
        // Later deposits deliberately do NOT raise it - see `ensure_nav_delta_within_bound`.
        // It also restarts the freshness clock: the deposit itself is the bootstrap
        // attestation (shares are minted 1:1 by construction), and the authority cannot
        // re-attest until shares exist. The baseline is the full NAV the new shares own,
        // including any residual the previous holders' rounding left behind.
        if ts == 0 {
            s.set(&DataKey::NavBaseline, &new_ta);
            s.set(&DataKey::NavLastUpdated, &e.ledger().timestamp());
        }
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

        let claimable_at = e
            .ledger()
            .timestamp()
            .checked_add(config.notice_period)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
        let req = RedemptionRequest {
            shares,
            claimable_at,
        };
        e.storage().persistent().set(&key, &req);
        // TTL must outlive the notice period, or the entry archives before it can ever be
        // claimed and the escrowed shares are only recoverable via state restoration.
        bump_request_ttl(e, &key, config.notice_period);

        RedemptionRequested {
            from,
            shares,
            claimable_at: req.claimable_at,
        }
        .publish(e);
    }

    /// Claim a matured redemption: burn the escrowed baUSD and pay USDC at claim-time NAV.
    pub fn claim_redemption(e: &Env, from: Address) -> i128 {
        from.require_auth();
        ensure_not_paused(e);
        ensure_redemptions_not_suspended(e);
        // Claims ARE compliance-gated: this pays USDC out of the vault. `cancel_redemption`
        // deliberately is not, so a de-allowlisted holder can always recover their own
        // escrowed shares even while barred from exiting to cash.
        ensure_allowed(e, &from);

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
        // Keep the entry alive on access: a claim that reverts below (short sleeve, pause)
        // must not leave the request closer to archival than it started.
        bump_request_ttl(e, &key, config.notice_period);
        let token_addr = read_token(e);
        let ts = read_i128(e, &DataKey::TotalShares);
        let ta = read_i128(e, &DataKey::TotalAssets);

        // Payout at CURRENT (claim-time) NAV, rounded DOWN.
        let assets = assets_for_shares(req.shares, ts, ta)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));

        // Sleeve liquidity. Most capital sits in treaties, so a sleeve too small to pay
        // in full is an EXPECTED state, not a failure. Pay what the sleeve covers and
        // leave the remainder queued, rather than failing the whole claim.
        let usdc = TokenClient::new(e, &config.usdc);
        let vault_addr = e.current_contract_address();
        let available = usdc.balance(&vault_addr);
        if available <= 0 {
            panic_with_error!(e, Error::InsufficientSleeve);
        }

        let (shares_to_burn, assets_to_pay) = if assets <= available {
            (req.shares, assets)
        } else {
            // Partial fill: burn only the shares the sleeve actually covers. Both
            // conversions floor, so rounding favors the vault at each step.
            let covered = shares_for_deposit(available, ts, ta)
                .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
            if covered <= 0 || covered >= req.shares {
                // Nothing meaningful to pay, or the arithmetic disagrees with the branch
                // we are in - refuse rather than guess.
                panic_with_error!(e, Error::InsufficientSleeve);
            }
            let pay = assets_for_shares(covered, ts, ta)
                .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
            (covered, pay)
        };
        let shares_remaining = req.shares - shares_to_burn;

        // Effects: shrink bookkept totals and drop the request. The totals must actually
        // cover what is leaving; enforce that rather than assuming the invariant holds.
        let new_ts = ts
            .checked_sub(shares_to_burn)
            .filter(|v| *v >= 0)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
        let new_ta = ta
            .checked_sub(assets_to_pay)
            .filter(|v| *v >= 0)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
        let s = e.storage().instance();
        s.set(&DataKey::TotalShares, &new_ts);
        s.set(&DataKey::TotalAssets, &new_ta);
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        if shares_remaining > 0 {
            // Keep the request alive, reduced. `claimable_at` is unchanged: the holder
            // already served their notice and must not restart it because the vault was
            // short.
            let rest = RedemptionRequest {
                shares: shares_remaining,
                claimable_at: req.claimable_at,
            };
            e.storage().persistent().set(&key, &rest);
            bump_request_ttl(e, &key, config.notice_period);
        } else {
            e.storage().persistent().remove(&key);
        }

        // Interactions: burn the escrowed baUSD (vault authorizes as holder), pay USDC out.
        TokenClient::new(e, &token_addr).burn(&vault_addr, &shares_to_burn);
        usdc.transfer(&vault_addr, from.clone(), &assets_to_pay);

        if shares_remaining > 0 {
            RedemptionPartiallyFilled {
                from,
                shares_burned: shares_to_burn,
                assets_paid: assets_to_pay,
                shares_remaining,
            }
            .publish(e);
        } else {
            RedemptionClaimed {
                from,
                shares: shares_to_burn,
                assets: assets_to_pay,
            }
            .publish(e);
        }
        assets_to_pay
    }

    /// Cancel a pending redemption and take back the escrowed baUSD.
    ///
    /// Deliberately gated on NOTHING but the caller's own auth - not pause, not the
    /// allowlist. This returns the caller's own shares and moves no USDC, so blocking it
    /// would recreate exactly the trap it exists to prevent: shares escrowed, claim
    /// impossible (short sleeve, pause, or a NAV the holder will not accept), and no way
    /// out. A request that cannot be claimed must always be reversible.
    pub fn cancel_redemption(e: &Env, from: Address) {
        from.require_auth();

        let key = DataKey::Redemption(from.clone());
        let req: RedemptionRequest = e
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(e, Error::NoRequest));

        let token_addr = read_token(e);

        // Effects before interactions: drop the request, then release the escrow.
        e.storage().persistent().remove(&key);
        TokenClient::new(e, &token_addr).transfer(
            &e.current_contract_address(),
            &from,
            &req.shares,
        );

        RedemptionCancelled {
            from,
            shares: req.shares,
        }
        .publish(e);
    }

    /// The caller's pending redemption, if any. Bumps the entry's TTL on access.
    pub fn get_redemption(e: &Env, who: Address) -> Option<RedemptionRequest> {
        let key = DataKey::Redemption(who);
        let found: Option<RedemptionRequest> = e.storage().persistent().get(&key);
        if found.is_some() {
            bump_request_ttl(e, &key, read_config(e).notice_period);
        }
        found
    }

    // --- authority setters (governance/admin only; swap roles without redeploy) -----

    /// Step 1 of the admin handover: nominate `new_admin`. Nothing changes yet.
    ///
    /// Two-step by design, unlike the other authority setters. Admin is the only role
    /// that can destroy its own recovery path: a single-step handover to a mistyped or
    /// uncontrollable address would permanently disable every admin-gated function,
    /// INCLUDING upgrade - the escape hatch for any other bug - with user funds inside.
    /// Requiring the nominee to accept makes an unreachable address unreachable by
    /// construction. A fresh proposal replaces any pending one.
    pub fn propose_admin(e: &Env, new_admin: Address) {
        read_config(e).admin.require_auth();
        let s = e.storage().instance();
        s.set(&DataKey::PendingAdmin, &new_admin);
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        AdminProposed { new_admin }.publish(e);
    }

    /// Step 2 of the admin handover: the nominated address claims the role, proving it
    /// can actually authorize. Only the pending admin can call this.
    pub fn accept_admin(e: &Env) {
        let pending: Address = e
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or_else(|| panic_with_error!(e, Error::NoPendingAdmin));
        pending.require_auth();

        let mut c = read_config(e);
        c.admin = pending.clone();
        write_config(e, &c);
        e.storage().instance().remove(&DataKey::PendingAdmin);

        AuthorityUpdated {
            role: symbol_short!("admin"),
            new: pending,
        }
        .publish(e);
    }

    pub fn get_pending_admin(e: &Env) -> Option<Address> {
        e.storage().instance().get(&DataKey::PendingAdmin)
    }

    /// Adjust the redemption notice period. Admin-gated and bounded by
    /// `MAX_NOTICE_PERIOD_SECS`.
    ///
    /// NOT retroactive: `RedemptionRequest` stores an absolute `claimable_at` fixed when
    /// the request was made, so already-pending requests keep the terms they were made
    /// under and governance cannot extend a lockup on capital already in the queue.
    pub fn set_notice_period(e: &Env, new_notice_period: u64) {
        let mut c = read_config(e);
        c.admin.require_auth();
        if new_notice_period > MAX_NOTICE_PERIOD_SECS {
            panic_with_error!(e, Error::NoticeTooLong);
        }
        c.notice_period = new_notice_period;
        write_config(e, &c);
        NoticePeriodUpdated {
            notice_period: new_notice_period,
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

    /// Replace the NAV attestation quorum. Admin-gated.
    ///
    /// `threshold` must be at least 1 and no greater than the number of attestors -
    /// a threshold above the set size would make NAV permanently un-updatable, and a
    /// threshold of zero would let anyone attest.
    pub fn set_attestors(e: &Env, attestors: Vec<Address>, threshold: u32) {
        read_config(e).admin.require_auth();
        if threshold == 0 || threshold > attestors.len() {
            panic_with_error!(e, Error::InvalidThreshold);
        }
        // Duplicates in the set would let one key satisfy several slots of the quorum.
        let n = attestors.len();
        for i in 0..n {
            let a = attestors.get(i).unwrap();
            for j in 0..i {
                if attestors.get(j).unwrap() == a {
                    panic_with_error!(e, Error::DuplicateAttestor);
                }
            }
        }

        let s = e.storage().instance();
        s.set(&DataKey::Attestors, &attestors);
        s.set(&DataKey::AttestationThreshold, &threshold);
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        AttestorsUpdated {
            count: n,
            threshold,
        }
        .publish(e);
    }

    pub fn get_attestors(e: &Env) -> Vec<Address> {
        read_attestors(e)
    }

    pub fn get_attestation_threshold(e: &Env) -> u32 {
        read_threshold(e)
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

    // --- settlement-asset depeg guard --------------------------------------------------

    /// Point the depeg guard at a SEP-40 price feed (Reflector's, or any other) and tell it
    /// which `Asset` that feed knows USDC by. Admin-gated. Pass `None` to disable.
    ///
    /// Optional on purpose: a testnet deployment uses a mock USDC that no public oracle
    /// carries, and the vault must remain deployable and testable without one.
    pub fn set_price_oracle(e: &Env, oracle: Option<Address>, asset: Option<Asset>) {
        read_config(e).admin.require_auth();
        let s = e.storage().instance();
        match (&oracle, &asset) {
            (Some(o), Some(a)) => {
                s.set(&DataKey::PriceOracle, o);
                s.set(&DataKey::PriceOracleAsset, a);
            }
            _ => {
                s.remove(&DataKey::PriceOracle);
                s.remove(&DataKey::PriceOracleAsset);
            }
        }
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        PriceOracleSet { oracle }.publish(e);
    }

    pub fn get_price_oracle(e: &Env) -> Option<Address> {
        e.storage().instance().get(&DataKey::PriceOracle)
    }

    /// The settlement asset's current price per the configured feed, if one is set.
    pub fn settlement_asset_price(e: &Env) -> Option<PriceData> {
        let oracle: Address = e.storage().instance().get(&DataKey::PriceOracle)?;
        let asset: Asset = e.storage().instance().get(&DataKey::PriceOracleAsset)?;
        PriceFeedClient::new(e, &oracle).lastprice(&asset)
    }

    // --- compliance allowlist ---------------------------------------------------------

    /// Add or remove an address from the allowlist. Compliance-authority-gated.
    ///
    /// KYC/AML happens entirely off-chain. On approval the compliance authority writes the
    /// address here; the contract never sees identity data, only allowlisted addresses.
    pub fn set_allowed(e: &Env, who: Address, allowed: bool) {
        read_config(e).compliance_authority.require_auth();
        let key = DataKey::Allowlist(who.clone());
        let p = e.storage().persistent();
        if allowed {
            p.set(&key, &true);
            p.extend_ttl(&key, ALLOWLIST_LIFETIME_THRESHOLD, ALLOWLIST_BUMP_AMOUNT);
        } else {
            p.remove(&key);
        }
        AllowlistUpdated { who, allowed }.publish(e);
    }

    /// Allowlist several addresses in one call - onboarding usually arrives in batches.
    pub fn set_allowed_many(e: &Env, addresses: Vec<Address>, allowed: bool) {
        read_config(e).compliance_authority.require_auth();
        let p = e.storage().persistent();
        for who in addresses.iter() {
            let key = DataKey::Allowlist(who.clone());
            if allowed {
                p.set(&key, &true);
                p.extend_ttl(&key, ALLOWLIST_LIFETIME_THRESHOLD, ALLOWLIST_BUMP_AMOUNT);
            } else {
                p.remove(&key);
            }
            AllowlistUpdated { who, allowed }.publish(e);
        }
    }

    /// Turn compliance gating on or off wholesale. Admin-gated, NOT compliance-gated:
    /// disabling the entire gate is a governance decision, while deciding who is on the
    /// list is the compliance authority's job. Separating them means the compliance
    /// authority cannot switch off its own oversight.
    pub fn set_allowlist_enabled(e: &Env, enabled: bool) {
        read_config(e).admin.require_auth();
        let s = e.storage().instance();
        s.set(&DataKey::AllowlistEnabled, &enabled);
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        AllowlistEnabledSet { enabled }.publish(e);
    }

    pub fn allowlist_enabled(e: &Env) -> bool {
        e.storage()
            .instance()
            .get(&DataKey::AllowlistEnabled)
            .unwrap_or(false)
    }

    /// Whether `who` may currently subscribe or claim. Always true while the gate is off.
    pub fn is_allowed(e: &Env, who: Address) -> bool {
        if !Self::allowlist_enabled(e) {
            return true;
        }
        read_allowed(e, &who)
    }

    // --- treasury: liquidity sleeve --------------------------------------------------

    /// Move USDC OUT of the vault to the treasury, to be deployed into reinsurance
    /// treaties. Treasury-gated.
    ///
    /// NAV-NEUTRAL by design: the capital still belongs to the vault, it has simply moved
    /// from the on-chain sleeve into treaties. `total_assets` is the attested NAV of
    /// everything the vault owns, on-chain or not, so it must NOT change here. Only
    /// `update_nav` moves NAV.
    pub fn deploy_capital(e: &Env, amount: i128) {
        let config = read_config(e);
        config.treasury.require_auth();
        ensure_not_paused(e);
        if amount <= 0 {
            panic_with_error!(e, Error::InvalidAmount);
        }

        let usdc = TokenClient::new(e, &config.usdc);
        let vault_addr = e.current_contract_address();
        if amount > usdc.balance(&vault_addr) {
            panic_with_error!(e, Error::InsufficientSleeve);
        }
        usdc.transfer(&vault_addr, &config.treasury, &amount);

        CapitalDeployed { amount }.publish(e);
    }

    /// Move USDC INTO the vault's sleeve from the treasury — returning capital from
    /// treaties, or topping up to meet redemptions. Treasury-gated.
    ///
    /// Also NAV-neutral, for the mirror-image reason: this is the same capital coming
    /// back, not new value. Attesting yield is `update_nav`'s job alone. Keeping these
    /// two concerns apart is what stops the sleeve balance from being mistaken for NAV.
    pub fn fund_sleeve(e: &Env, amount: i128) {
        let config = read_config(e);
        config.treasury.require_auth();
        if amount <= 0 {
            panic_with_error!(e, Error::InvalidAmount);
        }

        TokenClient::new(e, &config.usdc).transfer(
            &config.treasury,
            e.current_contract_address(),
            &amount,
        );

        SleeveFunded { amount }.publish(e);
    }

    /// USDC currently held on-chain and available to pay claims. This is NOT `total_assets`
    /// — most capital sits in treaties.
    pub fn sleeve_balance(e: &Env) -> i128 {
        let config = read_config(e);
        TokenClient::new(e, &config.usdc).balance(&e.current_contract_address())
    }

    /// Suspend or resume redemption claims. Admin-gated.
    ///
    /// Distinct from the guardian pause on purpose. Pause is an emergency brake on
    /// everything; suspension is a deliberate economic decision — capital is committed to
    /// treaties and cannot be recalled yet. Separating them keeps the roles honest: an
    /// operational emergency and a liquidity decision should not share one switch.
    ///
    /// Suspension blocks `claim_redemption`. It deliberately does NOT block
    /// `request_redemption` (holders may still queue) or `cancel_redemption` (holders may
    /// always leave the queue). Deferring an exit indefinitely is only acceptable if the
    /// holder can withdraw the request instead.
    pub fn set_redemptions_suspended(e: &Env, suspended: bool) {
        read_config(e).admin.require_auth();
        let s = e.storage().instance();
        s.set(&DataKey::RedemptionsSuspended, &suspended);
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        RedemptionsSuspendedSet { suspended }.publish(e);
    }

    pub fn redemptions_suspended(e: &Env) -> bool {
        e.storage()
            .instance()
            .get(&DataKey::RedemptionsSuspended)
            .unwrap_or(false)
    }

    // --- NAV attestation ------------------------------------------------------------

    /// Write a new attested NAV. Requires `threshold`-of-`n` attestor signatures on one
    /// transaction, plus a `proof_ref` hash committing to the off-chain attestation
    /// document.
    ///
    /// NAV MAY DECREASE - reinsurance NAV is not a yield curve, and a treaty loss is a real
    /// downward move. Routine updates are bounded two ways: at most `MAX_NAV_DELTA_BPS` per
    /// update, and no more often than the configured cadence. The cadence floor is what
    /// makes the delta cap meaningful; without it, repeated small updates could walk NAV
    /// anywhere in an afternoon.
    ///
    /// Deliberately callable while paused: pausing halts user flows, but the books should
    /// still be able to record reality, and with subscribe/claim frozen an update has no
    /// exploitable surface.
    pub fn update_nav(
        e: &Env,
        new_total_assets: i128,
        proof_ref: BytesN<32>,
        signers: Vec<Address>,
    ) {
        require_attestations(e, &signers);
        let old = prepare_nav_update(e, new_total_assets);
        ensure_nav_cadence(e);
        ensure_nav_delta_within_bound(e, old, new_total_assets);
        commit_nav(e, old, new_total_assets, proof_ref, false);
    }

    /// Write a NAV that exceeds the routine delta cap - a catastrophe writedown, or a
    /// correction after a missed cycle.
    ///
    /// Requires the attestor quorum AND governance, so a large move is always possible but
    /// can never be made quietly by the attestation quorum alone. The cadence floor and
    /// delta cap are waived; everything else (non-negative, shares outstanding, proof_ref,
    /// event) still applies, and the event marks it `extraordinary` so it is trivially
    /// auditable after the fact.
    pub fn update_nav_extraordinary(
        e: &Env,
        new_total_assets: i128,
        proof_ref: BytesN<32>,
        signers: Vec<Address>,
    ) {
        read_config(e).admin.require_auth();
        require_attestations(e, &signers);
        let old = prepare_nav_update(e, new_total_assets);
        commit_nav(e, old, new_total_assets, proof_ref, true);
    }

    /// Set the minimum spacing between routine NAV attestations. Admin-gated.
    ///
    /// The cadence floor is what gives the delta cap its teeth: without it, repeated
    /// within-cap updates could walk NAV a long way in a short time. Lowering it therefore
    /// WEAKENS that protection, and zero removes it entirely, leaving only the per-update
    /// size cap. Mainnet governance should set this to the real attestation cadence and
    /// leave it alone. It exists as a setting because testnet and demos need a short
    /// interval, and because the right production value is an operational fact rather than
    /// something to hardcode.
    ///
    /// Same trust model as `set_notice_period`: admin already controls upgrade, so this
    /// grants no authority admin did not effectively have.
    pub fn set_nav_cadence(e: &Env, interval_secs: u64) {
        read_config(e).admin.require_auth();
        // The cadence must fit inside the staleness window. An interval above
        // MAX_NAV_AGE_SECS would make NAV go stale BETWEEN permitted attestations, blocking
        // deposits on a schedule the operator configured themselves - a recurring outage
        // that looks like an attestation failure. Mirrors set_notice_period's bound.
        if interval_secs > MAX_NAV_AGE_SECS {
            panic_with_error!(e, Error::CadenceTooLong);
        }
        let s = e.storage().instance();
        s.set(&DataKey::NavInterval, &interval_secs);
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        NavCadenceUpdated { interval_secs }.publish(e);
    }

    /// `total_assets` as of the last attestation. The delta cap is measured against this,
    /// NOT against current `total_assets` - see `ensure_nav_delta_within_bound`.
    pub fn nav_baseline(e: &Env) -> i128 {
        read_i128(e, &DataKey::NavBaseline)
    }

    /// The largest absolute NAV move a routine attestation may currently make.
    ///
    /// Operators need this: after large subscriptions the attestable move is bounded by the
    /// OLDER baseline, so the permitted delta can be far smaller than a naive percentage of
    /// today's `total_assets` would suggest. Attesting without checking this is how a
    /// legitimate update gets rejected.
    pub fn max_nav_delta(e: &Env) -> i128 {
        let baseline = read_i128(e, &DataKey::NavBaseline);
        let current = read_i128(e, &DataKey::TotalAssets);
        let bound_base = if current < baseline {
            current
        } else {
            baseline
        };
        bound_base
            .checked_mul(MAX_NAV_DELTA_BPS)
            .and_then(|v| v.checked_div(BPS_DENOM))
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow))
    }

    pub fn nav_cadence(e: &Env) -> u64 {
        read_nav_interval(e)
    }

    pub fn nav_last_updated(e: &Env) -> u64 {
        read_nav_last_updated(e)
    }

    /// Whether NAV is older than `MAX_NAV_AGE_SECS`. Stale NAV blocks new deposits; it
    /// never blocks redemption requests, claims, or cancels.
    pub fn is_nav_stale(e: &Env) -> bool {
        e.ledger()
            .timestamp()
            .saturating_sub(read_nav_last_updated(e))
            > MAX_NAV_AGE_SECS
    }

    /// Assets per 1.0 baUSD, scaled by 10^7 (baUSD's decimals).
    ///
    /// The vault is the single source of truth for baUSD's price. baUSD does not trade on
    /// any market, so price cannot be discovered - it is published from attested NAV. An
    /// external SEP-40 price-feed adapter reads this, which is why it is exposed at a
    /// fixed scale rather than left for callers to derive.
    pub fn share_price(e: &Env) -> i128 {
        let ts = read_i128(e, &DataKey::TotalShares);
        let ta = read_i128(e, &DataKey::TotalAssets);
        assets_for_shares(PRICE_SCALE, ts, ta)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow))
    }

    // --- governance-gated upgrade with timelock -------------------------------------

    /// Queue an upgrade to `new_wasm_hash`, executable after the timelock. Admin-only.
    /// A fresh proposal replaces any pending one.
    pub fn propose_upgrade(e: &Env, new_wasm_hash: BytesN<32>) {
        read_config(e).admin.require_auth();
        let eta = e
            .ledger()
            .timestamp()
            .checked_add(UPGRADE_TIMELOCK_SECS)
            .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
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

    /// Shares required to redeem AT LEAST `assets`, rounded UP.
    ///
    /// The mirror of `convert_to_shares` for callers that must guarantee a minimum payout
    /// rather than price a deposit. Rounding UP still keeps the vault's favour: the caller
    /// surrenders slightly more shares, never fewer. Integrators wiring the vault into a
    /// protocol that expects an exact withdrawal amount need this - using the flooring
    /// conversion there lands a stroop short and the withdrawal fails.
    pub fn convert_to_shares_ceil(e: &Env, assets: i128) -> i128 {
        // The ceil adjustment below is only correct for non-negative numerators; reject
        // rather than return a silently-wrong figure.
        if assets < 0 {
            panic_with_error!(e, Error::InvalidAmount);
        }
        let ts = read_i128(e, &DataKey::TotalShares);
        let ta = read_i128(e, &DataKey::TotalAssets);
        shares_for_deposit_ceil(assets, ts, ta)
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

/// Read an allowlist flag, bumping its TTL so an active LP's entry cannot be archived out
/// from under them. Absent entry = not allowed.
fn read_allowed(e: &Env, who: &Address) -> bool {
    let key = DataKey::Allowlist(who.clone());
    let p = e.storage().persistent();
    if p.get::<DataKey, bool>(&key).unwrap_or(false) {
        p.extend_ttl(&key, ALLOWLIST_LIFETIME_THRESHOLD, ALLOWLIST_BUMP_AMOUNT);
        true
    } else {
        false
    }
}

/// Compliance gate on the value-moving user flows: `subscribe`, `request_redemption` and
/// `claim_redemption`.
///
/// Deliberately NOT applied to `cancel_redemption`: returning a holder's own escrowed
/// shares moves no USDC and must never be blockable, so a de-allowlisted address is barred
/// from exiting to cash but can always recover its shares. Transfers are not gated either
/// - that lives in the token contract and remains an open question.
fn ensure_allowed(e: &Env, who: &Address) {
    if !e
        .storage()
        .instance()
        .get(&DataKey::AllowlistEnabled)
        .unwrap_or(false)
    {
        return; // gate off: everyone allowed
    }
    if !read_allowed(e, who) {
        panic_with_error!(e, Error::NotAllowed);
    }
}

fn ensure_redemptions_not_suspended(e: &Env) {
    if e.storage()
        .instance()
        .get(&DataKey::RedemptionsSuspended)
        .unwrap_or(false)
    {
        panic_with_error!(e, Error::RedemptionsSuspended);
    }
}

fn read_attestors(e: &Env) -> Vec<Address> {
    e.storage()
        .instance()
        .get(&DataKey::Attestors)
        .unwrap_or_else(|| panic_with_error!(e, Error::NotInitialized))
}

fn read_threshold(e: &Env) -> u32 {
    e.storage()
        .instance()
        .get(&DataKey::AttestationThreshold)
        .unwrap_or_else(|| panic_with_error!(e, Error::NotInitialized))
}

fn read_nav_last_updated(e: &Env) -> u64 {
    e.storage()
        .instance()
        .get(&DataKey::NavLastUpdated)
        .unwrap_or_else(|| panic_with_error!(e, Error::NotInitialized))
}

/// Verify an m-of-n attestation: every signer must be a known attestor, no signer may be
/// counted twice, at least `threshold` of them must sign, and each must authorize THIS
/// invocation. Auth is enforced per signer via `require_auth`, so there is no bespoke
/// signature verification to get wrong - Soroban's own auth framework does the checking.
fn require_attestations(e: &Env, signers: &Vec<Address>) {
    let attestors = read_attestors(e);
    let threshold = read_threshold(e);

    if signers.len() < threshold {
        panic_with_error!(e, Error::InsufficientAttestations);
    }

    let n = signers.len();
    for i in 0..n {
        let signer = signers.get(i).unwrap();

        let mut known = false;
        for a in attestors.iter() {
            if a == signer {
                known = true;
                break;
            }
        }
        if !known {
            panic_with_error!(e, Error::UnknownAttestor);
        }

        // One key must not fill several slots of the quorum.
        for j in 0..i {
            if signers.get(j).unwrap() == signer {
                panic_with_error!(e, Error::DuplicateAttestor);
            }
        }

        signer.require_auth();
    }
}

/// Shared validation for both NAV paths. Returns the previous NAV.
fn prepare_nav_update(e: &Env, new_total_assets: i128) -> i128 {
    if new_total_assets < 0 {
        panic_with_error!(e, Error::InvalidAmount);
    }
    // With no shares outstanding there is nothing to revalue, and a nonzero NAV against
    // zero shares would hand the entire balance to the next depositor.
    if read_i128(e, &DataKey::TotalShares) == 0 {
        panic_with_error!(e, Error::NoSharesOutstanding);
    }
    read_i128(e, &DataKey::TotalAssets)
}

fn read_nav_interval(e: &Env) -> u64 {
    e.storage()
        .instance()
        .get(&DataKey::NavInterval)
        .unwrap_or(DEFAULT_NAV_INTERVAL_SECS)
}

fn ensure_nav_cadence(e: &Env) {
    let last = read_nav_last_updated(e);
    if e.ledger().timestamp() < last.saturating_add(read_nav_interval(e)) {
        panic_with_error!(e, Error::NavTooSoon);
    }
}

/// |new - old| must be within MAX_NAV_DELTA_BPS of the NAV BASELINE - not of `old`.
///
/// The distinction is the whole point. `old` is `total_assets`, which any caller can inflate
/// on demand simply by depositing: subscribe adds to it immediately. Measuring the cap
/// against `old` would let an attacker deposit a large sum, have a compromised quorum attest
/// a percentage of the inflated total, redeem, and walk away with far more fabricated value
/// than the cap was ever meant to permit - with a zero notice period, all in one transaction.
/// The cadence floor does not help: it limits how OFTEN that happens, not how large it is.
///
/// So the bound is a percentage of the last ATTESTED total (set by `commit_nav`, and by the
/// first deposit at bootstrap), floored by the current total so large redemptions shrink the
/// budget too. Deposits are real value and are fully credited to `total_assets`; they simply
/// do not expand how much a quorum may fabricate before being verified.
///
/// Multiply before divide - in fact never divide at all - so the bound is exact at any
/// magnitude.
fn ensure_nav_delta_within_bound(e: &Env, old: i128, new: i128) {
    let baseline = read_i128(e, &DataKey::NavBaseline);
    let bound_base = if old < baseline { old } else { baseline };
    let diff = if new > old { new - old } else { old - new };
    let lhs = diff
        .checked_mul(BPS_DENOM)
        .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
    let rhs = bound_base
        .checked_mul(MAX_NAV_DELTA_BPS)
        .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
    if lhs > rhs {
        panic_with_error!(e, Error::NavDeltaTooLarge);
    }
}

fn commit_nav(e: &Env, old: i128, new: i128, proof_ref: BytesN<32>, extraordinary: bool) {
    let s = e.storage().instance();
    s.set(&DataKey::TotalAssets, &new);
    // The attested figure becomes the baseline the next delta cap is measured against.
    s.set(&DataKey::NavBaseline, &new);
    s.set(&DataKey::NavLastUpdated, &e.ledger().timestamp());
    s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
    NavUpdated {
        old_total_assets: old,
        new_total_assets: new,
        proof_ref,
        extraordinary,
    }
    .publish(e);
}

/// Refuse new deposits while the settlement asset has drifted from its peg.
///
/// NAV is attested in USD terms, but subscriptions and redemptions settle in USDC. While
/// USDC trades at $1 those are the same thing. If it depegs they are not, and the gap is
/// directly exploitable: at $0.90, depositing 100 USDC (worth $90) mints shares against
/// $100 of NAV, taking $10 of value from existing holders on every deposit.
///
/// Gated on SUBSCRIBE ONLY, consistent with every other gate in this contract. A holder who
/// wants out during a depeg — arguably exactly when they want out most — is never blocked,
/// even though they receive USDC worth less than the NAV it represents. That is their call
/// to make, not ours.
///
/// Fails CLOSED: if no price is available or the feed has gone stale, deposits stop. This
/// hands the oracle the power to halt subscriptions, which is a real dependency and the
/// reason the guard is optional. The blast radius is deliberately limited to deposits.
fn ensure_settlement_asset_pegged(e: &Env) {
    let s = e.storage().instance();
    let oracle: Address = match s.get(&DataKey::PriceOracle) {
        Some(o) => o,
        None => return, // guard not configured
    };
    let asset: Asset = match s.get(&DataKey::PriceOracleAsset) {
        Some(a) => a,
        None => return,
    };

    let client = PriceFeedClient::new(e, &oracle);
    let quote = match client.lastprice(&asset) {
        Some(q) => q,
        None => panic_with_error!(e, Error::OraclePriceUnavailable),
    };

    if e.ledger().timestamp().saturating_sub(quote.timestamp) > ORACLE_MAX_AGE_SECS {
        panic_with_error!(e, Error::OraclePriceUnavailable);
    }

    // The feed reports prices scaled by 10^decimals, so parity is exactly that scale.
    let parity = 10i128
        .checked_pow(client.decimals())
        .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
    let drift = if quote.price > parity {
        quote.price - parity
    } else {
        parity - quote.price
    };
    // Multiply before divide; never divide at all.
    let lhs = drift
        .checked_mul(BPS_DENOM)
        .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
    let rhs = parity
        .checked_mul(MAX_DEPEG_BPS)
        .unwrap_or_else(|| panic_with_error!(e, Error::MathOverflow));
    if lhs > rhs {
        panic_with_error!(e, Error::SettlementAssetDepegged);
    }
}

fn ensure_nav_fresh(e: &Env) {
    if e.ledger()
        .timestamp()
        .saturating_sub(read_nav_last_updated(e))
        > MAX_NAV_AGE_SECS
    {
        panic_with_error!(e, Error::NavStale);
    }
}

/// TTL for a redemption request, in ledgers: the standard persistent window PLUS the
/// notice period. A request must never archive before it becomes claimable, otherwise the
/// escrowed shares are recoverable only through state restoration rather than the
/// contract's own interface. `notice_period` is bounded by `MAX_NOTICE_PERIOD_SECS` so
/// this cannot exceed Soroban's maximum entry TTL.
fn request_ttl_ledgers(notice_period: u64) -> u32 {
    let notice_ledgers = (notice_period / SECS_PER_LEDGER) as u32;
    PERSISTENT_BUMP_AMOUNT.saturating_add(notice_ledgers)
}

/// Extend a redemption request's TTL on access.
fn bump_request_ttl(e: &Env, key: &DataKey, notice_period: u64) {
    let extend_to = request_ttl_ledgers(notice_period);
    let threshold = extend_to.saturating_sub(DAY_IN_LEDGERS);
    e.storage()
        .persistent()
        .extend_ttl(key, threshold, extend_to);
}

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
//
// A vault with NO shares outstanding is priced at exactly 1:1, regardless of what
// `total_assets` reads. Two states reach it: a fresh deployment (assets 0) and the
// everyone-redeemed state, where floor rounding on the final claims leaves a few stroops
// of attested NAV that no share owns. Feeding that residual through the virtual-offset
// formula would price the next deposit at (residual + 1) assets per share - one leftover
// stroop doubles the share price, and the SEP-40 feed would publish that jump although
// nothing economic happened. Instead the next deposit re-bootstraps the vault at 1.0 and
// the residual is simply folded into the NAV it owns (see `subscribe`).

fn shares_for_deposit(assets_in: i128, total_shares: i128, total_assets: i128) -> Option<i128> {
    if total_shares == 0 {
        return Some(assets_in);
    }
    let num = assets_in.checked_mul(total_shares.checked_add(VIRTUAL_SHARES)?)?;
    let den = total_assets.checked_add(VIRTUAL_ASSETS)?;
    num.checked_div(den)
}

/// As `shares_for_deposit`, but rounded UP. Used where a caller must be guaranteed at least
/// a given payout; see `convert_to_shares_ceil`.
fn shares_for_deposit_ceil(
    assets_in: i128,
    total_shares: i128,
    total_assets: i128,
) -> Option<i128> {
    if total_shares == 0 {
        return Some(assets_in);
    }
    let num = assets_in.checked_mul(total_shares.checked_add(VIRTUAL_SHARES)?)?;
    let den = total_assets.checked_add(VIRTUAL_ASSETS)?;
    // ceil(num/den) for non-negative num, den > 0.
    num.checked_add(den.checked_sub(1)?)?.checked_div(den)
}

fn assets_for_shares(shares_in: i128, total_shares: i128, total_assets: i128) -> Option<i128> {
    if total_shares == 0 {
        return Some(shares_in);
    }
    let num = shares_in.checked_mul(total_assets.checked_add(VIRTUAL_ASSETS)?)?;
    let den = total_shares.checked_add(VIRTUAL_SHARES)?;
    num.checked_div(den)
}

#[cfg(test)]
mod test;
