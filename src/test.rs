#![cfg(test)]
//! Behavioral test suite for [`crate::ConstantProductPool`].
//!
//! Covers: a normal exact-in swap and its fee math, add/remove liquidity
//! (including the first-deposit share pricing), the zero-liquidity edge
//! case, slippage rejection on both swap directions and on liquidity ops,
//! and overflow rejection on oversized inputs.

use crate::{ConstantProductPool, ConstantProductPoolClient};
use soroban_sdk::{
    testutils::{Address as _, Events as _},
    token, Address, Env,
};

/// Deploys a Stellar Asset Contract test token and returns both a
/// transfer-capable `token::Client` and an admin `StellarAssetClient` for
/// minting test balances.
fn create_token<'a>(env: &Env, admin: &Address) -> (token::Client<'a>, token::StellarAssetClient<'a>) {
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    (
        token::Client::new(env, &sac.address()),
        token::StellarAssetClient::new(env, &sac.address()),
    )
}

struct TestPool<'a> {
    env: Env,
    pool: ConstantProductPoolClient<'a>,
    token_a: token::Client<'a>,
    token_b: token::Client<'a>,
    user1: Address,
    user2: Address,
}

/// Sets up a pool over two freshly-minted test tokens, with `user1` and
/// `user2` each funded with a large balance of both. Does not seed any
/// liquidity — call `add_liquidity` from the test to do that.
fn setup() -> TestPool<'static> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let (token_a, token_a_admin) = create_token(&env, &admin);
    let (token_b, token_b_admin) = create_token(&env, &admin);

    // Canonicalize so the pool's token_a/token_b matches whichever of the
    // two SAC addresses sorts first -- the contract itself does not care
    // about ordering, but keeping it consistent makes test assertions
    // (e.g. get_reserves() as (a, b)) predictable across runs.
    let (token_a, token_a_admin, token_b, token_b_admin) =
        if token_a.address < token_b.address {
            (token_a, token_a_admin, token_b, token_b_admin)
        } else {
            (token_b, token_b_admin, token_a, token_a_admin)
        };

    let pool_id = env.register(ConstantProductPool, ());
    let pool = ConstantProductPoolClient::new(&env, &pool_id);
    pool.initialize(&token_a.address, &token_b.address);

    let user1 = Address::generate(&env);
    let user2 = Address::generate(&env);
    for user in [&user1, &user2] {
        token_a_admin.mint(user, &1_000_000_000);
        token_b_admin.mint(user, &1_000_000_000);
    }

    TestPool {
        env,
        pool,
        token_a,
        token_b,
        user1,
        user2,
    }
}

// ---------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------

#[test]
fn test_initialize_sets_tokens_and_zero_reserves() {
    let t = setup();
    assert_eq!(t.pool.token_a(), t.token_a.address);
    assert_eq!(t.pool.token_b(), t.token_b.address);
    assert_eq!(t.pool.get_reserves(), (0, 0));
    assert_eq!(t.pool.total_shares(), 0);
}

#[test]
fn test_initialize_twice_rejected() {
    let t = setup();
    let res = t.pool.try_initialize(&t.token_a.address, &t.token_b.address);
    assert!(res.is_err());
}

#[test]
fn test_initialize_identical_tokens_rejected() {
    let env = Env::default();
    let admin = Address::generate(&env);
    let (token_a, _) = create_token(&env, &admin);
    let pool_id = env.register(ConstantProductPool, ());
    let pool = ConstantProductPoolClient::new(&env, &pool_id);
    let res = pool.try_initialize(&token_a.address, &token_a.address);
    assert!(res.is_err());
}

// ---------------------------------------------------------------------
// add_liquidity / remove_liquidity
// ---------------------------------------------------------------------

#[test]
fn test_first_deposit_prices_shares_as_sqrt_and_locks_minimum_liquidity() {
    let t = setup();

    // 10_000 * 40_000 = 400_000_000 = 20_000^2, an exact square so the
    // expected share count is exact too: 20_000 - MINIMUM_LIQUIDITY(1_000).
    let (amount_a, amount_b, shares) = t.pool.add_liquidity(&t.user1, &10_000, &40_000, &0, &0);

    assert_eq!(amount_a, 10_000);
    assert_eq!(amount_b, 40_000);
    assert_eq!(shares, 19_000);
    assert_eq!(t.pool.balance(&t.user1), 19_000);
    // Total supply includes the 1_000 permanently-locked shares that were
    // never credited to any account.
    assert_eq!(t.pool.total_shares(), 20_000);
    assert_eq!(t.pool.get_reserves(), (10_000, 40_000));

    assert_eq!(t.token_a.balance(&t.pool.address), 10_000);
    assert_eq!(t.token_b.balance(&t.pool.address), 40_000);
}

