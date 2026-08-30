//! `process_output` — `SwapV0` and `DepositV0`. Spec §7, §7.4.
//!
//! Each case is its own `#[tokio::test]` with real assertions, so a failure
//! names the case directly rather than a table index.

use std::collections::BTreeMap;

use fedimint_amm_common::config::{AmmConfigConsensus, UnitParams};
use fedimint_amm_common::math::{self, MINIMUM_LIQUIDITY};
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_common::pop;
use fedimint_amm_common::types::{AmmOutput, AmmOutputError};
use fedimint_amm_server::Amm;
use fedimint_amm_server::db::{BalanceEntry, BalanceKey, LpPositionKey, Pool, PoolKey};
use fedimint_core::db::mem_impl::MemDatabase;
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::module::AmountUnit;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::secp256k1::{self, Keypair, SECP256K1};
use fedimint_core::{Amount, BitcoinHash, NumPeers, OutPoint, PeerId, TransactionId};
use fedimint_server_core::ServerModule;

fn db() -> Database {
    Database::new(MemDatabase::new(), ModuleDecoderRegistry::default())
}

fn unit(n: u64) -> AmountUnit {
    AmountUnit::new_custom(n)
}

/// A deterministic keypair per seed. Outputs need a full keypair (not just a
/// pubkey) since every `SwapV0`/`DepositV0` carries a proof of possession
/// signed by the key it names — see `fedimint-amm-common`'s `pop` module.
fn kp(seed: u8) -> Keypair {
    Keypair::from_seckey_slice(SECP256K1, &[seed; 32])
        .expect("a repeated non-zero byte is a valid secret key")
}

fn test_pubkey(seed: u8) -> secp256k1::PublicKey {
    kp(seed).public_key()
}

fn out_point() -> OutPoint {
    OutPoint {
        txid: TransactionId::all_zeros(),
        out_idx: 0,
    }
}

/// Units 0 and 1 are in the allowlist with `min_swap_in` 1_000 msats each;
/// default fee 3/1000 (0.30%), matching the reference Uniswap V2 fee.
fn amm() -> Amm {
    Amm::new(
        AmmConfigConsensus {
            units: BTreeMap::from([
                (
                    unit(0),
                    UnitParams {
                        min_swap_in: Amount::from_msats(1_000),
                    },
                ),
                (
                    unit(1),
                    UnitParams {
                        min_swap_in: Amount::from_msats(1_000),
                    },
                ),
            ]),
            default_fee_per_mille: 3,
            fee_overrides: BTreeMap::new(),
            // Wide open: these tests predate the guardian-voted fee and assert
            // curve and settlement behaviour, so the band must not be the thing
            // that changes their effective fee. The band itself is exercised by
            // the fee-vote tests.
            min_fee_per_mille: 0,
            max_fee_per_mille: 999,
        },
        NumPeers::from(4),
        PeerId::from(0),
    )
}

fn pool01() -> PoolId {
    PoolId::new(unit(0), unit(1)).unwrap()
}

/// 1. `SwapV0` into a fresh pool fails with `NoSuchPool`.
#[tokio::test]
async fn swap_into_fresh_pool_fails_with_no_such_pool() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();

    let output = AmmOutput::new_swap_v0(
        &kp(1),
        unit(0),
        unit(1),
        Amount::from_msats(10_000),
        Amount::ZERO,
        [0u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::NoSuchPool));
}

/// 2. `DepositV0` into an empty `PoolId` creates the pool, mints
///    `isqrt(da*db) - MINIMUM_LIQUIDITY` to `owner_pk`, and returns
///    `amounts == {lo: da, hi: db}`.
#[tokio::test]
async fn deposit_into_empty_pool_creates_it_and_mints_shares() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let owner_kp = kp(2);
    let owner = owner_kp.public_key();
    let pool = pool01();

    let da = Amount::from_msats(1_000_000);
    let db_amt = Amount::from_msats(1_000_000);

    let output = AmmOutput::new_deposit_v0(&owner_kp, pool, da, db_amt, 0, [1u8; 16]);

    let result = module
        .process_output(&mut dbtx, &output, out_point())
        .await
        .expect("deposit into a fresh pool must succeed");

    assert_eq!(result.amounts.get(&pool.lo()), Some(&da));
    assert_eq!(result.amounts.get(&pool.hi()), Some(&db_amt));
    assert_eq!(result.amounts.len(), 2);
    assert!(result.fees.is_empty());

    // isqrt(1_000_000 * 1_000_000) == 1_000_000
    let expected_minted = 1_000_000u64;
    let stored_pool = dbtx
        .get_value(&PoolKey(pool))
        .await
        .expect("pool must now exist");
    assert_eq!(stored_pool.reserve_lo, da);
    assert_eq!(stored_pool.reserve_hi, db_amt);
    assert_eq!(stored_pool.total_shares, expected_minted);

    let position = dbtx
        .get_value(&LpPositionKey { pool, owner })
        .await
        .expect("LP position must be created");
    assert_eq!(position.shares, expected_minted - MINIMUM_LIQUIDITY);
    assert_eq!(position.tweak, [1u8; 16]);
}

/// 3. `DepositV0` below `MINIMUM_LIQUIDITY` is rejected and writes NOTHING.
#[tokio::test]
async fn deposit_below_minimum_liquidity_is_rejected_and_writes_nothing() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    // isqrt(1_000 * 1_000) == 1_000, which is not > MINIMUM_LIQUIDITY.
    let output = AmmOutput::new_deposit_v0(
        &kp(3),
        pool,
        Amount::from_msats(1_000),
        Amount::from_msats(1_000),
        0,
        [2u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(
        result,
        Err(AmmOutputError::Curve(
            math::CurveError::InsufficientInitialLiquidity.to_string()
        ))
    );
    assert!(
        dbtx.get_value(&PoolKey(pool)).await.is_none(),
        "a rejected DepositV0 must leave no Pool record behind"
    );
}

/// 4. `DepositV0` with `min_shares` above what is mintable returns
///    `SlippageExceeded`, and writes nothing beyond the first deposit.
#[tokio::test]
async fn deposit_with_min_shares_too_high_returns_slippage_exceeded() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    // Two DISTINCT owners on purpose: this test is about the second
    // depositor being rejected without touching the first depositor's state.
    let first_owner_kp = kp(4);
    let second_owner_kp = kp(5);
    let second_owner = second_owner_kp.public_key();

    let first = AmmOutput::new_deposit_v0(
        &first_owner_kp,
        pool,
        Amount::from_msats(1_000_000),
        Amount::from_msats(1_000_000),
        0,
        [3u8; 16],
    );
    module
        .process_output(&mut dbtx, &first, out_point())
        .await
        .expect("first deposit must succeed");
    let pool_after_first = dbtx.get_value(&PoolKey(pool)).await.unwrap();

    // Second deposit at the same ratio would mint `100`, so `min_shares` set
    // above that must be rejected.
    let second = AmmOutput::new_deposit_v0(
        &second_owner_kp,
        pool,
        Amount::from_msats(100),
        Amount::from_msats(100),
        u64::MAX,
        [4u8; 16],
    );
    let result = module.process_output(&mut dbtx, &second, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::SlippageExceeded));

    // Nothing changed: pool unchanged, no position for the second owner.
    let pool_after_second = dbtx.get_value(&PoolKey(pool)).await.unwrap();
    assert_eq!(pool_after_first, pool_after_second);
    assert!(
        dbtx.get_value(&LpPositionKey {
            pool,
            owner: second_owner
        })
        .await
        .is_none()
    );
}

