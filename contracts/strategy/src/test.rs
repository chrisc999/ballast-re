#![cfg(test)]
use crate::{StrategyContract, StrategyContractClient};
use ba_usd_token::TokenContract;
use ba_usd_vault::{VaultContract, VaultContractClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Env, IntoVal, String, Val, Vec,
};

struct Harness {
    e: Env,
    strategy: StrategyContractClient<'static>,
    vault: VaultContractClient<'static>,
    usdc: Address,
    treasury: Address,
}

fn setup(notice: u64) -> Harness {
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
            notice,
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

    let mut init: Vec<Val> = Vec::new(&e);
    init.push_back(vault.clone().into_val(&e));
    let strategy = e.register(StrategyContract, (usdc_addr.clone(), init));

    Harness {
        strategy: StrategyContractClient::new(&e, &strategy),
        vault: vc,
        usdc: usdc_addr,
        treasury: ops,
        e,
    }
}

impl Harness {
    fn depositor(&self, amount: i128) -> Address {
        let who = Address::generate(&self.e);
        StellarAssetClient::new(&self.e, &self.usdc).mint(&who, &amount);
        who
    }
    fn usdc_of(&self, who: &Address) -> i128 {
        TokenClient::new(&self.e, &self.usdc).balance(who)
    }
}

// ---- the DeFindex interface --------------------------------------------------------

#[test]
fn reports_its_underlying_asset() {
    let h = setup(0);
    assert_eq!(h.strategy.asset(), h.usdc);
}

#[test]
fn deposit_subscribes_to_the_vault_and_credits_the_depositor() {
    let h = setup(0);
    let dfx = h.depositor(100_000_000);

    let balance = h.strategy.deposit(&100_000_000, &dfx);

    assert_eq!(balance, 100_000_000); // reported in the underlying, not shares
    assert_eq!(h.strategy.balance(&dfx), 100_000_000);
    assert_eq!(h.usdc_of(&dfx), 0);
    // The vault sees the STRATEGY as the subscriber, not the end depositor.
    assert_eq!(h.vault.total_assets(), 100_000_000);
    assert_eq!(h.strategy.shares_of(&dfx), 100_000_000);
}

#[test]
fn balance_is_zero_for_a_stranger() {
    let h = setup(0);
    assert_eq!(h.strategy.balance(&Address::generate(&h.e)), 0);
}

#[test]
fn withdraw_returns_underlying_within_one_transaction() {
    let h = setup(0);
    let dfx = h.depositor(100_000_000);
    h.strategy.deposit(&100_000_000, &dfx);

    let recipient = Address::generate(&h.e);
    let remaining = h.strategy.withdraw(&40_000_000, &dfx, &recipient);

    assert_eq!(h.usdc_of(&recipient), 40_000_000);
    assert_eq!(remaining, 60_000_000);
    assert_eq!(h.strategy.balance(&dfx), 60_000_000);
    assert!(h.vault.get_redemption(&h.strategy.address).is_none());
}

#[test]
fn full_withdrawal_closes_the_position() {
    let h = setup(0);
    let dfx = h.depositor(100_000_000);
    h.strategy.deposit(&100_000_000, &dfx);

    let recipient = Address::generate(&h.e);
    assert_eq!(h.strategy.withdraw(&100_000_000, &dfx, &recipient), 0);
    assert_eq!(h.usdc_of(&recipient), 100_000_000);
    assert_eq!(h.strategy.shares_of(&dfx), 0);
    assert_eq!(h.vault.total_shares(), 0);
}

#[test]
fn withdrawing_more_than_the_position_is_refused() {
    let h = setup(0);
    let dfx = h.depositor(100_000_000);
    h.strategy.deposit(&100_000_000, &dfx);

    let res = h
        .strategy
        .try_withdraw(&200_000_000, &dfx, &Address::generate(&h.e));
    assert!(res.is_err());
    // Position untouched.
    assert_eq!(h.strategy.balance(&dfx), 100_000_000);
}

// ---- the liquidity constraint, reported truthfully ---------------------------------

