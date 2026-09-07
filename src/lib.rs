//! StellarSwap — a constant-product (`x * y = k`) automated market maker for
//! two arbitrary Stellar tokens, in the style of Uniswap V2.
//!
//! One deployed instance of [`ConstantProductPool`] holds a pool for exactly
//! one token pair, configured once via [`ConstantProductPool::initialize`].
//! See `README.md` for the AMM math, the fee model, and deployment
//! instructions; see `src/test.rs` for the behavioral test suite.
#![no_std]

mod errors;
mod events;
mod lp_token;
mod math;
mod storage;

#[cfg(test)]
mod test;

use errors::PoolError;
use math::{
    checked_add, checked_div, checked_mul, checked_mul_div, checked_sub, isqrt, min,
    FEE_DENOMINATOR, FEE_NUMERATOR, MINIMUM_LIQUIDITY,
};
use soroban_sdk::{contract, contractimpl, panic_with_error, token, Address, Env};

#[contract]
pub struct ConstantProductPool;

/// RAII reentrancy guard. Acquires the pool's single entry lock when a
/// state-mutating call begins and releases it when the call returns. If a
/// token contract invoked mid-call (a malicious or buggy `transfer`
/// implementation) calls back into any guarded pool function, that call
/// hits an already-held lock and panics with [`PoolError::ReentrancyDetected`],
/// which aborts and rolls back the entire host transaction.
struct EntryGuard<'a> {
    env: &'a Env,
}

impl<'a> EntryGuard<'a> {
    fn new(env: &'a Env) -> Self {
        storage::enter(env);
        Self { env }
    }
}

impl<'a> Drop for EntryGuard<'a> {
    fn drop(&mut self) {
        storage::exit(self.env);
    }
}

#[contractimpl]
impl ConstantProductPool {
    /// Configure a freshly-deployed pool instance for the `(token_a,
    /// token_b)` pair. Can only be called once. Does not take any deposit —
    /// call [`Self::add_liquidity`] afterwards to seed the pool.
    pub fn initialize(env: Env, token_a: Address, token_b: Address) {
        if storage::is_initialized(&env) {
            panic_with_error!(&env, PoolError::AlreadyInitialized);
        }
        if token_a == token_b {
            panic_with_error!(&env, PoolError::IdenticalTokens);
        }

        storage::set_token_a(&env, &token_a);
        storage::set_token_b(&env, &token_b);
        storage::set_reserve_a(&env, 0);
        storage::set_reserve_b(&env, 0);
        storage::set_total_shares(&env, 0);
        storage::bump_instance(&env);

        events::initialize(&env, &token_a, &token_b);
    }