/// 4b. `DepositV0` naming a `PoolId` whose **`lo`** is outside `cfg.units`
///     returns `UnknownUnit` and writes nothing.
///
/// Unlike the `SwapV0` allowlist tests (case 8), no `Pool` needs to be seeded
/// here: `DepositV0` is the only pool-creation path (case 2's doc comment),
/// so an unseeded pool cannot pre-empt this check with `NoSuchPool` — there
/// is no such error on the deposit path to begin with. The allowlist here is
/// deliberately just `{unit(5)}` rather than the shared `amm()` fixture's
/// `{unit(0), unit(1)}`: `PoolId::lo()` is always the numerically smaller
/// unit (`pool_id.rs`'s `PoolId::new`), so with `amm()`'s allowlist starting
/// at `0` there is no unit number left to be both `< 1` and outside the
/// allowlist. `unit(1)` here is deliberately NOT in this test's allowlist,
/// so `lo` is the only side excluded.
#[tokio::test]
async fn deposit_with_lo_outside_allowlist_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = Amm::new(
        AmmConfigConsensus {
            units: BTreeMap::from([(
                unit(5),
                UnitParams {
                    min_swap_in: Amount::from_msats(1_000),
                },
            )]),
            default_fee_per_mille: 3,
            fee_overrides: BTreeMap::new(),
            // Wide open: these tests predate the guardian-voted fee and assert
            // curve and settlement behaviour, so the band must not be the thing
            // that changes their effective fee. The band itself is exercised by
            // the fee-vote tests.
            min_fee_per_mille: 0,
            max_fee_per_mille: 999,
        },
        NumPeers::from(4),
        PeerId::from(0),
    );
    let pool = PoolId::new(unit(1), unit(5)).unwrap();
    assert_eq!(
        pool.lo(),
        unit(1),
        "test setup: lo must be the excluded unit"
    );
    assert_eq!(
        pool.hi(),
        unit(5),
        "test setup: hi must be the allowed unit"
    );

    let output = AmmOutput::new_deposit_v0(
        &kp(20),
        pool,
        Amount::from_msats(1_000_000),
        Amount::from_msats(1_000_000),
        0,
        [20u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::UnknownUnit));
    assert!(
        dbtx.get_value(&PoolKey(pool)).await.is_none(),
        "a rejected DepositV0 must leave no Pool record behind"
    );
    assert!(
        dbtx.get_value(&LpPositionKey {
            pool,
            owner: test_pubkey(20)
        })
        .await
        .is_none(),
        "a rejected DepositV0 must leave no LpPosition record behind"
    );
}

/// 4c. `DepositV0` naming a `PoolId` whose **`hi`** is outside `cfg.units`
///     returns `UnknownUnit` and writes nothing. Mirror of case 4b — see its
///     doc comment for why no `Pool` needs to be seeded — covering the other
///     operand of the `||` in the allowlist guard, so deleting either half of
///     that guard is caught by one of these two tests.
#[tokio::test]
async fn deposit_with_hi_outside_allowlist_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = Amm::new(
        AmmConfigConsensus {
            units: BTreeMap::from([(
                unit(1),
                UnitParams {
                    min_swap_in: Amount::from_msats(1_000),
                },
            )]),
            default_fee_per_mille: 3,
            fee_overrides: BTreeMap::new(),
            // Wide open: these tests predate the guardian-voted fee and assert
            // curve and settlement behaviour, so the band must not be the thing
            // that changes their effective fee. The band itself is exercised by
            // the fee-vote tests.
            min_fee_per_mille: 0,
            max_fee_per_mille: 999,
        },
        NumPeers::from(4),
        PeerId::from(0),
    );
    let pool = PoolId::new(unit(1), unit(5)).unwrap();
    assert_eq!(
        pool.lo(),
        unit(1),
        "test setup: lo must be the allowed unit"
    );
    assert_eq!(
        pool.hi(),
        unit(5),
        "test setup: hi must be the excluded unit"
    );

    let output = AmmOutput::new_deposit_v0(
        &kp(21),
        pool,
        Amount::from_msats(1_000_000),
        Amount::from_msats(1_000_000),
        0,
        [21u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::UnknownUnit));
    assert!(
        dbtx.get_value(&PoolKey(pool)).await.is_none(),
        "a rejected DepositV0 must leave no Pool record behind"
    );
    assert!(
        dbtx.get_value(&LpPositionKey {
            pool,
            owner: test_pubkey(21)
        })
        .await
        .is_none(),
        "a rejected DepositV0 must leave no LpPosition record behind"
    );
}

/// 5. `SwapV0` on a live pool moves reserves by exactly `amount_in` / `dy`,
///    credits `Balance[(recipient_pk, unit_out)] == dy`, and returns
///    `amounts == {unit_in: amount_in}`. The credited balance and the reserve
///    debit must be the SAME integer — spec §7.4.
#[tokio::test]
async fn swap_moves_reserves_and_credits_balance_by_the_same_dy() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    // Seed the pool directly so reserves match the worked example in
    // `fedimint-amm-common`'s `amount_out_matches_reference_vector` test.
    let reserve_lo = Amount::from_msats(1_000_000_000);
    let reserve_hi = Amount::from_msats(1_000_000);
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo,
            reserve_hi,
            total_shares: 1_000_000_000,
        },
    )
    .await;

    let amount_in = Amount::from_msats(10_000_000);
    let recipient_kp = kp(6);
    let recipient = recipient_kp.public_key();
    let output = AmmOutput::new_swap_v0(
        &recipient_kp,
        unit(0),
        unit(1),
        amount_in,
        Amount::ZERO,
        [5u8; 16],
    );

    let result = module
        .process_output(&mut dbtx, &output, out_point())
        .await
        .expect("swap on a live pool must succeed");

    let expected_dy = math::amount_out(reserve_lo.msats, reserve_hi.msats, amount_in.msats, 3)
        .expect("reference vector must compute cleanly");
    assert_eq!(expected_dy, 9_871);

    assert_eq!(result.amounts.get(&unit(0)), Some(&amount_in));
    assert_eq!(result.amounts.len(), 1);
    assert!(result.fees.is_empty());

    let stored_pool = dbtx.get_value(&PoolKey(pool)).await.unwrap();
    assert_eq!(stored_pool.reserve_lo, Amount::from_msats(1_010_000_000));
    assert_eq!(
        stored_pool.reserve_hi,
        Amount::from_msats(reserve_hi.msats - expected_dy)
    );

    let balance = dbtx
        .get_value(&BalanceKey {
            owner: recipient,
            unit: unit(1),
        })
        .await
        .expect("balance must be credited");
    assert_eq!(balance.amount, Amount::from_msats(expected_dy));

    // The reserve debit and the balance credit are literally the same
    // integer: reserve_hi dropped by exactly `expected_dy`, and the balance
    // is exactly `expected_dy`.
    let reserve_hi_delta = reserve_hi.msats - stored_pool.reserve_hi.msats;
    assert_eq!(reserve_hi_delta, expected_dy);
    assert_eq!(balance.amount.msats, reserve_hi_delta);
}

