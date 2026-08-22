# fedimint-amm Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Fedimint server/client module providing constant-product AMM pools over the `AmountUnit`s a federation issues, as a mechanical translation of Uniswap V2.

**Architecture:** Four crates under `modules/amm/`. All curve and share arithmetic lives in `-common` as pure functions with no I/O, so client quotes and server settlement run identical code. The server exposes four transaction items; a swap spans two transactions because core processes all inputs before all outputs. Client keys are derived via mintv2's ground-random-tweak scheme so recovery is a table scan and concurrent clients on one seed never collide.

**Tech Stack:** Rust (stable 1.98, edition 2024), fedimint master, `nix develop` shell, `proptest` for property tests, `devimint` for integration tests.

**Spec:** `modules/amm/fedimint-amm-spec.md`. Section references below (§N) point there. Read it before starting any task.

## Global Constraints

- fedimint is pinned at rev `4794ee166afc191e0125c092893bd8f080939b53`. Do not bump it.
- All builds and tests run inside the nix shell: `nix develop --command cargo …`.
- **`nix develop` fails on untracked files.** The repo must have everything `git add`ed before entering the shell. If a build errors with `Path 'flake.nix' … is not tracked by Git`, run `git add -A` and retry.
- `MAX_RESERVE = 1 << 58`. Every reserve, and every `amount_in`, must be `<= MAX_RESERVE`.
- `MINIMUM_LIQUIDITY = 1000` — the Uniswap V2 constant, not configurable.
- Fee is per-mille (`u16`, `< 1000`), default `3`. Never parts-per-million; §7.1 derives why the extra 10 bits of headroom are needed.
- **No floating point** on any consensus-reachable path.
- All arithmetic intermediates are `u128`. Use `checked_*` or `u64::try_from`; never bare `as` narrowing.
- **Never `panic!`, `assert!`, `unwrap()` or `expect()` in `process_input`, `process_output`, `process_consensus_item` or `audit`.** A panic there halts the federation (§9.2). Return an error so the transaction is rejected instead.
- `BTreeMap`/`BTreeSet` only in consensus paths. No `HashMap` iteration.
- No `SystemTime`, no RNG, no I/O inside consensus handlers.
- **Never compute a value twice** (§7.4): compute once, use that one binding for both the DB mutation and the returned `amounts`.
- Commit after every task. Conventional-commit subjects (`feat:`, `test:`, `fix:`).

---

## File Structure

```
modules/amm/fedimint-amm-common/src/
  lib.rs            module kind, consensus version, AmmInput/AmmOutput, AmmError
  math.rs           MAX_RESERVE, amount_out, mint_shares, burn_shares, k_check
  pool_id.rs        PoolId + canonical Decodable
  config.rs         AmmConfigConsensus, AmmClientConfig, AmmConfigPrivate, UnitParams
  endpoints.rs      endpoint name constants + request/response types

modules/amm/fedimint-amm-server/src/
  lib.rs            ServerModule impl, init, config gen, audit, API
  db.rs             DbKeyPrefix, key/record types, typed accessors

modules/amm/fedimint-amm-client/src/
  lib.rs            ClientModule impl, operations
  derivation.rs     tweak_filter / grind_tweak / check_tweak / key derivation
  swap.rs           two-transaction swap state machine
  db.rs             client-side operation state

modules/amm/fedimint-amm-tests/tests/
  integration.rs    devimint end-to-end
```

---

## Task 1: Curve and share arithmetic

The highest-value task in the plan. Pure functions, no fedimint types, fully testable in isolation. Everything else depends on these signatures.

**Files:**
- Create: `modules/amm/fedimint-amm-common/src/math.rs`
- Modify: `modules/amm/fedimint-amm-common/src/lib.rs` (add `pub mod math;`)
- Modify: `modules/amm/fedimint-amm-common/Cargo.toml` (add `thiserror` dep, `proptest` dev-dep)

**Interfaces:**
- Produces:
  - `pub const MAX_RESERVE: u64 = 1 << 58;`
  - `pub const MINIMUM_LIQUIDITY: u64 = 1000;`
  - `pub enum CurveError` (variants listed in Step 3)
  - `pub fn amount_out(reserve_in: u64, reserve_out: u64, amount_in: u64, fee_per_mille: u16) -> Result<u64, CurveError>`
  - `pub struct MintOutcome { pub to_owner: u64, pub new_total_shares: u64 }`
  - `pub fn mint_shares(reserve_lo: u64, reserve_hi: u64, total_shares: u64, da: u64, db: u64) -> Result<MintOutcome, CurveError>`
  - `pub struct BurnOutcome { pub da: u64, pub db: u64, pub new_total_shares: u64 }`
  - `pub fn burn_shares(reserve_lo: u64, reserve_hi: u64, total_shares: u64, shares: u64) -> Result<BurnOutcome, CurveError>`
  - `pub fn k_non_decreasing(r_in_old: u64, r_out_old: u64, r_in_new: u64, r_out_new: u64) -> bool`

- [ ] **Step 1: Add dependencies**

In `modules/amm/fedimint-amm-common/Cargo.toml`:

```toml
[dependencies]
thiserror = { workspace = true }

[dev-dependencies]
proptest = "1"
```

Add to the root `Cargo.toml` `[workspace.dependencies]` if not already present:

```toml
thiserror = "2"
```

- [ ] **Step 2: Write the failing tests**

Create `modules/amm/fedimint-amm-common/src/math.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from spec §Step 2 / the Uniswap V2 formula.
    #[test]
    fn amount_out_matches_reference_vector() {
        // reserve_in 1_000_000_000, reserve_out 1_000_000, in 10_000_000, fee 3/1000
        assert_eq!(amount_out(1_000_000_000, 1_000_000, 10_000_000, 3), Ok(9_871));
    }

    #[test]
    fn amount_out_rejects_degenerate_inputs() {
        assert_eq!(amount_out(1_000, 1_000, 0, 3), Err(CurveError::ZeroAmount));
        assert_eq!(amount_out(0, 1_000, 10, 3), Err(CurveError::ZeroReserve));
        assert_eq!(amount_out(1_000, 0, 10, 3), Err(CurveError::ZeroReserve));
        assert_eq!(amount_out(1_000, 1_000, 10, 1_000), Err(CurveError::InvalidFee));
        assert_eq!(
            amount_out(MAX_RESERVE + 1, 1_000, 10, 3),
            Err(CurveError::ReserveCapExceeded)
        );
        assert_eq!(
            amount_out(1_000, 1_000, MAX_RESERVE + 1, 3),
            Err(CurveError::ReserveCapExceeded)
        );
    }

    #[test]
    fn amount_out_rejects_dust_that_rounds_to_zero() {
        // 1 unit into a pool where reserve_out is tiny relative to reserve_in
        assert_eq!(
            amount_out(1_000_000_000, 10, 1, 3),
            Err(CurveError::OutputRoundsToZero)
        );
    }

    #[test]
    fn amount_out_never_drains_the_pool() {
        // Even an enormous input leaves at least one unit behind.
        let out = amount_out(1_000, MAX_RESERVE, MAX_RESERVE, 3).unwrap();
        assert!(out < MAX_RESERVE);
    }

    #[test]
    fn first_mint_burns_minimum_liquidity() {
        // isqrt(1_000_000 * 1_000_000) == 1_000_000
        let outcome = mint_shares(0, 0, 0, 1_000_000, 1_000_000).unwrap();
        assert_eq!(outcome.new_total_shares, 1_000_000);
        assert_eq!(outcome.to_owner, 1_000_000 - MINIMUM_LIQUIDITY);
    }

    #[test]
    fn first_mint_rejects_below_minimum_liquidity() {
        // isqrt(1000 * 1000) == 1000, which is not > MINIMUM_LIQUIDITY
        assert_eq!(
            mint_shares(0, 0, 0, 1_000, 1_000),
            Err(CurveError::InsufficientInitialLiquidity)
        );
    }

    #[test]
    fn later_mint_takes_the_minimum_of_both_ratios() {
        // Pool 1_000 : 1_000 with 1_000 shares. Deposit 100 : 500.
        // min(100 * 1000 / 1000, 500 * 1000 / 1000) = min(100, 500) = 100
        let outcome = mint_shares(1_000, 1_000, 1_000, 100, 500).unwrap();
        assert_eq!(outcome.to_owner, 100);
        assert_eq!(outcome.new_total_shares, 1_100);
    }

    #[test]
    fn burn_pays_out_pro_rata_and_floors() {
        // 1_000 shares outstanding, pool 1_001 : 1_001, burn 500
        // 1_001 * 500 / 1_000 = 500 (floor of 500.5)
        let outcome = burn_shares(1_001, 1_001, 1_000, 500).unwrap();
        assert_eq!(outcome.da, 500);
        assert_eq!(outcome.db, 500);
        assert_eq!(outcome.new_total_shares, 500);
    }

    #[test]
    fn burn_rejects_more_shares_than_exist() {
        assert_eq!(
            burn_shares(1_000, 1_000, 1_000, 1_001),
            Err(CurveError::InsufficientShares)
        );
        assert_eq!(
            burn_shares(1_000, 1_000, 1_000, 0),
            Err(CurveError::ZeroAmount)
        );
    }

    #[test]
    fn burn_rejects_a_position_too_small_to_pay_anything() {
        // 1 share out of 1_000_000, pool 10 : 10 -> both legs floor to 0
        assert_eq!(
            burn_shares(10, 10, 1_000_000, 1),
            Err(CurveError::OutputRoundsToZero)
        );
    }

    proptest::proptest! {
        /// Spec §14: k is non-decreasing under any swap.
        #[test]
        fn k_never_decreases(
            r_in in 1u64..=(1 << 40),
            r_out in 1u64..=(1 << 40),
            amt in 1u64..=(1 << 40),
        ) {
            if let Ok(out) = amount_out(r_in, r_out, amt, 3) {
                let r_in_new = r_in + amt;
                let r_out_new = r_out - out;
                proptest::prop_assert!(k_non_decreasing(r_in, r_out, r_in_new, r_out_new));
            }
        }

        /// Spec §14: a round trip always loses value.
        #[test]
        fn round_trip_always_loses(
            r_a in 1_000_000u64..=(1 << 40),
            r_b in 1_000_000u64..=(1 << 40),
            amt in 1_000u64..=(1 << 30),
        ) {
            if let Ok(out) = amount_out(r_a, r_b, amt, 3) {
                let back = amount_out(r_b - out, r_a + amt, out, 3);
                if let Ok(back) = back {
                    proptest::prop_assert!(back < amt);
                }
            }
        }

        /// Spec §14: deposit then immediate withdraw never returns more.
        #[test]
        fn deposit_withdraw_never_profits(
            r_lo in 1_000_000u64..=(1 << 40),
            r_hi in 1_000_000u64..=(1 << 40),
            shares in 1_000_000u64..=(1 << 40),
            da in 1_000u64..=(1 << 30),
            db in 1_000u64..=(1 << 30),
        ) {
            if let Ok(m) = mint_shares(r_lo, r_hi, shares, da, db) {
                let b = burn_shares(r_lo + da, r_hi + db, m.new_total_shares, m.to_owner);
                if let Ok(b) = b {
                    proptest::prop_assert!(b.da <= da);
                    proptest::prop_assert!(b.db <= db);
                }
            }
        }

        /// Nothing inside the caps may overflow u128 (would panic in debug).
        #[test]
        fn no_overflow_within_caps(
            r_in in 1u64..=MAX_RESERVE,
            r_out in 1u64..=MAX_RESERVE,
            amt in 1u64..=MAX_RESERVE,
            fee in 0u16..1000,
        ) {
            let _ = amount_out(r_in, r_out, amt, fee);
        }
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

```bash
nix develop --command cargo test -p fedimint-amm-common
```

Expected: compile failure — `cannot find function 'amount_out' in this scope`, and likewise for the other items. That is the correct failure; the test module references nothing that exists yet.

- [ ] **Step 4: Write the implementation**

Prepend to `modules/amm/fedimint-amm-common/src/math.rs`, above the test module:

```rust
//! Constant-product AMM arithmetic — a mechanical translation of Uniswap V2.
//!
//! See `modules/amm/fedimint-amm-spec.md` §7. Every intermediate is `u128`;
//! there is no floating point anywhere in this file.

