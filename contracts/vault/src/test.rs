#![cfg(test)]
use crate::{
    assets_for_shares, shares_for_deposit, VaultContract, VaultContractClient, MIN_INITIAL_DEPOSIT,
};
use ba_usd_token::{TokenContract, TokenContractClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger, MockAuth, MockAuthInvoke},
    token::{StellarAssetClient, TokenClient},
    Address, BytesN, Env, IntoVal, String,
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
    (vault, token, usdc_addr, admin, guardian)
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
            fn_name: "set_admin",
            args: (new_admin.clone(),).into_val(&e),
            sub_invokes: &[],
        },
    }]);
    VaultContractClient::new(&e, &vault).set_admin(&new_admin);
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