/// 5b. Mirror of case 5 but swapping in the **hi** direction (`unit_in ==
///     pool.hi()`), so `in_is_lo` is `false` and the `else` arm of both the
///     reserve read (lib.rs) and the write-back executes. Every other test
///     in this file swaps `unit(0) -> unit(1)`, i.e. lo -> hi, so `in_is_lo`
///     is `true` throughout the rest of the suite and this arm is otherwise
///     completely uncovered — swapping the two assignments in the `else` arm
///     of the write-back would corrupt every hi->lo swap's reserves and the
///     rest of the suite would still pass. `expected_dy` is hand-computed
///     from the Uniswap V2 formula and hard-coded (not derived by calling
///     `math::amount_out`), so an inverted read can't cancel out against an
///     inverted expectation.
#[tokio::test]
async fn swap_in_hi_direction_moves_reserves_and_credits_balance_by_the_same_dy() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    // Same seeded pool as case 5: reserve_lo (unit 0) = 1_000_000_000,
    // reserve_hi (unit 1) = 1_000_000.
    let reserve_lo = Amount::from_msats(1_000_000_000);
    let reserve_hi = Amount::from_msats(1_000_000);
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo,
            reserve_hi,
            total_shares: 1_000_000_000,
        },
    )
    .await;

    // Swap unit(1) [[hi]] -> unit(0) [[lo]]: unit_in == pool.hi(), so
    // `in_is_lo` is false, reserve_in == reserve_hi, reserve_out == reserve_lo.
    //
    // By hand, Uniswap V2's getAmountOut with reserve_in = 1_000_000,
    // reserve_out = 1_000_000_000, amount_in = 10_000, fee 3/1000:
    //   in_with_fee = 10_000 * 997 = 9_970_000
    //   numerator   = 9_970_000 * 1_000_000_000 = 9_970_000_000_000_000
    //   denominator = 1_000_000 * 1000 + 9_970_000 = 1_009_970_000
    //   out         = floor(9_970_000_000_000_000 / 1_009_970_000) = 9_871_580
    let amount_in = Amount::from_msats(10_000);
    let expected_dy: u64 = 9_871_580;
    let recipient_kp = kp(16);
    let recipient = recipient_kp.public_key();
    let output = AmmOutput::new_swap_v0(
        &recipient_kp,
        unit(1),
        unit(0),
        amount_in,
        Amount::ZERO,
        [13u8; 16],
    );

    let result = module
        .process_output(&mut dbtx, &output, out_point())
        .await
        .expect("swap on a live pool must succeed");

    assert_eq!(result.amounts.get(&unit(1)), Some(&amount_in));
    assert_eq!(result.amounts.len(), 1);
    assert!(result.fees.is_empty());

    let stored_pool = dbtx.get_value(&PoolKey(pool)).await.unwrap();
    // The correct reserve is credited: reserve_hi (unit 1, the `in` side)
    // goes UP by amount_in.
    assert_eq!(
        stored_pool.reserve_hi,
        Amount::from_msats(reserve_hi.msats + amount_in.msats)
    );
    // The correct reserve is debited: reserve_lo (unit 0, the `out` side)
    // goes DOWN by expected_dy. An inverted write-back would instead leave
    // reserve_lo unchanged and debit reserve_hi, failing this assertion.
    assert_eq!(
        stored_pool.reserve_lo,
        Amount::from_msats(reserve_lo.msats - expected_dy)
    );

    // The balance lands under unit(0) -- the `out` unit -- not unit(1).
    let balance = dbtx
        .get_value(&BalanceKey {
            owner: recipient,
            unit: unit(0),
        })
        .await
        .expect("balance must be credited under the OUT unit");
    assert_eq!(balance.amount, Amount::from_msats(expected_dy));
    assert!(
        dbtx.get_value(&BalanceKey {
            owner: recipient,
            unit: unit(1)
        })
        .await
        .is_none(),
        "must not credit the IN unit"
    );
}

/// 6. `SwapV0` with `min_out` above `dy` returns `SlippageExceeded` and
///    writes nothing.
#[tokio::test]
async fn swap_with_min_out_above_dy_returns_slippage_exceeded_and_writes_nothing() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    let reserve_lo = Amount::from_msats(1_000_000_000);
    let reserve_hi = Amount::from_msats(1_000_000);
    let seeded_pool = Pool {
        reserve_lo,
        reserve_hi,
        total_shares: 1_000_000_000,
    };
    dbtx.insert_new_entry(&PoolKey(pool), &seeded_pool).await;

    let amount_in = Amount::from_msats(10_000_000);
    let dy = math::amount_out(reserve_lo.msats, reserve_hi.msats, amount_in.msats, 3).unwrap();
    let recipient_kp = kp(7);
    let recipient = recipient_kp.public_key();

    let output = AmmOutput::new_swap_v0(
        &recipient_kp,
        unit(0),
        unit(1),
        amount_in,
        Amount::from_msats(dy + 1),
        [6u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::SlippageExceeded));

    assert_eq!(dbtx.get_value(&PoolKey(pool)).await.unwrap(), seeded_pool);
    assert!(
        dbtx.get_value(&BalanceKey {
            owner: recipient,
            unit: unit(1)
        })
        .await
        .is_none()
    );
}

/// 7. `SwapV0` with `unit_in == unit_out` returns `IdenticalUnits`.
#[tokio::test]
async fn swap_with_identical_units_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();

    let output = AmmOutput::new_swap_v0(
        &kp(8),
        unit(0),
        unit(0),
        Amount::from_msats(10_000),
        Amount::ZERO,
        [7u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::IdenticalUnits));
}

