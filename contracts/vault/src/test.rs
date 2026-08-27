#![cfg(test)]
use crate::{
    assets_for_shares, shares_for_deposit, shares_for_deposit_ceil, Asset, PriceData,
    VaultContract, VaultContractClient, MIN_INITIAL_DEPOSIT,
};
use ba_usd_token::{TokenContract, TokenContractClient};
use soroban_sdk::{
    contract, contractimpl, symbol_short,
    testutils::{Address as _, Ledger, MockAuth, MockAuthInvoke},
    token::{StellarAssetClient, TokenClient},
    Address, BytesN, Env, IntoVal, String, Vec,
};

// ---- pure share-math tests (no Env needed) ----------------------------------------

#[test]
fn first_deposit_is_one_to_one() {
    assert_eq!(shares_for_deposit(1_000, 0, 0), Some(1_000));
    assert_eq!(assets_for_shares(1_000, 1_000, 1_000), Some(1_000));
}

#[test]
fn rounds_down_in_vault_favor() {
    // total_shares=3, total_assets=2 -> price (2+1)/(3+1)=0.75
    assert_eq!(shares_for_deposit(1, 3, 2), Some(1)); // 1*(3+1)/(2+1)=1.33 -> 1
    assert_eq!(assets_for_shares(1, 3, 2), Some(0)); // 1*(2+1)/(3+1)=0.75 -> 0
}

#[test]
fn deposit_then_redeem_never_profits_at_flat_nav() {
    let cases: [(i128, i128, i128); 6] = [
        (100, 0, 0),
        (1, 10, 10),
        (7, 3, 5),
        (1_000_000, 12_345, 9_999),
        (1, 1_000_000, 1),
        (300_000_000_000_000, 50_000_000_000_000, 60_000_000_000_000), // ~$30M NAV, 7 decimals
    ];
    for (x, ts, ta) in cases {
        let minted = shares_for_deposit(x, ts, ta).unwrap();
        let back = assets_for_shares(minted, ts + minted, ta + x).unwrap();
        assert!(
            back <= x,
            "redeem {back} > deposit {x} (ts={ts}, ta={ta}, minted={minted})"
        );
    }
}

#[test]
fn inflation_attack_is_neutralized() {
    let attacker_shares = shares_for_deposit(1, 0, 0).unwrap();
    assert_eq!(attacker_shares, 1);
    let (ts, ta) = (attacker_shares, 1);

    let big = 1_000_000_000i128;
    let big_shares = shares_for_deposit(big, ts, ta).unwrap();
    assert!(
        big_shares >= big - 1,
        "honest depositor shorted: {big_shares}"
    );

    let (ts2, ta2) = (ts + big_shares, ta + big);
    let attacker_out = assets_for_shares(attacker_shares, ts2, ta2).unwrap();
    assert!(attacker_out <= 1, "attacker extracted {attacker_out}");
}

// ---- the everyone-redeemed state, with residual NAV -------------------------------
// Today share price is exactly 1.0 (total_assets always equals total_shares), so every
// division is exact and no residue can accumulate. Once NAV updates move the price,
// `assets_for_shares` floors on each claim and leaves residue behind. The vault can then
// reach total_shares == 0 with total_assets > 0. These pin down that state's behavior
// BEFORE the code that makes it reachable lands.

#[test]
fn deposit_into_redeemed_empty_vault_with_residual() {
    // All shares gone, a little attested NAV left behind by rounding.
    let residual = 37i128;
    let deposit = MIN_INITIAL_DEPOSIT;

    let shares = shares_for_deposit(deposit, 0, residual).unwrap();

    // Share granularity coarsens: each share is now worth ~(residual + 1) assets, so a
    // 1.0-baUSD deposit mints far fewer than 1.0 baUSD of shares.
    assert_eq!(shares, 263_157);

    let back = assets_for_shares(shares, shares, residual + deposit).unwrap();

    // Rounding still favors the vault - the depositor gets back one stroop less than they
    // put in, never more, even though they are the sole owner of the residual.
    assert_eq!(back, deposit - 1);
    assert!(back <= deposit + residual, "rounding favored the depositor");
    // Loss is bounded by the share granularity, not unbounded.
    assert!(
        back >= deposit - (residual + 1),
        "loss exceeded one share of value"
    );
}

/// The brick case: if residual ever exceeds the deposit, shares floor to zero. `subscribe`
/// rejects that with ZeroShares rather than silently taking the funds - but it means new
/// deposits below the residual are refused, so the residual must stay dust-sized.
#[test]
fn residual_larger_than_deposit_rounds_shares_to_zero() {
    let residual = 1_000i128;
    assert_eq!(shares_for_deposit(999, 0, residual), Some(0));
    // One unit past the residual is the smallest deposit that still mints.
    assert_eq!(shares_for_deposit(1_001, 0, residual), Some(1));
}

#[test]
fn empty_vault_with_no_residual_is_one_to_one() {
    // A genuinely fresh vault: no shares, no assets, so the first deposit sets price 1.0.
    assert_eq!(shares_for_deposit(1_000, 0, 0), Some(1_000));
    // Note `assets_for_shares(n, 0, 0)` returns n, but that input is unreachable through
    // the contract: no shares can exist to redeem while total_shares is 0.
}

#[test]
fn overflow_returns_none_not_panic() {
    assert_eq!(shares_for_deposit(i128::MAX, i128::MAX, 0), None);
    assert_eq!(assets_for_shares(i128::MAX, i128::MAX, 0), None);
}

// ---- view-only contract tests -----------------------------------------------------

fn register_vault_only(e: &Env) -> Address {
    let a = Address::generate(e);
    e.register(
        VaultContract,
        (
            a.clone(),
            a.clone(),
            a.clone(),
            a.clone(),
            a.clone(),
            a.clone(),
            0u64,
        ),
    )
}

#[test]
fn initializes_empty() {
    let e = Env::default();
    let client = VaultContractClient::new(&e, &register_vault_only(&e));
    assert_eq!(client.total_shares(), 0);
    assert_eq!(client.total_assets(), 0);
    assert_eq!(client.get_config().notice_period, 0);
    assert!(!client.is_paused());
}

#[test]
fn convert_views_on_empty_vault_are_one_to_one() {
    let e = Env::default();
    let client = VaultContractClient::new(&e, &register_vault_only(&e));
    assert_eq!(client.convert_to_shares(&500), 500);
    assert_eq!(client.convert_to_assets(&500), 500);
}