/// The case Manu identified: baUSD cannot pay on the spot once capital is deployed. The
/// adapter must decline rather than pretend, and must leave no escrow behind.
#[test]
fn short_sleeve_declines_and_strands_nothing() {
    let h = setup(0);
    let dfx = h.depositor(100_000_000);
    h.strategy.deposit(&100_000_000, &dfx);

    // Treasury moves capital into treaties: NAV unchanged, sleeve nearly empty.
    h.vault.deploy_capital(&90_000_000);
    assert_eq!(h.vault.sleeve_balance(), 10_000_000);

    let res = h
        .strategy
        .try_withdraw(&50_000_000, &dfx, &Address::generate(&h.e));
    assert!(res.is_err());

    // Critically: the failed attempt left NO half-open redemption request at the vault,
    // and the position is intact.
    assert!(h.vault.get_redemption(&h.strategy.address).is_none());
    assert_eq!(h.strategy.balance(&dfx), 100_000_000);
    assert_eq!(h.strategy.shares_of(&dfx), 100_000_000);
}

#[test]
fn withdrawal_succeeds_again_once_the_sleeve_is_refilled() {
    let h = setup(0);
    let dfx = h.depositor(100_000_000);
    h.strategy.deposit(&100_000_000, &dfx);
    h.vault.deploy_capital(&90_000_000);

    let recipient = Address::generate(&h.e);
    assert!(h
        .strategy
        .try_withdraw(&50_000_000, &dfx, &recipient)
        .is_err());

    h.vault.fund_sleeve(&90_000_000); // capital returns from treaties
    h.strategy.withdraw(&50_000_000, &dfx, &recipient);
    assert_eq!(h.usdc_of(&recipient), 50_000_000);
}

/// With a real notice period, no withdrawal can be synchronous. The adapter must fail
/// cleanly rather than wedge itself with a pending request.
#[test]
fn a_notice_period_makes_withdrawal_impossible_and_that_is_reported() {
    let h = setup(86_400); // 24h notice
    let dfx = h.depositor(100_000_000);
    h.strategy.deposit(&100_000_000, &dfx);

    let res = h
        .strategy
        .try_withdraw(&10_000_000, &dfx, &Address::generate(&h.e));
    assert!(res.is_err());
    assert!(h.vault.get_redemption(&h.strategy.address).is_none());
    assert_eq!(h.strategy.balance(&dfx), 100_000_000);
}

// ---- NAV exposure passes through ---------------------------------------------------

#[test]
fn position_value_tracks_nav_in_both_directions() {
    let h = setup(0);
    let dfx = h.depositor(100_000_000);
    h.strategy.deposit(&100_000_000, &dfx);

    let mut signers = Vec::new(&h.e);
    signers.push_back(h.treasury.clone()); // ops address seeds the attestor set

    h.e.ledger().with_mut(|l| l.timestamp += 20 * 60 * 60 + 1);
    h.vault.update_nav(
        &101_000_000,
        &soroban_sdk::BytesN::from_array(&h.e, &[1u8; 32]),
        &signers,
    );
    assert!(h.strategy.balance(&dfx) > 100_000_000);

    h.e.ledger().with_mut(|l| l.timestamp += 20 * 60 * 60 + 1);
    h.vault.update_nav(
        &99_000_000,
        &soroban_sdk::BytesN::from_array(&h.e, &[2u8; 32]),
        &signers,
    );
    assert!(h.strategy.balance(&dfx) < 100_000_000);
}

#[test]
fn harvest_is_a_no_op_because_yield_accrues_through_nav() {
    let h = setup(0);
    let dfx = h.depositor(100_000_000);
    h.strategy.deposit(&100_000_000, &dfx);
    let before = h.strategy.balance(&dfx);

    h.strategy.harvest(&dfx, &None);

    assert_eq!(h.strategy.balance(&dfx), before);
}

// ---- compliance ---------------------------------------------------------------------

/// Only the adapter's own address needs allowlisting: it is the subscriber of record at
/// the vault. End depositors are never seen by the vault's compliance gate.
#[test]
fn allowlisting_only_the_adapter_is_enough() {
    let h = setup(0);
    h.vault.set_allowlist_enabled(&true);
    h.vault.set_allowed(&h.strategy.address, &true);

    let dfx = h.depositor(100_000_000);
    assert!(!h.vault.is_allowed(&dfx)); // never allowlisted
    h.strategy.deposit(&100_000_000, &dfx);
    assert_eq!(h.strategy.balance(&dfx), 100_000_000);

    let recipient = Address::generate(&h.e);
    h.strategy.withdraw(&100_000_000, &dfx, &recipient);
    assert_eq!(h.usdc_of(&recipient), 100_000_000);
}

#[test]
fn an_unlisted_adapter_cannot_subscribe() {
    let h = setup(0);
    h.vault.set_allowlist_enabled(&true); // adapter deliberately NOT allowlisted

    let dfx = h.depositor(100_000_000);
    assert!(h.strategy.try_deposit(&100_000_000, &dfx).is_err());
}
