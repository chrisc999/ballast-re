#![cfg(test)]
use crate::{Asset, PriceFeedContract, PriceFeedContractClient};
use ba_usd_token::TokenContract;
use ba_usd_vault::{VaultContract, VaultContractClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::StellarAssetClient,
    Address, BytesN, Env, String, Symbol, Vec,
};

/// Wire a full system: vault + baUSD token + mock USDC + the price feed over the vault.
/// Returns (feed, vault client pieces we need, attestor).
struct Harness {
    e: Env,
    feed: PriceFeedContractClient<'static>,
    vault: VaultContractClient<'static>,
    baus: Address,
    usdc: Address,
    attestor: Address,
}

fn setup() -> Harness {
    let e = Env::default();
    e.mock_all_auths();
    let admin = Address::generate(&e);
    let guardian = Address::generate(&e);
    let ops = Address::generate(&e);
    let usdc = e.register_stellar_asset_contract_v2(admin.clone());
    let usdc_addr = usdc.address();

    let vault = e.register(
        VaultContract,
        (
            admin.clone(),
            guardian,
            ops.clone(),
            ops.clone(),
            ops.clone(),
            usdc_addr.clone(),
            0u64,
        ),
    );
    let token = e.register(
        TokenContract,
        (
            vault.clone(),
            String::from_str(&e, "Ballast USD"),
            String::from_str(&e, "baUSD"),
        ),
    );
    let vc = VaultContractClient::new(&e, &vault);
    vc.set_token(&token);

    let feed = e.register(
        PriceFeedContract,
        (vault.clone(), token.clone(), Symbol::new(&e, "USD")),
    );

    Harness {
        feed: PriceFeedContractClient::new(&e, &feed),
        vault: vc,
        baus: token,
        usdc: usdc_addr,
        attestor: ops,
        e,
    }
}

impl Harness {
    /// Seed the vault with a depositor so NAV can be attested against real shares.
    fn seed(&self, amount: i128) {
        let user = Address::generate(&self.e);
        StellarAssetClient::new(&self.e, &self.usdc).mint(&user, &amount);
        self.vault.subscribe(&user, &amount);
    }

    fn attest(&self, new_nav: i128) {
        self.e
            .ledger()
            .with_mut(|l| l.timestamp += 20 * 60 * 60 + 1);
        let mut signers = Vec::new(&self.e);
        signers.push_back(self.attestor.clone());
        self.vault
            .update_nav(&new_nav, &BytesN::from_array(&self.e, &[3u8; 32]), &signers);
    }

    fn asset(&self) -> Asset {
        Asset::Stellar(self.baus.clone())
    }
}

// ---- SEP-40 metadata ---------------------------------------------------------------

#[test]
fn reports_sep40_metadata() {
    let h = setup();
    assert_eq!(h.feed.base(), Asset::Other(Symbol::new(&h.e, "USD")));
    assert_eq!(h.feed.assets(), {
        let mut v = Vec::new(&h.e);
        v.push_back(h.asset());
        v
    });
    assert_eq!(h.feed.decimals(), 7); // matches baUSD and share_price's scale
    assert_eq!(h.feed.resolution(), 86_400);
}

#[test]
fn unknown_asset_returns_none() {
    let h = setup();
    h.seed(100_000_000);
    h.feed.record();

    let stranger = Asset::Stellar(Address::generate(&h.e));
    assert_eq!(h.feed.lastprice(&stranger), None);
    assert_eq!(h.feed.prices(&stranger, &10), None);
    assert_eq!(h.feed.price(&stranger, &u64::MAX), None);
}

#[test]
fn lastprice_is_none_before_anything_is_recorded() {
    let h = setup();
    assert_eq!(h.feed.lastprice(&h.asset()), None);
}

// ---- publishing the vault's attested price -----------------------------------------

#[test]
fn records_the_vaults_share_price() {
    let h = setup();
    h.seed(100_000_000); // price is exactly 1.0

    let point = h.feed.record();

    assert_eq!(point.price, 10_000_000); // 1.0 at 7 decimals
    assert_eq!(point.timestamp, h.vault.nav_last_updated());
    assert_eq!(h.feed.lastprice(&h.asset()).unwrap().price, 10_000_000);
}