// ---- full deploy: vault + baUSD token (owned by vault) + mock USDC -----------------

/// Deploy a wired system: vault, baUSD token owned by the vault, and a mock USDC SAC.
/// Enables all auths (deploy-time set_token needs admin auth). Returns
/// (vault, token, usdc, admin, guardian).
fn deploy(e: &Env) -> (Address, Address, Address, Address, Address) {
    deploy_with_notice(e, 0)
}

fn deploy_with_notice(e: &Env, notice: u64) -> (Address, Address, Address, Address, Address) {
    let (v, t, u, a, g, _attestor) = deploy_inner(e, notice);
    (v, t, u, a, g)
}

/// Same wiring, but also hands back the seeded attestor so NAV tests can sign. The
/// attestation, compliance and treasury roles all share this address in tests.
fn deploy_with_attestor(e: &Env) -> (Address, Address, Address, Address, Address, Address) {
    deploy_inner(e, 0)
}

fn deploy_with_notice_and_attestor(
    e: &Env,
    notice: u64,
) -> (Address, Address, Address, Address, Address, Address) {
    deploy_inner(e, notice)
}

fn deploy_inner(e: &Env, notice: u64) -> (Address, Address, Address, Address, Address, Address) {
    e.mock_all_auths();
    let admin = Address::generate(e);
    let guardian = Address::generate(e);
    let ops = Address::generate(e); // attestation/compliance/treasury (inactive roles for now)
    let usdc = e.register_stellar_asset_contract_v2(admin.clone());
    let usdc_addr = usdc.address();
    let vault = e.register(
        VaultContract,
        (
            admin.clone(),
            guardian.clone(),
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
            String::from_str(e, "Ballast USD"),
            String::from_str(e, "baUSD"),
        ),
    );
    VaultContractClient::new(e, &vault).set_token(&token);
    (vault, token, usdc_addr, admin, guardian, ops)
}

/// A NAV attestation proof reference (hash of the off-chain attestation document).
fn proof(e: &Env) -> BytesN<32> {
    BytesN::from_array(e, &[7u8; 32])
}

/// Advance past the NAV cadence floor (20h).
fn advance_past_cadence(e: &Env) {
    e.ledger().with_mut(|l| l.timestamp += 20 * 60 * 60 + 1);
}

fn signers(e: &Env, who: &[&Address]) -> Vec<Address> {
    let mut v = Vec::new(e);
    for a in who {
        v.push_back((*a).clone());
    }
    v
}

fn fund_usdc(e: &Env, usdc: &Address, to: &Address, amount: i128) {
    StellarAssetClient::new(e, usdc).mint(to, &amount);
}

#[test]
fn subscribe_mints_shares_and_pulls_usdc() {
    let e = Env::default();
    let (vault, token, usdc, _admin, _guardian) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000); // 10 USDC
    let deposit = 50_000_000i128; // 5 USDC

    let shares = vc.subscribe(&user, &deposit);
    assert_eq!(shares, deposit); // first deposit is 1:1

    assert_eq!(vc.total_shares(), deposit);
    assert_eq!(vc.total_assets(), deposit);

    let baustd = TokenContractClient::new(&e, &token);
    assert_eq!(baustd.balance(&user), deposit);
    assert_eq!(baustd.total_supply(), deposit);

    let usdc_tok = TokenClient::new(&e, &usdc);
    assert_eq!(usdc_tok.balance(&user), 50_000_000); // 10 - 5 USDC
    assert_eq!(usdc_tok.balance(&vault), deposit); // vault now holds the 5 USDC
}

#[test]
fn second_deposit_at_flat_nav_is_proportional() {
    let e = Env::default();
    let (vault, token, usdc, _, _) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);

    let a = Address::generate(&e);
    let b = Address::generate(&e);
    fund_usdc(&e, &usdc, &a, 100_000_000);
    fund_usdc(&e, &usdc, &b, 100_000_000);

    vc.subscribe(&a, &40_000_000);
    vc.subscribe(&b, &60_000_000);

    let baustd = TokenContractClient::new(&e, &token);
    assert_eq!(baustd.balance(&a), 40_000_000);
    assert_eq!(baustd.balance(&b), 60_000_000);
    assert_eq!(vc.total_shares(), 100_000_000);
    assert_eq!(vc.total_assets(), 100_000_000);
}

#[test]
#[should_panic] // BelowMinInitialDeposit
fn first_deposit_below_minimum_reverts() {
    let e = Env::default();
    let (vault, _t, usdc, _, _) = deploy(&e);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    VaultContractClient::new(&e, &vault).subscribe(&user, &(MIN_INITIAL_DEPOSIT - 1));
}

#[test]
#[should_panic] // Paused
fn subscribe_reverts_when_paused() {
    let e = Env::default();
    let (vault, _t, usdc, _, _) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.pause();
    vc.subscribe(&user, &50_000_000);
}

#[test]
fn guardian_can_pause_and_unpause() {
    let e = Env::default();
    let (vault, _t, _u, _, _) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    assert!(!vc.is_paused());
    vc.pause();
    assert!(vc.is_paused());
    vc.unpause();
    assert!(!vc.is_paused());
}

#[test]
#[should_panic] // guardian auth absent
fn non_guardian_cannot_pause() {
    let e = Env::default();
    let (vault, _t, _u, _admin, _guardian) = deploy(&e);
    let attacker = Address::generate(&e);
    // Authorize only the attacker (not the guardian) for the pause call.
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "pause",
            args: ().into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).pause();
}