/// 8. `SwapV0` with a unit outside `units` returns `UnknownUnit`.
///
/// A `Pool` is seeded for `(unit(0), unit(99))` even though `unit(99)` is
/// outside the config's allowlist — nothing stops a `Pool` DB record from
/// existing for a unit pair the current config no longer lists, and seeding
/// one here isolates the unit-allowlist check from `NoSuchPool`: since the
/// I1/M9 shared-helper refactor, `quote_swap`'s admission checks (including
/// this one) run AFTER the pool lookup, so an unseeded pool would report
/// `NoSuchPool` first and never reach the check this test targets.
#[tokio::test]
async fn swap_with_unit_outside_allowlist_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = PoolId::new(unit(0), unit(99)).unwrap();
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000,
        },
    )
    .await;

    let output = AmmOutput::new_swap_v0(
        &kp(9),
        unit(0),
        unit(99),
        Amount::from_msats(10_000),
        Amount::ZERO,
        [8u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::UnknownUnit));
}

/// 8b. `SwapV0` with `unit_in` outside `units` returns `UnknownUnit`. Case 8
///     only covers an unknown `unit_out`; this exercises the separate
///     `cfg.units.get(unit_in)` guard, which was otherwise untested.
///
/// A `Pool` is seeded so `NoSuchPool` cannot pre-empt the check under test —
/// see case 8's doc comment.
#[tokio::test]
async fn swap_with_unknown_unit_in_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = PoolId::new(unit(99), unit(0)).unwrap();
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000,
        },
    )
    .await;

    let output = AmmOutput::new_swap_v0(
        &kp(17),
        unit(99),
        unit(0),
        Amount::from_msats(10_000),
        Amount::ZERO,
        [14u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::UnknownUnit));
}

/// 9. `SwapV0` with `amount_in` below `min_swap_in` returns `BelowMinSwapIn`.
///
/// A `Pool` is seeded so `NoSuchPool` cannot pre-empt the check under test —
/// see case 8's doc comment.
#[tokio::test]
async fn swap_below_min_swap_in_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000_000,
        },
    )
    .await;

    let output = AmmOutput::new_swap_v0(
        &kp(10),
        unit(0),
        unit(1),
        Amount::from_msats(500), // cfg's min_swap_in is 1_000
        Amount::ZERO,
        [9u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::BelowMinSwapIn));
}

/// 10. `SwapV0` that would push a reserve above `MAX_RESERVE` returns
///     `ReserveCapExceeded`, and writes nothing.
#[tokio::test]
async fn swap_that_would_exceed_max_reserve_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    let seeded_pool = Pool {
        reserve_lo: Amount::from_msats(math::MAX_RESERVE),
        reserve_hi: Amount::from_msats(math::MAX_RESERVE),
        total_shares: 1,
    };
    dbtx.insert_new_entry(&PoolKey(pool), &seeded_pool).await;

    let recipient_kp = kp(11);
    let recipient = recipient_kp.public_key();
    let output = AmmOutput::new_swap_v0(
        &recipient_kp,
        unit(0),
        unit(1),
        Amount::from_msats(1_000),
        Amount::ZERO,
        [10u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::ReserveCapExceeded));

    assert_eq!(dbtx.get_value(&PoolKey(pool)).await.unwrap(), seeded_pool);
    assert!(
        dbtx.get_value(&BalanceKey {
            owner: recipient,
            unit: unit(1)
        })
        .await
        .is_none()
    );
}

/// 10b. `DepositV0` that would push a reserve above `MAX_RESERVE` returns
///      `AmmOutputError::ReserveCapExceeded` specifically, not just some
///      `Curve(..)` string. `mint_shares` reports this case as
///      `CurveError::ReserveCapExceeded`; the server must map it to the
///      dedicated output-error variant (the same one the swap path uses in
///      case 10 above) rather than flattening it into `Curve(..)`, so
///      `AmmOutputError::ReserveCapExceeded` is actually reachable on the
///      deposit path.
#[tokio::test]
async fn deposit_that_would_exceed_max_reserve_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    let seeded_pool = Pool {
        reserve_lo: Amount::from_msats(math::MAX_RESERVE),
        reserve_hi: Amount::from_msats(math::MAX_RESERVE),
        total_shares: 1,
    };
    dbtx.insert_new_entry(&PoolKey(pool), &seeded_pool).await;

    let output = AmmOutput::new_deposit_v0(
        &kp(20),
        pool,
        Amount::from_msats(1_000),
        Amount::from_msats(1_000),
        0,
        [0x11u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::ReserveCapExceeded));

    assert_eq!(dbtx.get_value(&PoolKey(pool)).await.unwrap(), seeded_pool);
    assert!(
        dbtx.get_value(&LpPositionKey {
            pool,
            owner: test_pubkey(20)
        })
        .await
        .is_none()
    );
}

/// 10c. Finding I4: a `SwapV0` whose credit would push an existing
///      `BalanceEntry.amount` above `MAX_RESERVE` is rejected with
///      `ReserveCapExceeded`, and writes nothing — neither the pool nor the
///      balance changes. Before the fix, this accumulation was only checked
///      against `u64::MAX`, which made `audit`'s
///      `.expect("bounded by MAX_RESERVE")` on `BalanceEntry.amount`
///      unjustified: nothing in the code actually enforced that bound.
#[tokio::test]
async fn swap_that_would_push_balance_above_max_reserve_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    let reserve_lo = Amount::from_msats(1_000_000_000);
    let reserve_hi = Amount::from_msats(1_000_000);
    let seeded_pool = Pool {
        reserve_lo,
        reserve_hi,
        total_shares: 1_000_000_000,
    };
    dbtx.insert_new_entry(&PoolKey(pool), &seeded_pool).await;

    // Reference vector: this exact swap yields dy = 9_871 (see
    // `swap_moves_reserves_and_credits_balance_by_the_same_dy`). Seed an
    // existing balance just under MAX_RESERVE, close enough that adding
    // 9_871 pushes it over.
    let recipient_kp = kp(18);
    let recipient = recipient_kp.public_key();
    let seeded_balance = BalanceEntry {
        amount: Amount::from_msats(math::MAX_RESERVE - 100),
        tweak: [0x22u8; 16],
    };
    let bkey = BalanceKey {
        owner: recipient,
        unit: unit(1),
    };
    dbtx.insert_new_entry(&bkey, &seeded_balance).await;

    let amount_in = Amount::from_msats(10_000_000);
    let output = AmmOutput::new_swap_v0(
        &recipient_kp,
        unit(0),
        unit(1),
        amount_in,
        Amount::ZERO,
        [0x23u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::ReserveCapExceeded));

    // Writes nothing: neither the pool nor the balance changed.
    assert_eq!(dbtx.get_value(&PoolKey(pool)).await.unwrap(), seeded_pool);
    assert_eq!(dbtx.get_value(&bkey).await.unwrap(), seeded_balance);
}

