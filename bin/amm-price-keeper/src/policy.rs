//! The whole decision, as one pure function (design §5).
//!
//! [`decide`] performs no I/O, holds no state, and knows nothing about
//! fedimint's client: it takes reserves, a fee, the two `min_swap_in`
//! thresholds, the oracle price, the bot's balances and its configuration, and
//! returns a [`Decision`]. Every rule in design §4 lives here, and every test
//! in design §7 points here.
//!
//! # No floating point
//!
//! The oracle price arrives as `p_micro` — micro-USD per BTC, already an
//! integer (see [`crate::oracle`]). With `R_lo` msats and `R_hi` micros, the
//! pool's spot price is `(R_hi / R_lo) * 10^5` USD per BTC, so
//!
//! ```text
//! P_pool >= P_oracle   <=>   R_hi * 10^11  >=  R_lo * p_micro
//! ```
//!
//! and that cross-multiplication — [`DECIMAL_GAP`] is the `10^11` — is the
//! only form the comparison ever takes, in [`dev_bps`], in the binary search's
//! overshoot test, and in the property test that checks them against each
//! other. There is no float anywhere in this file.
//!
//! # Sizing is a binary search over the module's own curve
//!
//! The closed form for "the `dx` that lands the post-trade price exactly on
//! the oracle" is a quadratic, and it is deliberately not used (design §4.2):
//! it needs an integer square root to stay off floats, and it would be a
//! second copy of the curve that could drift from settlement without any test
//! noticing. Instead [`largest_non_crossing`] binary-searches `dx` calling
//! [`math::amount_out`] — the exact function consensus settles with — and
//! keeps the largest `dx` that does not carry the price past the oracle.

use fedimint_amm_common::math::{self, CurveError, MAX_RESERVE};
use fedimint_core::module::AmountUnit;

/// `10^11 = 10^(11-6)` (the msat/micro decimal gap) times `10^6` (the
/// micro-USD the oracle price is denominated in). See the module docs.
pub const DECIMAL_GAP: u128 = 100_000_000_000;

/// One basis point in the ratio the deviation is measured in.
const BPS: u128 = 10_000;

/// The pool as of this tick, plus the units it is over.
///
/// `unit_lo` must be [`AmountUnit::BITCOIN`] for any of this to mean anything
/// — see [`crate::main`]'s startup assertion — because [`DECIMAL_GAP`] encodes
/// "unit lo is msats, unit hi is micros" and nothing in the pool id says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    pub unit_lo: AmountUnit,
    pub unit_hi: AmountUnit,
    /// Bitcoin reserve, msats.
    pub reserve_lo: u64,
    /// USDt reserve, micros.
    pub reserve_hi: u64,
    /// The effective, guardian-voted fee, read from `PoolSummary` every tick
    /// (design §4.2) — never from config, so a fee vote mid-flight sizes
    /// correctly.
    pub fee_per_mille: u16,
}

/// The AMM's own per-unit anti-dust thresholds, from its client config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinSwapIn {
    pub lo: u64,
    pub hi: u64,
}

/// What the bot can actually spend, in each unit's base denomination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balances {
    pub lo: u64,
    pub hi: u64,
}

/// The operator's knobs (design §8), reduced to integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeeperConfig {
    /// Deadband half-width. Must exceed the fee — see
    /// [`check_band_exceeds_fee`].
    pub band_bps: u64,
    /// `--max-trade-usd`, in micro-USD.
    pub max_trade_micro_usd: u128,
    pub btc_floor_msat: u64,
    pub usdt_floor_micros: u64,
}

/// Which way the correction goes. Selling BTC into the pool raises `R_lo` and
/// lowers `R_hi`, so `P_pool` falls; buying does the reverse (design §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The pool is overvaluing BTC: swap unit lo -> unit hi, spending msats.
    SellBtc,
    /// The pool is undervaluing BTC: swap unit hi -> unit lo, spending micros.
    BuyBtc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Hold(HoldReason),
    Trade {
        unit_in: AmountUnit,
        /// Derived from `unit_in` and the pool, and carried here so that the
        /// direction is decided in exactly one place.
        unit_out: AmountUnit,
        amount_in: u64,
    },
}