#[test]
#[should_panic] // `from` auth absent
fn subscribe_requires_from_auth() {
    let e = Env::default();
    let (vault, _t, usdc, _, _) = deploy(&e);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);

    let other = Address::generate(&e);
    e.mock_auths(&[MockAuth {
        address: &other,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "subscribe",
            args: (user.clone(), 50_000_000i128).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).subscribe(&user, &50_000_000);
}

#[test]
#[should_panic] // TokenAlreadySet
fn set_token_twice_reverts() {
    let e = Env::default();
    let (vault, token, _u, _, _) = deploy(&e);
    VaultContractClient::new(&e, &vault).set_token(&token);
}

// ---- redemption: request -> claim -------------------------------------------------

#[test]
fn full_redemption_cycle_notice_zero() {
    let e = Env::default();
    let (vault, token, usdc, _, _) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);

    let shares = vc.subscribe(&user, &50_000_000);
    vc.request_redemption(&user, &shares);

    // Shares are escrowed: user holds 0 baUSD, the vault holds them.
    let baustd = TokenContractClient::new(&e, &token);
    assert_eq!(baustd.balance(&user), 0);
    assert_eq!(baustd.balance(&vault), shares);

    vc.claim_redemption(&user);

    let usdc_tok = TokenClient::new(&e, &usdc);
    assert_eq!(usdc_tok.balance(&user), 100_000_000); // fully returned
    assert_eq!(usdc_tok.balance(&vault), 0);
    assert_eq!(baustd.total_supply(), 0); // escrowed shares burned
    assert_eq!(vc.total_shares(), 0);
    assert_eq!(vc.total_assets(), 0);
    assert!(vc.get_redemption(&user).is_none());
}

#[test]
fn get_redemption_reflects_request() {
    let e = Env::default();
    let (vault, _t, usdc, _, _) = deploy_with_notice(&e, 100);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &50_000_000);

    assert!(vc.get_redemption(&user).is_none());
    vc.request_redemption(&user, &shares);
    assert_eq!(vc.get_redemption(&user).unwrap().shares, shares);
}

#[test]
fn partial_redemption_leaves_remaining_shares() {
    let e = Env::default();
    let (vault, token, usdc, _, _) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &80_000_000); // 8 USDC
    let half = shares / 2;

    vc.request_redemption(&user, &half);
    vc.claim_redemption(&user);

    let baustd = TokenContractClient::new(&e, &token);
    assert_eq!(baustd.balance(&user), shares - half); // remaining shares still theirs
    assert_eq!(vc.total_shares(), shares - half);

    let usdc_tok = TokenClient::new(&e, &usdc);
    // 10 in, deposited 8 (=> 2 left), claimed back 4 => 6 USDC.
    assert_eq!(
        usdc_tok.balance(&user),
        100_000_000 - 80_000_000 + 40_000_000
    );
}

#[test]
fn claim_after_notice_succeeds() {
    let e = Env::default();
    let (vault, _t, usdc, _, _) = deploy_with_notice(&e, 100);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &50_000_000);

    vc.request_redemption(&user, &shares);
    e.ledger().with_mut(|li| li.timestamp += 200); // let the notice elapse
    vc.claim_redemption(&user);

    assert_eq!(TokenClient::new(&e, &usdc).balance(&user), 100_000_000);
}

#[test]
#[should_panic] // NoticeNotElapsed
fn claim_before_notice_reverts() {
    let e = Env::default();
    let (vault, _t, usdc, _, _) = deploy_with_notice(&e, 100);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &50_000_000);
    vc.request_redemption(&user, &shares);
    vc.claim_redemption(&user); // notice has not elapsed
}

#[test]
#[should_panic] // RequestPending
fn double_request_reverts() {
    let e = Env::default();
    let (vault, _t, usdc, _, _) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &50_000_000);
    vc.request_redemption(&user, &(shares / 2));
    vc.request_redemption(&user, &(shares / 2)); // already pending
}

#[test]
#[should_panic] // NoRequest
fn claim_without_request_reverts() {
    let e = Env::default();
    let (vault, _t, _u, _, _) = deploy(&e);
    let user = Address::generate(&e);
    VaultContractClient::new(&e, &vault).claim_redemption(&user);
}

#[test]
#[should_panic] // Paused
fn request_when_paused_reverts() {
    let e = Env::default();
    let (vault, _t, usdc, _, _) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &50_000_000);
    vc.pause();
    vc.request_redemption(&user, &shares);
}

// ---- authority rotation + upgrade timelock ----------------------------------------

#[test]
fn admin_can_rotate_guardian() {
    let e = Env::default();
    let (vault, _t, _u, _admin, _g) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let new_guardian = Address::generate(&e);
    vc.set_guardian(&new_guardian);
    assert_eq!(vc.get_config().guardian, new_guardian);
}

#[test]
#[should_panic] // admin auth absent
fn non_admin_cannot_set_authority() {
    let e = Env::default();
    let (vault, _t, _u, _admin, _g) = deploy(&e);
    let attacker = Address::generate(&e);
    let new_admin = Address::generate(&e);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "propose_admin",
            args: (new_admin.clone(),).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).propose_admin(&new_admin);
}

// ---- admin handover is two-step ---------------------------------------------------
// Admin is the only role that can destroy its own recovery path (it gates upgrade), so a
// nomination does nothing until the nominee proves it can authorize.

#[test]
fn proposing_admin_does_not_change_admin() {
    let e = Env::default();
    let (vault, _t, _u, admin, _g) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let new_admin = Address::generate(&e);

    vc.propose_admin(&new_admin);

    assert_eq!(vc.get_config().admin, admin); // unchanged
    assert_eq!(vc.get_pending_admin(), Some(new_admin));
}

#[test]
fn admin_handover_completes_on_acceptance() {
    let e = Env::default();
    let (vault, _t, _u, _admin, _g) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let new_admin = Address::generate(&e);

    vc.propose_admin(&new_admin);
    vc.accept_admin();

    assert_eq!(vc.get_config().admin, new_admin);
    assert_eq!(vc.get_pending_admin(), None);
}

#[test]
#[should_panic] // no proposal pending
fn accept_admin_without_proposal_reverts() {
    let e = Env::default();
    let (vault, _t, _u, _admin, _g) = deploy(&e);
    VaultContractClient::new(&e, &vault).accept_admin();
}

#[test]
#[should_panic] // pending admin's auth absent
fn only_proposed_admin_can_accept() {
    let e = Env::default();
    let (vault, _t, _u, _admin, _g) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let new_admin = Address::generate(&e);
    let attacker = Address::generate(&e);
    vc.propose_admin(&new_admin);

    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "accept_admin",
            args: ().into_val(&e),
            sub_invokes: &[],
        },
    }]);
    vc.accept_admin();
}

// ---- cancel_redemption: an unclaimable request must always be reversible -----------