/// 11. A second `SwapV0` into an existing `(recipient_pk, unit)` ADDS to the
///     balance rather than replacing it.
#[tokio::test]
async fn second_swap_into_existing_balance_adds_rather_than_replaces() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000_000,
        },
    )
    .await;

    // ONE keypair for both swaps: this test exercises the accumulate path
    // (two credits to one balance record), which requires the same
    // `recipient_pk` — and with the PoP, only that key's holder can produce
    // both outputs.
    let recipient_kp = kp(12);
    let recipient = recipient_kp.public_key();
    let amount_in = Amount::from_msats(10_000_000);

    let first = AmmOutput::new_swap_v0(
        &recipient_kp,
        unit(0),
        unit(1),
        amount_in,
        Amount::ZERO,
        [11u8; 16],
    );
    module
        .process_output(&mut dbtx, &first, out_point())
        .await
        .expect("first swap must succeed");
    let balance_after_first = dbtx
        .get_value(&BalanceKey {
            owner: recipient,
            unit: unit(1),
        })
        .await
        .unwrap();
    let dy1 = balance_after_first.amount.msats;

    // Compute the second swap's expected dy from the reserves left by the
    // first swap, so this test doesn't hardcode a derived number.
    let pool_after_first = dbtx.get_value(&PoolKey(pool)).await.unwrap();
    let dy2 = math::amount_out(
        pool_after_first.reserve_lo.msats,
        pool_after_first.reserve_hi.msats,
        amount_in.msats,
        3,
    )
    .unwrap();

    let second = AmmOutput::new_swap_v0(
        &recipient_kp,
        unit(0),
        unit(1),
        amount_in,
        Amount::ZERO,
        [12u8; 16],
    );
    module
        .process_output(&mut dbtx, &second, out_point())
        .await
        .expect("second swap must succeed");

    let balance_after_second = dbtx
        .get_value(&BalanceKey {
            owner: recipient,
            unit: unit(1),
        })
        .await
        .unwrap();
    assert_eq!(balance_after_second.amount.msats, dy1 + dy2);
    // The INCOMING (second) tweak wins on the shared balance record. This
    // used to assert first-writer-wins, which existed to blunt an attacker
    // squatting a victim's key with a garbage tweak — but the proof of
    // possession now makes writing at a key you don't hold impossible, so
    // the first-writer-wins rule was removed: every writer here IS the
    // owner, and the owner's own later write updates the tweak.
    assert_eq!(balance_after_second.tweak, [12u8; 16]);
}

/// 12. A second `SwapV0` into an existing `(recipient_pk, unit)` balance,
///     carrying a DIFFERENT `tweak`, updates the stored tweak to the
///     incoming one.
///
///     This test previously asserted the OPPOSITE (first-writer-wins): the
///     pubkey and tweak on the wire used to be unverified, so anyone could
///     "credit" anyone else's balance with a garbage tweak, and preserving
///     the first tweak was the defence for the victim's seed-only recovery
///     (spec §13). The proof of possession retired that rule — only the
///     key's holder can write at a key at all (see
///     `swap_naming_a_victims_key_is_rejected_and_creates_no_balance`), so
///     every writer here is the owner, and first-writer-wins would now only
///     preserve an owner's own mistake against their own correction. Both
///     swaps are therefore signed by the SAME keypair, and the later write
///     must win.
#[tokio::test]
async fn balance_tweak_is_updated_by_the_owners_later_swap() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000_000,
        },
    )
    .await;

    // ONE keypair for both swaps — the accumulate path needs the same
    // `recipient_pk`, and the PoP means only its holder can produce both.
    let recipient_kp = kp(13);
    let recipient = recipient_kp.public_key();
    let amount_in = Amount::from_msats(10_000_000);
    let tweak_a = [0xAAu8; 16];
    let tweak_b = [0xBBu8; 16];

    let first = AmmOutput::new_swap_v0(
        &recipient_kp,
        unit(0),
        unit(1),
        amount_in,
        Amount::ZERO,
        tweak_a,
    );
    module
        .process_output(&mut dbtx, &first, out_point())
        .await
        .expect("first swap must succeed");
    let dy1 = dbtx
        .get_value(&BalanceKey {
            owner: recipient,
            unit: unit(1),
        })
        .await
        .unwrap()
        .amount
        .msats;

    let pool_after_first = dbtx.get_value(&PoolKey(pool)).await.unwrap();
    let dy2 = math::amount_out(
        pool_after_first.reserve_lo.msats,
        pool_after_first.reserve_hi.msats,
        amount_in.msats,
        3,
    )
    .unwrap();

    // The owner's own second swap: same recipient/unit, different tweak.
    let second = AmmOutput::new_swap_v0(
        &recipient_kp,
        unit(0),
        unit(1),
        amount_in,
        Amount::ZERO,
        tweak_b,
    );
    module
        .process_output(&mut dbtx, &second, out_point())
        .await
        .expect("second swap must succeed");

    let balance = dbtx
        .get_value(&BalanceKey {
            owner: recipient,
            unit: unit(1),
        })
        .await
        .unwrap();
    assert_eq!(balance.amount.msats, dy1 + dy2, "amount must accumulate");
    assert_eq!(
        balance.tweak, tweak_b,
        "the owner's later write must update the tweak — first-writer-wins was retired \
         along with the squatting attack the PoP eliminated"
    );
}

/// 13. A second `DepositV0` into an existing `(pool, owner_pk)` LP position,
///     carrying a DIFFERENT `tweak`, updates the stored tweak to the
///     incoming one. Mirror of case 12 for LP positions — like it, this
///     previously asserted first-writer-wins, which the proof of possession
///     retired (see case 12's doc comment): only the owner can write here,
///     so the owner's own later write must win.
#[tokio::test]
async fn lp_position_tweak_is_updated_by_the_owners_later_deposit() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    // ONE keypair for both deposits — the accumulate path needs the same
    // `owner_pk`, and the PoP means only its holder can produce both.
    let owner_kp = kp(14);
    let owner = owner_kp.public_key();
    let tweak_a = [0xCCu8; 16];
    let tweak_b = [0xDDu8; 16];

    let first = AmmOutput::new_deposit_v0(
        &owner_kp,
        pool,
        Amount::from_msats(1_000_000),
        Amount::from_msats(1_000_000),
        0,
        tweak_a,
    );
    module
        .process_output(&mut dbtx, &first, out_point())
        .await
        .expect("first deposit must succeed");
    let shares_after_first = dbtx
        .get_value(&LpPositionKey { pool, owner })
        .await
        .unwrap()
        .shares;

    // The owner's own second deposit: same pool/owner, different tweak.
    let second = AmmOutput::new_deposit_v0(
        &owner_kp,
        pool,
        Amount::from_msats(100),
        Amount::from_msats(100),
        0,
        tweak_b,
    );
    let result = module
        .process_output(&mut dbtx, &second, out_point())
        .await
        .expect("second deposit must succeed");
    assert!(!result.amounts.is_empty());

    let position = dbtx
        .get_value(&LpPositionKey { pool, owner })
        .await
        .unwrap();
    assert!(
        position.shares > shares_after_first,
        "shares must accumulate: {shares_after_first} -> {}",
        position.shares
    );
    assert_eq!(
        position.tweak, tweak_b,
        "the owner's later write must update the tweak — first-writer-wins was retired \
         along with the squatting attack the PoP eliminated"
    );
}