/// Why a tick did not trade. Every variant is a log line; the ones
/// [`HoldReason::is_warning`] returns `true` for are the ones an operator
/// needs to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// No pool, or a pool with nothing in it. Design §2: a hold with a
    /// warning, not a crash.
    EmptyPool,
    /// One side is empty. There is no price and no viable swap.
    ZeroReserve { reserve_lo: u64, reserve_hi: u64 },
    /// The oracle price or the reserves make the deviation uncomputable.
    /// Unreachable for any price [`crate::oracle`] accepts; kept because the
    /// alternative to a branch here is a panic in a bot that must not have
    /// any.
    Unpriceable,
    /// Design §4.4: at `fee_per_mille = 3` a round trip burns ~60 bps, so a
    /// band under `fee * 10` means oracle noise alone pays fees to move the
    /// price nowhere. Refused at startup; if a fee vote lands mid-flight and
    /// narrows the margin, every later tick holds here instead of trading.
    BandBelowFee { band_bps: u64, fee_per_mille: u16 },
    /// Inside the deadband. The normal outcome.
    InBand { dev_bps: i128 },
    /// Outside the band, but no swap of any size moves the price toward the
    /// oracle without immediately carrying it past — the correction is
    /// smaller than one unit of the input.
    NoSizeAvailable { dev_bps: i128 },
    /// The clamps (design §4.3) left less than this unit's `min_swap_in`.
    /// **This is the "side is exhausted" signal**, and it is expected to fire
    /// eventually.
    BelowMinSwapIn {
        unit_in: AmountUnit,
        amount_in: u64,
        min_swap_in: u64,
    },
    /// The survivor of the clamps is large enough for the AMM's dust rule but
    /// would still settle to a zero output, which the module rejects. Checked
    /// against `math::amount_out` itself rather than reasoned about.
    OutputRoundsToZero { unit_in: AmountUnit, amount_in: u64 },
}

