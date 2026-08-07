#![cfg(test)]
use crate::{TokenContract, TokenContractClient, DECIMALS};
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    Address, Env, IntoVal, MuxedAddress, String,
};

/// Register a fresh baUSD token owned by a generated `owner` (stands in for the vault).
/// Returns (owner, contract_id).
fn setup(e: &Env) -> (Address, Address) {
    let owner = Address::generate(e);
    let id = e.register(
        TokenContract,
        (
            owner.clone(),
            String::from_str(e, "Ballast USD"),
            String::from_str(e, "baUSD"),
        ),
    );
    (owner, id)
}

#[test]
fn constructor_sets_metadata_and_zero_supply() {
    let e = Env::default();
    let (_owner, id) = setup(&e);
    let client = TokenContractClient::new(&e, &id);

    assert_eq!(client.name(), String::from_str(&e, "Ballast USD"));
    assert_eq!(client.symbol(), String::from_str(&e, "baUSD"));
    assert_eq!(client.decimals(), DECIMALS);
    // baUSD mints no initial supply; it grows only via the vault.
    assert_eq!(client.total_supply(), 0);
}

#[test]
fn owner_can_mint() {
    let e = Env::default();
    e.mock_all_auths();
    let (_owner, id) = setup(&e);
    let client = TokenContractClient::new(&e, &id);

    let user = Address::generate(&e);
    let amount = 10_000_000_000i128; // 1000 baUSD at 7 decimals (1000 * 10^7)
    client.mint(&user, &amount);

    assert_eq!(client.balance(&user), amount);
    assert_eq!(client.total_supply(), amount);
}

/// The core invariant: an authenticated party that is NOT the owner still cannot mint.
/// We authorize the attacker for the exact `mint` call; it must still fail because
/// `#[only_owner]` requires the OWNER's authorization, which is absent.
#[test]
#[should_panic]
fn authenticated_non_owner_cannot_mint() {
    let e = Env::default();
    let (_owner, id) = setup(&e);
    let client = TokenContractClient::new(&e, &id);

    let attacker = Address::generate(&e);
    let user = Address::generate(&e);

    e.mock_auths(&[MockAuth {
        address: &attacker,
        invoke: &MockAuthInvoke {
            contract: &id,
            fn_name: "mint",
            args: (user.clone(), 100i128).into_val(&e),
            sub_invokes: &[],
        },
    }]);

    client.mint(&user, &100i128);
}

#[test]
fn holder_can_burn_their_own() {
    let e = Env::default();
    e.mock_all_auths();
    let (_owner, id) = setup(&e);
    let client = TokenContractClient::new(&e, &id);

    let user = Address::generate(&e);
    client.mint(&user, &1_000i128);
    client.burn(&user, &400i128);

    assert_eq!(client.balance(&user), 600i128);
    assert_eq!(client.total_supply(), 600i128);
}

#[test]
fn transfer_moves_balance() {
    let e = Env::default();
    e.mock_all_auths();
    let (_owner, id) = setup(&e);
    let client = TokenContractClient::new(&e, &id);

    let a = Address::generate(&e);
    let b = Address::generate(&e);
    client.mint(&a, &1_000i128);

    let to: MuxedAddress = b.clone().into();
    client.transfer(&a, &to, &250i128);

    assert_eq!(client.balance(&a), 750i128);
    assert_eq!(client.balance(&b), 250i128);
}
