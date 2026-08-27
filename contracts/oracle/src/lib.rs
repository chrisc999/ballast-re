#![no_std]
//! baUSD price feed — a SEP-40 oracle publishing the vault's attested share price.
//!
//! baUSD does not trade on any market. There is no order book to observe and no AMM to
//! sample, so its price cannot be DISCOVERED the way an oracle network discovers the price
//! of a liquid asset — it can only be PUBLISHED, from NAV attested off-chain against real
//! reinsurance treaty performance.
//!
//! SEP-40 is a pure interface standard: any contract may implement `PriceFeedTrait`, and
//! there is no registration with any provider. So rather than trying to get baUSD listed on
//! a network that samples trades, this contract implements the same interface those networks
//! implement. Anything that can already read a SEP-40 feed can read baUSD's NAV, unchanged.
//!
//! The vault remains the single source of truth. This contract stores no price of its own
//! choosing: it reads `share_price()` and `nav_last_updated()` from the vault and records
//! them. `record()` is deliberately permissionless — it can only ever copy the vault's own
//! numbers, so letting anyone keep the history current costs nothing and removes an
//! operational dependency on us.

use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype,
    panic_with_error, Address, Env, Symbol, Vec,
};

/// Minimal view of the vault: the attested share price and when it was last attested.
#[contractclient(name = "VaultClient")]
pub trait VaultPrice {
    fn share_price(env: Env) -> i128;
    fn nav_last_updated(env: Env) -> u64;
}

// --- TTL / rent ---------------------------------------------------------------------
const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_LIFETIME_THRESHOLD: u32 = INSTANCE_BUMP_AMOUNT - DAY_IN_LEDGERS;
// History entries are kept for a year: a price record is only useful if it outlives the
// period someone might want to look back over.
const HISTORY_BUMP_AMOUNT: u32 = 365 * DAY_IN_LEDGERS;
const HISTORY_LIFETIME_THRESHOLD: u32 = HISTORY_BUMP_AMOUNT - DAY_IN_LEDGERS;

/// Ring-buffer capacity: ~one year of daily attestations. Bounded on purpose — an
/// ever-growing history would be unbounded state, and the oldest entries are the least
/// useful.
const CAPACITY: u32 = 365;

/// baUSD reports 7 decimals, and `share_price` is already scaled by 10^7.
const DECIMALS: u32 = 7;

/// Nominal tick period in seconds. NAV is attested about daily; SEP-40 consumers use this
/// to reason about how often a new point can appear.
const RESOLUTION: u32 = 86_400;

/// SEP-40 asset identifier.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum Asset {
    Stellar(Address),
    Other(Symbol),
}

/// SEP-40 price record. `price` is scaled by 10^`decimals()`.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PriceData {
    pub price: i128,
    pub timestamp: u64,
}

#[contractevent]
pub struct PriceRecorded {
    pub price: i128,
    pub timestamp: u64,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    /// The vault has not attested anything newer than the last recorded point.
    NoNewPrice = 2,
}

#[contracttype]
pub enum DataKey {
    Vault,
    /// The quoted asset (baUSD) and the base it is quoted in (USD).
    QuotedAsset,
    BaseAsset,
    /// Ring buffer: next slot to write, and how many slots are populated.
    Head,
    Count,
    Point(u32),
}

#[contract]
pub struct PriceFeedContract;

#[contractimpl]
impl PriceFeedContract {
    /// `vault` is the baUSD vault; `quoted` is the baUSD token address; `base` is the unit
    /// prices are expressed in (USD).
    pub fn __constructor(e: &Env, vault: Address, quoted: Address, base: Symbol) {
        let s = e.storage().instance();
        s.set(&DataKey::Vault, &vault);
        s.set(&DataKey::QuotedAsset, &Asset::Stellar(quoted));
        s.set(&DataKey::BaseAsset, &Asset::Other(base));
        s.set(&DataKey::Head, &0u32);
        s.set(&DataKey::Count, &0u32);
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
    }

    /// Copy the vault's current share price into the history.
    ///
    /// Permissionless by design: this can only ever record what the vault already says, so
    /// there is nothing to gain by calling it and nothing to lose by letting anyone do so.
    /// Reverts with `NoNewPrice` unless the vault's NAV timestamp has advanced, which makes
    /// the call idempotent and stops repeat calls from filling the buffer with duplicates.
    pub fn record(e: &Env) -> PriceData {
        let vault: Address = read_instance(e, &DataKey::Vault);
        let client = VaultClient::new(e, &vault);
        let timestamp = client.nav_last_updated();
        let price = client.share_price();

        if let Some(latest) = Self::lastprice_inner(e) {
            if timestamp <= latest.timestamp {
                panic_with_error!(e, Error::NoNewPrice);
            }
        }

        let head: u32 = read_instance(e, &DataKey::Head);
        let count: u32 = read_instance(e, &DataKey::Count);
        let point = PriceData { price, timestamp };

        let key = DataKey::Point(head);
        let p = e.storage().persistent();
        p.set(&key, &point);
        p.extend_ttl(&key, HISTORY_LIFETIME_THRESHOLD, HISTORY_BUMP_AMOUNT);

        let s = e.storage().instance();
        s.set(&DataKey::Head, &((head + 1) % CAPACITY));
        if count < CAPACITY {
            s.set(&DataKey::Count, &(count + 1));
        }
        s.extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        PriceRecorded { price, timestamp }.publish(e);
        point
    }