#[test]
fn cancel_redemption_returns_escrowed_shares() {
    let e = Env::default();
    let (vault, token, usdc, _a, _g) = deploy_with_notice(&e, 100);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &50_000_000);
    vc.request_redemption(&user, &shares);

    let baustd = TokenContractClient::new(&e, &token);
    assert_eq!(baustd.balance(&user), 0); // escrowed

    vc.cancel_redemption(&user);

    assert_eq!(baustd.balance(&user), shares); // returned
    assert_eq!(baustd.balance(&vault), 0);
    assert!(vc.get_redemption(&user).is_none());
    // Bookkept totals are untouched: nothing was ever redeemed.
    assert_eq!(vc.total_shares(), shares);
}

/// The case that matters most: pause must not trap already-escrowed shares. A cancel
/// returns the holder's own property and moves no USDC, so it stays available.
#[test]
fn cancel_redemption_works_while_paused() {
    let e = Env::default();
    let (vault, token, usdc, _a, _g) = deploy_with_notice(&e, 100);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &50_000_000);
    vc.request_redemption(&user, &shares);

    vc.pause();
    assert!(vc.is_paused());

    vc.cancel_redemption(&user);
    assert_eq!(TokenContractClient::new(&e, &token).balance(&user), shares);
}

#[test]
fn cancel_then_request_again_succeeds() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g) = deploy_with_notice(&e, 100);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &50_000_000);

    vc.request_redemption(&user, &shares);
    vc.cancel_redemption(&user);
    vc.request_redemption(&user, &shares); // no longer blocked by RequestPending

    assert_eq!(vc.get_redemption(&user).unwrap().shares, shares);
}

#[test]
#[should_panic] // NoRequest
fn cancel_without_request_reverts() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g) = deploy(&e);
    let user = Address::generate(&e);
    VaultContractClient::new(&e, &vault).cancel_redemption(&user);
}

#[test]
#[should_panic] // caller's auth absent
fn cancel_redemption_requires_auth() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g) = deploy_with_notice(&e, 100);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &50_000_000);
    vc.request_redemption(&user, &shares);

    let attacker = Address::generate(&e);
    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "cancel_redemption",
            args: (user.clone(),).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    vc.cancel_redemption(&user);
}

// ---- notice period is configurable, bounded, and non-retroactive -------------------

#[test]
fn admin_can_set_notice_period() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    vc.set_notice_period(&86_400);
    assert_eq!(vc.get_config().notice_period, 86_400);
}

#[test]
#[should_panic] // NoticeTooLong
fn notice_period_above_max_reverts() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g) = deploy(&e);
    // 91 days, one past MAX_NOTICE_PERIOD_SECS.
    VaultContractClient::new(&e, &vault).set_notice_period(&(91 * 24 * 60 * 60));
}

#[test]
#[should_panic] // admin auth absent
fn non_admin_cannot_set_notice_period() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g) = deploy(&e);
    let attacker = Address::generate(&e);
    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "set_notice_period",
            args: (86_400u64,).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).set_notice_period(&86_400);
}

/// Governance must not be able to extend a lockup on capital already in the queue:
/// `claimable_at` is fixed when the request is made.
#[test]
fn notice_period_change_is_not_retroactive() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g) = deploy_with_notice(&e, 100);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &50_000_000);
    vc.request_redemption(&user, &shares);
    let claimable_at = vc.get_redemption(&user).unwrap().claimable_at;

    vc.set_notice_period(&(30 * 24 * 60 * 60)); // 30 days, far longer

    assert_eq!(vc.get_redemption(&user).unwrap().claimable_at, claimable_at);
    e.ledger().with_mut(|l| l.timestamp = claimable_at);
    vc.claim_redemption(&user); // still claimable on the original terms
}

#[test]
fn upgrade_proposal_is_recorded() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let hash = BytesN::from_array(&e, &[7u8; 32]);
    vc.propose_upgrade(&hash);
    assert_eq!(vc.get_pending_upgrade().unwrap().wasm_hash, hash);
}

#[test]
#[should_panic] // TimelockNotElapsed
fn execute_upgrade_before_timelock_reverts() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g) = deploy(&e);
    let vc = VaultContractClient::new(&e, &vault);
    vc.propose_upgrade(&BytesN::from_array(&e, &[7u8; 32]));
    vc.execute_upgrade(); // timelock has not elapsed
}

#[test]
#[should_panic] // NoPendingUpgrade
fn execute_upgrade_without_proposal_reverts() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g) = deploy(&e);
    VaultContractClient::new(&e, &vault).execute_upgrade();
}

#[test]
#[should_panic] // admin auth absent
fn non_admin_cannot_propose_upgrade() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g) = deploy(&e);
    let attacker = Address::generate(&e);
    let hash = BytesN::from_array(&e, &[7u8; 32]);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "propose_upgrade",
            args: (hash.clone(),).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).propose_upgrade(&hash);
}

// ---- NAV attestation ---------------------------------------------------------------

#[test]
fn nav_increase_raises_share_price_and_payout() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    let shares = vc.subscribe(&user, &100_000_000); // 10 USDC, price 1.0
    assert_eq!(vc.share_price(), 10_000_000);

    advance_past_cadence(&e);
    vc.update_nav(&101_000_000, &proof(&e), &signers(&e, &[&attestor])); // +1%

    assert!(vc.share_price() > 10_000_000);
    // The same share count is now worth more USDC.
    assert!(vc.convert_to_assets(&shares) > 100_000_000);
}

/// NAV must be able to fall - a treaty loss is a real downward move, not an anomaly.
#[test]
fn nav_can_decrease() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    let shares = vc.subscribe(&user, &100_000_000);

    advance_past_cadence(&e);
    vc.update_nav(&99_000_000, &proof(&e), &signers(&e, &[&attestor])); // -1%

    assert!(vc.share_price() < 10_000_000);
    assert!(vc.convert_to_assets(&shares) < 100_000_000);
}

#[test]
#[should_panic] // NavDeltaTooLarge
fn nav_move_above_cap_reverts() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    advance_past_cadence(&e);
    vc.update_nav(&110_000_000, &proof(&e), &signers(&e, &[&attestor])); // +10%, cap is 2%
}

/// The cadence floor is what makes the delta cap meaningful: without it, repeated
/// within-cap updates could walk NAV anywhere in an afternoon.
#[test]
#[should_panic] // NavTooSoon
fn second_nav_update_within_cadence_reverts() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    advance_past_cadence(&e);
    vc.update_nav(&101_000_000, &proof(&e), &signers(&e, &[&attestor]));
    vc.update_nav(&102_000_000, &proof(&e), &signers(&e, &[&attestor])); // immediately again
}