/// 14. Two `DepositV0`s to the same `(pool, owner_pk)` must not panic — the
///     previous fix pass changed `insert_new_entry` (which panics on a
///     duplicate key) to an accumulate, but shipped no test for it. A panic
///     in `process_output` halts every guardian (it runs inside consensus),
///     so this is a federation-halt regression test, not just a
///     correctness check.
#[tokio::test]
async fn duplicate_owner_deposit_accumulates_without_panicking() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    // ONE keypair for both deposits — the duplicate-owner collision under
    // test needs the same `owner_pk` on both outputs.
    let owner_kp = kp(15);
    let owner = owner_kp.public_key();

    let first = AmmOutput::new_deposit_v0(
        &owner_kp,
        pool,
        Amount::from_msats(1_000_000),
        Amount::from_msats(1_000_000),
        0,
        [0xEEu8; 16],
    );
    module
        .process_output(&mut dbtx, &first, out_point())
        .await
        .expect("first deposit must succeed");
    let shares_after_first = dbtx
        .get_value(&LpPositionKey { pool, owner })
        .await
        .unwrap()
        .shares;
    let pool_after_first = dbtx.get_value(&PoolKey(pool)).await.unwrap();

    let second = AmmOutput::new_deposit_v0(
        &owner_kp,
        pool,
        Amount::from_msats(100),
        Amount::from_msats(100),
        0,
        [0xFFu8; 16],
    );
    // Must return Ok, not panic, despite `owner_pk` colliding with an
    // existing LP position.
    let result = module.process_output(&mut dbtx, &second, out_point()).await;
    assert!(
        result.is_ok(),
        "duplicate-owner deposit must not error, got {result:?}"
    );

    let position = dbtx
        .get_value(&LpPositionKey { pool, owner })
        .await
        .unwrap();
    let expected_second_mint = math::mint_shares(
        pool_after_first.reserve_lo.msats,
        pool_after_first.reserve_hi.msats,
        pool_after_first.total_shares,
        100,
        100,
    )
    .unwrap()
    .to_owner;
    assert_eq!(position.shares, shares_after_first + expected_second_mint);
}

/// 15. `AmmOutput::Default { .. }` — the catch-all for a wire variant this
///     binary doesn't understand — is rejected, never accepted or panicked
///     on. Since the PoP gate runs before the variant match and `Default`
///     has no key to prove (`AmmOutput::verify_pop` returns `false` for it),
///     the rejection now surfaces as `InvalidProofOfPossession` rather than
///     the old `UnknownVariant`; both are rejections, and `process_output`'s
///     own comment documents this deliberately.
#[tokio::test]
async fn default_output_variant_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();

    let output = AmmOutput::Default {
        variant: 42,
        bytes: vec![1, 2, 3],
    };

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::InvalidProofOfPossession));
}

/// 16. Final-review finding T1: `cfg.fee_overrides` must actually reach
///     settlement, not just `AmmConfigConsensus::fee_for` in isolation
///     (already covered at the config level). Every other test in this file
///     uses an empty `fee_overrides`, so this pins a config with a
///     per-pool override that is FAR from `default_fee_per_mille` (100
///     vs. 3) and asserts the swap settles at the OVERRIDE fee: `expected_dy`
///     is computed by calling `math::amount_out` directly with the literal
///     override fee (100) baked in here, never by reading `cfg.fee_for` or
///     any other value the module under test could also get wrong the same
///     way — so a settlement path that silently fell back to
///     `default_fee_per_mille` (mutating `cfg.fee_for(pool_id)` to
///     `cfg.default_fee_per_mille`) would settle at fee 3 instead, landing
///     on a different `dy` and failing this assertion.
#[tokio::test]
async fn swap_settles_at_the_pool_fee_override_not_the_default() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let pool = pool01();

    let module = Amm::new(
        AmmConfigConsensus {
            units: BTreeMap::from([
                (
                    unit(0),
                    UnitParams {
                        min_swap_in: Amount::from_msats(1_000),
                    },
                ),
                (
                    unit(1),
                    UnitParams {
                        min_swap_in: Amount::from_msats(1_000),
                    },
                ),
            ]),
            default_fee_per_mille: 3,
            fee_overrides: BTreeMap::from([(pool, 100)]),
            // Wide open: these tests predate the guardian-voted fee and assert
            // curve and settlement behaviour, so the band must not be the thing
            // that changes their effective fee. The band itself is exercised by
            // the fee-vote tests.
            min_fee_per_mille: 0,
            max_fee_per_mille: 999,
        },
        NumPeers::from(4),
        PeerId::from(0),
    );

    // Same reserves as the reference-vector tests, so the ONLY thing that
    // could make `expected_dy` differ from the well-known fee-3 answer
    // (9_871) is the fee actually applied.
    let reserve_lo = Amount::from_msats(1_000_000_000);
    let reserve_hi = Amount::from_msats(1_000_000);
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo,
            reserve_hi,
            total_shares: 1_000_000_000,
        },
    )
    .await;

    let amount_in = Amount::from_msats(10_000_000);
    let expected_dy = math::amount_out(reserve_lo.msats, reserve_hi.msats, amount_in.msats, 100)
        .expect("override-fee amount_out must compute cleanly");
    // Sanity: this really is a different number from the fee-3 reference
    // vector (9_871), so a settlement that used the wrong fee would be
    // caught, not accidentally agree by coincidence.
    assert_ne!(expected_dy, 9_871);

    let output = AmmOutput::new_swap_v0(
        &kp(20),
        unit(0),
        unit(1),
        amount_in,
        Amount::ZERO,
        [20u8; 16],
    );

    let result = module
        .process_output(&mut dbtx, &output, out_point())
        .await
        .expect("swap at the override fee must succeed");
    assert_eq!(result.amounts.get(&unit(0)), Some(&amount_in));

    let balance = dbtx
        .get_value(&BalanceKey {
            owner: test_pubkey(20),
            unit: unit(1),
        })
        .await
        .expect("balance must be credited");
    assert_eq!(
        balance.amount.msats, expected_dy,
        "settlement must use the pool's fee OVERRIDE (100), not default_fee_per_mille (3)"
    );
}