use thiserror::Error;

/// Upper bound on any reserve or swap input.
///
/// The largest intermediate below is `997 * amount_in * reserve_out`. Since
/// `997 < 2^10`, safety needs `amount_in * reserve_out < 2^118`; capping both
/// at `2^58` yields `997 * 2^116 < 2^126`, four bits of headroom. `2^58` msats
/// is ~2.88e6 BTC — two orders of magnitude above the entire supply. Spec §7.1.
pub const MAX_RESERVE: u64 = 1 << 58;

/// Shares permanently burned on pool creation. The Uniswap V2 constant.
pub const MINIMUM_LIQUIDITY: u64 = 1000;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CurveError {
    #[error("amount must be non-zero")]
    ZeroAmount,
    #[error("reserves must be non-zero")]
    ZeroReserve,
    #[error("value exceeds MAX_RESERVE")]
    ReserveCapExceeded,
    #[error("fee_per_mille must be < 1000")]
    InvalidFee,
    #[error("output rounds to zero")]
    OutputRoundsToZero,
    #[error("pool has insufficient liquidity")]
    InsufficientLiquidity,
    #[error("initial liquidity must exceed MINIMUM_LIQUIDITY")]
    InsufficientInitialLiquidity,
    #[error("not enough shares")]
    InsufficientShares,
    #[error("arithmetic overflow")]
    Overflow,
}