#[test]
#[should_panic] // NoSharesOutstanding
fn nav_update_with_no_shares_reverts() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g, attestor) = deploy_with_attestor(&e);
    advance_past_cadence(&e);
    VaultContractClient::new(&e, &vault).update_nav(
        &50_000_000,
        &proof(&e),
        &signers(&e, &[&attestor]),
    );
}

// ---- the m-of-n quorum -------------------------------------------------------------

#[test]
fn quorum_of_two_of_three_accepts_two_signers() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _seed) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let (a1, a2, a3) = (
        Address::generate(&e),
        Address::generate(&e),
        Address::generate(&e),
    );
    vc.set_attestors(&signers(&e, &[&a1, &a2, &a3]), &2);
    assert_eq!(vc.get_attestation_threshold(), 2);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    advance_past_cadence(&e);
    vc.update_nav(&101_000_000, &proof(&e), &signers(&e, &[&a1, &a2]));
    assert!(vc.share_price() > 10_000_000);
}

#[test]
#[should_panic] // InsufficientAttestations
fn quorum_of_two_rejects_a_single_signer() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _seed) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let (a1, a2, a3) = (
        Address::generate(&e),
        Address::generate(&e),
        Address::generate(&e),
    );
    vc.set_attestors(&signers(&e, &[&a1, &a2, &a3]), &2);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    advance_past_cadence(&e);
    vc.update_nav(&101_000_000, &proof(&e), &signers(&e, &[&a1]));
}

/// One key must not be able to fill several slots of the quorum.
#[test]
#[should_panic] // DuplicateAttestor
fn same_signer_twice_does_not_satisfy_quorum() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _seed) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let (a1, a2) = (Address::generate(&e), Address::generate(&e));
    vc.set_attestors(&signers(&e, &[&a1, &a2]), &2);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    advance_past_cadence(&e);
    vc.update_nav(&101_000_000, &proof(&e), &signers(&e, &[&a1, &a1]));
}

#[test]
#[should_panic] // UnknownAttestor
fn non_attestor_cannot_sign() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let outsider = Address::generate(&e);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    advance_past_cadence(&e);
    vc.update_nav(
        &101_000_000,
        &proof(&e),
        &signers(&e, &[&outsider, &attestor]),
    );
}

#[test]
#[should_panic] // InvalidThreshold: would make NAV permanently un-updatable
fn threshold_above_set_size_reverts() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g, _s) = deploy_with_attestor(&e);
    let (a1, a2) = (Address::generate(&e), Address::generate(&e));
    VaultContractClient::new(&e, &vault).set_attestors(&signers(&e, &[&a1, &a2]), &3);
}

#[test]
#[should_panic] // InvalidThreshold: zero would let anyone attest
fn zero_threshold_reverts() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g, _s) = deploy_with_attestor(&e);
    let a1 = Address::generate(&e);
    VaultContractClient::new(&e, &vault).set_attestors(&signers(&e, &[&a1]), &0);
}

#[test]
#[should_panic] // DuplicateAttestor in the set itself
fn duplicate_attestor_in_set_reverts() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g, _s) = deploy_with_attestor(&e);
    let a1 = Address::generate(&e);
    VaultContractClient::new(&e, &vault).set_attestors(&signers(&e, &[&a1, &a1]), &2);
}

#[test]
#[should_panic] // admin auth absent
fn non_admin_cannot_set_attestors() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g, _s) = deploy_with_attestor(&e);
    let attacker = Address::generate(&e);
    let a1 = Address::generate(&e);
    let set = signers(&e, &[&a1]);
    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "set_attestors",
            args: (set.clone(), 1u32).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).set_attestors(&set, &1);
}

// ---- the extraordinary path (catastrophe writedowns) -------------------------------

#[test]
fn extraordinary_update_bypasses_the_cap() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    // A 40% catastrophe writedown - far beyond the 2% routine cap, and with no cadence wait.
    vc.update_nav_extraordinary(&60_000_000, &proof(&e), &signers(&e, &[&attestor]));

    assert_eq!(vc.total_assets(), 60_000_000);
    assert!(vc.share_price() < 10_000_000);
}

/// The quorum alone must never be able to make a large move quietly.
#[test]
#[should_panic] // admin auth absent
fn extraordinary_update_requires_governance() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    let sigs = signers(&e, &[&attestor]);
    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &attestor,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "update_nav_extraordinary",
            args: (60_000_000i128, proof(&e), sigs.clone()).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    vc.update_nav_extraordinary(&60_000_000, &proof(&e), &sigs);
}

// ---- staleness gates deposits, never exits -----------------------------------------

#[test]
#[should_panic] // NavStale
fn stale_nav_blocks_new_deposits() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _s) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    e.ledger().with_mut(|l| l.timestamp += 49 * 60 * 60); // past the 48h window
    assert!(vc.is_nav_stale());
    vc.subscribe(&user, &50_000_000);
}

/// An operational failure to attest is the vault's fault. It must never trap an LP.
#[test]
fn stale_nav_does_not_block_exits() {
    let e = Env::default();
    let (vault, token, usdc, _a, _g, _s) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    let shares = vc.subscribe(&user, &100_000_000);

    e.ledger().with_mut(|l| l.timestamp += 49 * 60 * 60);
    assert!(vc.is_nav_stale());

    // Requesting, cancelling and claiming all remain available.
    vc.request_redemption(&user, &shares);
    vc.cancel_redemption(&user);
    assert_eq!(TokenContractClient::new(&e, &token).balance(&user), shares);

    vc.request_redemption(&user, &shares);
    vc.claim_redemption(&user);
    assert_eq!(TokenClient::new(&e, &usdc).balance(&user), 200_000_000);
}

#[test]
fn nav_last_updated_tracks_attestations() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    advance_past_cadence(&e);
    let now = e.ledger().timestamp();
    vc.update_nav(&101_000_000, &proof(&e), &signers(&e, &[&attestor]));
    assert_eq!(vc.nav_last_updated(), now);
    assert!(!vc.is_nav_stale());
}