/// 17. Final-review finding T2: `fee_per_mille == 0` is a legal config
///     (`validate()` only rejects `fee >= 1000`), and at fee 0 an
///     exactly-dividing swap hits `k_new == k_old` EXACTLY. This is the
///     case that distinguishes the correct `k_non_decreasing`'s `new >= old`
///     from a wrongly-tightened `new > old`: under the mutant, this
///     legitimate swap would be wrongly rejected with `KInvariantViolated`.
///     No other server-side test in this suite uses any fee but 3, so this
///     is also the only test that exercises `fee_per_mille == 0` at all.
///
///     reserve_in 1_000, reserve_out 1_000, amount_in 1_000, fee 0:
///     in_with_fee = 1_000 * 1_000 = 1_000_000; numerator = 1_000_000 *
///     1_000 = 1_000_000_000; denominator = 1_000*1_000 + 1_000_000 =
///     2_000_000; out = 500. k_old = 1_000*1_000 = 1_000_000; k_new =
///     (1_000+1_000)*(1_000-500) = 2_000*500 = 1_000_000 — unchanged.
#[tokio::test]
async fn swap_with_fee_zero_and_exact_division_is_accepted() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let pool = pool01();

    let module = Amm::new(
        AmmConfigConsensus {
            units: BTreeMap::from([
                (
                    unit(0),
                    UnitParams {
                        min_swap_in: Amount::from_msats(1),
                    },
                ),
                (
                    unit(1),
                    UnitParams {
                        min_swap_in: Amount::from_msats(1),
                    },
                ),
            ]),
            default_fee_per_mille: 0,
            fee_overrides: BTreeMap::new(),
            // Wide open: these tests predate the guardian-voted fee and assert
            // curve and settlement behaviour, so the band must not be the thing
            // that changes their effective fee. The band itself is exercised by
            // the fee-vote tests.
            min_fee_per_mille: 0,
            max_fee_per_mille: 999,
        },
        NumPeers::from(4),
        PeerId::from(0),
    );

    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000),
            reserve_hi: Amount::from_msats(1_000),
            total_shares: 1_000,
        },
    )
    .await;

    let output = AmmOutput::new_swap_v0(
        &kp(21),
        unit(0),
        unit(1),
        Amount::from_msats(1_000),
        Amount::ZERO,
        [21u8; 16],
    );

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(
        result.map(|r| r.amounts.get(&unit(0)).copied()),
        Ok(Some(Amount::from_msats(1_000))),
        "a fee-0 exactly-dividing swap (k_new == k_old exactly) must be ACCEPTED, not rejected \
         with KInvariantViolated"
    );

    let balance = dbtx
        .get_value(&BalanceKey {
            owner: test_pubkey(21),
            unit: unit(1),
        })
        .await
        .expect("balance must be credited");
    assert_eq!(balance.amount, Amount::from_msats(500));
}

/// 18. Final-review finding T3: a later deposit too small to mint even one
///     share must be REJECTED (`OutputRoundsToZero`), not silently accepted
///     with `to_owner = 0` — which would still grow the reserves by the
///     full deposit and write a 0-share `LpPosition`, donating the
///     depositor's funds to existing LPs with no error. Seeds a live pool
///     directly (as e.g. `swap_moves_reserves_and_credits_balance_by_the_same_dy`
///     does) with `total_shares` (1) far below `reserve_lo`
///     (1_000_000), so `via_lo = floor(amount_lo * total_shares /
///     reserve_lo) = floor(1 * 1 / 1_000_000) = 0` — with a huge
///     `amount_hi` alongside it (so `via_hi` is never the binding minimum),
///     isolating the `via_lo` floor as the sole cause of `minted == 0`.
///     Same fixture as `fedimint-amm-common`'s
///     `later_mint_rejects_a_deposit_too_small_to_mint_any_shares`, driven
///     here through the real `process_output` path instead of `math::mint_shares`
///     directly.
#[tokio::test]
async fn later_deposit_too_small_to_mint_any_shares_is_rejected_and_writes_nothing() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner_kp = kp(22);
    let owner = owner_kp.public_key();

    let seeded_pool = Pool {
        reserve_lo: Amount::from_msats(1_000_000),
        reserve_hi: Amount::from_msats(1_000_000),
        total_shares: 1,
    };
    dbtx.insert_new_entry(&PoolKey(pool), &seeded_pool).await;

    let output = AmmOutput::new_deposit_v0(
        &owner_kp,
        pool,
        Amount::from_msats(1),
        Amount::from_msats(1_000_000_000),
        0,
        [22u8; 16],
    );
    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(
        result,
        Err(AmmOutputError::Curve(
            math::CurveError::OutputRoundsToZero.to_string()
        )),
        "a deposit that mints zero shares must be rejected, never silently accepted"
    );

    // Nothing changed: no reserves grown, no LP position for the rejected
    // depositor — the donation this guard exists to prevent.
    let pool_after = dbtx.get_value(&PoolKey(pool)).await.unwrap();
    assert_eq!(
        seeded_pool, pool_after,
        "a rejected deposit must not grow the reserves"
    );
    assert!(
        dbtx.get_value(&LpPositionKey { pool, owner })
            .await
            .is_none(),
        "a rejected deposit must not create a 0-share LpPosition"
    );
}

/// 19. The squatting attack is dead, deposit side: an attacker who does NOT
///     hold the victim's key builds a `DepositV0` naming the VICTIM's
///     `owner_pk` with a garbage tweak, signing the PoP with the ATTACKER's
///     own key. `process_output` must reject it with
///     `InvalidProofOfPossession` — and, just as importantly, must leave NO
///     `LpPosition` row (and no `Pool`) behind for the victim's key: the
///     attack was never the rejection message, it was the record with the
///     garbage tweak that destroyed the victim's seed-only recovery.
///     Case 21 is the positive control proving this pool/amount fixture
///     would otherwise succeed.
#[tokio::test]
async fn deposit_naming_a_victims_key_is_rejected_and_creates_no_position() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    let attacker_kp = kp(30);
    let victim = test_pubkey(31);
    let garbage_tweak = [0xF0u8; 16];

    // The attacker's best effort: a REAL signature by the attacker's key
    // over the exact message the output claims — everything is right except
    // that the signing key is not the named key.
    let forged_pop = pop::sign_pop(
        &attacker_kp,
        &pop::deposit_pop_message(
            pool,
            Amount::from_msats(1_000_000),
            Amount::from_msats(1_000_000),
            0,
            victim,
            garbage_tweak,
        ),
    );
    let output = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(1_000_000),
        amount_hi: Amount::from_msats(1_000_000),
        min_shares: 0,
        owner_pk: victim,
        tweak: garbage_tweak,
        pop: forged_pop,
    };

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::InvalidProofOfPossession));

    // The other half of the property: no state at the victim's key. A
    // rejection that still wrote the LpPosition would leave the squatting
    // attack fully intact.
    assert!(
        dbtx.get_value(&LpPositionKey {
            pool,
            owner: victim
        })
        .await
        .is_none(),
        "a rejected deposit must leave no LpPosition at the victim's key"
    );
    assert!(
        dbtx.get_value(&PoolKey(pool)).await.is_none(),
        "a rejected deposit must not create the pool either"
    );
}