    /// Deposit up to `(amount_a_desired, amount_b_desired)` of the pool's
    /// two tokens, at the pool's current price, minting LP shares to
    /// `from` in return. If the pool already has liquidity, the actual
    /// amounts taken are whichever of the two desired amounts keeps the
    /// pool's existing `reserve_a / reserve_b` ratio, clamped down from the
    /// desired amounts — never up. `amount_a_min` / `amount_b_min` bound how
    /// far that clamping is allowed to go (slippage protection): the call
    /// panics rather than accept a worse ratio than the caller specified.
    ///
    /// Returns `(amount_a_deposited, amount_b_deposited, shares_minted)`.
    pub fn add_liquidity(
        env: Env,
        from: Address,
        amount_a_desired: i128,
        amount_b_desired: i128,
        amount_a_min: i128,
        amount_b_min: i128,
    ) -> (i128, i128, i128) {
        let _guard = EntryGuard::new(&env);
        storage::require_initialized(&env);
        from.require_auth();

        if amount_a_desired <= 0 || amount_b_desired <= 0 {
            panic_with_error!(&env, PoolError::AmountMustBePositive);
        }
        if amount_a_min < 0 || amount_b_min < 0 {
            panic_with_error!(&env, PoolError::AmountMustBePositive);
        }

        let reserve_a = storage::get_reserve_a(&env);
        let reserve_b = storage::get_reserve_b(&env);

        let (amount_a, amount_b) = Self::optimal_deposit_amounts(
            &env,
            reserve_a,
            reserve_b,
            amount_a_desired,
            amount_b_desired,
            amount_a_min,
            amount_b_min,
        );

        let total_shares = lp_token::total_supply(&env);
        let shares_minted = if total_shares == 0 {
            // First-ever deposit: price shares as sqrt(a * b), Uniswap-V2
            // style, and permanently lock MINIMUM_LIQUIDITY of them so the
            // share price can never be manipulated back down to zero
            // reserves by a full withdrawal.
            let liquidity = isqrt(&env, checked_mul(&env, amount_a, amount_b));
            if liquidity <= MINIMUM_LIQUIDITY {
                panic_with_error!(&env, PoolError::InsufficientInitialLiquidity);
            }
            let shares = checked_sub(&env, liquidity, MINIMUM_LIQUIDITY);
            lp_token::mint_unallocated(&env, MINIMUM_LIQUIDITY);
            lp_token::mint(&env, &from, shares);
            shares
        } else {
            let shares_from_a = checked_mul_div(&env, amount_a, total_shares, reserve_a);
            let shares_from_b = checked_mul_div(&env, amount_b, total_shares, reserve_b);
            let shares = min(shares_from_a, shares_from_b);
            if shares <= 0 {
                panic_with_error!(&env, PoolError::InsufficientInitialLiquidity);
            }
            lp_token::mint(&env, &from, shares);
            shares
        };

        // Effects before interactions: reserves and share balances are
        // committed before the token contracts are ever invoked.
        storage::set_reserve_a(&env, checked_add(&env, reserve_a, amount_a));
        storage::set_reserve_b(&env, checked_add(&env, reserve_b, amount_b));
        storage::bump_instance(&env);

        let token_a_client = token::Client::new(&env, &storage::get_token_a(&env));
        let token_b_client = token::Client::new(&env, &storage::get_token_b(&env));
        token_a_client.transfer(&from, env.current_contract_address(), &amount_a);
        token_b_client.transfer(&from, env.current_contract_address(), &amount_b);

        events::add_liquidity(&env, &from, amount_a, amount_b, shares_minted);

        (amount_a, amount_b, shares_minted)
    }

    /// Burn `shares` LP shares held by `from`, returning a proportional
    /// slice of both reserves. `amount_a_min` / `amount_b_min` are slippage
    /// protection: the call panics if either leg would return less than
    /// requested.
    ///
    /// Returns `(amount_a_returned, amount_b_returned)`.
    pub fn remove_liquidity(
        env: Env,
        from: Address,
        shares: i128,
        amount_a_min: i128,
        amount_b_min: i128,
    ) -> (i128, i128) {
        let _guard = EntryGuard::new(&env);
        storage::require_initialized(&env);
        from.require_auth();

        if shares <= 0 {
            panic_with_error!(&env, PoolError::AmountMustBePositive);
        }

        let total_shares = lp_token::total_supply(&env);
        if total_shares == 0 {
            panic_with_error!(&env, PoolError::InsufficientLiquidity);
        }

        let reserve_a = storage::get_reserve_a(&env);
        let reserve_b = storage::get_reserve_b(&env);

        let amount_a = checked_mul_div(&env, shares, reserve_a, total_shares);
        let amount_b = checked_mul_div(&env, shares, reserve_b, total_shares);

        if amount_a <= 0 || amount_b <= 0 {
            panic_with_error!(&env, PoolError::InsufficientLiquidity);
        }
        if amount_a < amount_a_min {
            panic_with_error!(&env, PoolError::InsufficientAOutput);
        }
        if amount_b < amount_b_min {
            panic_with_error!(&env, PoolError::InsufficientBOutput);
        }

        // Effects before interactions.
        lp_token::burn(&env, &from, shares);
        storage::set_reserve_a(&env, checked_sub(&env, reserve_a, amount_a));
        storage::set_reserve_b(&env, checked_sub(&env, reserve_b, amount_b));
        storage::bump_instance(&env);

        let token_a_client = token::Client::new(&env, &storage::get_token_a(&env));
        let token_b_client = token::Client::new(&env, &storage::get_token_b(&env));
        token_a_client.transfer(&env.current_contract_address(), &from, &amount_a);
        token_b_client.transfer(&env.current_contract_address(), &from, &amount_b);

        events::remove_liquidity(&env, &from, amount_a, amount_b, shares);

        (amount_a, amount_b)
    }