// ---- treasury sleeve management ----------------------------------------------------
// Both directions are NAV-NEUTRAL: capital moving between the on-chain sleeve and the
// treaties is the same capital. Only update_nav moves NAV.

#[test]
fn deploy_capital_drains_sleeve_without_touching_nav() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, treasury) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);
    assert_eq!(vc.sleeve_balance(), 100_000_000);

    vc.deploy_capital(&60_000_000);

    assert_eq!(vc.sleeve_balance(), 40_000_000);
    assert_eq!(vc.total_assets(), 100_000_000); // NAV unchanged
    assert_eq!(vc.share_price(), 10_000_000);
    assert_eq!(TokenClient::new(&e, &usdc).balance(&treasury), 60_000_000);
}

#[test]
fn fund_sleeve_refills_without_touching_nav() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _treasury) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);
    vc.deploy_capital(&60_000_000);

    vc.fund_sleeve(&60_000_000);

    assert_eq!(vc.sleeve_balance(), 100_000_000);
    assert_eq!(vc.total_assets(), 100_000_000); // still unchanged
}

#[test]
#[should_panic] // InsufficientSleeve
fn deploy_capital_beyond_sleeve_reverts() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _tr) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);
    vc.deploy_capital(&150_000_000);
}

#[test]
#[should_panic] // treasury auth absent
fn non_treasury_cannot_deploy_capital() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _tr) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);

    let attacker = Address::generate(&e);
    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "deploy_capital",
            args: (10_000_000i128,).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    vc.deploy_capital(&10_000_000);
}

// ---- partial fills: a short sleeve queues the remainder, it does not fail -----------

#[test]
fn short_sleeve_pays_what_it_covers_and_queues_the_rest() {
    let e = Env::default();
    let (vault, token, usdc, _a, _g, _tr) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &100_000_000);

    vc.deploy_capital(&60_000_000); // only 40 USDC left on-chain
    vc.request_redemption(&user, &shares);

    let paid = vc.claim_redemption(&user);

    assert_eq!(paid, 40_000_000);
    assert_eq!(TokenClient::new(&e, &usdc).balance(&user), 40_000_000);
    // The rest stays queued, and the escrowed remainder is still held by the vault.
    let rest = vc.get_redemption(&user).unwrap();
    assert_eq!(rest.shares, shares - 40_000_000);
    assert_eq!(
        TokenContractClient::new(&e, &token).balance(&vault),
        shares - 40_000_000
    );
    assert_eq!(vc.total_shares(), shares - 40_000_000);
    assert_eq!(vc.total_assets(), 60_000_000);
}

#[test]
fn refunding_the_sleeve_completes_a_partially_filled_claim() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _tr) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &100_000_000);

    vc.deploy_capital(&60_000_000);
    vc.request_redemption(&user, &shares);
    vc.claim_redemption(&user); // partial: 40

    vc.fund_sleeve(&60_000_000); // capital returns from treaties
    let paid = vc.claim_redemption(&user);

    assert_eq!(paid, 60_000_000);
    assert_eq!(TokenClient::new(&e, &usdc).balance(&user), 100_000_000);
    assert!(vc.get_redemption(&user).is_none());
    assert_eq!(vc.total_shares(), 0);
    assert_eq!(vc.total_assets(), 0);
}

/// A partial fill must not restart the clock: the holder already served their notice.
#[test]
fn partial_fill_preserves_the_original_maturity() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _tr) = deploy_with_notice_and_attestor(&e, 100);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &100_000_000);

    vc.deploy_capital(&60_000_000);
    vc.request_redemption(&user, &shares);
    let due = vc.get_redemption(&user).unwrap().claimable_at;

    e.ledger().with_mut(|l| l.timestamp = due);
    vc.claim_redemption(&user);

    assert_eq!(vc.get_redemption(&user).unwrap().claimable_at, due);
}

// ---- redemption suspension: deliberate, and never a trap ---------------------------

#[test]
#[should_panic] // RedemptionsSuspended
fn suspension_blocks_claims() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _tr) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &100_000_000);
    vc.request_redemption(&user, &shares);

    vc.set_redemptions_suspended(&true);
    assert!(vc.redemptions_suspended());
    vc.claim_redemption(&user);
}

/// Deferring an exit is only acceptable if the holder can leave the queue instead.
#[test]
fn suspension_still_allows_queueing_and_cancelling() {
    let e = Env::default();
    let (vault, token, usdc, _a, _g, _tr) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &100_000_000);

    vc.set_redemptions_suspended(&true);

    vc.request_redemption(&user, &shares); // may still join the queue
    vc.cancel_redemption(&user); // and may always leave it
    assert_eq!(TokenContractClient::new(&e, &token).balance(&user), shares);
}

#[test]
fn resuming_redemptions_restores_claims() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _tr) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &100_000_000);
    vc.request_redemption(&user, &shares);

    vc.set_redemptions_suspended(&true);
    vc.set_redemptions_suspended(&false);
    let paid = vc.claim_redemption(&user);
    assert_eq!(paid, 100_000_000);
}

#[test]
#[should_panic] // admin auth absent
fn non_admin_cannot_suspend_redemptions() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g, _tr) = deploy_with_attestor(&e);
    let attacker = Address::generate(&e);
    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "set_redemptions_suspended",
            args: (true,).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).set_redemptions_suspended(&true);
}

// ---- compliance allowlist ----------------------------------------------------------
// KYC/AML runs off-chain; the contract only ever sees allowlisted addresses.

#[test]
fn allowlist_is_off_by_default() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _ops) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);

    assert!(!vc.allowlist_enabled());
    assert!(vc.is_allowed(&user)); // everyone allowed while the gate is off

    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);
}

#[test]
#[should_panic] // NotAllowed
fn enabled_allowlist_blocks_an_unlisted_subscriber() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _ops) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);

    vc.set_allowlist_enabled(&true);
    vc.subscribe(&user, &100_000_000);
}

#[test]
fn allowlisted_address_can_subscribe_and_claim() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _ops) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);

    vc.set_allowlist_enabled(&true);
    vc.set_allowed(&user, &true);
    assert!(vc.is_allowed(&user));

    let shares = vc.subscribe(&user, &100_000_000);
    vc.request_redemption(&user, &shares);
    assert_eq!(vc.claim_redemption(&user), 100_000_000);
}