/// 20. The squatting attack is dead, swap side: same as case 19 but for
///     `SwapV0` / the `Balance` table. The pool is seeded and the swap is
///     otherwise perfectly valid (case 21 proves it), so the PoP check is
///     the ONLY thing standing between the attacker and a `BalanceEntry`
///     with a garbage tweak at the victim's key.
#[tokio::test]
async fn swap_naming_a_victims_key_is_rejected_and_creates_no_balance() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    let seeded_pool = Pool {
        reserve_lo: Amount::from_msats(1_000_000_000),
        reserve_hi: Amount::from_msats(1_000_000),
        total_shares: 1_000_000_000,
    };
    dbtx.insert_new_entry(&PoolKey(pool), &seeded_pool).await;

    let attacker_kp = kp(32);
    let victim = test_pubkey(33);
    let garbage_tweak = [0xF1u8; 16];
    let amount_in = Amount::from_msats(10_000_000);

    let forged_pop = pop::sign_pop(
        &attacker_kp,
        &pop::swap_pop_message(
            unit(0),
            unit(1),
            amount_in,
            Amount::ZERO,
            victim,
            garbage_tweak,
        ),
    );
    let output = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in,
        min_out: Amount::ZERO,
        recipient_pk: victim,
        tweak: garbage_tweak,
        pop: forged_pop,
    };

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::InvalidProofOfPossession));

    // No state at the victim's key, and the pool untouched — the rejection
    // happens before any DB write.
    assert!(
        dbtx.get_value(&BalanceKey {
            owner: victim,
            unit: unit(1)
        })
        .await
        .is_none(),
        "a rejected swap must leave no Balance at the victim's key"
    );
    assert_eq!(
        dbtx.get_value(&PoolKey(pool)).await.unwrap(),
        seeded_pool,
        "a rejected swap must not move the reserves"
    );
}

/// 21. Positive control for cases 19 and 20: the SAME outputs, field for
///     field, but signed by the key they name, succeed and create the
///     records. Without this, 19/20 would pass trivially under a
///     reject-everything `process_output`.
#[tokio::test]
async fn the_same_outputs_signed_by_their_own_key_succeed() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    // Case 19's deposit, owner-signed.
    let owner_kp = kp(31);
    let deposit = AmmOutput::new_deposit_v0(
        &owner_kp,
        pool,
        Amount::from_msats(1_000_000),
        Amount::from_msats(1_000_000),
        0,
        [0xF0u8; 16],
    );
    module
        .process_output(&mut dbtx, &deposit, out_point())
        .await
        .expect("the owner-signed twin of case 19's deposit must succeed");
    let position = dbtx
        .get_value(&LpPositionKey {
            pool,
            owner: owner_kp.public_key(),
        })
        .await
        .expect("the owner-signed deposit must create the LpPosition");
    assert_eq!(position.tweak, [0xF0u8; 16]);

    // Case 20's swap, recipient-signed (the deposit above created the pool,
    // at different reserves than case 20's seed — irrelevant to whether the
    // output is admitted).
    let recipient_kp = kp(33);
    let swap = AmmOutput::new_swap_v0(
        &recipient_kp,
        unit(0),
        unit(1),
        Amount::from_msats(10_000),
        Amount::ZERO,
        [0xF1u8; 16],
    );
    module
        .process_output(&mut dbtx, &swap, out_point())
        .await
        .expect("the recipient-signed twin of case 20's swap must succeed");
    let balance = dbtx
        .get_value(&BalanceKey {
            owner: recipient_kp.public_key(),
            unit: unit(1),
        })
        .await
        .expect("the recipient-signed swap must create the Balance");
    assert_eq!(balance.tweak, [0xF1u8; 16]);
}

/// 22. Transplant resistance at the `process_output` level (the unit-level
///     twin lives in `fedimint-amm-common`'s `pop` tests): a PoP that IS
///     valid — made by the right key over the right fields — for tweak A
///     does not admit the same output re-paired with tweak B. This is the
///     lift-a-pending-transaction's-signature variant of the squatting
///     attack, and it too must leave no state behind.
#[tokio::test]
async fn a_pop_transplanted_to_a_different_tweak_is_rejected_and_writes_nothing() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    let seeded_pool = Pool {
        reserve_lo: Amount::from_msats(1_000_000_000),
        reserve_hi: Amount::from_msats(1_000_000),
        total_shares: 1_000_000_000,
    };
    dbtx.insert_new_entry(&PoolKey(pool), &seeded_pool).await;

    let owner_kp = kp(34);
    let owner = owner_kp.public_key();
    let honest_tweak = [0x01u8; 16];
    let garbage_tweak = [0xFFu8; 16];
    let amount_in = Amount::from_msats(10_000_000);

    // Swap: lift the pop off an honest output, re-pair it with a garbage
    // tweak.
    let honest_swap = AmmOutput::new_swap_v0(
        &owner_kp,
        unit(0),
        unit(1),
        amount_in,
        Amount::ZERO,
        honest_tweak,
    );
    let AmmOutput::SwapV0 { pop, .. } = honest_swap else {
        panic!("new_swap_v0 builds a SwapV0");
    };
    let transplanted_swap = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in,
        min_out: Amount::ZERO,
        recipient_pk: owner,
        tweak: garbage_tweak,
        pop,
    };
    let result = module
        .process_output(&mut dbtx, &transplanted_swap, out_point())
        .await;
    assert_eq!(result, Err(AmmOutputError::InvalidProofOfPossession));
    assert!(
        dbtx.get_value(&BalanceKey {
            owner,
            unit: unit(1)
        })
        .await
        .is_none(),
        "a transplanted swap PoP must leave no Balance behind"
    );
    assert_eq!(
        dbtx.get_value(&PoolKey(pool)).await.unwrap(),
        seeded_pool,
        "a transplanted swap PoP must not move the reserves"
    );

    // Deposit: same lift-and-re-pair, against the LpPosition table.
    let honest_deposit = AmmOutput::new_deposit_v0(
        &owner_kp,
        pool,
        Amount::from_msats(1_000_000),
        Amount::from_msats(1_000_000),
        0,
        honest_tweak,
    );
    let AmmOutput::DepositV0 { pop, .. } = honest_deposit else {
        panic!("new_deposit_v0 builds a DepositV0");
    };
    let transplanted_deposit = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(1_000_000),
        amount_hi: Amount::from_msats(1_000_000),
        min_shares: 0,
        owner_pk: owner,
        tweak: garbage_tweak,
        pop,
    };
    let result = module
        .process_output(&mut dbtx, &transplanted_deposit, out_point())
        .await;
    assert_eq!(result, Err(AmmOutputError::InvalidProofOfPossession));
    assert!(
        dbtx.get_value(&LpPositionKey { pool, owner })
            .await
            .is_none(),
        "a transplanted deposit PoP must leave no LpPosition behind"
    );
}