    pub fn vault(e: &Env) -> Address {
        read_instance(e, &DataKey::Vault)
    }

    /// The vault's share price right now, bypassing the recorded history. Useful for
    /// callers that want the live figure rather than the last recorded point.
    pub fn current_price(e: &Env) -> PriceData {
        let vault: Address = read_instance(e, &DataKey::Vault);
        let client = VaultClient::new(e, &vault);
        PriceData {
            price: client.share_price(),
            timestamp: client.nav_last_updated(),
        }
    }

    // --- SEP-40 -----------------------------------------------------------------------

    pub fn base(e: &Env) -> Asset {
        read_instance(e, &DataKey::BaseAsset)
    }

    pub fn assets(e: &Env) -> Vec<Asset> {
        let mut v = Vec::new(e);
        v.push_back(read_instance::<Asset>(e, &DataKey::QuotedAsset));
        v
    }

    pub fn decimals(_e: &Env) -> u32 {
        DECIMALS
    }

    pub fn resolution(_e: &Env) -> u32 {
        RESOLUTION
    }

    /// Most recent recorded price for `asset`, or None if the asset is not baUSD or nothing
    /// has been recorded yet.
    pub fn lastprice(e: &Env, asset: Asset) -> Option<PriceData> {
        if !Self::is_quoted(e, &asset) {
            return None;
        }
        Self::lastprice_inner(e)
    }

    /// The most recent recorded price at or before `timestamp`.
    ///
    /// SEP-40 leaves the exact semantics to the implementer. "At or before" is the honest
    /// choice for attested NAV: it never invents a value for a moment we had not yet
    /// attested, and it never reports a price from the future.
    pub fn price(e: &Env, asset: Asset, timestamp: u64) -> Option<PriceData> {
        if !Self::is_quoted(e, &asset) {
            return None;
        }
        let mut best: Option<PriceData> = None;
        for point in Self::history(e).iter() {
            if point.timestamp <= timestamp {
                let better = match &best {
                    Some(b) => point.timestamp > b.timestamp,
                    None => true,
                };
                if better {
                    best = Some(point);
                }
            }
        }
        best
    }

    /// The last `records` prices, newest first.
    pub fn prices(e: &Env, asset: Asset, records: u32) -> Option<Vec<PriceData>> {
        if !Self::is_quoted(e, &asset) {
            return None;
        }
        let history = Self::history(e); // oldest first
        let len = history.len();
        if len == 0 {
            return None;
        }
        let take = if records < len { records } else { len };
        let mut out = Vec::new(e);
        for i in 0..take {
            out.push_back(history.get(len - 1 - i).unwrap());
        }
        Some(out)
    }

    // --- internals --------------------------------------------------------------------

    fn is_quoted(e: &Env, asset: &Asset) -> bool {
        &read_instance::<Asset>(e, &DataKey::QuotedAsset) == asset
    }

    fn lastprice_inner(e: &Env) -> Option<PriceData> {
        let count: u32 = read_instance(e, &DataKey::Count);
        if count == 0 {
            return None;
        }
        let head: u32 = read_instance(e, &DataKey::Head);
        let last = (head + CAPACITY - 1) % CAPACITY;
        e.storage().persistent().get(&DataKey::Point(last))
    }

    /// Recorded points, oldest first.
    fn history(e: &Env) -> Vec<PriceData> {
        let count: u32 = read_instance(e, &DataKey::Count);
        let head: u32 = read_instance(e, &DataKey::Head);
        let mut out = Vec::new(e);
        for i in 0..count {
            // Walk backwards from the newest so the ring is read in age order.
            let slot = (head + CAPACITY - count + i) % CAPACITY;
            if let Some(p) = e
                .storage()
                .persistent()
                .get::<DataKey, PriceData>(&DataKey::Point(slot))
            {
                out.push_back(p);
            }
        }
        out
    }
}

fn read_instance<T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>>(e: &Env, key: &DataKey) -> T {
    e.storage()
        .instance()
        .get(key)
        .unwrap_or_else(|| panic_with_error!(e, Error::NotInitialized))
}

#[cfg(test)]
mod test;
