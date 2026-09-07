//! Contract events, one [`contractevent`]-derived struct per state change.
//! Using `#[contractevent]` (rather than raw `env.events().publish(...)`)
//! bakes each event's shape into the contract's on-chain spec, so indexers
//! and generated SDK clients can decode them without hand-maintained
//! knowledge of topic/data layout. See the README's "Events" section for
//! the resulting topic/data shape of each one.

use soroban_sdk::{contractevent, Address, Env};

#[contractevent]
pub struct Initialize {
    #[topic]
    pub token_a: Address,
    #[topic]
    pub token_b: Address,
}

#[contractevent]
pub struct AddLiquidity {
    #[topic]
    pub provider: Address,
    pub amount_a: i128,
    pub amount_b: i128,
    pub shares_minted: i128,
}

#[contractevent]
pub struct RemoveLiquidity {
    #[topic]
    pub provider: Address,
    pub amount_a: i128,
    pub amount_b: i128,
    pub shares_burned: i128,
}

#[contractevent]
pub struct Swap {
    #[topic]
    pub trader: Address,
    #[topic]
    pub token_in: Address,
    pub amount_in: i128,
    pub token_out: Address,
    pub amount_out: i128,
}

pub fn initialize(env: &Env, token_a: &Address, token_b: &Address) {
    Initialize {
        token_a: token_a.clone(),
        token_b: token_b.clone(),
    }
    .publish(env);
}

pub fn add_liquidity(env: &Env, provider: &Address, amount_a: i128, amount_b: i128, shares_minted: i128) {
    AddLiquidity {
        provider: provider.clone(),
        amount_a,
        amount_b,
        shares_minted,
    }
    .publish(env);
}

pub fn remove_liquidity(env: &Env, provider: &Address, amount_a: i128, amount_b: i128, shares_burned: i128) {
    RemoveLiquidity {
        provider: provider.clone(),
        amount_a,
        amount_b,
        shares_burned,
    }
    .publish(env);
}

pub fn swap(
    env: &Env,
    trader: &Address,
    token_in: &Address,
    amount_in: i128,
    token_out: &Address,
    amount_out: i128,
) {
    Swap {
        trader: trader.clone(),
        token_in: token_in.clone(),
        amount_in,
        token_out: token_out.clone(),
        amount_out,
    }
    .publish(env);
}
