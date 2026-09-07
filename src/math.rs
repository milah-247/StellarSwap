//! Fixed-point integer math helpers.
//!
//! Everything in this contract is `i128`. There are no floats anywhere in
//! the math path. Token amounts are already fixed-point integers scaled by
//! each token's own `decimals` (the same convention every Soroban token,
//! including the Stellar Asset Contract, uses) — the pool never needs a
//! separate internal "precision scale" because it never mixes units: it
//! only ever multiplies/divides amounts of the same token, or cross-multiplies
//! `a * reserve_b` against `b * reserve_a`, which is scale-safe for any pair
//! of token decimals since both sides carry the same implicit scaling.
//!
//! Every operation below is a `checked_*` call that panics with
//! [`PoolError::ArithmeticOverflow`] on overflow, underflow, or division by
//! zero — nothing wraps or saturates silently.

use crate::errors::PoolError;
use soroban_sdk::{panic_with_error, Env};

/// Numerator of the swap fee, out of [`FEE_DENOMINATOR`]. 30 / 10_000 = 0.30%.
pub const FEE_NUMERATOR: i128 = 30;
pub const FEE_DENOMINATOR: i128 = 10_000;

/// Permanently-locked LP shares minted (and never assigned to any account)
/// on the very first deposit. This is the standard Uniswap V2 defense
/// against the "first depositor sets an absurd share price" attack: it
/// guarantees `total_shares` can never be driven back down to a value where
/// a later depositor's rounding error is worth manipulating.
pub const MINIMUM_LIQUIDITY: i128 = 1_000;

pub fn checked_add(env: &Env, a: i128, b: i128) -> i128 {
    a.checked_add(b)
        .unwrap_or_else(|| panic_with_error!(env, PoolError::ArithmeticOverflow))
}

pub fn checked_sub(env: &Env, a: i128, b: i128) -> i128 {
    a.checked_sub(b)
        .unwrap_or_else(|| panic_with_error!(env, PoolError::ArithmeticOverflow))
}

pub fn checked_mul(env: &Env, a: i128, b: i128) -> i128 {
    a.checked_mul(b)
        .unwrap_or_else(|| panic_with_error!(env, PoolError::ArithmeticOverflow))
}

pub fn checked_div(env: &Env, a: i128, b: i128) -> i128 {
    if b == 0 {
        panic_with_error!(env, PoolError::ArithmeticOverflow);
    }
    a.checked_div(b)
        .unwrap_or_else(|| panic_with_error!(env, PoolError::ArithmeticOverflow))
}

/// `a * b / c`, evaluated with all intermediate arithmetic checked so a
/// large-but-valid result never silently wraps.
pub fn checked_mul_div(env: &Env, a: i128, b: i128, c: i128) -> i128 {
    let product = checked_mul(env, a, b);
    checked_div(env, product, c)
}

/// Integer square root via the Babylonian (Newton's) method. Used only to
/// price the first liquidity deposit as `sqrt(amount_a * amount_b)`, exactly
/// as Uniswap V2 does.
pub fn isqrt(env: &Env, value: i128) -> i128 {
    if value < 0 {
        panic_with_error!(env, PoolError::ArithmeticOverflow);
    }
    if value == 0 {
        return 0;
    }
    let mut x = value;
    let mut y = checked_div(env, checked_add(env, x, 1), 2);
    while y < x {
        x = y;
        let term = checked_div(env, value, x);
        y = checked_div(env, checked_add(env, x, term), 2);
    }
    x
}

pub fn min(a: i128, b: i128) -> i128 {
    if a < b {
        a
    } else {
        b
    }
}