#[test]
fn published_price_follows_nav_upward() {
    let h = setup();
    h.seed(100_000_000);
    h.feed.record();

    h.attest(101_000_000); // +1%
    let point = h.feed.record();

    assert!(point.price > 10_000_000);
    assert_eq!(h.feed.lastprice(&h.asset()).unwrap().price, point.price);
}

/// NAV can fall, and the feed must report that faithfully - a price feed that only ever
/// goes up is not a price feed.
#[test]
fn published_price_follows_nav_downward() {
    let h = setup();
    h.seed(100_000_000);
    h.feed.record();

    h.attest(99_000_000); // -1%
    let point = h.feed.record();

    assert!(point.price < 10_000_000);
}

#[test]
#[should_panic] // NoNewPrice
fn recording_twice_without_a_new_attestation_reverts() {
    let h = setup();
    h.seed(100_000_000);
    h.feed.record();
    h.feed.record(); // vault's NAV timestamp has not moved
}

#[test]
fn current_price_reads_the_vault_live() {
    let h = setup();
    h.seed(100_000_000);
    h.attest(101_000_000);

    // Nothing recorded yet, but the live figure is already available.
    assert_eq!(h.feed.lastprice(&h.asset()), None);
    let live = h.feed.current_price();
    assert!(live.price > 10_000_000);
    assert_eq!(live.timestamp, h.vault.nav_last_updated());
}

// ---- history -----------------------------------------------------------------------

#[test]
fn prices_returns_newest_first() {
    let h = setup();
    h.seed(100_000_000);
    h.feed.record(); // 1.00
    h.attest(101_000_000);
    h.feed.record(); // ~1.01
    h.attest(102_000_000);
    h.feed.record(); // ~1.02

    let series = h.feed.prices(&h.asset(), &3).unwrap();
    assert_eq!(series.len(), 3);
    assert!(series.get(0).unwrap().price > series.get(1).unwrap().price);
    assert!(series.get(1).unwrap().price > series.get(2).unwrap().price);
    assert_eq!(
        series.get(0).unwrap().price,
        h.feed.lastprice(&h.asset()).unwrap().price
    );
}

#[test]
fn prices_caps_at_what_exists() {
    let h = setup();
    h.seed(100_000_000);
    h.feed.record();
    h.attest(101_000_000);
    h.feed.record();

    assert_eq!(h.feed.prices(&h.asset(), &50).unwrap().len(), 2);
}

/// "At or before" is the honest semantic for attested NAV: never invent a value for a
/// moment we had not yet attested, and never report a price from the future.
#[test]
fn price_at_timestamp_returns_the_point_at_or_before() {
    let h = setup();
    h.seed(100_000_000);
    // Attest once first, so the recorded timestamps are real ledger times rather than the
    // zero the vault is constructed with.
    h.attest(100_000_000);
    let first = h.feed.record();
    h.attest(101_000_000);
    let second = h.feed.record();

    // Before anything was attested: nothing to report.
    assert_eq!(h.feed.price(&h.asset(), &(first.timestamp - 1)), None);
    // Exactly at the first point.
    assert_eq!(
        h.feed.price(&h.asset(), &first.timestamp).unwrap().price,
        first.price
    );
    // Between the two: still the first, never the later one.
    assert_eq!(
        h.feed
            .price(&h.asset(), &(second.timestamp - 1))
            .unwrap()
            .price,
        first.price
    );
    // After the second.
    assert_eq!(
        h.feed
            .price(&h.asset(), &(second.timestamp + 10_000))
            .unwrap()
            .price,
        second.price
    );
}

#[test]
fn record_is_permissionless() {
    let h = setup();
    h.seed(100_000_000);
    h.attest(101_000_000);

    // No auth mocked for any particular caller: `record` can only copy the vault's own
    // numbers, so anyone may keep the history current.
    h.e.set_auths(&[]);
    let point = h.feed.record();
    assert!(point.price > 10_000_000);
}
