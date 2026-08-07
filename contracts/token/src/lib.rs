#![no_std]
//! baUSD — SEP-41-compatible Soroban token.
//!
//! Mint authority is held EXCLUSIVELY by the token owner, which is set to the vault
//! contract at deployment. There is no discretionary issuance: `mint` is gated by
//! `#[only_owner]`, so it requires the owner's authorization, and only the vault can
//! produce that authorization. The vault only ever mints against real deposits.
//!
//! Burning and transfers are standard SEP-41 (the holder authorizes). In redemption the
//! vault holds a redeemer's shares in escrow and burns them as the holder.
//!
//! Built on the audited OpenZeppelin `stellar-tokens` fungible base. baUSD deliberately
//! mints NO initial supply; total supply grows only via the
//! vault. To meet SEP-41, the contract implements both `FungibleToken` and
//! `FungibleBurnable`.

use soroban_sdk::{contract, contractimpl, Address, Env, MuxedAddress, String};
use stellar_access::ownable::{set_owner, Ownable};
use stellar_macros::only_owner;
use stellar_tokens::fungible::{burnable::FungibleBurnable, Base, FungibleToken};

/// baUSD uses 7 decimals to match Stellar-native USDC and keep vault share math aligned.
pub const DECIMALS: u32 = 7;

#[contract]
pub struct TokenContract;

#[contractimpl]
impl TokenContract {
    /// Initialize baUSD. `owner` is the vault contract address and becomes the sole
    /// minter. No initial supply is minted here — supply grows only via vault deposits.
    pub fn __constructor(e: &Env, owner: Address, name: String, symbol: String) {
        Base::set_metadata(e, DECIMALS, name, symbol);
        set_owner(e, &owner);
    }

    /// Mint new baUSD to `to`. Gated by `#[only_owner]`: requires the owner's (the
    /// vault's) authorization, so no other party can ever mint. This is the ONLY path
    /// that increases total supply.
    #[only_owner]
    pub fn mint(e: &Env, to: Address, amount: i128) {
        Base::mint(e, &to, amount);
    }
}

// --- SEP-41 fungible token interface (delegated to the audited OZ Base) ------------
#[contractimpl]
impl FungibleToken for TokenContract {
    type ContractType = Base;

    fn total_supply(e: &Env) -> i128 {
        Self::ContractType::total_supply(e)
    }

    fn balance(e: &Env, account: Address) -> i128 {
        Self::ContractType::balance(e, &account)
    }

    fn allowance(e: &Env, owner: Address, spender: Address) -> i128 {
        Self::ContractType::allowance(e, &owner, &spender)
    }

    fn transfer(e: &Env, from: Address, to: MuxedAddress, amount: i128) {
        Self::ContractType::transfer(e, &from, &to, amount);
    }

    fn transfer_from(e: &Env, spender: Address, from: Address, to: Address, amount: i128) {
        Self::ContractType::transfer_from(e, &spender, &from, &to, amount);
    }

    fn approve(e: &Env, owner: Address, spender: Address, amount: i128, live_until_ledger: u32) {
        Self::ContractType::approve(e, &owner, &spender, amount, live_until_ledger);
    }

    fn decimals(e: &Env) -> u32 {
        Self::ContractType::decimals(e)
    }

    fn name(e: &Env) -> String {
        Self::ContractType::name(e)
    }

    fn symbol(e: &Env) -> String {
        Self::ContractType::symbol(e)
    }
}

#[contractimpl]
impl FungibleBurnable for TokenContract {
    fn burn(e: &Env, from: Address, amount: i128) {
        Self::ContractType::burn(e, &from, amount)
    }

    fn burn_from(e: &Env, spender: Address, from: Address, amount: i128) {
        Self::ContractType::burn_from(e, &spender, &from, amount)
    }
}

// Exposes owner-management entrypoints (get_owner, transfer_ownership, ...), each gated
// on the current owner's auth. Owner = vault, which has no code path to call these, so
// authority stays put unless governance deliberately moves it later.
#[contractimpl(contracttrait)]
impl Ownable for TokenContract {}

#[cfg(test)]
mod test;