    /// Swap an exact `amount_in` of `token_in` (which must be one of the
    /// pool's two configured tokens) for as much of the other token as the
    /// constant-product curve yields after the 0.3% fee, sending the output
    /// to `to`. Panics if the realized output is below `min_amount_out`.
    ///
    /// Returns the amount of the output token sent.
    pub fn swap_exact_in(
        env: Env,
        from: Address,
        token_in: Address,
        amount_in: i128,
        min_amount_out: i128,
        to: Address,
    ) -> i128 {
        let _guard = EntryGuard::new(&env);
        storage::require_initialized(&env);
        from.require_auth();

        if amount_in <= 0 {
            panic_with_error!(&env, PoolError::AmountMustBePositive);
        }
        if min_amount_out < 0 {
            panic_with_error!(&env, PoolError::AmountMustBePositive);
        }

        let token_a = storage::get_token_a(&env);
        let token_b = storage::get_token_b(&env);
        let (reserve_in, reserve_out, is_a_in) = if token_in == token_a {
            (storage::get_reserve_a(&env), storage::get_reserve_b(&env), true)
        } else if token_in == token_b {
            (storage::get_reserve_b(&env), storage::get_reserve_a(&env), false)
        } else {
            panic_with_error!(&env, PoolError::InvalidToken);
        };

        if reserve_in <= 0 || reserve_out <= 0 {
            panic_with_error!(&env, PoolError::InsufficientLiquidity);
        }

        let amount_out = Self::get_amount_out(&env, amount_in, reserve_in, reserve_out);

        if amount_out <= 0 || amount_out >= reserve_out {
            panic_with_error!(&env, PoolError::InsufficientOutputReserve);
        }
        if amount_out < min_amount_out {
            panic_with_error!(&env, PoolError::SlippageExceededMinOut);
        }

        let token_out = if is_a_in { token_b.clone() } else { token_a.clone() };

        // Effects before interactions.
        if is_a_in {
            storage::set_reserve_a(&env, checked_add(&env, reserve_in, amount_in));
            storage::set_reserve_b(&env, checked_sub(&env, reserve_out, amount_out));
        } else {
            storage::set_reserve_b(&env, checked_add(&env, reserve_in, amount_in));
            storage::set_reserve_a(&env, checked_sub(&env, reserve_out, amount_out));
        }
        storage::bump_instance(&env);

        let token_in_client = token::Client::new(&env, &token_in);
        let token_out_client = token::Client::new(&env, &token_out);
        token_in_client.transfer(&from, env.current_contract_address(), &amount_in);
        token_out_client.transfer(&env.current_contract_address(), &to, &amount_out);

        events::swap(&env, &from, &token_in, amount_in, &token_out, amount_out);

        amount_out
    }

