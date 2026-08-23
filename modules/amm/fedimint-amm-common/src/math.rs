//! Constant-product AMM arithmetic — a mechanical translation of Uniswap V2.
//!
//! See `modules/amm/fedimint-amm-spec.md` §7. Every intermediate is `u128`;
//! there is no floating point anywhere in this file.

use thiserror::Error;

/// Upper bound on any reserve or swap input.
///
/// The largest intermediate below is `997 * amount_in * reserve_out`. Since
/// `997 < 2^10`, safety needs `amount_in * reserve_out < 2^118`; capping both
/// at `2^58` yields `997 * 2^116 < 2^126`, two bits of headroom. `2^58` msats
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
    // Final-review finding T7: this branch is provably UNREACHABLE, kept
    // as defence in depth rather than deleted. `denominator = reserve_in *
    // 1000 + in_with_fee`, and `reserve_in >= 1` is already guaranteed by
    // the `ZeroReserve` guard above, so `denominator > in_with_fee`
    // strictly. Therefore `out = numerator / denominator = (in_with_fee *
    // reserve_out) / denominator < (in_with_fee * reserve_out) /
    // in_with_fee = reserve_out` always — `out >= reserve_out` would
    // require `denominator <= in_with_fee`, i.e. `reserve_in * 1000 <= 0`,
    // impossible for `reserve_in >= 1`. `amount_out_never_drains_the_pool`
    // below pins the guaranteed conclusion (`out < reserve_out`), not
    // coverage of this branch — it cannot be exercised by any input.
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