/// The policy from D-0014/D-0022, tested end to end: a de-allowlisted holder is barred
/// from exiting to CASH, but can always recover their own escrowed SHARES. Compliance
/// keeps its teeth; nobody gets trapped.
#[test]
fn delisted_holder_cannot_claim_but_can_always_cancel() {
    let e = Env::default();
    let (vault, token, usdc, _a, _g, _ops) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);

    vc.set_allowlist_enabled(&true);
    vc.set_allowed(&user, &true);
    let shares = vc.subscribe(&user, &100_000_000);
    vc.request_redemption(&user, &shares);

    // Sanctions screening removes them mid-flight.
    vc.set_allowed(&user, &false);
    assert!(!vc.is_allowed(&user));

    // Cash exit is closed...
    assert!(vc.try_claim_redemption(&user).is_err());
    // ...but their own shares come back.
    vc.cancel_redemption(&user);
    assert_eq!(TokenContractClient::new(&e, &token).balance(&user), shares);
    assert_eq!(TokenClient::new(&e, &usdc).balance(&user), 0);
}

#[test]
#[should_panic] // NotAllowed
fn delisting_blocks_further_subscription() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _ops) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);

    vc.set_allowlist_enabled(&true);
    vc.set_allowed(&user, &true);
    vc.subscribe(&user, &100_000_000);

    vc.set_allowed(&user, &false);
    vc.subscribe(&user, &100_000_000);
}

#[test]
fn batch_allowlisting_admits_every_address() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _ops) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let (u1, u2, u3) = (
        Address::generate(&e),
        Address::generate(&e),
        Address::generate(&e),
    );

    vc.set_allowlist_enabled(&true);
    vc.set_allowed_many(&signers(&e, &[&u1, &u2, &u3]), &true);

    for u in [&u1, &u2, &u3] {
        assert!(vc.is_allowed(u));
        fund_usdc(&e, &usdc, u, 100_000_000);
        vc.subscribe(u, &100_000_000);
    }
    assert_eq!(vc.total_assets(), 300_000_000);
}

#[test]
#[should_panic] // compliance authority auth absent
fn non_compliance_authority_cannot_allowlist() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g, _ops) = deploy_with_attestor(&e);
    let attacker = Address::generate(&e);
    let target = Address::generate(&e);
    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "set_allowed",
            args: (target.clone(), true).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).set_allowed(&target, &true);
}

/// Separation of duties: the compliance authority decides WHO is on the list, but must not
/// be able to switch off its own oversight. Only governance can disable the gate.
#[test]
#[should_panic] // admin auth absent
fn compliance_authority_cannot_disable_the_gate() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g, compliance) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    vc.set_allowlist_enabled(&true);

    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &compliance,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "set_allowlist_enabled",
            args: (false,).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    vc.set_allowlist_enabled(&false);
}

#[test]
fn governance_can_disable_the_gate_again() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _ops) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);

    vc.set_allowlist_enabled(&true);
    assert!(!vc.is_allowed(&user));
    vc.set_allowlist_enabled(&false);
    assert!(vc.is_allowed(&user));
    vc.subscribe(&user, &100_000_000);
}

// ---- settlement-asset depeg guard --------------------------------------------------

/// A minimal SEP-40 feed, so the depeg guard can be tested without a live oracle.
/// Reports 14 decimals, matching Reflector's public feeds.
#[contract]
pub struct MockFeed;

#[contractimpl]
impl MockFeed {
    pub fn __constructor(e: &Env, price: i128, timestamp: u64) {
        e.storage().instance().set(&symbol_short!("p"), &price);
        e.storage().instance().set(&symbol_short!("t"), &timestamp);
    }
    pub fn set_price(e: &Env, price: i128, timestamp: u64) {
        e.storage().instance().set(&symbol_short!("p"), &price);
        e.storage().instance().set(&symbol_short!("t"), &timestamp);
    }
    /// Drop the price entirely, standing in for a feed with nothing to report.
    pub fn clear(e: &Env) {
        e.storage().instance().remove(&symbol_short!("p"));
    }
    pub fn lastprice(e: &Env, _asset: Asset) -> Option<PriceData> {
        let price: i128 = e.storage().instance().get(&symbol_short!("p"))?;
        let timestamp: u64 = e.storage().instance().get(&symbol_short!("t")).unwrap_or(0);
        Some(PriceData { price, timestamp })
    }
    pub fn decimals(_e: &Env) -> u32 {
        14
    }
}

/// $1.00 at 14 decimals.
const PARITY_14: i128 = 100_000_000_000_000;

fn attach_feed(e: &Env, vc: &VaultContractClient, usdc: &Address, price: i128) -> Address {
    let feed = e.register(MockFeed, (price, e.ledger().timestamp()));
    vc.set_price_oracle(&Some(feed.clone()), &Some(Asset::Stellar(usdc.clone())));
    feed
}

#[test]
fn pegged_settlement_asset_allows_subscription() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _o) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    attach_feed(&e, &vc, &usdc, PARITY_14);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);
    assert_eq!(vc.total_assets(), 100_000_000);
}

#[test]
fn drift_inside_the_band_is_tolerated() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _o) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    // $0.99 — 1% off, inside the 2% band. Ordinary noise must not halt the vault.
    attach_feed(&e, &vc, &usdc, PARITY_14 * 99 / 100);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);
}

/// At $0.90, depositing 100 USDC (worth $90) would mint shares against $100 of NAV,
/// taking $10 from existing holders on every deposit.
#[test]
#[should_panic] // SettlementAssetDepegged
fn depegged_settlement_asset_blocks_subscription() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _o) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    attach_feed(&e, &vc, &usdc, PARITY_14 * 90 / 100);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);
}

#[test]
#[should_panic] // SettlementAssetDepegged — the guard is symmetric
fn upward_depeg_also_blocks_subscription() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _o) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    attach_feed(&e, &vc, &usdc, PARITY_14 * 110 / 100);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);
}

#[test]
#[should_panic] // OraclePriceUnavailable — fails closed
fn stale_oracle_blocks_subscription() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _o) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    attach_feed(&e, &vc, &usdc, PARITY_14);

    e.ledger().with_mut(|l| l.timestamp += 2 * 60 * 60); // past the 1h window
    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);
}

#[test]
#[should_panic] // OraclePriceUnavailable — fails closed
fn absent_oracle_price_blocks_subscription() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _o) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let feed = attach_feed(&e, &vc, &usdc, PARITY_14);
    MockFeedClient::new(&e, &feed).clear();

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000);
}