impl HoldReason {
    /// Whether this hold should be logged at WARN rather than INFO.
    #[must_use]
    pub fn is_warning(self) -> bool {
        match self {
            HoldReason::EmptyPool
            | HoldReason::ZeroReserve { .. }
            | HoldReason::Unpriceable
            | HoldReason::BandBelowFee { .. }
            | HoldReason::BelowMinSwapIn { .. }
            | HoldReason::OutputRoundsToZero { .. } => true,
            HoldReason::InBand { .. } | HoldReason::NoSizeAvailable { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "band of {band_bps} bps does not exceed the pool's {fee_per_mille} per-mille fee \
     ({} bps); a band under the fee pays fees to move the price nowhere",
    u64::from(*fee_per_mille) * 10
)]
pub struct BandTooNarrow {
    pub band_bps: u64,
    pub fee_per_mille: u16,
}

/// Design §4.4. The bot refuses to start when this fails, reading the fee from
/// the live pool.
pub fn check_band_exceeds_fee(band_bps: u64, fee_per_mille: u16) -> Result<(), BandTooNarrow> {
    if band_bps < u64::from(fee_per_mille) * 10 {
        return Err(BandTooNarrow {
            band_bps,
            fee_per_mille,
        });
    }
    Ok(())
}

/// The pool's spot price in micro-USD per BTC, for logging. `None` for an
/// empty bitcoin reserve.
#[must_use]
pub fn pool_price_micro_usd(reserve_lo: u64, reserve_hi: u64) -> Option<u128> {
    if reserve_lo == 0 {
        return None;
    }
    Some(u128::from(reserve_hi) * DECIMAL_GAP / u128::from(reserve_lo))
}

/// How far the pool's price sits above (positive) or below (negative) the
/// oracle, in basis points, by design §3's exact cross-multiplication.
///
/// `None` only if a reserve or the price is zero, or the products leave
/// `i128` — neither reachable for a pool inside `MAX_RESERVE` at a price
/// [`crate::oracle`] accepts.
#[must_use]
pub fn dev_bps(reserve_lo: u64, reserve_hi: u64, oracle_micro: u128) -> Option<i128> {
    let oracle_side = u128::from(reserve_lo).checked_mul(oracle_micro)?;
    if oracle_side == 0 {
        return None;
    }
    let pool_side = u128::from(reserve_hi).checked_mul(DECIMAL_GAP)?;

    let numerator = i128::try_from(pool_side).ok()? - i128::try_from(oracle_side).ok()?;
    let denominator = i128::try_from(oracle_side).ok()?;

    // `saturating_mul`, not `checked_mul`: the product only overflows for a
    // deviation of order 1e34 basis points, which is not a number the caller
    // can act on differently from "enormous". Saturation preserves the sign,
    // which is the only property the direction rule depends on.
    Some(numerator.saturating_mul(i128::try_from(BPS).expect("10_000 fits in i128")) / denominator)
}

/// The whole policy (design §4), as one pure function.
#[must_use]
pub fn decide(
    pool: &PoolState,
    min_swap_in: MinSwapIn,
    oracle_micro: u128,
    balances: Balances,
    cfg: &KeeperConfig,
) -> Decision {
    if pool.reserve_lo == 0 && pool.reserve_hi == 0 {
        return Decision::Hold(HoldReason::EmptyPool);
    }
    if pool.reserve_lo == 0 || pool.reserve_hi == 0 {
        return Decision::Hold(HoldReason::ZeroReserve {
            reserve_lo: pool.reserve_lo,
            reserve_hi: pool.reserve_hi,
        });
    }
    if let Err(error) = check_band_exceeds_fee(cfg.band_bps, pool.fee_per_mille) {
        return Decision::Hold(HoldReason::BandBelowFee {
            band_bps: error.band_bps,
            fee_per_mille: error.fee_per_mille,
        });
    }

    let Some(deviation) = dev_bps(pool.reserve_lo, pool.reserve_hi, oracle_micro) else {
        return Decision::Hold(HoldReason::Unpriceable);
    };

    // Compared rather than `abs()`-ed: `i128::MIN.abs()` panics, and
    // `dev_bps` saturates.
    let band = i128::from(cfg.band_bps);
    let side = if deviation > band {
        Side::SellBtc
    } else if deviation < -band {
        Side::BuyBtc
    } else {
        return Decision::Hold(HoldReason::InBand { dev_bps: deviation });
    };

    let (unit_in, unit_out) = match side {
        Side::SellBtc => (pool.unit_lo, pool.unit_hi),
        Side::BuyBtc => (pool.unit_hi, pool.unit_lo),
    };
    let (reserve_in, reserve_out) = reserves_for(side, pool);
    let (floor, balance, min_in) = match side {
        Side::SellBtc => (cfg.btc_floor_msat, balances.lo, min_swap_in.lo),
        Side::BuyBtc => (cfg.usdt_floor_micros, balances.hi, min_swap_in.hi),
    };

    // Clamp 1 (design §4.3): overshoot.
    let mut amount_in = largest_non_crossing(side, pool, oracle_micro);
    if amount_in == 0 {
        return Decision::Hold(HoldReason::NoSizeAvailable { dev_bps: deviation });
    }

    // Clamp 2: the per-tick size cap, converted to the input unit at the
    // oracle price.
    amount_in = amount_in.min(max_trade_cap(side, cfg.max_trade_micro_usd, oracle_micro));

    // Clamp 3: never spend below the inventory floor.
    amount_in = amount_in.min(balance.saturating_sub(floor));

    // Clamp 4: dust. A clamped trade is still a partial correction, but one
    // the module would reject is not a trade at all.
    if amount_in < min_in {
        return Decision::Hold(HoldReason::BelowMinSwapIn {
            unit_in,
            amount_in,
            min_swap_in: min_in,
        });
    }

    // `min_swap_in` is a config threshold, not a guarantee that the curve
    // produces an output: ask the settlement function itself.
    if math::amount_out(reserve_in, reserve_out, amount_in, pool.fee_per_mille).is_err() {
        return Decision::Hold(HoldReason::OutputRoundsToZero { unit_in, amount_in });
    }

    Decision::Trade {
        unit_in,
        unit_out,
        amount_in,
    }
}

/// `(reserve_in, reserve_out)` for a side.
fn reserves_for(side: Side, pool: &PoolState) -> (u64, u64) {
    match side {
        Side::SellBtc => (pool.reserve_lo, pool.reserve_hi),
        Side::BuyBtc => (pool.reserve_hi, pool.reserve_lo),
    }
}

/// `--max-trade-usd` in the input unit, at the oracle price (design §4.3.2).
///
/// Buying BTC spends micros of USDt, and design §2 assumes 1 USDt = 1 USD, so
/// the cap is the micro-USD figure unchanged. Selling BTC spends msats, and 1
/// msat is `p_micro * 1e-11` micro-USD, so `M` micro-USD is
/// `M * 10^11 / p_micro` msats.
fn max_trade_cap(side: Side, max_trade_micro_usd: u128, oracle_micro: u128) -> u64 {
    let cap = match side {
        Side::BuyBtc => max_trade_micro_usd,
        Side::SellBtc => {
            if oracle_micro == 0 {
                return 0;
            }
            max_trade_micro_usd.saturating_mul(DECIMAL_GAP) / oracle_micro
        }
    };
    u64::try_from(cap).unwrap_or(u64::MAX)
}

/// The largest `dx` whose post-trade price has not passed the oracle price
/// (design §4.2).
///
/// [`crosses_target`] is monotone in `dx` — false for a prefix, true after —
/// which is what makes this binary search sound; see that function for why the
/// two error classes fall on the sides they do. Roughly 64 iterations.
#[must_use]
pub fn largest_non_crossing(side: Side, pool: &PoolState, oracle_micro: u128) -> u64 {
    let (reserve_in, _) = reserves_for(side, pool);

    let mut low = 1u64;
    // The module rejects any `amount_in` above `MAX_RESERVE`, and a reserve
    // may not grow past it either, so nothing above this is ever executable.
    let mut high = MAX_RESERVE.saturating_sub(reserve_in);
    let mut best = 0u64;

    while low <= high {
        let mid = low + (high - low) / 2;
        if crosses_target(side, pool, mid, oracle_micro) {
            // `mid >= low >= 1`, so this cannot underflow.
            high = mid - 1;
        } else {
            best = mid;
            low = mid + 1;
        }
    }

    best
}

/// Whether swapping `dx` carries the pool's price *past* the oracle.
///
/// Landing exactly on the oracle price is not crossing (design §4.2: "stop
/// while `P'(dx) >= P_oracle`" when selling), which is what makes the answer
/// the largest non-overshooting size rather than one unit short of it.
///
/// The two error classes fall on opposite sides on purpose. An input too small
/// to produce any output cannot have moved the price at all, let alone past
/// the target, so it is *not* crossing; that keeps the predicate false for a
/// prefix of `dx` and true afterwards, which is exactly the monotonicity the
/// binary search needs. Every other error — the reserve cap, a would-be
/// drain, an overflow — is a `dx` that is not executable and whose successors
/// are worse, so it is crossing.
fn crosses_target(side: Side, pool: &PoolState, dx: u64, oracle_micro: u128) -> bool {
    let (reserve_in, reserve_out) = reserves_for(side, pool);

    let amount_out = match math::amount_out(reserve_in, reserve_out, dx, pool.fee_per_mille) {
        Ok(amount_out) => amount_out,
        Err(CurveError::ZeroAmount | CurveError::OutputRoundsToZero) => return false,
        Err(_) => return true,
    };

    let (Some(reserve_in_new), Some(reserve_out_new)) = (
        reserve_in.checked_add(dx),
        reserve_out.checked_sub(amount_out),
    ) else {
        return true;
    };

    let (reserve_lo, reserve_hi) = match side {
        Side::SellBtc => (reserve_in_new, reserve_out_new),
        Side::BuyBtc => (reserve_out_new, reserve_in_new),
    };

    let (Some(pool_side), Some(oracle_side)) = (
        u128::from(reserve_hi).checked_mul(DECIMAL_GAP),
        u128::from(reserve_lo).checked_mul(oracle_micro),
    ) else {
        return true;
    };

    match side {
        // Selling BTC drives the price down; it has gone too far once the
        // pool is cheaper than the oracle.
        Side::SellBtc => pool_side < oracle_side,
        // Buying drives it up.
        Side::BuyBtc => pool_side > oracle_side,
    }
}

/// The reserves a trade would leave behind, by the same `math::amount_out`
/// settlement uses. `None` if the module would reject the swap.
///
/// Used by the property test in design §7 and by the log line that reports
/// where a trade is expected to land the price.
#[must_use]
pub fn apply_trade(pool: &PoolState, unit_in: AmountUnit, amount_in: u64) -> Option<(u64, u64)> {
    let side = if unit_in == pool.unit_lo {
        Side::SellBtc
    } else if unit_in == pool.unit_hi {
        Side::BuyBtc
    } else {
        return None;
    };

    let (reserve_in, reserve_out) = reserves_for(side, pool);
    let amount_out =
        math::amount_out(reserve_in, reserve_out, amount_in, pool.fee_per_mille).ok()?;
    let reserve_in_new = reserve_in.checked_add(amount_in)?;
    let reserve_out_new = reserve_out.checked_sub(amount_out)?;

    Some(match side {
        Side::SellBtc => (reserve_in_new, reserve_out_new),
        Side::BuyBtc => (reserve_out_new, reserve_in_new),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIT_LO: AmountUnit = AmountUnit::BITCOIN;
    const UNIT_HI: AmountUnit = AmountUnit::new_custom(1);

    /// A pool at exactly 100 000 USD/BTC: 0.01 BTC against 1 000 USDt.
    /// `(1e9 micros / 1e9 msats) * 1e5 = 1e5`.
    const RESERVE_LO: u64 = 1_000_000_000;
    const RESERVE_HI: u64 = 1_000_000_000;
    /// 100 000 USD/BTC in micro-USD.
    const AT_PAR: u128 = 100_000 * 1_000_000;

    fn pool(reserve_lo: u64, reserve_hi: u64, fee_per_mille: u16) -> PoolState {
        PoolState {
            unit_lo: UNIT_LO,
            unit_hi: UNIT_HI,
            reserve_lo,
            reserve_hi,
            fee_per_mille,
        }
    }

    /// Deep enough pockets and a wide enough size cap that only the overshoot
    /// clamp can bind.
    fn unconstrained() -> (MinSwapIn, Balances, KeeperConfig) {
        (
            MinSwapIn {
                lo: 1_000,
                hi: 1_000,
            },
            Balances {
                lo: u64::MAX / 2,
                hi: u64::MAX / 2,
            },
            KeeperConfig {
                band_bps: 50,
                max_trade_micro_usd: u128::from(u64::MAX),
                btc_floor_msat: 0,
                usdt_floor_micros: 0,
            },
        )
    }

    fn trade_of(decision: Decision) -> (AmountUnit, AmountUnit, u64) {
        match decision {
            Decision::Trade {
                unit_in,
                unit_out,
                amount_in,
            } => (unit_in, unit_out, amount_in),
            Decision::Hold(reason) => panic!("expected a trade, got Hold({reason:?})"),
        }
    }

    /// The deviation arithmetic itself, on hand-computed values.
    ///
    /// `dev = (R_hi * 1e11 - R_lo * p) * 1e4 / (R_lo * p)`. With both reserves
    /// at 1e9 and `p = 1.01e11`: `(1e20 - 1.01e20) * 1e4 / 1.01e20 =
    /// -1e22/1.01e20 = -99.0099...`, truncated toward zero by i128 division.
    #[test]
    fn deviation_is_exact_integer_cross_multiplication() {
        assert_eq!(dev_bps(RESERVE_LO, RESERVE_HI, AT_PAR), Some(0));
        assert_eq!(
            dev_bps(RESERVE_LO, RESERVE_HI, 101_000 * 1_000_000),
            Some(-99)
        );
        assert_eq!(
            dev_bps(RESERVE_LO, RESERVE_HI, 99_000 * 1_000_000),
            Some(101)
        );
        assert_eq!(dev_bps(0, RESERVE_HI, AT_PAR), None);
        assert_eq!(dev_bps(RESERVE_LO, RESERVE_HI, 0), None);

        assert_eq!(pool_price_micro_usd(RESERVE_LO, RESERVE_HI), Some(AT_PAR));
        assert_eq!(pool_price_micro_usd(0, RESERVE_HI), None);
    }

    #[test]
    fn a_pool_on_the_oracle_price_holds() {
        let (min_swap_in, balances, cfg) = unconstrained();

        assert_eq!(
            decide(
                &pool(RESERVE_LO, RESERVE_HI, 3),
                min_swap_in,
                AT_PAR,
                balances,
                &cfg
            ),
            Decision::Hold(HoldReason::InBand { dev_bps: 0 })
        );
    }

    /// Both edges of the deadband, from both sides. 101 bps out with a 50 bps
    /// band trades; the same deviation with a 200 bps band holds.
    #[test]
    fn the_deadband_is_inclusive_at_its_edge() {
        let (min_swap_in, balances, mut cfg) = unconstrained();

        for oracle in [99_000 * 1_000_000, 101_000 * 1_000_000] {
            cfg.band_bps = 200;
            let decision = decide(
                &pool(RESERVE_LO, RESERVE_HI, 3),
                min_swap_in,
                oracle,
                balances,
                &cfg,
            );
            assert!(
                matches!(decision, Decision::Hold(HoldReason::InBand { .. })),
                "{oracle} with a 200 bps band: {decision:?}"
            );

            cfg.band_bps = 50;
            let decision = decide(
                &pool(RESERVE_LO, RESERVE_HI, 3),
                min_swap_in,
                oracle,
                balances,
                &cfg,
            );
            assert!(
                matches!(decision, Decision::Trade { .. }),
                "{oracle} with a 50 bps band: {decision:?}"
            );
        }

        // Exactly on the band edge is inside it: `dev == band` holds.
        cfg.band_bps = 101;
        assert!(matches!(
            decide(
                &pool(RESERVE_LO, RESERVE_HI, 3),
                min_swap_in,
                99_000 * 1_000_000,
                balances,
                &cfg,
            ),
            Decision::Hold(HoldReason::InBand { dev_bps: 101 })
        ));
    }

    /// **The sign test.** Design §4.1: getting this backwards makes the bot
    /// diverge rather than converge, at increasing speed, so both directions
    /// are pinned explicitly — by the unit spent *and* by the direction the
    /// resulting price actually moves.
    #[test]
    fn both_directions_are_pinned_by_sign() {
        let (min_swap_in, balances, cfg) = unconstrained();
        let pool = pool(RESERVE_LO, RESERVE_HI, 3);

        // Oracle below the pool: the pool overvalues BTC, so sell BTC into it.
        let oracle = 99_000 * 1_000_000;
        assert!(dev_bps(RESERVE_LO, RESERVE_HI, oracle).expect("priced") > 0);
        let (unit_in, unit_out, amount_in) =
            trade_of(decide(&pool, min_swap_in, oracle, balances, &cfg));
        assert_eq!((unit_in, unit_out), (UNIT_LO, UNIT_HI), "must spend msats");
        let (lo, hi) = apply_trade(&pool, unit_in, amount_in).expect("settles");
        assert!(
            lo > RESERVE_LO && hi < RESERVE_HI,
            "reserves moved the wrong way"
        );
        assert!(
            pool_price_micro_usd(lo, hi).expect("priced") < AT_PAR,
            "selling BTC must lower the pool price"
        );

        // Oracle above the pool: the pool undervalues BTC, so buy BTC from it.
        let oracle = 101_000 * 1_000_000;
        assert!(dev_bps(RESERVE_LO, RESERVE_HI, oracle).expect("priced") < 0);
        let (unit_in, unit_out, amount_in) =
            trade_of(decide(&pool, min_swap_in, oracle, balances, &cfg));
        assert_eq!((unit_in, unit_out), (UNIT_HI, UNIT_LO), "must spend micros");
        let (lo, hi) = apply_trade(&pool, unit_in, amount_in).expect("settles");
        assert!(
            lo < RESERVE_LO && hi > RESERVE_HI,
            "reserves moved the wrong way"
        );
        assert!(
            pool_price_micro_usd(lo, hi).expect("priced") > AT_PAR,
            "buying BTC must raise the pool price"
        );
    }

    /// The binary search returns a maximum, not merely *a* non-overshooting
    /// size: `dx` lands at or before the oracle price and `dx + 1` goes past.
    /// Asserted against `crosses_target` rather than against a remembered
    /// number, so it stays a statement about the curve.
    #[test]
    fn the_search_returns_the_largest_non_overshooting_size() {
        for (oracle, side) in [
            (99_000 * 1_000_000, Side::SellBtc),
            (101_000 * 1_000_000, Side::BuyBtc),
        ] {
            for fee in [0u16, 1, 3, 50] {
                let pool = pool(RESERVE_LO, RESERVE_HI, fee);
                let dx = largest_non_crossing(side, &pool, oracle);

                assert!(dx > 0, "{side:?} at fee {fee}: no size found");
                assert!(
                    !crosses_target(side, &pool, dx, oracle),
                    "{side:?} at fee {fee}: dx={dx} overshoots"
                );
                assert!(
                    crosses_target(side, &pool, dx + 1, oracle),
                    "{side:?} at fee {fee}: dx={dx} is not the largest"
                );
            }
        }
    }

    /// A correction the curve cannot make in one unit of input is a hold, not
    /// a trade that overshoots.
    #[test]
    fn a_correction_smaller_than_one_unit_holds() {
        let (min_swap_in, balances, mut cfg) = unconstrained();
        cfg.band_bps = 0;

        // 1 msat against 1 000 micros: the pool prices BTC at 1e14 micro-USD,
        // the oracle 1 bp under that, and the smallest possible swap — one
        // msat, which the curve pays 500 micros for — lands the price at
        // 2.5e13, a factor of four past the target.
        let pool = pool(1, 1_000, 0);
        let oracle = 99_990_000_000_000;

        assert_eq!(dev_bps(1, 1_000, oracle), Some(1));
        assert!(crosses_target(Side::SellBtc, &pool, 1, oracle));
        assert_eq!(
            decide(&pool, min_swap_in, oracle, balances, &cfg),
            Decision::Hold(HoldReason::NoSizeAvailable { dev_bps: 1 })
        );
    }

    /// Design §4.3.2: `--max-trade-usd` is converted to the input unit at the
    /// **oracle** price, not the pool's. Selling into a pool the oracle prices
    /// at 99 000 USD/BTC, one dollar is `1e6 * 1e11 / 9.9e10 = 1 010 101`
    /// msats; buying, one dollar is 1e6 micros whatever the price, since
    /// design §2 takes 1 USDt to be 1 USD.
    #[test]
    fn the_max_trade_clamp_binds_in_the_input_unit() {
        let (min_swap_in, balances, mut cfg) = unconstrained();
        cfg.max_trade_micro_usd = 1_000_000; // $1

        let unclamped = trade_of(decide(
            &pool(RESERVE_LO, RESERVE_HI, 3),
            min_swap_in,
            99_000 * 1_000_000,
            balances,
            &unconstrained().2,
        ))
        .2;
        assert!(unclamped > 1_000_000, "the clamp would not have bound");

        let (unit_in, _, amount_in) = trade_of(decide(
            &pool(RESERVE_LO, RESERVE_HI, 3),
            min_swap_in,
            99_000 * 1_000_000,
            balances,
            &cfg,
        ));
        assert_eq!((unit_in, amount_in), (UNIT_LO, 1_010_101));

        let (unit_in, _, amount_in) = trade_of(decide(
            &pool(RESERVE_LO, RESERVE_HI, 3),
            min_swap_in,
            101_000 * 1_000_000,
            balances,
            &cfg,
        ));
        assert_eq!((unit_in, amount_in), (UNIT_HI, 1_000_000));
    }

    /// Design §4.3.3: the floor is a floor on the balance, so what is
    /// spendable is `balance - floor` and a clamped trade is still a partial
    /// correction.
    #[test]
    fn the_inventory_floor_clamps_without_refusing() {
        let (min_swap_in, _, mut cfg) = unconstrained();
        cfg.btc_floor_msat = 4_000_000;
        cfg.usdt_floor_micros = 4_000_000;
        let balances = Balances {
            lo: 5_000_000,
            hi: 5_000_000,
        };

        let (unit_in, _, amount_in) = trade_of(decide(
            &pool(RESERVE_LO, RESERVE_HI, 3),
            min_swap_in,
            99_000 * 1_000_000,
            balances,
            &cfg,
        ));
        assert_eq!((unit_in, amount_in), (UNIT_LO, 1_000_000));

        let (unit_in, _, amount_in) = trade_of(decide(
            &pool(RESERVE_LO, RESERVE_HI, 3),
            min_swap_in,
            101_000 * 1_000_000,
            balances,
            &cfg,
        ));
        assert_eq!((unit_in, amount_in), (UNIT_HI, 1_000_000));
    }

    /// Design §4.3.4: what survives the clamps must still clear the AMM's own
    /// `min_swap_in`, and falling under it is the "side is exhausted" signal —
    /// a hold, at WARN.
    #[test]
    fn dust_below_min_swap_in_holds_at_warn() {
        let (_, _, mut cfg) = unconstrained();
        cfg.btc_floor_msat = 4_000_000;
        let min_swap_in = MinSwapIn {
            lo: 2_000_000,
            hi: 1_000,
        };
        let balances = Balances {
            lo: 5_000_000,
            hi: 5_000_000,
        };

        let decision = decide(
            &pool(RESERVE_LO, RESERVE_HI, 3),
            min_swap_in,
            99_000 * 1_000_000,
            balances,
            &cfg,
        );

        assert_eq!(
            decision,
            Decision::Hold(HoldReason::BelowMinSwapIn {
                unit_in: UNIT_LO,
                amount_in: 1_000_000,
                min_swap_in: 2_000_000,
            })
        );
        let Decision::Hold(reason) = decision else {
            unreachable!()
        };
        assert!(reason.is_warning());
    }

    /// A side with nothing left in it is the same hold, with `amount_in` zero
    /// — the exhaustion case the operator is meant to act on.
    #[test]
    fn an_exhausted_side_holds_below_min_swap_in() {
        let (min_swap_in, _, cfg) = unconstrained();
        let balances = Balances { lo: 0, hi: 0 };

        assert_eq!(
            decide(
                &pool(RESERVE_LO, RESERVE_HI, 3),
                min_swap_in,
                99_000 * 1_000_000,
                balances,
                &cfg,
            ),
            Decision::Hold(HoldReason::BelowMinSwapIn {
                unit_in: UNIT_LO,
                amount_in: 0,
                min_swap_in: 1_000,
            })
        );
    }

    /// A trade that clears `min_swap_in` but whose output would floor to zero
    /// is refused by asking `math::amount_out`, not by reasoning about it.
    #[test]
    fn a_swap_the_module_would_reject_holds() {
        let (_, _, mut cfg) = unconstrained();
        cfg.band_bps = 0;
        // A `min_swap_in` of one lets a single msat past the dust clamp, and a
        // balance of one msat is what the inventory clamp cuts the correction
        // down to.
        let min_swap_in = MinSwapIn { lo: 1, hi: 1 };
        let balances = Balances { lo: 1, hi: 1 };
        // The pool prices BTC at 1e8 micro-USD ($100) against an oracle of
        // $10, so it overvalues BTC enormously and the correction is to sell
        // msats — but one msat into 1e9 msats against 1e6 micros pays out
        // `1e6 / (1e9 + 1)`, which floors to nothing.
        let pool = pool(1_000_000_000, 1_000_000, 0);
        let oracle = 10_000_000;

        let decision = decide(&pool, min_swap_in, oracle, balances, &cfg);

        assert_eq!(
            decision,
            Decision::Hold(HoldReason::OutputRoundsToZero {
                unit_in: UNIT_LO,
                amount_in: 1,
            })
        );
    }

    #[test]
    fn an_empty_or_one_sided_pool_holds() {
        let (min_swap_in, balances, cfg) = unconstrained();

        assert_eq!(
            decide(&pool(0, 0, 3), min_swap_in, AT_PAR, balances, &cfg),
            Decision::Hold(HoldReason::EmptyPool)
        );
        assert_eq!(
            decide(&pool(0, RESERVE_HI, 3), min_swap_in, AT_PAR, balances, &cfg),
            Decision::Hold(HoldReason::ZeroReserve {
                reserve_lo: 0,
                reserve_hi: RESERVE_HI,
            })
        );
        assert_eq!(
            decide(&pool(RESERVE_LO, 0, 3), min_swap_in, AT_PAR, balances, &cfg),
            Decision::Hold(HoldReason::ZeroReserve {
                reserve_lo: RESERVE_LO,
                reserve_hi: 0,
            })
        );
    }

    /// Design §4.4, at both ends of the fee band the module's config permits.
    #[test]
    fn the_band_must_exceed_the_fee() {
        assert_eq!(check_band_exceeds_fee(50, 3), Ok(()));
        assert_eq!(check_band_exceeds_fee(30, 3), Ok(()));
        assert_eq!(
            check_band_exceeds_fee(29, 3),
            Err(BandTooNarrow {
                band_bps: 29,
                fee_per_mille: 3
            })
        );
        assert_eq!(check_band_exceeds_fee(500, 50), Ok(()));
        assert_eq!(
            check_band_exceeds_fee(499, 50),
            Err(BandTooNarrow {
                band_bps: 499,
                fee_per_mille: 50
            })
        );
        assert_eq!(check_band_exceeds_fee(10, 1), Ok(()));
        assert_eq!(
            check_band_exceeds_fee(9, 1),
            Err(BandTooNarrow {
                band_bps: 9,
                fee_per_mille: 1
            })
        );

        // And the same rule, reached through `decide`: a fee vote that lands
        // mid-flight and narrows the margin holds instead of trading.
        let (min_swap_in, balances, cfg) = unconstrained();
        assert_eq!(
            decide(
                &pool(RESERVE_LO, RESERVE_HI, 6),
                min_swap_in,
                99_000 * 1_000_000,
                balances,
                &cfg,
            ),
            Decision::Hold(HoldReason::BandBelowFee {
                band_bps: 50,
                fee_per_mille: 6
            })
        );
    }

    /// Both ends of the `[1, 50]` per-mille fee band the AMM's config allows,
    /// with a band wide enough for each, trade in the right direction and
    /// size down as the fee rises — the fee is paid out of the same reserves
    /// the price is computed from, so a larger fee needs a larger input to
    /// reach the same post-trade price.
    #[test]
    fn the_fee_is_honoured_at_both_ends_of_its_band() {
        let (min_swap_in, balances, mut cfg) = unconstrained();
        cfg.band_bps = 500;
        let oracle = 90_000 * 1_000_000;

        let mut sizes = Vec::new();
        for fee in [1u16, 50] {
            let pool = pool(RESERVE_LO, RESERVE_HI, fee);
            let (unit_in, _, amount_in) =
                trade_of(decide(&pool, min_swap_in, oracle, balances, &cfg));

            assert_eq!(unit_in, UNIT_LO);
            let (lo, hi) = apply_trade(&pool, unit_in, amount_in).expect("settles");
            let landed = pool_price_micro_usd(lo, hi).expect("priced");
            assert!(
                landed >= oracle,
                "fee {fee} overshot: landed at {landed}, oracle {oracle}"
            );
            sizes.push(amount_in);
        }

        assert!(
            sizes[1] > sizes[0],
            "a 50 per-mille fee needs a larger input than a 1 per-mille one: {sizes:?}"
        );
    }

    proptest::proptest! {
        /// Design §7's property: applying the returned trade via
        /// `amount_out` must reduce `|dev_bps|` and must not overshoot past
        /// the target — the sign of `dev_bps` never flips.
        ///
        /// This is the property a sign error in §4.1 breaks loudly: a bot
        /// trading the wrong way increases `|dev_bps|` on every tick.
        #[test]
        fn a_trade_always_moves_the_price_toward_the_oracle_and_never_past_it(
            reserve_lo in 1_000_000u64..=(1 << 40),
            reserve_hi in 1_000_000u64..=(1 << 40),
            oracle_micro in 1_000_000u128..=1_000_000_000_000u128,
            fee in 0u16..=50,
        ) {
            let pool = pool(reserve_lo, reserve_hi, fee);
            let (min_swap_in, balances, mut cfg) = unconstrained();
            cfg.band_bps = u64::from(fee) * 10 + 50;

            if let Decision::Trade { unit_in, amount_in, .. } =
                decide(&pool, min_swap_in, oracle_micro, balances, &cfg)
            {
                let before = dev_bps(reserve_lo, reserve_hi, oracle_micro).expect("priced");
                let (lo, hi) =
                    apply_trade(&pool, unit_in, amount_in).expect("a decided trade settles");
                let after = dev_bps(lo, hi, oracle_micro).expect("priced");

                // The correction never crosses: a pool that was expensive
                // stays at or above the oracle, one that was cheap stays at
                // or below.
                if before > 0 {
                    proptest::prop_assert!(after >= 0, "sign flipped: {} -> {}", before, after);
                    proptest::prop_assert_eq!(unit_in, UNIT_LO, "sold the wrong unit");
                } else {
                    proptest::prop_assert!(after <= 0, "sign flipped: {} -> {}", before, after);
                    proptest::prop_assert_eq!(unit_in, UNIT_HI, "sold the wrong unit");
                }

                // And it never gets worse.
                proptest::prop_assert!(
                    after.unsigned_abs() <= before.unsigned_abs(),
                    "|dev| grew: {} -> {}", before, after
                );
            }
        }

        /// The binary search's contract, over the same domain: what it
        /// returns does not overshoot, and one more unit does.
        #[test]
        fn the_search_is_always_maximal(
            reserve_lo in 1_000_000u64..=(1 << 40),
            reserve_hi in 1_000_000u64..=(1 << 40),
            oracle_micro in 1_000_000u128..=1_000_000_000_000u128,
            fee in 0u16..=50,
            sell in proptest::bool::ANY,
        ) {
            let pool = pool(reserve_lo, reserve_hi, fee);
            let side = if sell { Side::SellBtc } else { Side::BuyBtc };

            let dx = largest_non_crossing(side, &pool, oracle_micro);

            if dx > 0 {
                proptest::prop_assert!(!crosses_target(side, &pool, dx, oracle_micro));
            }
            proptest::prop_assert!(crosses_target(side, &pool, dx + 1, oracle_micro));
        }

        /// Nothing in the policy may panic, whatever the federation and the
        /// feed report — a bot that dies on a surprising tick stops keeping
        /// the price.
        #[test]
        fn decide_never_panics(
            reserve_lo in 0u64..=MAX_RESERVE,
            reserve_hi in 0u64..=MAX_RESERVE,
            oracle_micro in 0u128..=crate::oracle::MAX_MICRO_USD_PER_BTC,
            fee in 0u16..1000,
            band_bps in 0u64..=100_000,
            max_trade_micro_usd in 0u128..=u128::from(u64::MAX),
            balance_lo in 0u64..=u64::MAX,
            balance_hi in 0u64..=u64::MAX,
            floor_lo in 0u64..=u64::MAX,
            floor_hi in 0u64..=u64::MAX,
            min_lo in 1u64..=u64::MAX,
            min_hi in 1u64..=u64::MAX,
        ) {
            let _ = decide(
                &pool(reserve_lo, reserve_hi, fee),
                MinSwapIn { lo: min_lo, hi: min_hi },
                oracle_micro,
                Balances { lo: balance_lo, hi: balance_hi },
                &KeeperConfig {
                    band_bps,
                    max_trade_micro_usd,
                    btc_floor_msat: floor_lo,
                    usdt_floor_micros: floor_hi,
                },
            );
        }
    }
}
