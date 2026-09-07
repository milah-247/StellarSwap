use soroban_sdk::contracterror;

/// All error conditions the pool can raise. Every panic path in the contract
/// goes through one of these codes so callers (and tests) can match on a
/// stable, documented value instead of a raw string.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum PoolError {
    /// `initialize` was called on a pool that already has tokens configured.
    AlreadyInitialized = 1,
    /// A call was made before `initialize` set up the pool.
    NotInitialized = 2,
    /// `token_a` and `token_b` were the same address.
    IdenticalTokens = 3,
    /// A deposit/withdraw/swap amount was <= 0.
    AmountMustBePositive = 4,
    /// `add_liquidity` could not satisfy the caller's min_a/min_b bounds
    /// given the pool's current price.
    InsufficientAAmount = 5,
    InsufficientBAmount = 6,
    /// `remove_liquidity` would return less than the caller's requested
    /// minimums.
    InsufficientAOutput = 7,
    InsufficientBOutput = 8,
    /// A swap's realized output was below `min_amount_out`.
    SlippageExceededMinOut = 9,
    /// A swap's realized input was above `max_amount_in`.
    SlippageExceededMaxIn = 10,
    /// The pool has zero shares outstanding and cannot service a swap.
    InsufficientLiquidity = 11,
    /// `remove_liquidity` was asked to burn more shares than the caller owns.
    InsufficientShareBalance = 12,
    /// A swap requested more of the output token than the pool holds.
    InsufficientOutputReserve = 13,
    /// `token_in` / `token_out` was not one of the pool's two configured
    /// tokens.
    InvalidToken = 14,
    /// A checked arithmetic operation (add/sub/mul/div) would have
    /// overflowed or divided by zero.
    ArithmeticOverflow = 15,
    /// The first deposit was too small to satisfy the minimum-liquidity
    /// lock, or would mint zero shares.
    InsufficientInitialLiquidity = 16,
    /// Reentrant call detected: the pool is already executing a
    /// state-mutating call for this contract instance.
    ReentrancyDetected = 17,
}