    /// Swap up to `max_amount_in` of the other token for an exact
    /// `amount_out` of `token_out` (which must be one of the pool's two
    /// configured tokens), sending the output to `to`. Panics if the
    /// required input exceeds `max_amount_in`.
    ///
    /// Returns the amount of the input token actually taken.
    pub fn swap_exact_out(
        env: Env,
        from: Address,
        token_out: Address,
        amount_out: i128,
        max_amount_in: i128,
        to: Address,
    ) -> i128 {
        let _guard = EntryGuard::new(&env);
        storage::require_initialized(&env);
        from.require_auth();

        if amount_out <= 0 {
            panic_with_error!(&env, PoolError::AmountMustBePositive);
        }
        if max_amount_in <= 0 {
            panic_with_error!(&env, PoolError::AmountMustBePositive);
        }

        let token_a = storage::get_token_a(&env);
        let token_b = storage::get_token_b(&env);
        let (reserve_out, reserve_in, is_a_out) = if token_out == token_a {
            (storage::get_reserve_a(&env), storage::get_reserve_b(&env), true)
        } else if token_out == token_b {
            (storage::get_reserve_b(&env), storage::get_reserve_a(&env), false)
        } else {
            panic_with_error!(&env, PoolError::InvalidToken);
        };

        if reserve_in <= 0 || reserve_out <= 0 {
            panic_with_error!(&env, PoolError::InsufficientLiquidity);
        }

        let amount_in = Self::get_amount_in(&env, amount_out, reserve_in, reserve_out);

        if amount_in <= 0 {
            panic_with_error!(&env, PoolError::AmountMustBePositive);
        }
        if amount_in > max_amount_in {
            panic_with_error!(&env, PoolError::SlippageExceededMaxIn);
        }

        let token_in = if is_a_out { token_b.clone() } else { token_a.clone() };

        // Effects before interactions.
        if is_a_out {
            storage::set_reserve_b(&env, checked_add(&env, reserve_in, amount_in));
            storage::set_reserve_a(&env, checked_sub(&env, reserve_out, amount_out));
        } else {
            storage::set_reserve_a(&env, checked_add(&env, reserve_in, amount_in));
            storage::set_reserve_b(&env, checked_sub(&env, reserve_out, amount_out));
        }
        storage::bump_instance(&env);

        let token_in_client = token::Client::new(&env, &token_in);
        let token_out_client = token::Client::new(&env, &token_out);
        token_in_client.transfer(&from, env.current_contract_address(), &amount_in);
        token_out_client.transfer(&env.current_contract_address(), &to, &amount_out);

        events::swap(&env, &from, &token_in, amount_in, &token_out, amount_out);

        amount_in
    }

    /// Transfer `amount` LP shares from `from` to `to`. Requires `from`'s
    /// authorization, just like a normal fungible token transfer.
    pub fn transfer_shares(env: Env, from: Address, to: Address, amount: i128) {
        let _guard = EntryGuard::new(&env);
        storage::require_initialized(&env);
        lp_token::transfer(&env, &from, &to, amount);
        storage::bump_instance(&env);
    }

    // ---- Read-only views ----

    pub fn token_a(env: Env) -> Address {
        storage::require_initialized(&env);
        storage::get_token_a(&env)
    }

    pub fn token_b(env: Env) -> Address {
        storage::require_initialized(&env);
        storage::get_token_b(&env)
    }

    /// Returns `(reserve_a, reserve_b)`.
    pub fn get_reserves(env: Env) -> (i128, i128) {
        storage::require_initialized(&env);
        (storage::get_reserve_a(&env), storage::get_reserve_b(&env))
    }

    pub fn total_shares(env: Env) -> i128 {
        lp_token::total_supply(&env)
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        lp_token::balance(&env, &id)
    }

    /// Quote-only: how much `token_out` `swap_exact_in` would currently
    /// return for `amount_in` of the other pool token, after fees. Does not
    /// touch state or require auth — safe to call from a wallet UI before
    /// building a real swap.
    pub fn quote_amount_out(env: Env, token_in: Address, amount_in: i128) -> i128 {
        storage::require_initialized(&env);
        if amount_in <= 0 {
            panic_with_error!(&env, PoolError::AmountMustBePositive);
        }
        let token_a = storage::get_token_a(&env);
        let token_b = storage::get_token_b(&env);
        let (reserve_in, reserve_out) = if token_in == token_a {
            (storage::get_reserve_a(&env), storage::get_reserve_b(&env))
        } else if token_in == token_b {
            (storage::get_reserve_b(&env), storage::get_reserve_a(&env))
        } else {
            panic_with_error!(&env, PoolError::InvalidToken);
        };
        if reserve_in <= 0 || reserve_out <= 0 {
            panic_with_error!(&env, PoolError::InsufficientLiquidity);
        }
        Self::get_amount_out(&env, amount_in, reserve_in, reserve_out)
    }

