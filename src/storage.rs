//! Contract storage layout and typed accessors.
//!
//! Instance storage holds the small, always-needed pool state (token
//! addresses, reserves, total shares, the reentrancy flag). Persistent
//! storage holds the one thing that scales with the number of users: each
//! LP's share balance.

use crate::errors::PoolError;
use soroban_sdk::{contracttype, panic_with_error, Address, Env};

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    TokenA,
    TokenB,
    ReserveA,
    ReserveB,
    TotalShares,
    /// Per-account LP share balance.
    Share(Address),
    /// Reentrancy guard: present (and `true`) while a state-mutating entry
    /// point is executing.
    EntryLock,
}

// How long (in ledgers) instance/persistent entries are kept alive for, and
// the point at which a call refreshes that TTL. These are the values the
// Soroban examples use as a reasonable default for a live pool; a
// production deployment should tune them to its own economics.
const LEDGER_THRESHOLD: u32 = 17_280; // ~1 day at 5s/ledger
const LEDGER_BUMP: u32 = 30 * LEDGER_THRESHOLD; // ~30 days

pub fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
}

fn bump_persistent(env: &Env, key: &DataKey) {
    env.storage()
        .persistent()
        .extend_ttl(key, LEDGER_THRESHOLD, LEDGER_BUMP);
}

pub fn is_initialized(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::TokenA)
}

pub fn require_initialized(env: &Env) {
    if !is_initialized(env) {
        panic_with_error!(env, PoolError::NotInitialized);
    }
}

pub fn set_token_a(env: &Env, token: &Address) {
    env.storage().instance().set(&DataKey::TokenA, token);
}

pub fn get_token_a(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::TokenA).unwrap()
}

pub fn set_token_b(env: &Env, token: &Address) {
    env.storage().instance().set(&DataKey::TokenB, token);
}

pub fn get_token_b(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::TokenB).unwrap()
}

pub fn get_reserve_a(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::ReserveA)
        .unwrap_or(0)
}

pub fn set_reserve_a(env: &Env, amount: i128) {
    env.storage().instance().set(&DataKey::ReserveA, &amount);
}

pub fn get_reserve_b(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::ReserveB)
        .unwrap_or(0)
}

pub fn set_reserve_b(env: &Env, amount: i128) {
    env.storage().instance().set(&DataKey::ReserveB, &amount);
}

pub fn get_total_shares(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::TotalShares)
        .unwrap_or(0)
}

pub fn set_total_shares(env: &Env, amount: i128) {
    env.storage().instance().set(&DataKey::TotalShares, &amount);
}

pub fn get_share_balance(env: &Env, id: &Address) -> i128 {
    let key = DataKey::Share(id.clone());
    let balance = env.storage().persistent().get(&key).unwrap_or(0);
    if env.storage().persistent().has(&key) {
        bump_persistent(env, &key);
    }
    balance
}

pub fn set_share_balance(env: &Env, id: &Address, amount: i128) {
    let key = DataKey::Share(id.clone());
    if amount == 0 {
        // Nothing owed to this account any more; free the ledger entry
        // instead of leaving a zero-balance row behind forever.
        env.storage().persistent().remove(&key);
    } else {
        env.storage().persistent().set(&key, &amount);
        bump_persistent(env, &key);
    }
}

/// Reentrancy guard. Every public, state-mutating entry point must call
/// [`enter`] before touching state and [`exit`] as its very last step
/// (via an RAII guard, see `lib.rs`), so a malicious/misbehaving token
/// contract that calls back into the pool mid-transfer hits a hard panic
/// instead of re-entering with stale reserves.
pub fn enter(env: &Env) {
    if env.storage().instance().get(&DataKey::EntryLock).unwrap_or(false) {
        panic_with_error!(env, PoolError::ReentrancyDetected);
    }
    env.storage().instance().set(&DataKey::EntryLock, &true);
}

pub fn exit(env: &Env) {
    env.storage().instance().set(&DataKey::EntryLock, &false);
}
