# StellarSwap

A constant-product (`x * y = k`) automated market maker for two arbitrary
Stellar tokens, written as a single [Soroban](https://developers.stellar.org/docs/build/smart-contracts)
smart contract in Rust. One deployed instance is a pool for exactly one
token pair — think Uniswap V2, ported to Soroban's storage and auth model.

- **Contract:** [`src/lib.rs`](src/lib.rs) (`ConstantProductPool`)
- **Tests:** [`src/test.rs`](src/test.rs) — 21 tests, `cargo test`
- **Contributing:** [`CONTRIBUTING.md`](CONTRIBUTING.md)
- **Live on testnet:** [`CA3C2D2TUG4CWQJRU4AA35SUKXH6L72L4B5U3YOFTI3T7PXRF226OJSQ`](https://stellar.expert/explorer/testnet/contract/CA3C2D2TUG4CWQJRU4AA35SUKXH6L72L4B5U3YOFTI3T7PXRF226OJSQ) — see [Live testnet deployment](#live-testnet-deployment)

## Contents

- [Architecture](#architecture)
- [The AMM math](#the-amm-math)
- [The fee model](#the-fee-model)
- [Security properties](#security-properties)
- [Public interface](#public-interface)
- [Events](#events)
- [Building and testing](#building-and-testing)
- [Deploying to Stellar testnet](#deploying-to-stellar-testnet)
  - [Live testnet deployment](#live-testnet-deployment)
- [Design notes and known limitations](#design-notes-and-known-limitations)

## Architecture

```
src/
├── lib.rs        contract entry points (initialize, add/remove_liquidity, swaps, views)
├── math.rs        checked i128 arithmetic, integer sqrt, the 0.3% fee constants
├── storage.rs     typed storage accessors + TTL bumping + the reentrancy lock
├── lp_token.rs    the pool's LP share ledger (mint / burn / transfer / balance)
├── events.rs      #[contractevent] definitions for every state change
├── errors.rs       PoolError — every panic path's stable error code
└── test.rs         behavioral test suite (cfg(test))
```

One pool contract manages exactly two token contracts, `token_a` and
`token_b` — either can be a Stellar Classic asset wrapped as a
[Stellar Asset Contract](https://developers.stellar.org/docs/tokens/stellar-asset-contract)
(SAC) or any contract implementing the standard
[token interface](https://developers.stellar.org/docs/tokens/token-interface).
The contract doesn't care which is which; it just needs both addresses at
`initialize` time. To run pools for many pairs you deploy one instance of
this Wasm per pair (there's no on-chain factory — see
[CONTRIBUTING.md](CONTRIBUTING.md) for that as a project idea).

### Why an in-contract LP token instead of a separate SAC

LP shares are tracked directly in the pool's own storage
([`lp_token.rs`](src/lp_token.rs)) rather than by deploying a second SEP-41
token contract per pool. Functionally it's still a fungible token — it has
`mint`/`burn` (used internally on deposit/withdraw), a `transfer` any
holder can call, and `balance`/`total_shares` views — it just isn't a
*separate deployment*. That keeps one pool to one contract ID, one
`initialize` call, and no cross-contract bootstrapping order to get right.
The tradeoff is that these shares aren't independently discoverable by
generic SEP-41 tooling the way a real deployed token is; a production
fork that needs that should deploy a small companion token contract for
shares instead and have the pool mint/burn against it.

## The AMM math

Everything is `i128` fixed-point. There is no floating-point anywhere in
the math path, and no separate internal "precision scale" — token amounts
are already fixed-point integers scaled by each token's own `decimals`
(the same convention every Soroban token, SAC included, uses), and the
pool never mixes units: every computation is either an operation on a
single token's amounts, or a cross-multiplication like `a * reserve_b`
vs. `b * reserve_a` that stays correct for any pair of decimals because
both sides carry the same implicit scale factor.

### The invariant

The pool holds reserves `reserve_a` and `reserve_b` and maintains, up to
the fee accrual described below, the Uniswap V2 invariant:

```
reserve_a * reserve_b = k
```

Every swap moves along this curve; every deposit/withdraw scales it.

### First deposit and LP share pricing

The very first `add_liquidity` call on an empty pool has no existing price
to respect, so it takes the caller's amounts as given and prices shares as
the geometric mean, exactly as Uniswap V2 does:

```
shares_minted = isqrt(amount_a * amount_b) - MINIMUM_LIQUIDITY
```

`MINIMUM_LIQUIDITY` is `1_000` shares that are permanently added to
`total_shares` but never credited to any account — `lp_token::mint_unallocated`.
This is the standard defense against the first-depositor griefing attack:
without it, someone could seed a pool with a tiny amount, drive the
implied share price to an extreme, and profit off the rounding error of
the next real depositor. Locking a fixed floor of supply away forever
means `total_shares` can never be driven back down to a value where that
manipulation is worth it. (`isqrt` is Newton's method on integers, in
[`math.rs`](src/math.rs); the deposit panics rather than mint zero or
negative shares if the two amounts are too small relative to
`MINIMUM_LIQUIDITY`.)

### Subsequent deposits

Once a pool has reserves, `add_liquidity(from, amount_a_desired,
amount_b_desired, amount_a_min, amount_b_min)` must preserve the existing
`reserve_a / reserve_b` ratio — otherwise a deposit would itself be an
unpriced, feeless trade. So the contract computes the largest
`(amount_a, amount_b) ≤ (amount_a_desired, amount_b_desired)` that keeps
the ratio exact:

```
amount_b_optimal = amount_a_desired * reserve_b / reserve_a
if amount_b_optimal ≤ amount_b_desired:
    take (amount_a_desired, amount_b_optimal)
else:
    amount_a_optimal = amount_b_desired * reserve_a / reserve_b
    take (amount_a_optimal, amount_b_desired)
```

`amount_a_min` / `amount_b_min` are the slippage bound on whichever side
got clamped: if the ratio has moved (another trade landed between when the
caller decided on their desired amounts and when this call executes) far
enough that the clamped amount would fall below the caller's minimum, the
call panics with `InsufficientAAmount` / `InsufficientBAmount` instead of
silently taking a worse deal.

Shares are then minted proportionally to the smaller of what each side's
deposit is worth against the current supply:

```
shares_minted = min(
    amount_a * total_shares / reserve_a,
    amount_b * total_shares / reserve_b,
)
```

(The two are equal whenever the ratio-preservation above landed exactly,
up to integer rounding — `min` just guards against a depositor never being
credited more than their weaker side justifies.)

### Withdrawing

`remove_liquidity(from, shares, amount_a_min, amount_b_min)` burns `shares`
and returns a proportional slice of both reserves:

```
amount_a = shares * reserve_a / total_shares
amount_b = shares * reserve_b / total_shares
```

`amount_a_min` / `amount_b_min` are again slippage protection: if the
pool's reserves moved since the caller decided how much they were owed
(e.g. a swap landed in the same ledger, ahead of this call), and the
realized payout would be less than requested, the call panics
(`InsufficientAOutput` / `InsufficientBOutput`) instead of returning less.

### Swaps

**Exact input** — `swap_exact_in(from, token_in, amount_in, min_amount_out, to)`.
The 0.3% fee is taken off the input before it's run through the constant
product curve:

```
amount_in_with_fee = amount_in * (10_000 - 30)          //  = amount_in * 9_970
amount_out = (amount_in_with_fee * reserve_out)
           / (reserve_in * 10_000 + amount_in_with_fee)
```

This is algebraically `reserve_out - (reserve_in * reserve_out) /
(reserve_in + amount_in_with_fee / 10_000)` — i.e. exactly the Uniswap V2
`getAmountOut` formula, expressed to avoid any division before the final
step so no precision is lost to early rounding. `min_amount_out` is the
slippage bound: if `amount_out` comes out below it, the call panics with
`SlippageExceededMinOut` rather than execute a worse-than-requested trade.

**Exact output** — `swap_exact_out(from, token_out, amount_out, max_amount_in, to)`
is the algebraic inverse, rounded *up* (via a remainder check) so the pool
is never shorted a fractional unit by floor-division in the trader's
favor:

```
amount_in = ceil(
    (reserve_in * amount_out * 10_000)
    / ((reserve_out - amount_out) * (10_000 - 30))
)
```

`max_amount_in` is the slippage bound in the other direction: if the
required input exceeds it, the call panics with `SlippageExceededMaxIn`.

In both directions, reserves are updated (`effects`) before the token
contracts are ever invoked (`interactions`) — see
[Security properties](#security-properties).

## The fee model

Every swap pays a flat **0.3%** fee (`30` basis points out of a `10_000`
denominator — [`math::FEE_NUMERATOR`](src/math.rs) /
[`math::FEE_DENOMINATOR`](src/math.rs)), taken out of the input token
before the constant-product formula runs. The fee is never transferred out
of the pool — it's simply left in the reserves, which means:

- It directly grows `reserve_in` relative to what a feeless swap would
  have left behind, so `reserve_a * reserve_b` (the invariant `k`) strictly
  increases with every swap.
- Because LP share value is `shares / total_shares` of the reserves, and
  `total_shares` only changes on deposits/withdrawals (never on swaps),
  every swap's fee accrues pro-rata to whoever holds LP shares at
  withdrawal time. There's no separate fee-claim step — withdrawing later
  simply returns a larger slice of a bigger pool.
- 100% of the fee goes to LPs. There's no protocol/treasury cut in this
  version — adding a configurable split (a "fee switch," in Uniswap
  parlance) is one of the suggested first issues in
  [CONTRIBUTING.md](CONTRIBUTING.md).

## Security properties

- **Checked arithmetic everywhere.** Every add/sub/mul/div in the math
  path goes through [`math::checked_add`](src/math.rs) /
  `checked_sub` / `checked_mul` / `checked_div` / `checked_mul_div`, each
  of which panics with `PoolError::ArithmeticOverflow` (via
  `panic_with_error!`) on overflow, underflow, or division by zero — never
  wraps, never saturates. `isqrt` similarly panics on a negative input. A
  panic aborts the whole host transaction: nothing partially commits.
- **Reentrancy guard.** [`EntryGuard`](src/lib.rs) is an RAII lock acquired
  at the top of every state-mutating entry point
  (`add_liquidity`/`remove_liquidity`/`swap_exact_in`/`swap_exact_out`/`transfer_shares`)
  and released on return. If a token contract invoked mid-call — a
  malicious or simply buggy `transfer` implementation — calls back into
  any of those entry points, it hits an already-held lock and panics with
  `PoolError::ReentrancyDetected`, unwinding the entire transaction. See
  [`storage::enter`/`storage::exit`](src/storage.rs).
- **Effects before interactions.** In every mutating function, reserve and
  share-balance updates are committed to storage *before* the pool ever
  calls out to `token_a`'s or `token_b`'s contract. This is defense in
  depth on top of the reentrancy guard, not a substitute for it — a panic
  anywhere still reverts everything, but keeping state changes ahead of
  external calls is the same discipline Solidity's checks-effects-interactions
  pattern encodes, and it means a future refactor that (for some reason)
  caught a panic instead of propagating it still wouldn't leave reserves
  inconsistent with an executed transfer.
- **Slippage protection on every value-moving call.** `add_liquidity`
  (`amount_a_min`/`amount_b_min`), `remove_liquidity`
  (`amount_a_min`/`amount_b_min`), `swap_exact_in` (`min_amount_out`), and
  `swap_exact_out` (`max_amount_in`) all panic rather than execute at a
  worse price/ratio than the caller specified.
- **Auth on every state change.** `from.require_auth()` is required for
  deposits, withdrawals, swaps, and share transfers — a caller can only
  move their own funds and shares.
- **Minimum liquidity lock.** See [First deposit](#first-deposit-and-lp-share-pricing)
  above — prevents the classic first-depositor share-price manipulation.
- **`InvalidToken` guard on swaps.** `swap_exact_in`/`swap_exact_out`
  panic if `token_in`/`token_out` isn't one of the pool's two configured
  addresses, rather than silently doing nothing sensible.

## Public interface

| Function | Effect | Auth required |
|---|---|---|
| `initialize(token_a, token_b)` | One-time setup of the pool's token pair. | — |
| `add_liquidity(from, amount_a_desired, amount_b_desired, amount_a_min, amount_b_min) -> (a, b, shares)` | Deposit tokens, mint LP shares. | `from` |
| `remove_liquidity(from, shares, amount_a_min, amount_b_min) -> (a, b)` | Burn LP shares, withdraw tokens. | `from` |
| `swap_exact_in(from, token_in, amount_in, min_amount_out, to) -> amount_out` | Trade an exact input amount. | `from` |
| `swap_exact_out(from, token_out, amount_out, max_amount_in, to) -> amount_in` | Trade for an exact output amount. | `from` |
| `transfer_shares(from, to, amount)` | Move LP shares between accounts. | `from` |
| `token_a() -> Address` / `token_b() -> Address` | View the pool's configured tokens. | — |
| `get_reserves() -> (i128, i128)` | View current `(reserve_a, reserve_b)`. | — |
| `total_shares() -> i128` | View LP share supply (includes the locked minimum). | — |
| `balance(id) -> i128` | View an account's LP share balance. | — |
| `quote_amount_out(token_in, amount_in) -> i128` | Pure quote of `swap_exact_in`'s output, for UIs — no state change, no auth. | — |

Full argument/return types are in the doc comments on each function in
[`src/lib.rs`](src/lib.rs); every panic path is one of the
[`PoolError`](src/errors.rs) variants.

## Events

Defined with `#[contractevent]` in [`events.rs`](src/events.rs), so the
shape below is also baked into the contract's on-chain spec:

| Event | Topics | Data |
|---|---|---|
| `Initialize` | `token_a`, `token_b` | — |
| `AddLiquidity` | `provider` | `amount_a`, `amount_b`, `shares_minted` |
| `RemoveLiquidity` | `provider` | `amount_a`, `amount_b`, `shares_burned` |
| `Swap` | `trader`, `token_in` | `amount_in`, `token_out`, `amount_out` |

## Building and testing

Requires a Rust toolchain with the `wasm32v1-none` target:

```bash
rustup target add wasm32v1-none
```

Run the test suite (native target — fast, uses Soroban's `testutils`):

```bash
cargo test
```

This runs 21 tests covering: a normal exact-in swap and its fee math
against a hand-computed expectation, exact-out as the algebraic inverse, a
round-trip swap losing value to the fee (proving `k` grows), first and
second deposits (including the ratio-clamping case), full withdrawal, LP
share transfer, the zero-liquidity edge case (swapping or withdrawing
against an empty pool), slippage rejection on all four value-moving calls,
overflow rejection on oversized swap and deposit inputs, and basic
`initialize` validation.

Build the deployable contract Wasm:

```bash
cargo build --target wasm32v1-none --release
# -> target/wasm32v1-none/release/stellarswap.wasm  (~29 KB)
```

(Or `stellar contract build`, once the CLI below is installed — it wraps
the same `cargo build` invocation and additionally strips/optimizes the
output.)

## Deploying to Stellar testnet

These steps use the [Stellar CLI](https://developers.stellar.org/docs/tools/cli)
(`stellar`, formerly `soroban-cli`) v28 against `soroban-sdk = "26.1.0"`.
They were run start-to-finish against live testnet to produce the
[deployed instance](#live-testnet-deployment) below — commands and output
are exactly what that run used, not a hypothetical.

### 1. Install the CLI and set up funded testnet accounts

```bash
# Prebuilt binary (faster) — grab the right asset for your OS/arch from
# https://github.com/stellar/stellar-cli/releases/latest, e.g. for Linux x86_64:
curl -sL -o stellar-cli.tar.gz \
  https://github.com/stellar/stellar-cli/releases/download/v28.0.0/stellar-cli-28.0.0-x86_64-unknown-linux-gnu.tar.gz
tar xzf stellar-cli.tar.gz && install -m 755 stellar ~/.local/bin/stellar

# ...or build from source:
# cargo install --locked stellar-cli --features opt

stellar keys generate alice --network testnet --fund   # issuer + contract deployer
stellar keys generate carol --network testnet --fund   # liquidity provider
stellar keys generate bob   --network testnet --fund   # trader
```

`--fund` uses testnet Friendbot to fund the new account automatically. (In
CLI versions before v28 this flag was combined with `--global`; v28
dropped `--global` — identities are stored in `~/.config/stellar` either
way.)

### 2. Get two test tokens on testnet

Any two token contracts work. The simplest option is to deploy the
[Stellar Asset Contract](https://developers.stellar.org/docs/tokens/stellar-asset-contract)
wrapper for two Classic assets issued by `alice`:

```bash
ALICE=$(stellar keys address alice)

stellar contract asset deploy --asset TOKA:$ALICE --source alice --network testnet --alias toka
stellar contract asset deploy --asset TOKB:$ALICE --source alice --network testnet --alias tokb
```

Each command prints the deployed SAC's contract ID — save them as
`TOKEN_A_ID` / `TOKEN_B_ID`. Two things about Classic assets trip people up
here, both enforced by the ledger itself rather than anything in this
contract:

- **The issuer can never hold its own asset** (minting to `alice` fails
  with `operation invalid on issuer`) — so liquidity providers and traders
  need to be *other* accounts, never the issuer.
- **A non-issuer account needs a trustline before it can receive the
  asset** at all — `mint` to an account with no trustline fails with
  `trustline entry is missing for account`. Establish one per asset per
  holder first:

```bash
for ACC in bob carol; do
  for CODE in TOKA TOKB; do
    stellar tx new change-trust --source $ACC --network testnet --line "$CODE:$ALICE"
  done
done
```

Then mint to `bob` and `carol` (minting requires the issuer's signature,
i.e. `--source alice`):

```bash
BOB=$(stellar keys address bob)
CAROL=$(stellar keys address carol)
for TOK in toka tokb; do
  stellar contract invoke --id $TOK --source alice --network testnet -- mint --to $BOB   --amount 1000000000
  stellar contract invoke --id $TOK --source alice --network testnet -- mint --to $CAROL --amount 1000000000
done
```

### 3. Build and deploy the pool contract

```bash
stellar contract build
# equivalent to: cargo build --target wasm32v1-none --release, plus wasm-opt

stellar contract deploy \
  --wasm target/wasm32v1-none/release/stellarswap.wasm \
  --source alice --network testnet --alias stellarswap_pool
```

This prints the pool's contract ID (also reachable afterwards as `stellarswap_pool`).

### 4. Initialize the pool and provide liquidity

```bash
stellar contract invoke --id stellarswap_pool --source alice --network testnet -- \
  initialize --token_a $TOKEN_A_ID --token_b $TOKEN_B_ID

stellar contract invoke --id stellarswap_pool --source carol --network testnet -- \
  add_liquidity --from $CAROL \
  --amount_a_desired 1000000 --amount_b_desired 4000000 \
  --amount_a_min 0 --amount_b_min 0
```

### 5. Swap

```bash
stellar contract invoke --id stellarswap_pool --source bob --network testnet -- \
  swap_exact_in --from $BOB \
  --token_in $TOKEN_A_ID --amount_in 10000 --min_amount_out 0 \
  --to $BOB
```

Drop `--min_amount_out 0` for a real deployment — that's disabling
slippage protection for the demo. Query the pool's state at any time with
a read-only invoke — note `--source` is required even for reads in CLI
v28, though it doesn't sign/submit anything:

```bash
stellar contract invoke --id stellarswap_pool --source alice --network testnet -- get_reserves
```

### Live testnet deployment

The steps above were run against Stellar testnet on 2026-09-07. Everything
below is a real, currently-live contract instance — click through to
Stellar Expert to inspect it directly:

| | Contract ID | Explorer |
|---|---|---|
| **Pool** (`ConstantProductPool`) | `CA3C2D2TUG4CWQJRU4AA35SUKXH6L72L4B5U3YOFTI3T7PXRF226OJSQ` | [stellar.expert](https://stellar.expert/explorer/testnet/contract/CA3C2D2TUG4CWQJRU4AA35SUKXH6L72L4B5U3YOFTI3T7PXRF226OJSQ) |
| Token A (`TOKA`, SAC) | `CCAI4UENTRZ4A27JEPLEB4E4EIUQDVCF5JWSLYEJYPSOAZYEDQJYAZ2M` | [stellar.expert](https://stellar.expert/explorer/testnet/contract/CCAI4UENTRZ4A27JEPLEB4E4EIUQDVCF5JWSLYEJYPSOAZYEDQJYAZ2M) |
| Token B (`TOKB`, SAC) | `CC2VND6OK53S4NBHKDCU2QR3KA3HJJFB2H7APOJ6KKEB6JSMTUNNS3IT` | [stellar.expert](https://stellar.expert/explorer/testnet/contract/CC2VND6OK53S4NBHKDCU2QR3KA3HJJFB2H7APOJ6KKEB6JSMTUNNS3IT) |

State after the run above: `carol` seeded 1,000,000 TOKA / 4,000,000 TOKB
and holds 1,999,000 LP shares (out of 2,000,000 total — 1,000 permanently
locked, see [Minimum liquidity lock](#first-deposit-and-lp-share-pricing));
`bob` then swapped 10,000 TOKA for **39,486 TOKB**
(`quote_amount_out` returned the identical figure beforehand, and the
realized `Swap` event confirms it), leaving reserves at
`(1,010,000, 3,960,514)` — you can verify that's still `≥` the
pre-swap product `1,000,000 × 4,000,000` net of the 0.3% fee by re-running
`get_reserves` yourself. Selected transactions from that run:

- [`initialize`](https://stellar.expert/explorer/testnet/tx/d10d8f768dfbc0068bcdf5ca2ab2f7f87dc017d850108277a915fb3b16910029)
- [`add_liquidity`](https://stellar.expert/explorer/testnet/tx/160e6ad1bd5e19f5f385bc34d3c8c71d76174973eb6a7a2753c42d77e63f0fd6)
- [`swap_exact_in`](https://stellar.expert/explorer/testnet/tx/9d601b1769c71d50eac1d5a9e6a2c32658025a7efb10883ab1a3a104db91b7a4)

This deployment is unaudited testnet software provided for demonstration —
don't treat its live state as a recommendation to route real value through
it, and expect it may be redeployed/reset without notice.

## Design notes and known limitations

- **One pair per deployment, no factory.** Deploying pools for many pairs
  means deploying this Wasm once per pair. A factory contract that
  deploys+initializes pools (and lets you look one up by token pair) is a
  natural extension — see [CONTRIBUTING.md](CONTRIBUTING.md).
- **No price oracle / TWAP.** Reserves are readable via `get_reserves`,
  but there's no time-weighted accumulator like Uniswap V2's
  `price0CumulativeLast`. Anything reading this pool's spot price on-chain
  should be aware it's manipulable within a single transaction/atomic
  auth group in the usual constant-product-AMM way.
- **No fee switch.** All 30 bps goes to LPs; see
  [CONTRIBUTING.md](CONTRIBUTING.md) for adding a protocol-fee split.
- **No multi-hop routing.** Each pool only knows its own two tokens; a
  router contract that chains swaps across pools is a separate piece —
  also in [CONTRIBUTING.md](CONTRIBUTING.md).
- **LP shares are pool-local**, not a separately deployed SEP-41 token —
  see [Why an in-contract LP token](#why-an-in-contract-lp-token-instead-of-a-separate-sac)
  above.

## License

MIT — see [`Cargo.toml`](Cargo.toml).