/// A depeg is arguably exactly when a holder most wants out. Exits are never gated,
/// even though the USDC they receive is worth less than the NAV it represents.
#[test]
fn depeg_never_blocks_exits() {
    let e = Env::default();
    let (vault, token, usdc, _a, _g, _o) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    let feed = attach_feed(&e, &vc, &usdc, PARITY_14);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    let shares = vc.subscribe(&user, &100_000_000);

    // USDC collapses.
    MockFeedClient::new(&e, &feed).set_price(&(PARITY_14 / 2), &e.ledger().timestamp());

    vc.request_redemption(&user, &shares);
    vc.cancel_redemption(&user);
    assert_eq!(TokenContractClient::new(&e, &token).balance(&user), shares);

    vc.request_redemption(&user, &shares);
    assert_eq!(vc.claim_redemption(&user), 100_000_000);
}

#[test]
fn disabling_the_oracle_restores_subscription() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _o) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    attach_feed(&e, &vc, &usdc, PARITY_14 * 50 / 100); // hard depeg
    assert!(vc.get_price_oracle().is_some());

    vc.set_price_oracle(&None, &None);
    assert!(vc.get_price_oracle().is_none());

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 100_000_000);
    vc.subscribe(&user, &100_000_000); // guard is off again
}

#[test]
#[should_panic] // admin auth absent
fn non_admin_cannot_set_the_price_oracle() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, _o) = deploy_with_attestor(&e);
    let feed = e.register(MockFeed, (PARITY_14, 0u64));
    let attacker = Address::generate(&e);
    let asset = Asset::Stellar(usdc.clone());

    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "set_price_oracle",
            args: (Some(feed.clone()), Some(asset.clone())).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).set_price_oracle(&Some(feed), &Some(asset));
}

// ---- the NAV delta cap cannot be amplified by depositing first ----------------------

/// `total_assets` is caller-inflatable: subscribe adds to it immediately. If the delta cap
/// were a percentage of THAT, an attacker could deposit a large sum, have a compromised
/// quorum attest a percentage of the inflated total, redeem, and extract far more
/// fabricated value than the cap was ever meant to permit - with notice at zero, in a
/// single transaction. The cap is therefore measured against the last ATTESTED baseline.
#[test]
#[should_panic] // NavDeltaTooLarge
fn a_large_deposit_does_not_widen_the_nav_delta_cap() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);

    let lp = Address::generate(&e);
    fund_usdc(&e, &usdc, &lp, 100_000_000);
    vc.subscribe(&lp, &100_000_000); // baseline: 100_000_000

    // Attacker inflates the bookkept total tenfold.
    let attacker = Address::generate(&e);
    fund_usdc(&e, &usdc, &attacker, 900_000_000);
    vc.subscribe(&attacker, &900_000_000);
    assert_eq!(vc.total_assets(), 1_000_000_000);

    advance_past_cadence(&e);
    // 2% of the INFLATED total would be 20_000_000 of fabricated value. 2% of the baseline
    // is 2_000_000. This must be refused.
    vc.update_nav(&1_020_000_000, &proof(&e), &signers(&e, &[&attestor]));
}

#[test]
fn an_honest_attestation_after_a_large_deposit_still_succeeds() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);

    let lp = Address::generate(&e);
    fund_usdc(&e, &usdc, &lp, 100_000_000);
    vc.subscribe(&lp, &100_000_000);

    let lp2 = Address::generate(&e);
    fund_usdc(&e, &usdc, &lp2, 900_000_000);
    vc.subscribe(&lp2, &900_000_000);

    advance_past_cadence(&e);
    // A real day's return on 1.0bn is nowhere near the 2m budget; deposits are fully
    // credited to total_assets, they just do not expand the fabrication budget.
    vc.update_nav(&1_000_500_000, &proof(&e), &signers(&e, &[&attestor]));
    assert_eq!(vc.total_assets(), 1_000_500_000);

    // And the newly attested figure becomes the baseline, so the budget grows legitimately.
    advance_past_cadence(&e);
    vc.update_nav(&1_015_000_000, &proof(&e), &signers(&e, &[&attestor]));
    assert_eq!(vc.total_assets(), 1_015_000_000);
}

#[test]
fn shares_ceil_conversion_guarantees_the_payout() {
    // At a share price above 1.0 the flooring conversion lands a stroop short.
    let ts = 100_000_000i128;
    let ta = 102_000_000i128;
    let amount = 10_000_000i128;

    let floored = shares_for_deposit(amount, ts, ta).unwrap();
    assert!(assets_for_shares(floored, ts, ta).unwrap() < amount);

    let ceiled = shares_for_deposit_ceil(amount, ts, ta).unwrap();
    assert!(assets_for_shares(ceiled, ts, ta).unwrap() >= amount);
    // Costs the caller at most one extra share.
    assert_eq!(ceiled, floored + 1);
}

#[test]
fn nav_cadence_defaults_and_is_governable() {
    let e = Env::default();
    let (vault, _t, usdc, _a, _g, attestor) = deploy_with_attestor(&e);
    let vc = VaultContractClient::new(&e, &vault);
    assert_eq!(vc.nav_cadence(), 20 * 60 * 60);

    let user = Address::generate(&e);
    fund_usdc(&e, &usdc, &user, 200_000_000);
    vc.subscribe(&user, &100_000_000);

    // Shortening the cadence lets attestations land sooner - the reason the setting exists,
    // and equally the reason it weakens the walk-protection.
    vc.set_nav_cadence(&60);
    assert_eq!(vc.nav_cadence(), 60);
    e.ledger().with_mut(|l| l.timestamp += 61);
    vc.update_nav(&101_000_000, &proof(&e), &signers(&e, &[&attestor]));
    assert_eq!(vc.total_assets(), 101_000_000);
}

#[test]
#[should_panic] // admin auth absent
fn non_admin_cannot_change_nav_cadence() {
    let e = Env::default();
    let (vault, _t, _u, _a, _g, _o) = deploy_with_attestor(&e);
    let attacker = Address::generate(&e);
    e.set_auths(&[]);
    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &vault,
            fn_name: "set_nav_cadence",
            args: (60u64,).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).set_nav_cadence(&60);
}