#[test]
fn test_second_deposit_mints_shares_proportionally() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &10_000, &40_000, &0, &0);

    // Depositing exactly the pool's current ratio (1:4) again should mint
    // exactly the same number of shares as total_shares currently holds
    // (doubling the pool doubles the supply).
    let total_before = t.pool.total_shares();
    let (amount_a, amount_b, shares) = t.pool.add_liquidity(&t.user2, &10_000, &40_000, &0, &0);

    assert_eq!(amount_a, 10_000);
    assert_eq!(amount_b, 40_000);
    assert_eq!(shares, total_before);
    assert_eq!(t.pool.get_reserves(), (20_000, 80_000));
}

#[test]
fn test_add_liquidity_clamps_to_pool_ratio() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &10_000, &40_000, &0, &0);

    // user2 offers a 1:1 ratio (10_000 / 10_000) into a pool priced 1:4.
    // The contract should only take as much of token_b as the ratio
    // requires (40_000) is too much -- so it clamps token_a down instead:
    // amount_b_optimal for 10_000 A would be 40_000 B, which exceeds the
    // 10_000 B desired, so it flips to clamping A: amount_a_optimal =
    // 10_000 (B desired) * 10_000 (reserve A) / 40_000 (reserve B) = 2_500.
    let (amount_a, amount_b, _shares) =
        t.pool.add_liquidity(&t.user2, &10_000, &10_000, &0, &0);

    assert_eq!(amount_a, 2_500);
    assert_eq!(amount_b, 10_000);
}

#[test]
fn test_remove_liquidity_returns_proportional_reserves() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &10_000, &40_000, &0, &0);

    let a_before = t.token_a.balance(&t.user1);
    let b_before = t.token_b.balance(&t.user1);

    let shares = t.pool.balance(&t.user1);
    let (amount_a, amount_b) = t.pool.remove_liquidity(&t.user1, &shares, &0, &0);

    // user1 holds all 19_000 non-locked shares out of 20_000 total, so
    // withdrawing all of them returns 19_000/20_000 of each reserve.
    assert_eq!(amount_a, 9_500);
    assert_eq!(amount_b, 38_000);
    assert_eq!(t.pool.balance(&t.user1), 0);
    assert_eq!(t.pool.get_reserves(), (500, 2_000));

    assert_eq!(t.token_a.balance(&t.user1), a_before + amount_a);
    assert_eq!(t.token_b.balance(&t.user1), b_before + amount_b);
}

#[test]
fn test_remove_liquidity_more_than_owned_rejected() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &10_000, &40_000, &0, &0);

    let owned = t.pool.balance(&t.user1);
    let res = t.pool.try_remove_liquidity(&t.user1, &(owned + 1), &0, &0);
    assert!(res.is_err());
}

// ---------------------------------------------------------------------
// zero-liquidity edge case
// ---------------------------------------------------------------------

#[test]
fn test_swap_against_empty_pool_rejected() {
    let t = setup();
    let res = t.pool.try_swap_exact_in(&t.user1, &t.token_a.address, &1_000, &0, &t.user1);
    assert!(res.is_err());
}

#[test]
fn test_remove_liquidity_from_empty_pool_rejected() {
    let t = setup();
    let res = t.pool.try_remove_liquidity(&t.user1, &1, &0, &0);
    assert!(res.is_err());
}

// ---------------------------------------------------------------------
// swap_exact_in / swap_exact_out — normal path + fee math
// ---------------------------------------------------------------------

#[test]
fn test_swap_exact_in_matches_constant_product_formula_with_fee() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &1_000_000, &1_000_000, &0, &0);

    let amount_in: i128 = 10_000;
    // amount_in_with_fee = 10_000 * 9_970 = 99_700_000
    // numerator = 99_700_000 * 1_000_000
    // denominator = 1_000_000 * 10_000 + 99_700_000 = 10_099_700_000
    let amount_in_with_fee = amount_in * 9_970;
    let expected_out = (amount_in_with_fee * 1_000_000) / (1_000_000 * 10_000 + amount_in_with_fee);
    assert_eq!(expected_out, 9_871); // sanity-check the hand-computed expectation

    let quoted = t.pool.quote_amount_out(&t.token_a.address, &amount_in);
    assert_eq!(quoted, expected_out);

    let a_before = t.token_a.balance(&t.user2);
    let b_before = t.token_b.balance(&t.user2);

    let amount_out = t.pool.swap_exact_in(
        &t.user2,
        &t.token_a.address,
        &amount_in,
        &expected_out, // min_amount_out == exact expectation: must not under-deliver
        &t.user2,
    );

    // `events().all()` only reflects the most recent contract invocation,
    // so capture it immediately after the swap, before any other call.
    let events = t.env.events().all();
    assert!(!events.events().is_empty());

    assert_eq!(amount_out, expected_out);
    assert_eq!(t.token_a.balance(&t.user2), a_before - amount_in);
    assert_eq!(t.token_b.balance(&t.user2), b_before + amount_out);
    assert_eq!(t.pool.get_reserves(), (1_000_000 + amount_in, 1_000_000 - amount_out));
}

