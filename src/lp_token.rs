//! The pool's LP share ledger.
//!
//! Shares are a minimal custom fungible token, tracked directly in this
//! contract's storage rather than deployed as a separate SEP-41 token
//! contract. Each share represents a claim on a proportional slice of the
//! pool's two reserves; they are minted on `add_liquidity`, burned on
//! `remove_liquidity`, and freely transferable between accounts via
//! [`transfer`] like any other fungible asset. See the README's
//! "Why an in-contract LP token instead of a separate SAC" note for the
//! reasoning.

use crate::errors::PoolError;
use crate::math::{checked_add, checked_sub};
use crate::storage;
use soroban_sdk::{panic_with_error, Address, Env};

pub fn total_supply(env: &Env) -> i128 {
    storage::get_total_shares(env)
}

pub fn balance(env: &Env, id: &Address) -> i128 {
    storage::get_share_balance(env, id)
}

/// Mint `amount` new shares to `to`, increasing total supply. `amount` must
/// already be validated as positive by the caller.
pub fn mint(env: &Env, to: &Address, amount: i128) {
    let balance = storage::get_share_balance(env, to);
    storage::set_share_balance(env, to, checked_add(env, balance, amount));

    let supply = storage::get_total_shares(env);
    storage::set_total_shares(env, checked_add(env, supply, amount));
}

/// Increase total supply only, without crediting any account. Used exactly
/// once, on the first deposit, to permanently lock `MINIMUM_LIQUIDITY`
/// shares (see `math::MINIMUM_LIQUIDITY`).
pub fn mint_unallocated(env: &Env, amount: i128) {
    let supply = storage::get_total_shares(env);
    storage::set_total_shares(env, checked_add(env, supply, amount));
}

/// Burn `amount` shares from `from`, decreasing total supply. Panics if
/// `from` does not hold at least `amount` shares.
pub fn burn(env: &Env, from: &Address, amount: i128) {
    let balance = storage::get_share_balance(env, from);
    if balance < amount {
        panic_with_error!(env, PoolError::InsufficientShareBalance);
    }
    storage::set_share_balance(env, from, checked_sub(env, balance, amount));

    let supply = storage::get_total_shares(env);
    storage::set_total_shares(env, checked_sub(env, supply, amount));
}

/// Move `amount` shares from `from` to `to`. Requires `from`'s
/// authorization. Exposed so LP positions can be transferred, sold, or used
/// as collateral elsewhere without withdrawing the underlying liquidity.
pub fn transfer(env: &Env, from: &Address, to: &Address, amount: i128) {
    from.require_auth();
    if amount <= 0 {
        panic_with_error!(env, PoolError::AmountMustBePositive);
    }
    let from_balance = storage::get_share_balance(env, from);
    if from_balance < amount {
        panic_with_error!(env, PoolError::InsufficientShareBalance);
    }
    storage::set_share_balance(env, from, checked_sub(env, from_balance, amount));

    let to_balance = storage::get_share_balance(env, to);
    storage::set_share_balance(env, to, checked_add(env, to_balance, amount));
}