/// Uniswap V2 `getAmountOut`, verbatim.
///
/// ```text
/// in_with_fee = amount_in * (1000 - fee_per_mille)
/// out         = in_with_fee * reserve_out / (reserve_in * 1000 + in_with_fee)
/// ```
///
/// The caller adds the **full** `amount_in` to `reserve_in` — fee included.
/// Nothing collects the fee; it stays as reserve and lifts `k`.
pub fn amount_out(
    reserve_in: u64,
    reserve_out: u64,
    amount_in: u64,
    fee_per_mille: u16,
) -> Result<u64, CurveError> {
    if amount_in == 0 {
        return Err(CurveError::ZeroAmount);
    }
    if reserve_in == 0 || reserve_out == 0 {
        return Err(CurveError::ZeroReserve);
    }
    if amount_in > MAX_RESERVE || reserve_in > MAX_RESERVE || reserve_out > MAX_RESERVE {
        return Err(CurveError::ReserveCapExceeded);
    }
    if fee_per_mille >= 1000 {
        return Err(CurveError::InvalidFee);
    }

    let in_with_fee = u128::from(amount_in) * u128::from(1000 - fee_per_mille);
    let numerator = in_with_fee * u128::from(reserve_out);
    let denominator = u128::from(reserve_in) * 1000 + in_with_fee;
    let out = numerator / denominator;

    if out == 0 {
        return Err(CurveError::OutputRoundsToZero);
    }
    if out >= u128::from(reserve_out) {
        return Err(CurveError::InsufficientLiquidity);
    }
    u64::try_from(out).map_err(|_| CurveError::Overflow)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MintOutcome {
    /// Shares credited to the depositor.
    pub to_owner: u64,
    /// Pool total afterwards, including the unassigned MINIMUM_LIQUIDITY.
    pub new_total_shares: u64,
}

/// Uniswap V2 `mint`.
///
/// On pool creation (`total_shares == 0`), mints `isqrt(da * db)` and assigns
/// all but `MINIMUM_LIQUIDITY` to the depositor; the remainder is credited to
/// nobody and is unwithdrawable forever, which both defeats the first-depositor
/// inflation attack and guarantees `total_shares` never returns to zero.
///
/// Otherwise mints `min(da * S / r_lo, db * S / r_hi)`. The `min` is what forces
/// deposits at the current ratio; excess on the over-supplied side is donated to
/// existing LPs. Spec §7.2.
pub fn mint_shares(
    reserve_lo: u64,
    reserve_hi: u64,
    total_shares: u64,
    da: u64,
    db: u64,
) -> Result<MintOutcome, CurveError> {
    if da == 0 || db == 0 {
        return Err(CurveError::ZeroAmount);
    }
    if da > MAX_RESERVE || db > MAX_RESERVE {
        return Err(CurveError::ReserveCapExceeded);
    }
    if reserve_lo.checked_add(da).is_none_or(|r| r > MAX_RESERVE)
        || reserve_hi.checked_add(db).is_none_or(|r| r > MAX_RESERVE)
    {
        return Err(CurveError::ReserveCapExceeded);
    }

    if total_shares == 0 {
        let minted = (u128::from(da) * u128::from(db)).isqrt();
        let minted = u64::try_from(minted).map_err(|_| CurveError::Overflow)?;
        if minted <= MINIMUM_LIQUIDITY {
            return Err(CurveError::InsufficientInitialLiquidity);
        }
        Ok(MintOutcome {
            to_owner: minted - MINIMUM_LIQUIDITY,
            new_total_shares: minted,
        })
    } else {
        if reserve_lo == 0 || reserve_hi == 0 {
            return Err(CurveError::ZeroReserve);
        }
        let via_lo = u128::from(da) * u128::from(total_shares) / u128::from(reserve_lo);
        let via_hi = u128::from(db) * u128::from(total_shares) / u128::from(reserve_hi);
        let minted = u64::try_from(via_lo.min(via_hi)).map_err(|_| CurveError::Overflow)?;
        if minted == 0 {
            return Err(CurveError::OutputRoundsToZero);
        }
        Ok(MintOutcome {
            to_owner: minted,
            new_total_shares: total_shares
                .checked_add(minted)
                .ok_or(CurveError::Overflow)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurnOutcome {
    pub da: u64,
    pub db: u64,
    pub new_total_shares: u64,
}

/// Uniswap V2 `burn`. Both payouts floor, so rounding dust stays with the pool.
/// Spec §7.3.
pub fn burn_shares(
    reserve_lo: u64,
    reserve_hi: u64,
    total_shares: u64,
    shares: u64,
) -> Result<BurnOutcome, CurveError> {
    if shares == 0 {
        return Err(CurveError::ZeroAmount);
    }
    if shares > total_shares {
        return Err(CurveError::InsufficientShares);
    }
    if total_shares == 0 {
        return Err(CurveError::InsufficientShares);
    }

    let da = u128::from(reserve_lo) * u128::from(shares) / u128::from(total_shares);
    let db = u128::from(reserve_hi) * u128::from(shares) / u128::from(total_shares);
    let da = u64::try_from(da).map_err(|_| CurveError::Overflow)?;
    let db = u64::try_from(db).map_err(|_| CurveError::Overflow)?;

    if da == 0 && db == 0 {
        return Err(CurveError::OutputRoundsToZero);
    }

    Ok(BurnOutcome {
        da,
        db,
        new_total_shares: total_shares - shares,
    })
}

/// Backstop for §7.1. Returns `false` if a swap would shrink `k`; the caller
/// must then reject the transaction — never panic.
pub fn k_non_decreasing(r_in_old: u64, r_out_old: u64, r_in_new: u64, r_out_new: u64) -> bool {
    let old = u128::from(r_in_old) * u128::from(r_out_old);
    let new = u128::from(r_in_new) * u128::from(r_out_new);
    new >= old
}
```

Note: this uses `u128::isqrt` from std (stable since Rust 1.84), which supersedes the spec's hand-rolled Newton iteration — std's is integer-only, deterministic, and far better tested than anything we would write. Update spec §7.2's final paragraph accordingly when committing.

- [ ] **Step 5: Wire the module in**

In `modules/amm/fedimint-amm-common/src/lib.rs`:

```rust
pub mod math;
```

- [ ] **Step 6: Run the tests to verify they pass**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-common
```

Expected: all tests pass, including the four proptest cases. If `no_overflow_within_caps` fails with an arithmetic overflow panic, the cap derivation is wrong — stop and report rather than widening the cap.

- [ ] **Step 7: Run clippy**

```bash
nix develop --command cargo clippy -p fedimint-amm-common --all-targets -- -D warnings
```

Expected: clean.

- [ ] **Step 8: Update the spec's isqrt paragraph**

In `modules/amm/fedimint-amm-spec.md` §7.2, replace the final paragraph with:

```markdown
`isqrt` uses `u128::isqrt` from std (stable since Rust 1.84) — integer-only,
deterministic, and better tested than a hand-rolled Newton iteration.
```

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat(amm): constant-product curve and share arithmetic"
```

---

## Task 2: Canonical `PoolId`

**Files:**
- Create: `modules/amm/fedimint-amm-common/src/pool_id.rs`
- Modify: `modules/amm/fedimint-amm-common/src/lib.rs` (add `pub mod pool_id;`)
- Modify: `modules/amm/fedimint-amm-common/Cargo.toml` (add `fedimint-core`)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces:
  - `pub struct PoolId { lo: AmountUnit, hi: AmountUnit }` — fields private
  - `pub fn PoolId::new(x: AmountUnit, y: AmountUnit) -> Option<Self>`
  - `pub fn PoolId::lo(&self) -> AmountUnit`, `pub fn PoolId::hi(&self) -> AmountUnit`
  - `pub fn PoolId::contains(&self, unit: AmountUnit) -> bool`
  - `impl Encodable for PoolId`, `impl Decodable for PoolId` (hand-written, canonicality-enforcing)

**Why this task exists:** `PoolId` is `Decodable`, so a hand-rolled client can encode it with `lo > hi`. Accepting that gives one unit pair **two** `Pool` records — liquidity splits and quotes stop matching reality. Spec §5.1.

- [ ] **Step 1: Write the failing tests**

Create `modules/amm/fedimint-amm-common/src/pool_id.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::module::AmountUnit;
    use fedimint_core::module::registry::ModuleDecoderRegistry;

    use super::*;

    #[test]
    fn new_sorts_the_pair() {
        let a = AmountUnit::new_custom(1);
        let b = AmountUnit::new_custom(7);
        assert_eq!(PoolId::new(a, b), PoolId::new(b, a));
        let id = PoolId::new(b, a).unwrap();
        assert_eq!(id.lo(), a);
        assert_eq!(id.hi(), b);
    }

    #[test]
    fn new_rejects_identical_units() {
        let a = AmountUnit::new_custom(3);
        assert_eq!(PoolId::new(a, a), None);
    }

    #[test]
    fn round_trips_through_encoding() {
        let id = PoolId::new(AmountUnit::new_custom(2), AmountUnit::new_custom(9)).unwrap();
        let bytes = id.consensus_encode_to_vec();
        let decoded =
            PoolId::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(id, decoded);
    }

    /// Spec §5.1 — the whole point of a hand-written Decodable.
    #[test]
    fn rejects_non_canonical_encoding() {
        // Hand-build the wire form with lo > hi.
        let mut bytes = Vec::new();
        AmountUnit::new_custom(9).consensus_encode(&mut bytes).unwrap();
        AmountUnit::new_custom(2).consensus_encode(&mut bytes).unwrap();
        assert!(
            PoolId::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).is_err()
        );
    }

    #[test]
    fn rejects_equal_units_in_encoding() {
        let mut bytes = Vec::new();
        AmountUnit::new_custom(4).consensus_encode(&mut bytes).unwrap();
        AmountUnit::new_custom(4).consensus_encode(&mut bytes).unwrap();
        assert!(
            PoolId::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).is_err()
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-common pool_id
```

Expected: compile failure, `cannot find type 'PoolId'`.

- [ ] **Step 3: Implement**

Read `fedimint-core/src/encoding/mod.rs` in the pinned checkout (`~/.cargo/git/checkouts/fedimint-*/4794ee1/`) for the exact `Encodable`/`Decodable` trait signatures on this rev before writing this — the trait shape has changed across versions. Confirm `AmountUnit`'s constructor name (`AmountUnit::new` vs a tuple struct) at `fedimint-core/src/module/mod.rs:77` and adjust the tests if it differs.

Prepend to `pool_id.rs`:

```rust
//! Canonical, unordered identifier for a pool. Spec §5.1.

use std::cmp::Ordering;
use std::io::{Error, Read, Write};

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::AmountUnit;
use fedimint_core::module::registry::ModuleDecoderRegistry;

/// A pool over an unordered pair of units, stored sorted so that `(A, B)` and
/// `(B, A)` resolve to the same record.
///
/// Fields are private and the `Decodable` impl is hand-written: a
/// non-canonical encoding (`lo >= hi`) MUST be rejected, or one unit pair
/// yields two distinct `Pool` records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PoolId {
    lo: AmountUnit,
    hi: AmountUnit,
}

impl PoolId {
    pub fn new(x: AmountUnit, y: AmountUnit) -> Option<Self> {
        match x.cmp(&y) {
            Ordering::Equal => None,
            Ordering::Less => Some(Self { lo: x, hi: y }),
            Ordering::Greater => Some(Self { lo: y, hi: x }),
        }
    }

    pub fn lo(&self) -> AmountUnit {
        self.lo
    }

    pub fn hi(&self) -> AmountUnit {
        self.hi
    }

    pub fn contains(&self, unit: AmountUnit) -> bool {
        self.lo == unit || self.hi == unit
    }

    /// Given one side, return the other. `None` if `unit` is not in this pair.
    pub fn other(&self, unit: AmountUnit) -> Option<AmountUnit> {
        if unit == self.lo {
            Some(self.hi)
        } else if unit == self.hi {
            Some(self.lo)
        } else {
            None
        }
    }
}

impl Encodable for PoolId {
    fn consensus_encode<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.lo.consensus_encode(writer)?;
        self.hi.consensus_encode(writer)
    }
}

impl Decodable for PoolId {
    fn consensus_decode_partial<R: Read>(
        reader: &mut R,
        modules: &ModuleDecoderRegistry,
    ) -> Result<Self, fedimint_core::encoding::DecodeError> {
        let lo = AmountUnit::consensus_decode_partial(reader, modules)?;
        let hi = AmountUnit::consensus_decode_partial(reader, modules)?;
        if lo >= hi {
            return Err(fedimint_core::encoding::DecodeError::from_str(
                "PoolId must be canonical: lo < hi",
            ));
        }
        Ok(Self { lo, hi })
    }
}
```

If the trait method names on this rev differ (e.g. `consensus_decode` rather than `consensus_decode_partial`), match what `fedimint-core` actually defines — the canonicality check is the invariant, not the method name.

- [ ] **Step 4: Wire the module in**

In `lib.rs`: `pub mod pool_id;`

- [ ] **Step 5: Run the tests to verify they pass**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-common pool_id
```

Expected: all five pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(amm): canonical PoolId with encoding-level ordering check"
```

---

## Task 3: Config types and DKG validation

**Files:**
- Create: `modules/amm/fedimint-amm-common/src/config.rs`
- Modify: `modules/amm/fedimint-amm-common/src/lib.rs`

**Interfaces:**
- Consumes: `PoolId` (Task 2), `MAX_RESERVE` (Task 1).
- Produces:
  - `pub struct UnitParams { pub min_swap_in: Amount }`
  - `pub struct AmmConfigConsensus { pub units: BTreeMap<AmountUnit, UnitParams>, pub default_fee_per_mille: u16, pub fee_overrides: BTreeMap<PoolId, u16> }`
  - `pub struct AmmClientConfig { … same three fields … }`
  - `pub struct AmmConfigPrivate;`
  - `pub fn AmmConfigConsensus::validate(&self) -> Result<(), ConfigError>`
  - `pub fn AmmConfigConsensus::fee_for(&self, pool: PoolId) -> u16`
  - `pub enum ConfigError`

- [ ] **Step 1: Write the failing tests**

In `modules/amm/fedimint-amm-common/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use fedimint_core::Amount;
    use fedimint_core::module::AmountUnit;

    use super::*;
    use crate::pool_id::PoolId;

    fn units() -> BTreeMap<AmountUnit, UnitParams> {
        BTreeMap::from([
            (AmountUnit::new_custom(0), UnitParams { min_swap_in: Amount::from_msats(1_000) }),
            (AmountUnit::new_custom(1), UnitParams { min_swap_in: Amount::from_msats(10) }),
        ])
    }

    fn cfg() -> AmmConfigConsensus {
        AmmConfigConsensus {
            units: units(),
            default_fee_per_mille: 3,
            fee_overrides: BTreeMap::new(),
        }
    }

    #[test]
    fn accepts_a_well_formed_config() {
        assert_eq!(cfg().validate(), Ok(()));
    }

    #[test]
    fn rejects_empty_units() {
        let mut c = cfg();
        c.units.clear();
        assert_eq!(c.validate(), Err(ConfigError::NoUnits));
    }

    #[test]
    fn rejects_fee_at_or_above_one_thousand() {
        let mut c = cfg();
        c.default_fee_per_mille = 1_000;
        assert_eq!(c.validate(), Err(ConfigError::InvalidFee));
    }

    #[test]
    fn rejects_zero_min_swap_in() {
        let mut c = cfg();
        c.units.insert(
            AmountUnit::new_custom(0),
            UnitParams { min_swap_in: Amount::ZERO },
        );
        assert_eq!(c.validate(), Err(ConfigError::ZeroMinSwapIn));
    }

    #[test]
    fn rejects_fee_override_for_an_unknown_unit() {
        let mut c = cfg();
        let unknown = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(99)).unwrap();
        c.fee_overrides.insert(unknown, 5);
        assert_eq!(c.validate(), Err(ConfigError::UnknownUnitInOverride));
    }

    #[test]
    fn fee_for_prefers_the_override() {
        let mut c = cfg();
        let pool = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap();
        assert_eq!(c.fee_for(pool), 3);
        c.fee_overrides.insert(pool, 1);
        assert_eq!(c.fee_for(pool), 1);
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-common config
```

Expected: compile failure.

- [ ] **Step 3: Implement**

Read `modules/fedimint-dummy-common/src/config.rs` in the pinned checkout first, and mirror its derive attributes and `plugin_types_trait_impl_config!` usage exactly — the config trait surface is boilerplate-heavy and version-specific.

```rust
//! Spec §11.

use std::collections::BTreeMap;

use fedimint_core::Amount;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::AmountUnit;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::pool_id::PoolId;

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct UnitParams {
    /// Minimum accepted swap input in this unit. Anti-dust and anti-spam; a
    /// DoS control, NOT a privacy control — see spec §13.1. Do not document
    /// it as one.
    pub min_swap_in: Amount,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct AmmConfigConsensus {
    /// Units this federation permits trading. A unit is only reachable if some
    /// mintv2 instance issues it — a setup requirement this module cannot
    /// verify.
    pub units: BTreeMap<AmountUnit, UnitParams>,
    /// Default 3 == 0.30%, the Uniswap V2 value.
    pub default_fee_per_mille: u16,
    pub fee_overrides: BTreeMap<PoolId, u16>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct AmmClientConfig {
    pub units: BTreeMap<AmountUnit, UnitParams>,
    pub default_fee_per_mille: u16,
    pub fee_overrides: BTreeMap<PoolId, u16>,
}

/// Empty: this module holds no key material, which is why one instance can
/// host many pools while a mintv2 instance hosts exactly one unit.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct AmmConfigPrivate;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    #[error("units must not be empty")]
    NoUnits,
    #[error("fee_per_mille must be < 1000")]
    InvalidFee,
    #[error("min_swap_in must be non-zero")]
    ZeroMinSwapIn,
    #[error("fee override names a unit not in `units`")]
    UnknownUnitInOverride,
}

impl AmmConfigConsensus {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.units.is_empty() {
            return Err(ConfigError::NoUnits);
        }
        if self.default_fee_per_mille >= 1000 {
            return Err(ConfigError::InvalidFee);
        }
        if self.units.values().any(|p| p.min_swap_in == Amount::ZERO) {
            return Err(ConfigError::ZeroMinSwapIn);
        }
        for (pool, fee) in &self.fee_overrides {
            if *fee >= 1000 {
                return Err(ConfigError::InvalidFee);
            }
            if !self.units.contains_key(&pool.lo()) || !self.units.contains_key(&pool.hi()) {
                return Err(ConfigError::UnknownUnitInOverride);
            }
        }
        Ok(())
    }

    pub fn fee_for(&self, pool: PoolId) -> u16 {
        self.fee_overrides
            .get(&pool)
            .copied()
            .unwrap_or(self.default_fee_per_mille)
    }
}

impl AmmClientConfig {
    pub fn fee_for(&self, pool: PoolId) -> u16 {
        self.fee_overrides
            .get(&pool)
            .copied()
            .unwrap_or(self.default_fee_per_mille)
    }
}
```

- [ ] **Step 4: Wire in and run**

Add `pub mod config;` to `lib.rs`, then:

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-common
```

Expected: all tests across math, pool_id and config pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(amm): config types with DKG validation"
```

---

## Task 4: Transaction item types

**Files:**
- Create: `modules/amm/fedimint-amm-common/src/types.rs`
- Modify: `modules/amm/fedimint-amm-common/src/lib.rs` (module kind, consensus version, re-exports)

**Interfaces:**
- Consumes: `PoolId` (Task 2).
- Produces: `AmmInput`, `AmmOutput`, `AmmInputError`, `AmmOutputError`, `AmmModuleTypes`, `KIND`, `MODULE_CONSENSUS_VERSION`.

**Reference:** copy the plugin-types boilerplate from `modules/fedimint-dummy-common/src/lib.rs` on the pinned rev.

- [ ] **Step 1: Write the failing tests**

In `types.rs`:

```rust
#[cfg(test)]
mod tests {
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::module::registry::ModuleDecoderRegistry;
    use fedimint_core::module::AmountUnit;
    use fedimint_core::{Amount, secp256k1};

    use super::*;
    use crate::pool_id::PoolId;

    fn pk() -> secp256k1::PublicKey {
        secp256k1::Keypair::from_seckey_slice(secp256k1::SECP256K1, &[1u8; 32])
            .expect("valid secret key")
            .public_key()
    }

    #[test]
    fn swap_output_round_trips() {
        let out = AmmOutput::SwapV0 {
            unit_in: AmountUnit::new_custom(0),
            unit_out: AmountUnit::new_custom(1),
            amount_in: Amount::from_msats(10_000),
            min_out: Amount::from_msats(9_000),
            recipient_pk: pk(),
            tweak: [7u8; 16],
        };
        let bytes = out.consensus_encode_to_vec();
        let back =
            AmmOutput::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(out, back);
    }

    #[test]
    fn claim_balance_input_round_trips() {
        let inp = AmmInput::ClaimBalanceV0 {
            pubkey: pk(),
            unit: AmountUnit::new_custom(1),
        };
        let bytes = inp.consensus_encode_to_vec();
        let back =
            AmmInput::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(inp, back);
    }

    #[test]
    fn withdraw_input_round_trips() {
        let inp = AmmInput::WithdrawV0 {
            pool: PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap(),
            owner_pk: pk(),
            shares: 500,
            min_lo: Amount::from_msats(1),
            min_hi: Amount::from_msats(1),
        };
        let bytes = inp.consensus_encode_to_vec();
        let back =
            AmmInput::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(inp, back);
    }

    /// Unknown variants must survive a round trip via #[encodable_default],
    /// so a newer peer's item does not break an older decoder.
    #[test]
    fn unknown_variant_round_trips_through_default() {
        let unknown = AmmInput::Default {
            variant: 42,
            bytes: vec![1, 2, 3],
        };
        let bytes = unknown.consensus_encode_to_vec();
        let back =
            AmmInput::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(unknown, back);
    }
}
```

If `fedimint-dummy-common`'s tests use a different idiom for constructing a test pubkey, prefer theirs.

- [ ] **Step 2: Run to verify failure**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-common types
```

Expected: compile failure.

- [ ] **Step 3: Implement**

```rust
//! Spec §6.

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::AmountUnit;
use fedimint_core::{Amount, secp256k1};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::pool_id::PoolId;

/// Outputs CREATE records, so they consume value. Spec §3.1.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub enum AmmOutput {
    SwapV0 {
        unit_in: AmountUnit,
        /// MUST differ from `unit_in`.
        unit_out: AmountUnit,
        amount_in: Amount,
        /// Router-equivalent `amountOutMin`, enforced server-side.
        min_out: Amount,
        recipient_pk: secp256k1::PublicKey,
        /// Ground per spec §8; stored on the Balance record for recovery.
        tweak: [u8; 16],
    },
    DepositV0 {
        pool: PoolId,
        amount_lo: Amount,
        amount_hi: Amount,
        min_shares: u64,
        owner_pk: secp256k1::PublicKey,
        tweak: [u8; 16],
    },
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}

/// Inputs DESTROY pre-existing authenticated records, so they provide value.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub enum AmmInput {
    /// Claims the ENTIRE balance. No amount field: spec §6.1 explains why a
    /// declared amount would be a second source of truth, and why partial
    /// claims would leave permanent residue in an ungarbage-collected table.
    ClaimBalanceV0 {
        pubkey: secp256k1::PublicKey,
        unit: AmountUnit,
    },
    WithdrawV0 {
        pool: PoolId,
        owner_pk: secp256k1::PublicKey,
        shares: u64,
        /// Router-equivalent `amountAMin` / `amountBMin`.
        min_lo: Amount,
        min_hi: Amount,
    },
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}

#[derive(Debug, Error, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable, Hash)]
pub enum AmmInputError {
    #[error("unknown input variant")]
    UnknownVariant,
    #[error("no balance for this key and unit")]
    NoSuchBalance,
    #[error("no such pool")]
    NoSuchPool,
    #[error("no LP position for this key")]
    NoSuchPosition,
    #[error("not enough shares")]
    InsufficientShares,
    #[error("payout below min_lo/min_hi")]
    SlippageExceeded,
    #[error("arithmetic error: {0}")]
    Curve(String),
}

#[derive(Debug, Error, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable, Hash)]
pub enum AmmOutputError {
    #[error("unknown output variant")]
    UnknownVariant,
    #[error("unit_in and unit_out must differ")]
    IdenticalUnits,
    #[error("unit not in the federation's allowlist")]
    UnknownUnit,
    #[error("no such pool")]
    NoSuchPool,
    #[error("amount_in below the unit's min_swap_in")]
    BelowMinSwapIn,
    #[error("output below min_out, or shares below min_shares")]
    SlippageExceeded,
    #[error("would exceed MAX_RESERVE")]
    ReserveCapExceeded,
    #[error("k invariant violated")]
    KInvariantViolated,
    #[error("arithmetic error: {0}")]
    Curve(String),
}
```

In `lib.rs`, add the module-kind boilerplate mirroring `fedimint-dummy-common`:

```rust
pub mod config;
pub mod math;
pub mod pool_id;
pub mod types;

use fedimint_core::core::ModuleKind;
use fedimint_core::module::{CommonModuleInit, ModuleCommon, ModuleConsensusVersion};

pub const KIND: ModuleKind = ModuleKind::from_static_str("amm");
pub const MODULE_CONSENSUS_VERSION: ModuleConsensusVersion = ModuleConsensusVersion::new(0, 0);
```

Then define `AmmCommonInit` and `AmmModuleTypes` exactly as `fedimint-dummy-common` does, substituting our types and using `()` for the consensus-item type — this module has **no consensus items** (spec §10).

- [ ] **Step 4: Run and commit**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-common
nix develop --command cargo clippy -p fedimint-amm-common --all-targets -- -D warnings
git add -A
git commit -m "feat(amm): transaction item types and module common boilerplate"
```

---

## Task 5: Server database schema

**Files:**
- Create: `modules/amm/fedimint-amm-server/src/db.rs`
- Modify: `modules/amm/fedimint-amm-server/src/lib.rs`
- Modify: `modules/amm/fedimint-amm-server/Cargo.toml` (`fedimint-core`, `fedimint-amm-common`, `futures`, `strum`, `strum_macros`)

**Interfaces:**
- Consumes: `PoolId` (Task 2).
- Produces: `DbKeyPrefix`, `PoolKey`/`PoolPrefix`/`Pool`, `LpPositionKey`/`LpPositionPrefix`/`LpPositionPoolPrefix`/`LpPosition`, `BalanceKey`/`BalancePrefix`/`BalanceEntry`.

**Reference:** `modules/fedimint-lnv2-server/src/db.rs` on the pinned rev — it has the closest key/prefix shape (composite keys with partial prefixes).

- [ ] **Step 1: Write the failing tests**

In `db.rs`:

```rust
#[cfg(test)]
mod tests {
    use fedimint_core::Amount;
    use fedimint_core::db::mem_impl::MemDatabase;
    use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
    use fedimint_core::module::AmountUnit;
    use fedimint_core::module::registry::ModuleDecoderRegistry;
    use futures::StreamExt;

    use super::*;

    fn db() -> Database {
        Database::new(MemDatabase::new(), ModuleDecoderRegistry::default())
    }

    fn pool_id(a: u64, b: u64) -> PoolId {
        PoolId::new(AmountUnit::new_custom(a), AmountUnit::new_custom(b)).unwrap()
    }

    #[tokio::test]
    async fn pool_records_round_trip() {
        let db = db();
        let mut dbtx = db.begin_transaction().await;
        let id = pool_id(0, 1);
        let pool = Pool {
            reserve_lo: Amount::from_msats(100),
            reserve_hi: Amount::from_msats(200),
            total_shares: 1_000,
        };
        dbtx.insert_new_entry(&PoolKey(id), &pool).await;
        assert_eq!(dbtx.get_value(&PoolKey(id)).await, Some(pool));
        dbtx.commit_tx().await;
    }

    /// Positions for one pool must enumerate under a partial prefix — the API
    /// and any future audit path both need this.
    #[tokio::test]
    async fn lp_positions_enumerate_by_pool() {
        let db = db();
        let mut dbtx = db.begin_transaction().await;
        let a = pool_id(0, 1);
        let b = pool_id(0, 2);
        let k1 = test_pubkey(1);
        let k2 = test_pubkey(2);

        for (pool, key) in [(a, k1), (a, k2), (b, k1)] {
            dbtx.insert_new_entry(
                &LpPositionKey { pool, owner: key },
                &LpPosition { shares: 10, tweak: [0u8; 16] },
            )
            .await;
        }

        let in_a: Vec<_> = dbtx
            .find_by_prefix(&LpPositionPoolPrefix(a))
            .await
            .collect()
            .await;
        assert_eq!(in_a.len(), 2);

        let all: Vec<_> = dbtx.find_by_prefix(&LpPositionPrefix).await.collect().await;
        assert_eq!(all.len(), 3);
        dbtx.commit_tx().await;
    }

    #[tokio::test]
    async fn balances_round_trip_and_delete() {
        let db = db();
        let mut dbtx = db.begin_transaction().await;
        let key = BalanceKey { owner: test_pubkey(3), unit: AmountUnit::new_custom(1) };
        dbtx.insert_new_entry(
            &key,
            &BalanceEntry { amount: Amount::from_msats(42), tweak: [1u8; 16] },
        )
        .await;
        assert!(dbtx.get_value(&key).await.is_some());
        dbtx.remove_entry(&key).await;
        assert!(dbtx.get_value(&key).await.is_none());
        dbtx.commit_tx().await;
    }
}
```

Add a `test_pubkey(seed: u8) -> secp256k1::PublicKey` helper in the test module deriving from `[seed; 32]` via `secp256k1::Keypair::from_seckey_slice`.

- [ ] **Step 2: Run to verify failure**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-server db
```

Expected: compile failure.

- [ ] **Step 3: Implement**

```rust
//! Spec §5.

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::AmountUnit;
use fedimint_core::{Amount, impl_db_lookup, impl_db_record, secp256k1};
use fedimint_amm_common::pool_id::PoolId;
use serde::Serialize;
use strum_macros::EnumIter;

#[repr(u8)]
#[derive(Clone, Copy, Debug, EnumIter)]
pub enum DbKeyPrefix {
    Pool = 0x01,
    LpPosition = 0x02,
    Balance = 0x03,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct PoolKey(pub PoolId);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct PoolPrefix;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Encodable, Decodable)]
pub struct Pool {
    pub reserve_lo: Amount,
    pub reserve_hi: Amount,
    /// Includes the unassigned MINIMUM_LIQUIDITY, so this never returns to 0.
    pub total_shares: u64,
}

impl_db_record!(key = PoolKey, value = Pool, db_prefix = DbKeyPrefix::Pool);
impl_db_lookup!(key = PoolKey, query_prefix = PoolPrefix);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct LpPositionKey {
    pub pool: PoolId,
    pub owner: secp256k1::PublicKey,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct LpPositionPrefix;

/// Partial prefix: all positions in one pool.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct LpPositionPoolPrefix(pub PoolId);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Encodable, Decodable)]
pub struct LpPosition {
    pub shares: u64,
    /// Ground tweak, stored so recovery is a table scan not a history replay.
    pub tweak: [u8; 16],
}

impl_db_record!(
    key = LpPositionKey,
    value = LpPosition,
    db_prefix = DbKeyPrefix::LpPosition
);
impl_db_lookup!(
    key = LpPositionKey,
    query_prefix = LpPositionPrefix,
    query_prefix = LpPositionPoolPrefix
);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct BalanceKey {
    pub owner: secp256k1::PublicKey,
    pub unit: AmountUnit,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct BalancePrefix;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Encodable, Decodable)]
pub struct BalanceEntry {
    pub amount: Amount,
    pub tweak: [u8; 16],
}

impl_db_record!(
    key = BalanceKey,
    value = BalanceEntry,
    db_prefix = DbKeyPrefix::Balance
);
impl_db_lookup!(key = BalanceKey, query_prefix = BalancePrefix);
```

Check `impl_db_lookup!`'s multi-prefix syntax against `fedimint-lnv2-server/src/db.rs` on the pinned rev; if it does not support two query prefixes in one invocation, use two invocations.

- [ ] **Step 4: Run and commit**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-server
git add -A
git commit -m "feat(amm): server database schema"
```

---

## Task 6: `ServerModule` skeleton, init and config generation

**Files:**
- Modify: `modules/amm/fedimint-amm-server/src/lib.rs`
- Modify: `modules/amm/fedimint-amm-server/Cargo.toml` (`fedimint-server-core`, `async-trait`, `anyhow`, `erased-serde`)

**Interfaces:**
- Consumes: everything from Tasks 2-5.
- Produces: `pub struct Amm { pub cfg: AmmConfigConsensus }`, `pub struct AmmInit`, with `ServerModuleInit` and `ServerModule` implemented. `process_input` / `process_output` return `UnknownVariant` for every variant at this stage; Tasks 7-8 fill them in.

**Reference:** `modules/fedimint-dummy-server/src/lib.rs` on the pinned rev is the template. Copy its structure wholesale and substitute types.

- [ ] **Step 1: Write the failing test**

Create `modules/amm/fedimint-amm-server/tests/config.rs`:

```rust
use fedimint_amm_server::AmmInit;
use fedimint_core::module::ServerModuleInit;

#[test]
fn init_reports_our_module_kind() {
    assert_eq!(AmmInit::kind(), fedimint_amm_common::KIND);
}

/// `trusted_dealer_gen` must emit a config that passes `validate()` for every
/// peer, and all peers must agree on it. Build the params with two units
/// (unit 0 with `min_swap_in` 1000 msats, unit 1 with 10) and a default fee of
/// 3, using whatever `ServerModuleInit::trusted_dealer_gen` signature the
/// pinned rev exposes — read `fedimint-dummy-server/src/lib.rs` for its shape.
#[test]
fn trusted_dealer_gen_emits_a_valid_config_for_every_peer() {
    let configs = /* AmmInit.trusted_dealer_gen(&peers, &params) */;
    assert_eq!(configs.len(), 4);
    let mut consensus = Vec::new();
    for (_peer, cfg) in configs {
        let cfg: AmmConfigConsensus = /* project cfg.consensus */;
        assert_eq!(cfg.validate(), Ok(()));
        consensus.push(cfg);
    }
    // Every peer must derive byte-identical consensus config.
    assert!(consensus.windows(2).all(|w| w[0] == w[1]));
}

/// The generator must never emit an empty unit set, which `validate` rejects.
#[test]
fn empty_units_are_rejected_by_validation() {
    let cfg = AmmConfigConsensus {
        units: Default::default(),
        default_fee_per_mille: 3,
        fee_overrides: Default::default(),
    };
    assert_eq!(cfg.validate(), Err(ConfigError::NoUnits));
}
```

- [ ] **Step 2: Run to verify failure**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-server --test config
```

Expected: compile failure, `AmmInit` not found.

- [ ] **Step 3: Implement**

Read `modules/fedimint-dummy-server/src/lib.rs` on the pinned rev in full before writing. Implement:

- `AmmInit` with `ServerModuleInit`: `versions`, `supported_api_versions`, `init`, `trusted_dealer_gen`, `distributed_gen`, `validate_config`, `get_client_config`, `dump_database`.
- `trusted_dealer_gen` / `distributed_gen` must call `AmmConfigConsensus::validate()` and fail the ceremony on error. There is no key material to generate — `AmmConfigPrivate` is a unit struct.
- `validate_config` calls `validate()`.
- `get_client_config` projects `AmmConfigConsensus` onto `AmmClientConfig` field-for-field.
- `Amm` with `ServerModule`: `consensus_version`, `consensus_proposal` returning an empty vec, `process_consensus_item` returning an error unconditionally (we have **no** consensus items — spec §10), `process_input` and `process_output` returning `UnknownVariant`, `output_status`, `audit` empty for now, `api_endpoints` empty for now.

Every handler must return `Result`, never panic.

- [ ] **Step 4: Run and commit**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-server
nix develop --command cargo clippy -p fedimint-amm-server --all-targets -- -D warnings
git add -A
git commit -m "feat(amm): ServerModule skeleton and config generation"
```

---

## Task 7: `process_output` — `SwapV0` and `DepositV0`

**Files:**
- Modify: `modules/amm/fedimint-amm-server/src/lib.rs`
- Create: `modules/amm/fedimint-amm-server/tests/output.rs`

**Interfaces:**
- Consumes: `math::{amount_out, mint_shares, k_non_decreasing, MAX_RESERVE}`, db types, `AmmOutput`, `AmmOutputError`.
- Produces: working `process_output` returning `TransactionItemAmounts`.

- [ ] **Step 1: Write the failing tests**

Create `modules/amm/fedimint-amm-server/tests/output.rs` with a helper that builds an `Amm` over a `MemDatabase` and calls `process_output` directly. Cases:

```rust
// 1. SwapV0 into a fresh pool fails with NoSuchPool.
// 2. DepositV0 into an empty PoolId creates the pool, mints
//    isqrt(da*db) - MINIMUM_LIQUIDITY to owner_pk, and returns
//    amounts == {lo: da, hi: db}.
// 3. DepositV0 below MINIMUM_LIQUIDITY is rejected and writes NOTHING
//    (assert the Pool key is still absent afterwards).
// 4. DepositV0 with min_shares above what is mintable returns
//    SlippageExceeded.
// 5. SwapV0 on a live pool moves reserves by exactly amount_in / dy,
//    credits Balance[(recipient_pk, unit_out)] == dy, and returns
//    amounts == {unit_in: amount_in}.
//    ** Assert the credited balance and the reserve debit are the SAME
//       integer — this is the §7.4 invariant.
// 6. SwapV0 with min_out above dy returns SlippageExceeded and writes nothing.
// 7. SwapV0 with unit_in == unit_out returns IdenticalUnits.
// 8. SwapV0 with a unit outside `units` returns UnknownUnit.
// 9. SwapV0 with amount_in below min_swap_in returns BelowMinSwapIn.
// 10. SwapV0 that would push a reserve above MAX_RESERVE returns
//     ReserveCapExceeded.
// 11. A second SwapV0 into an existing recipient_pk/unit ADDS to the balance
//     rather than replacing it.
```

Write each as a separate `#[tokio::test]` with real assertions — no loops over a table, so a failure names the case.

- [ ] **Step 2: Run to verify failure**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-server --test output
```

Expected: every test fails with `UnknownVariant`.

- [ ] **Step 3: Implement `SwapV0`**

```rust
AmmOutput::SwapV0 { unit_in, unit_out, amount_in, min_out, recipient_pk, tweak } => {
    if unit_in == unit_out {
        return Err(AmmOutputError::IdenticalUnits);
    }
    let params_in = self.cfg.units.get(unit_in).ok_or(AmmOutputError::UnknownUnit)?;
    if !self.cfg.units.contains_key(unit_out) {
        return Err(AmmOutputError::UnknownUnit);
    }
    if *amount_in < params_in.min_swap_in {
        return Err(AmmOutputError::BelowMinSwapIn);
    }
    let pool_id = PoolId::new(*unit_in, *unit_out).ok_or(AmmOutputError::IdenticalUnits)?;
    let mut pool = dbtx.get_value(&PoolKey(pool_id)).await.ok_or(AmmOutputError::NoSuchPool)?;

    // Orient reserves so `in` and `out` match the trader's direction.
    let in_is_lo = *unit_in == pool_id.lo();
    let (reserve_in, reserve_out) = if in_is_lo {
        (pool.reserve_lo, pool.reserve_hi)
    } else {
        (pool.reserve_hi, pool.reserve_lo)
    };

    let fee = self.cfg.fee_for(pool_id);
    // Computed ONCE. This one binding is used for the reserve debit AND the
    // balance credit — spec §7.4.
    let dy = math::amount_out(reserve_in.msats, reserve_out.msats, amount_in.msats, fee)
        .map_err(|e| AmmOutputError::Curve(e.to_string()))?;

    if dy < min_out.msats {
        return Err(AmmOutputError::SlippageExceeded);
    }

    let reserve_in_new = reserve_in.msats
        .checked_add(amount_in.msats)
        .ok_or(AmmOutputError::ReserveCapExceeded)?;
    if reserve_in_new > math::MAX_RESERVE {
        return Err(AmmOutputError::ReserveCapExceeded);
    }
    let reserve_out_new = reserve_out.msats - dy;   // dy < reserve_out, checked in amount_out

    if !math::k_non_decreasing(reserve_in.msats, reserve_out.msats, reserve_in_new, reserve_out_new) {
        return Err(AmmOutputError::KInvariantViolated);
    }

    if in_is_lo {
        pool.reserve_lo = Amount::from_msats(reserve_in_new);
        pool.reserve_hi = Amount::from_msats(reserve_out_new);
    } else {
        pool.reserve_hi = Amount::from_msats(reserve_in_new);
        pool.reserve_lo = Amount::from_msats(reserve_out_new);
    }
    dbtx.insert_entry(&PoolKey(pool_id), &pool).await;

    let bkey = BalanceKey { owner: *recipient_pk, unit: *unit_out };
    let existing = dbtx.get_value(&bkey).await;
    let credited = existing.map_or(dy, |e| e.amount.msats.saturating_add(dy));
    dbtx.insert_entry(&bkey, &BalanceEntry {
        amount: Amount::from_msats(credited),
        tweak: *tweak,
    }).await;

    Ok(TransactionItemAmounts {
        amounts: Amounts::from_iter([(*unit_in, *amount_in)]),
        fees: Amounts::default(),
    })
}
```

Confirm `Amounts`' constructor name at `fedimint-core/src/module/mod.rs:114` and adapt.

- [ ] **Step 4: Implement `DepositV0`**

Load or default the pool (`total_shares == 0` for a fresh one), call `math::mint_shares`, check `to_owner >= min_shares` returning `SlippageExceeded` otherwise, write the `Pool` and insert the `LpPosition` with the supplied `tweak`, and return `amounts = {lo: amount_lo, hi: amount_hi}`. Validate both units are in `self.cfg.units` first.

- [ ] **Step 5: Run to verify all pass**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-server --test output
```

Expected: all eleven cases pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(amm): process_output for swaps and deposits"
```

---

## Task 8: `process_input` — `ClaimBalanceV0` and `WithdrawV0`

**Files:**
- Modify: `modules/amm/fedimint-amm-server/src/lib.rs`
- Create: `modules/amm/fedimint-amm-server/tests/input.rs`

- [ ] **Step 1: Write the failing tests**

```rust
// 1. ClaimBalanceV0 for a non-existent balance returns NoSuchBalance.
// 2. ClaimBalanceV0 returns amounts == {unit: stored}, pub_key == pubkey,
//    and DELETES the record (assert absent afterwards).
// 3. A second ClaimBalanceV0 for the same key now returns NoSuchBalance
//    — the retry-safety property from §6.1.
// 4. WithdrawV0 for an unknown pool returns NoSuchPool.
// 5. WithdrawV0 for an unknown position returns NoSuchPosition.
// 6. WithdrawV0 for more shares than held returns InsufficientShares.
// 7. WithdrawV0 of a partial position debits reserves and total_shares,
//    decrements the position, and KEEPS the record.
// 8. WithdrawV0 of the full position DELETES the record.
// 9. WithdrawV0 whose payout is below min_lo/min_hi returns SlippageExceeded
//    and writes nothing.
// 10. WithdrawV0 whose payout floors to zero on BOTH legs is rejected.
// 11. A leg that floors to zero is OMITTED from `amounts`, not present as an
//     explicit zero (spec P3 — Amounts never stores zero entries).
// 12. total_shares never reaches zero even after every assigned position is
//     withdrawn — MINIMUM_LIQUIDITY remains.
```

- [ ] **Step 2: Run to verify failure**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-server --test input
```

Expected: all fail with `UnknownVariant`.

- [ ] **Step 3: Implement `ClaimBalanceV0`**

```rust
AmmInput::ClaimBalanceV0 { pubkey, unit } => {
    let key = BalanceKey { owner: *pubkey, unit: *unit };
    let entry = dbtx.remove_entry(&key).await.ok_or(AmmInputError::NoSuchBalance)?;
    Ok(InputMeta {
        amount: TransactionItemAmounts {
            amounts: Amounts::from_iter([(*unit, entry.amount)]),
            fees: Amounts::default(),
        },
        pub_key: *pubkey,
    })
}
```

Full claim, no amount field: the record is removed unconditionally and its stored amount is what we report. One read, one binding, used once.

- [ ] **Step 4: Implement `WithdrawV0`**

Load the pool and position, call `math::burn_shares`, check `da >= min_lo && db >= min_hi` returning `SlippageExceeded`, write back the pool, decrement or delete the position, and build `amounts` **omitting any zero leg**:

```rust
let mut amounts = Amounts::default();
if outcome.da > 0 {
    amounts.insert(pool_id.lo(), Amount::from_msats(outcome.da));
}
if outcome.db > 0 {
    amounts.insert(pool_id.hi(), Amount::from_msats(outcome.db));
}
```

- [ ] **Step 5: Run and commit**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-server
nix develop --command cargo clippy -p fedimint-amm-server --all-targets -- -D warnings
git add -A
git commit -m "feat(amm): process_input for balance claims and LP withdrawals"
```

---

## Task 9: Audit and API endpoints

**Files:**
- Modify: `modules/amm/fedimint-amm-server/src/lib.rs`
- Create: `modules/amm/fedimint-amm-common/src/endpoints.rs`
- Create: `modules/amm/fedimint-amm-server/tests/audit.rs`

**Interfaces:**
- Produces, in `endpoints.rs`:
  - `pub const POOLS_ENDPOINT: &str = "amm_pools";`
  - `pub const QUOTE_ENDPOINT: &str = "amm_quote";`
  - `pub const BALANCE_RECOVERY_ENDPOINT: &str = "amm_balance_recovery";`
  - `pub const LP_RECOVERY_ENDPOINT: &str = "amm_lp_recovery";`
  - `pub struct PoolSummary { pub pool: PoolId, pub reserve_lo: Amount, pub reserve_hi: Amount, pub total_shares: u64, pub fee_per_mille: u16 }`
  - `pub struct QuoteRequest { pub unit_in: AmountUnit, pub unit_out: AmountUnit, pub amount_in: Amount }`
  - `pub struct QuoteResponse { pub amount_out: Amount, pub price_impact_per_mille: u64 }`
  - `pub struct BalanceRecoveryEntry { pub tweak: [u8; 16], pub pubkey: PublicKey, pub unit: AmountUnit, pub amount: Amount }`
  - `pub struct LpRecoveryEntry { pub tweak: [u8; 16], pub pool: PoolId, pub pubkey: PublicKey, pub shares: u64 }`

- [ ] **Step 1: Write the failing audit test**

```rust
// Spec §14 audit lifecycle test, module-local form:
// Drive first deposit -> second deposit at a shifted ratio -> swap A->B ->
// swap B->A -> partial withdraw -> full withdraw. After EVERY step, call
// `audit` and assert the module's summed liability equals
//   (total deposited) - (total withdrawn) - (total claimed) + (total swapped in)
// computed independently in the test, i.e. NOT by re-running module code.
// Repeat the whole sequence at a 1_000_000:1 ratio and at minimum viable
// reserves (just above MINIMUM_LIQUIDITY).
```

- [ ] **Step 2: Run to verify failure**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-server --test audit
```

Expected: fails — `audit` currently reports nothing, so the sum is 0.

- [ ] **Step 3: Implement `audit`**

Exactly as spec §9.1. Three `add_items` calls: `PoolPrefix` for `reserve_lo`, `PoolPrefix` for `reserve_hi`, `BalancePrefix` for `amount` — each negated, each its own item so no item mixes units. **Do not report `LpPosition`**; shares are a claim against reserves already counted and reporting them double-counts.

Use `i64::try_from(...).expect("bounded by MAX_RESERVE")` — justified because Task 7 rejects anything above `MAX_RESERVE = 2^58 < i64::MAX`. Never `unwrap_or(i64::MAX)`: `calculate_net_assets` sums with `checked_add` behind an `.expect`, so a saturating clamp converts a misreport into a federation-halting overflow.

- [ ] **Step 4: Implement the four endpoints**

Mirror `public_api_endpoint!` usage from `fedimint-mintv2-server/src/lib.rs:505-520`. `QUOTE_ENDPOINT` must call `math::amount_out` — the same function the server settles with — so a quote can never disagree with settlement.

- [ ] **Step 5: Run and commit**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-server
git add -A
git commit -m "feat(amm): audit reporting and public API endpoints"
```

---

## Task 10: Client key derivation

**Files:**
- Create: `modules/amm/fedimint-amm-client/src/derivation.rs`
- Modify: `modules/amm/fedimint-amm-client/Cargo.toml` (`fedimint-derive-secret`, `bitcoin_hashes`, `rand`)

**Interfaces:**
- Produces:
  - `pub const CHILD_SWAP: u64 = 0;` / `pub const CHILD_LP: u64 = 1;`
  - `pub fn tweak_filter(root: &DerivableSecret) -> [u8; 32]`
  - `pub fn check_tweak(tweak: [u8; 16], filter: [u8; 32]) -> bool`
  - `pub fn grind_tweak(root: &DerivableSecret) -> [u8; 16]`
  - `pub fn derive_keypair(root: &DerivableSecret, child: u64, tweak: [u8; 16]) -> Keypair`

**Reference:** `modules/fedimint-mintv2-client/src/issuance.rs:50-93` — port those functions, do not depend on that crate (spec §8.4).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn ground_tweaks_pass_their_own_filter() {
    let root = test_root_secret();
    let filter = tweak_filter(&root);
    for _ in 0..8 {
        assert!(check_tweak(grind_tweak(&root), filter));
    }
}

#[test]
fn a_different_seed_rejects_the_tweak_almost_always() {
    let a = test_root_secret_from(&[1u8; 32]);
    let b = test_root_secret_from(&[2u8; 32]);
    let filter_b = tweak_filter(&b);
    let hits = (0..200).filter(|_| check_tweak(grind_tweak(&a), filter_b)).count();
    // ~1/65536 chance each; 200 trials should essentially never hit.
    assert!(hits <= 1, "filter is not seed-specific");
}

#[test]
fn derivation_is_deterministic_in_the_tweak() {
    let root = test_root_secret();
    let t = [9u8; 16];
    assert_eq!(
        derive_keypair(&root, CHILD_SWAP, t).public_key(),
        derive_keypair(&root, CHILD_SWAP, t).public_key()
    );
}

#[test]
fn child_ids_are_namespaced() {
    let root = test_root_secret();
    let t = [9u8; 16];
    assert_ne!(
        derive_keypair(&root, CHILD_SWAP, t).public_key(),
        derive_keypair(&root, CHILD_LP, t).public_key()
    );
}

/// The property that motivates random tweaks over a counter (spec §8):
/// two clients on the SAME seed must never collide.
#[test]
fn concurrent_clients_on_one_seed_do_not_collide() {
    let root = test_root_secret();
    let keys: std::collections::BTreeSet<_> = (0..100)
        .map(|_| derive_keypair(&root, CHILD_SWAP, grind_tweak(&root)).public_key())
        .collect();
    assert_eq!(keys.len(), 100);
}
```

- [ ] **Step 2: Run to verify failure, then implement**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-client derivation
```

Port from `issuance.rs`:

```rust
pub fn tweak_filter(root: &DerivableSecret) -> [u8; 32] {
    root.to_random_bytes()
}

pub fn check_tweak(tweak: [u8; 16], filter: [u8; 32]) -> bool {
    (tweak, filter)
        .consensus_hash::<sha256::Hash>()
        .to_byte_array()
        .iter()
        .take(2)
        .all(|b| *b == 0)
}

pub fn grind_tweak(root: &DerivableSecret) -> [u8; 16] {
    let filter = tweak_filter(root);
    loop {
        let tweak = rand::thread_rng().r#gen();
        if check_tweak(tweak, filter) {
            return tweak;
        }
    }
}

pub fn derive_keypair(root: &DerivableSecret, child: u64, tweak: [u8; 16]) -> Keypair {
    root.child_key(ChildId(child)).tweak(&tweak).to_secp_key(SECP256K1)
}
```

- [ ] **Step 3: Run and commit**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-client
git add -A
git commit -m "feat(amm): recovery-safe client key derivation"
```

---

## Task 11: Client module, quotes and the two-transaction swap

**Files:**
- Modify: `modules/amm/fedimint-amm-client/src/lib.rs`
- Create: `modules/amm/fedimint-amm-client/src/swap.rs`
- Create: `modules/amm/fedimint-amm-client/src/db.rs`

**Reference:** `modules/fedimint-dummy-client/src/lib.rs` for `ClientModule` boilerplate; `modules/fedimint-lnv2-client/src/` for a real multi-step state machine.

**Interfaces:**
- Produces: `AmmClientModule` with `swap`, `deposit`, `withdraw`, `quote`, `pools`, `recover`.

- [ ] **Step 1: Implement `ClientModule` and read-only operations**

`quote(unit_in, unit_out, amount_in)` calls `QUOTE_ENDPOINT`; `pools()` calls `POOLS_ENDPOINT`. Declare no primary-module support — this module never funds transactions itself (spec P10).

- [ ] **Step 2: Write the swap state machine**

States: `Tx1Submitted { operation_id, tweak, recipient_pk, unit_out }` → `Tx1Accepted` → `Tx2Submitted` → `Done`; terminal `Tx1Rejected`, `Tx2Rejected`.

Critical behaviours, each of which needs a test:
- `min_out` is computed from a **fresh** quote taken immediately before building Tx1, never a cached one.
- `recipient_pk` comes from `grind_tweak` + `derive_keypair(CHILD_SWAP, …)`, fresh per swap.
- On transition into `Tx1Accepted`, the client **re-reads the balance** via `BALANCE_RECOVERY_ENDPOINT` (or a direct balance query) before building Tx2, so a gift credited in the interim is captured rather than forfeited (spec §6.1).
- There is **no timeout and no refund path.** A crash between Tx1 and Tx2 is resumed on restart, never unwound, because the balance is permanently claimable.

- [ ] **Step 3: Implement `deposit` and `withdraw`**

Single-transaction operations: `Submitted → Accepted | Rejected`. Both grind a fresh tweak for `owner_pk`.

- [ ] **Step 4: Implement `recover`**

Scan `BALANCE_RECOVERY_ENDPOINT` and `LP_RECOVERY_ENDPOINT`; for each entry run `check_tweak` first and only derive on a hit; if the derived pubkey matches, restore the balance or position. Claim any recovered balance.

- [ ] **Step 5: Run and commit**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-client
nix develop --command cargo clippy -p fedimint-amm-client --all-targets -- -D warnings
git add -A
git commit -m "feat(amm): client module, quotes, swap state machine and recovery"
```

---

## Task 12: Integration tests

**Files:**
- Create: `modules/amm/fedimint-amm-tests/tests/integration.rs`

**Reference:** `modules/fedimint-lnv2-tests/tests/` for the `fedimint-testing` fixture setup with two module instances.

- [ ] **Step 1: Write the tests**

```
1. Two mintv2 instances (units 0 and 1) plus one amm instance start up.
2. Deposit creates a pool; POOLS_ENDPOINT reflects it.
3. A full swap round trip: Tx1 accepted, balance appears, Tx2 accepted,
   notes reissued, wallet balance in unit 1 increased by exactly dy.
4. A quote taken before Tx1 matches the dy actually settled.
5. min_out violation: Tx1 is rejected and the wallet is unchanged.
6. A claim for a non-existent balance is rejected.
7. Withdraw returns both legs as spendable notes.
8. OVERPAY DEPENDENCY (spec §6.1): credit the recipient's balance from a
   second client between Tx1 and Tx2, then submit a Tx2 built for the
   original dy. Assert it SUCCEEDS and forfeits the surplus. This test is
   the tripwire for CORE_CONSENSUS_VERSION dropping below 2.1.
9. RECOVERY: perform a swap and a deposit, wipe client state, recover from
   the seed alone, assert both the balance and the LP position are found.
10. CONCURRENT SEED: two clients on one seed each swap; assert both
    succeed and neither key collides.
11. MULTI-HOP: units 0 -> 1 -> 2 across three mintv2 instances in three
    transactions, entirely client-side.
```

- [ ] **Step 2: Run**

```bash
git add -A && nix develop --command cargo test -p fedimint-amm-tests
```

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "test(amm): devimint integration tests"
```

---

## Self-Review Notes

**Spec coverage.** §5 → Tasks 2, 5. §6 → Tasks 4, 7, 8. §7 → Task 1. §8 → Tasks 10, 11. §9 → Task 9. §10 (no consensus items) → Task 6. §11 → Task 3. §12 → Task 11. §13 threats → guards land in Tasks 7, 8. §14 → tests throughout, integration in Task 12.

**Deliberately deferred to follow-up work, not silently dropped:**
- `verify_input_submission` / `verify_output_submission` (spec §6.4) — an optimisation, not a correctness requirement, since consensus re-checks everything. Add after Task 12.
- `get_database_migrations` — no migration exists at consensus version `0.0`; the first shape change adds one.
- A guardian UI surface for realised fee income vs. reserve drift (spec §13 LVR row).

**Known plan risk.** Tasks 6, 11 and 12 depend on fedimint trait surfaces this plan does not reproduce verbatim, because they are large and version-specific. Each of those tasks instructs the implementer to read the corresponding `fedimint-dummy-*` or `fedimint-lnv2-*` file on the pinned rev first. If a trait signature differs from what a code block here assumes, **the pinned source wins** — adapt the surrounding code and note it, do not change the pin.