/// How much worse a swap's effective price (`amount_out / amount_in`) is
/// than the pool's spot price (`reserve_out / reserve_in`) before the swap,
/// in per-mille. Used only to annotate `QUOTE_ENDPOINT` (spec §12) — not
/// part of consensus — but takes the exact `reserve_in`/`reserve_out`/
/// `amount_in` fed to [`amount_out`] plus its result, so the two numbers can
/// never tell a different story.
///
/// `ratio = (amount_out * reserve_in) / (amount_in * reserve_out)` is the
/// effective price divided by the spot price; `1000 * (1 - ratio)` is the
/// impact in per-mille. Every input is `<= MAX_RESERVE` (2^58), so the
/// largest intermediate — `1000 * amount_out * reserve_in` — is
/// `< 2^10 * 2^58 * 2^58 = 2^126`, comfortably inside `u128`.
pub fn price_impact_per_mille(
    reserve_in: u64,
    reserve_out: u64,
    amount_in: u64,
    amount_out: u64,
) -> u64 {
    let denominator = u128::from(amount_in) * u128::from(reserve_out);
    if denominator == 0 {
        // Degenerate input (would already have been rejected by
        // `amount_out` above); report "no impact" rather than divide by
        // zero.
        return 0;
    }
    let numerator = 1000u128 * u128::from(amount_out) * u128::from(reserve_in);
    // Floors, so this is always <= 1000 for any swap that actually got worse
    // (the normal case): `saturating_sub` only guards against rounding
    // pushing a near-zero-impact swap's ratio a hair above 1000.
    let ratio_per_mille = u64::try_from(numerator / denominator).unwrap_or(u64::MAX);
    1000u64.saturating_sub(ratio_per_mille)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from spec §Step 2 / the Uniswap V2 formula.
    #[test]
    fn amount_out_matches_reference_vector() {
        // reserve_in 1_000_000_000, reserve_out 1_000_000, in 10_000_000, fee 3/1000
        assert_eq!(
            amount_out(1_000_000_000, 1_000_000, 10_000_000, 3),
            Ok(9_871)
        );
    }

    #[test]
    fn amount_out_rejects_degenerate_inputs() {
        assert_eq!(amount_out(1_000, 1_000, 0, 3), Err(CurveError::ZeroAmount));
        assert_eq!(amount_out(0, 1_000, 10, 3), Err(CurveError::ZeroReserve));
        assert_eq!(amount_out(1_000, 0, 10, 3), Err(CurveError::ZeroReserve));
        assert_eq!(
            amount_out(1_000, 1_000, 10, 1_000),
            Err(CurveError::InvalidFee)
        );
        assert_eq!(
            amount_out(MAX_RESERVE + 1, 1_000, 10, 3),
            Err(CurveError::ReserveCapExceeded)
        );
        assert_eq!(
            amount_out(1_000, 1_000, MAX_RESERVE + 1, 3),
            Err(CurveError::ReserveCapExceeded)
        );
        // Final-review finding T4: `reserve_out > MAX_RESERVE` is the one
        // of the three `MAX_RESERVE` operands with no test above it
        // (`reserve_in` and `amount_in` are both covered right above) —
        // and `reserve_out` is exactly the factor spec §7.1's `997 *
        // amount_in * reserve_out < 2^126` proof depends on.
        assert_eq!(
            amount_out(1_000, MAX_RESERVE + 1, 10, 3),
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

    /// Even an enormous input leaves at least one unit behind. NOT a test
    /// of the `InsufficientLiquidity` branch (`out >= reserve_out`) in
    /// `amount_out` — that branch is unreachable by construction (see its
    /// doc comment, finding T7) and this assertion holds regardless of
    /// whether that guard exists at all; it pins the mathematical
    /// guarantee itself (`out < reserve_out` always), not coverage of any
    /// particular line that enforces it.
    #[test]
    fn amount_out_never_drains_the_pool() {
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

    /// Final-review finding T3: the later-mint (`total_shares != 0`) branch's
    /// `minted == 0` guard has no test — the existing proptest
    /// (`deposit_withdraw_never_profits`) generates `da, db >= 1_000`
    /// against reserves `>= 10^6`, never small enough to floor. Without
    /// this guard, a too-small deposit is silently ACCEPTED with
    /// `to_owner = 0`: reserves still grow by the full `da`/`db`, and a
    /// 0-share `LpPosition` is written — the depositor's funds donated to
    /// existing LPs with no error. `via_lo = floor(da * total_shares /
    /// reserve_lo) = floor(1 * 1 / 1_000_000) = 0` here, so `minted =
    /// min(via_lo, via_hi) = 0` regardless of `via_hi`.
    #[test]
    fn later_mint_rejects_a_deposit_too_small_to_mint_any_shares() {
        assert_eq!(
            mint_shares(1_000_000, 1_000_000, 1, 1, 1_000_000),
            Err(CurveError::OutputRoundsToZero)
        );
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

    /// Same worked example as `amount_out_matches_reference_vector`: 1%
    /// swap of a 1000:1 pool at the reference 0.30% fee, expect ~1.3%
    /// impact (fee + slippage).
    #[test]
    fn price_impact_matches_worked_example() {
        let out = amount_out(1_000_000_000, 1_000_000, 10_000_000, 3).unwrap();
        assert_eq!(out, 9_871);
        assert_eq!(
            price_impact_per_mille(1_000_000_000, 1_000_000, 10_000_000, out),
            13
        );
    }

    #[test]
    fn price_impact_matches_a_hand_computed_exact_case() {
        // fee = 0, and chosen so `amount_out`'s division is exact (no floor
        // loss to muddy the check): out = 1 * 12 / (3 + 1) = 3.
        let out = amount_out(3, 12, 1, 0).unwrap();
        assert_eq!(out, 3);
        // ratio = (out * r_in) / (amt * r_out) = (3 * 3) / (1 * 12) = 0.75
        // impact = 1000 * (1 - 0.75) = 250
        assert_eq!(price_impact_per_mille(3, 12, 1, out), 250);
    }

    proptest::proptest! {
        /// Renamed from `price_impact_is_bounded` (finding M6): that name
        /// claimed to check `impact <= 1000`, but the function's last line is
        /// `1000u64.saturating_sub(..)`, which makes `impact <= 1000` true by
        /// construction for every possible implementation — the assertion
        /// could never fail. What this test actually has value for is fuzzing
        /// the internal `u128` arithmetic across the full input space for a
        /// panic (overflow, in debug builds).
        ///
        /// A prior version of this file also carried
        /// `price_impact_per_mille_is_monotonic_in_amount_in`, asserting that
        /// a larger `amount_in` against the same pool never reports LESS
        /// price impact, gated by an `amount_out >= 10_000` guard claimed to
        /// scope the property to a regime where it held. Re-review found
        /// that claim false, with a fabricated counterexample (the comment's
        /// own cited case, `reserve_in = reserve_out = 1000`, `fee = 3`,
        /// `amount_in` 100 vs 101, does not reproduce: both report impact
        /// 100). Removed rather than re-guarded, because the property is
        /// false above the guard too — reproduced directly against this
        /// file's own `amount_out`/`price_impact_per_mille`
        /// (`nix develop --command rustc`, not just asserted here):
        ///
        /// `reserve_in = 5_653_265`, `reserve_out = 4_045_128`,
        /// `fee_per_mille = 638`: `amount_in = 42_093` → `amount_out =
        /// 10_873`, impact 640; `amount_in = 42_449` (a LARGER trade) →
        /// `amount_out = 10_965`, impact 639 — a real decrease, with
        /// `amount_out` well into five digits on both sides, so no
        /// output-size threshold restores monotonicity in general. The
        /// underlying cause: `amount_out` floors once, and
        /// `price_impact_per_mille` floors the ratio of that already-floored
        /// value again; at mid-scale reserves this compounded floor error can
        /// exceed the genuine, monotonically-shrinking exact-rational
        /// improvement in effective price between two nearby `amount_in`
        /// values, flipping the reported (integer, twice-floored) impact the
        /// wrong way. No replacement monotonicity property is asserted here;
        /// none has been found to hold without a false caveat.
        #[test]
        fn price_impact_per_mille_never_panics_within_caps(
            r_in in 1u64..=MAX_RESERVE,
            r_out in 1u64..=MAX_RESERVE,
            amt in 1u64..=MAX_RESERVE,
            fee in 0u16..1000,
        ) {
            if let Ok(out) = amount_out(r_in, r_out, amt, fee) {
                let _ = price_impact_per_mille(r_in, r_out, amt, out);
            }
        }
    }

    /// Fix pass 2, Important 1: the violation cited in
    /// `price_impact_per_mille_never_panics_within_caps`'s doc comment,
    /// pinned as a regression test so it cannot silently start reproducing
    /// monotonicity again without this test being noticed and reconsidered.
    #[test]
    fn price_impact_per_mille_is_not_monotonic_above_the_former_guard() {
        let out_small = amount_out(5_653_265, 4_045_128, 42_093, 638).unwrap();
        let out_big = amount_out(5_653_265, 4_045_128, 42_449, 638).unwrap();
        assert_eq!(out_small, 10_873);
        assert_eq!(out_big, 10_965);

        let impact_small = price_impact_per_mille(5_653_265, 4_045_128, 42_093, out_small);
        let impact_big = price_impact_per_mille(5_653_265, 4_045_128, 42_449, out_big);
        assert_eq!(impact_small, 640);
        assert_eq!(impact_big, 639);
        assert!(
            impact_big < impact_small,
            "a larger amount_in reporting less impact is exactly the (real) violation this test pins"
        );
    }

    /// Final-review finding T2: `k_non_decreasing` itself is asserted by
    /// zero tests — the property test below only calls `amount_out` and
    /// checks the k identity directly, never `k_non_decreasing`, so it
    /// can't catch a bug localized to that function (e.g. `new >= old`
    /// weakened to `new > old`; a legitimate fee-0 exactly-dividing swap
    /// has `k_new == k_old` exactly — see `swap_with_fee_zero_and_exact_division_is_accepted`
    /// in `tests/output.rs`, which DOES exercise `k_non_decreasing` through
    /// real settlement and is what actually catches that mutation). This
    /// test instead hand-constructs a real decrease and checks
    /// `k_non_decreasing` reports it: not reachable through any real
    /// `amount_out` call (see `k_never_decreases` below, and
    /// `AmmOutputError::KInvariantViolated`'s doc comment in
    /// `fedimint-amm-common/src/types.rs` for why), but this proves the
    /// backstop function itself is correct in isolation, independent of
    /// whether real settlement can ever reach it.
    #[test]
    fn k_non_decreasing_rejects_a_manufactured_decrease() {
        // k: 1_000 * 1_000 = 1_000_000 -> 500 * 500 = 250_000. No real swap
        // produces this pair (amount_out always yields r_in_new * r_out_new
        // >= r_in_old * r_out_old — see k_never_decreases); constructed by
        // hand purely to give k_non_decreasing something to reject.
        assert!(!k_non_decreasing(1_000, 1_000, 500, 500));
        // And the boundary it must NOT reject: equal k.
        assert!(k_non_decreasing(1_000, 1_000, 2_000, 500));
    }

    proptest::proptest! {
        /// Spec §14: k is non-decreasing under any swap. `k_old`/`k_new`
        /// are computed independently here (direct `u128` multiplication),
        /// never by calling `k_non_decreasing` — the function under
        /// backstop, not the oracle for its own test (finding T2: the
        /// prior version of this test asserted `k_non_decreasing(...)`
        /// directly, making the function its own oracle, so no mutation of
        /// `k_non_decreasing` itself could ever fail it). Widened from
        /// `2^40` to the full `MAX_RESERVE` domain, and `fee_per_mille` now
        /// ranges over every legal value including 0 — `validate()` only
        /// rejects `fee >= 1000`, so `fee_per_mille == 0` is a legal
        /// config, and it is the regime where `k_new == k_old` can hold
        /// EXACTLY on an exactly-dividing swap (the prior version pinned
        /// `fee` at 3, where that boundary is never hit).
        #[test]
        fn k_never_decreases(
            r_in in 1u64..=MAX_RESERVE,
            r_out in 1u64..=MAX_RESERVE,
            amt in 1u64..=MAX_RESERVE,
            fee in 0u16..1000,
        ) {
            if let Ok(out) = amount_out(r_in, r_out, amt, fee) {
                let r_in_new = r_in + amt;
                let r_out_new = r_out - out;
                let k_old = u128::from(r_in) * u128::from(r_out);
                let k_new = u128::from(r_in_new) * u128::from(r_out_new);
                proptest::prop_assert!(k_new >= k_old);
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
        /// Only exercises the LATER-mint branch (`total_shares != 0`) —
        /// `shares` never generates 0; see
        /// `deposit_withdraw_never_profits_on_pool_creation` below for the
        /// creation branch (finding T5).
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

        /// Final-review finding T5: `deposit_withdraw_never_profits` above
        /// never generates `total_shares == 0`, so the pool-creation branch
        /// — `isqrt` plus the `MINIMUM_LIQUIDITY` burn, i.e. the
        /// first-depositor-inflation defence — was never property-tested
        /// against this "never profits" invariant. Mirrors the test above
        /// exactly, but starting from an empty pool (`total_shares == 0`,
        /// reserves `0`): the freshly created pool's reserves become
        /// exactly `(da, db)` (spec §7.2), and the immediate withdraw of
        /// everything actually credited (`m.to_owner` — deliberately NOT
        /// `m.new_total_shares`, since `MINIMUM_LIQUIDITY` is unassigned
        /// and unwithdrawable forever) must still return no more than was
        /// put in.
        #[test]
        fn deposit_withdraw_never_profits_on_pool_creation(
            da in 1u64..=(1 << 30),
            db in 1u64..=(1 << 30),
        ) {
            if let Ok(m) = mint_shares(0, 0, 0, da, db) {
                let b = burn_shares(da, db, m.new_total_shares, m.to_owner);
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

        /// Final-review finding T6: `isqrt` (`u128::isqrt`, used by
        /// `mint_shares`'s pool-creation branch) had no test of its own —
        /// spec §14 requires "isqrt agrees with a reference across a u128
        /// sample". The reference here is the algebraic definition of an
        /// integer square root: `r = isqrt(n)` iff `r * r <= n < (r+1) *
        /// (r+1)`. Sampled as `da * db` (rather than an unconstrained
        /// `u128`) so every value stays in the exact domain `mint_shares`
        /// actually calls `isqrt` on, and `(r+1)^2` cannot overflow `u128`:
        /// `da, db <= MAX_RESERVE == 2^58`, so `n <= 2^116` and `r <=
        /// 2^58`, comfortably inside `u128`.
        #[test]
        fn isqrt_agrees_with_the_algebraic_definition(
            da in 0u64..=MAX_RESERVE,
            db in 0u64..=MAX_RESERVE,
        ) {
            let n = u128::from(da) * u128::from(db);
            let r = n.isqrt();
            proptest::prop_assert!(r * r <= n);
            proptest::prop_assert!(n < (r + 1) * (r + 1));
        }
    }
}