#[test]
fn test_swap_exact_out_matches_exact_in_inverse() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &1_000_000, &1_000_000, &0, &0);

    let desired_out: i128 = 9_871;
    let amount_in = t
        .pool
        .swap_exact_out(&t.user2, &t.token_b.address, &desired_out, &10_000, &t.user2);

    // The exact-out quote should require no more than the exact-in swap
    // that produced (approximately) the same output.
    assert!(amount_in <= 10_000);
    assert_eq!(t.pool.get_reserves(), (1_000_000 + amount_in, 1_000_000 - desired_out));
}

#[test]
fn test_round_trip_swap_costs_the_fee() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &1_000_000, &1_000_000, &0, &0);

    let start = t.token_a.balance(&t.user2);
    let out_b = t.pool.swap_exact_in(&t.user2, &t.token_a.address, &100_000, &0, &t.user2);
    let out_a = t.pool.swap_exact_in(&t.user2, &t.token_b.address, &out_b, &0, &t.user2);

    // Two 0.3%-fee swaps should leave the trader strictly worse off than
    // where they started (the fee accrues to the pool/LPs, not back to the
    // trader), and the pool's invariant (x*y) should have grown.
    assert!(out_a < 100_000);
    assert_eq!(t.token_a.balance(&t.user2), start - 100_000 + out_a);

    let (reserve_a, reserve_b) = t.pool.get_reserves();
    assert!(reserve_a * reserve_b > 1_000_000_i128 * 1_000_000_i128);
}

// ---------------------------------------------------------------------
// slippage rejection
// ---------------------------------------------------------------------

#[test]
fn test_swap_exact_in_slippage_rejected() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &1_000_000, &1_000_000, &0, &0);

    let quoted = t.pool.quote_amount_out(&t.token_a.address, &10_000);
    // Demand one unit more than the pool will actually deliver.
    let res = t.pool.try_swap_exact_in(
        &t.user2,
        &t.token_a.address,
        &10_000,
        &(quoted + 1),
        &t.user2,
    );
    assert!(res.is_err());
}

#[test]
fn test_swap_exact_out_slippage_rejected() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &1_000_000, &1_000_000, &0, &0);

    let amount_out: i128 = 9_871;
    let res = t.pool.try_swap_exact_out(
        &t.user2,
        &t.token_b.address,
        &amount_out,
        &1, // absurdly low cap
        &t.user2,
    );
    assert!(res.is_err());
}

#[test]
fn test_add_liquidity_slippage_rejected() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &10_000, &40_000, &0, &0);

    // user2 wants at least 5_000 B for 1_000 A, but the pool's ratio (1:4)
    // only yields 4_000 B for 1_000 A.
    let res = t.pool.try_add_liquidity(&t.user2, &1_000, &5_000, &0, &5_000);
    assert!(res.is_err());
}

#[test]
fn test_remove_liquidity_slippage_rejected() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &10_000, &40_000, &0, &0);

    let shares = t.pool.balance(&t.user1);
    // Demand more A back than a full withdrawal of these shares yields.
    let res = t.pool.try_remove_liquidity(&t.user1, &shares, &1_000_000, &0);
    assert!(res.is_err());
}

// ---------------------------------------------------------------------
// overflow rejection
// ---------------------------------------------------------------------

#[test]
fn test_swap_with_overflowing_amount_rejected() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &1_000_000, &1_000_000, &0, &0);

    // amount_in near i128::MAX overflows the `amount_in * (FEE_DENOMINATOR
    // - FEE_NUMERATOR)` multiplication inside get_amount_out, and must be
    // rejected rather than wrapping.
    let res = t.pool.try_swap_exact_in(
        &t.user2,
        &t.token_a.address,
        &i128::MAX,
        &0,
        &t.user2,
    );
    assert!(res.is_err());
}

#[test]
fn test_add_liquidity_with_overflowing_amount_rejected() {
    let t = setup();
    // First deposit computes isqrt(amount_a * amount_b); i128::MAX * 2
    // overflows the multiplication and must be rejected.
    let res = t.pool.try_add_liquidity(&t.user1, &i128::MAX, &2, &0, &0);
    assert!(res.is_err());
}

#[test]
fn test_negative_amount_rejected() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &1_000_000, &1_000_000, &0, &0);

    let res = t.pool.try_swap_exact_in(&t.user2, &t.token_a.address, &-1, &0, &t.user2);
    assert!(res.is_err());
}

// ---------------------------------------------------------------------
// LP share transfer
// ---------------------------------------------------------------------

#[test]
fn test_transfer_shares_moves_balance() {
    let t = setup();
    t.pool.add_liquidity(&t.user1, &10_000, &40_000, &0, &0);

    let shares = t.pool.balance(&t.user1);
    t.pool.transfer_shares(&t.user1, &t.user2, &shares);

    assert_eq!(t.pool.balance(&t.user1), 0);
    assert_eq!(t.pool.balance(&t.user2), shares);

    // user2 can now redeem the shares they received.
    let (amount_a, amount_b) = t.pool.remove_liquidity(&t.user2, &shares, &0, &0);
    assert!(amount_a > 0 && amount_b > 0);
}