    // ---- Internal pricing helpers ----

    /// Given desired deposit amounts and the pool's current reserves,
    /// returns the largest `(amount_a, amount_b) <= (amount_a_desired,
    /// amount_b_desired)` that preserves the existing `reserve_a /
    /// reserve_b` ratio exactly (or the desired amounts unchanged, for a
    /// pool with no reserves yet).
    fn optimal_deposit_amounts(
        env: &Env,
        reserve_a: i128,
        reserve_b: i128,
        amount_a_desired: i128,
        amount_b_desired: i128,
        amount_a_min: i128,
        amount_b_min: i128,
    ) -> (i128, i128) {
        if reserve_a == 0 && reserve_b == 0 {
            return (amount_a_desired, amount_b_desired);
        }

        let amount_b_optimal = checked_mul_div(env, amount_a_desired, reserve_b, reserve_a);
        if amount_b_optimal <= amount_b_desired {
            if amount_b_optimal < amount_b_min {
                panic_with_error!(env, PoolError::InsufficientBAmount);
            }
            (amount_a_desired, amount_b_optimal)
        } else {
            let amount_a_optimal = checked_mul_div(env, amount_b_desired, reserve_a, reserve_b);
            if amount_a_optimal > amount_a_desired || amount_a_optimal < amount_a_min {
                panic_with_error!(env, PoolError::InsufficientAAmount);
            }
            (amount_a_optimal, amount_b_desired)
        }
    }

    /// Constant-product output for an exact input, net of the 0.3% fee:
    ///
    /// ```text
    /// amount_in_with_fee = amount_in * (10_000 - 30)
    /// amount_out = (amount_in_with_fee * reserve_out)
    ///            / (reserve_in * 10_000 + amount_in_with_fee)
    /// ```
    fn get_amount_out(env: &Env, amount_in: i128, reserve_in: i128, reserve_out: i128) -> i128 {
        let amount_in_with_fee =
            checked_mul(env, amount_in, checked_sub(env, FEE_DENOMINATOR, FEE_NUMERATOR));
        let numerator = checked_mul(env, amount_in_with_fee, reserve_out);
        let denominator = checked_add(
            env,
            checked_mul(env, reserve_in, FEE_DENOMINATOR),
            amount_in_with_fee,
        );
        checked_div(env, numerator, denominator)
    }

    /// Constant-product required input for an exact output, net of the
    /// 0.3% fee. The exact inverse of [`Self::get_amount_out`], rounded up
    /// so the pool is never shorted by floor-division:
    ///
    /// ```text
    /// amount_in = (reserve_in * amount_out * 10_000)
    ///           / ((reserve_out - amount_out) * (10_000 - 30))
    ///           + (1 if there was a remainder else 0)
    /// ```
    fn get_amount_in(env: &Env, amount_out: i128, reserve_in: i128, reserve_out: i128) -> i128 {
        if amount_out >= reserve_out {
            panic_with_error!(env, PoolError::InsufficientOutputReserve);
        }
        let numerator = checked_mul(
            env,
            checked_mul(env, reserve_in, amount_out),
            FEE_DENOMINATOR,
        );
        let denominator = checked_mul(
            env,
            checked_sub(env, reserve_out, amount_out),
            checked_sub(env, FEE_DENOMINATOR, FEE_NUMERATOR),
        );
        let amount_in = checked_div(env, numerator, denominator);
        if checked_mul(env, amount_in, denominator) < numerator {
            checked_add(env, amount_in, 1)
        } else {
            amount_in
        }
    }
}
