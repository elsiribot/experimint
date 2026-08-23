//! `process_input` — `ClaimBalanceV0` and `WithdrawV0`. Spec §6, §6.1, §7.3.
//!
//! Each case is its own `#[tokio::test]` with real assertions, so a failure
//! names the case directly rather than a table index.

use std::collections::BTreeMap;

use fedimint_amm_common::config::{AmmConfigConsensus, UnitParams};
use fedimint_amm_common::math::{self, MINIMUM_LIQUIDITY};
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_common::types::{AmmInput, AmmInputError};
use fedimint_amm_server::Amm;
use fedimint_amm_server::db::{BalanceEntry, BalanceKey, LpPosition, LpPositionKey, Pool, PoolKey};
use fedimint_core::db::mem_impl::MemDatabase;
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::module::AmountUnit;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::secp256k1::{self, Keypair, SECP256K1};
use fedimint_core::{Amount, BitcoinHash, InPoint, TransactionId};
use fedimint_server_core::ServerModule;

fn db() -> Database {
    Database::new(MemDatabase::new(), ModuleDecoderRegistry::default())
}

fn unit(n: u64) -> AmountUnit {
    AmountUnit::new_custom(n)
}

fn test_pubkey(seed: u8) -> secp256k1::PublicKey {
    Keypair::from_seckey_slice(SECP256K1, &[seed; 32])
        .expect("a repeated non-zero byte is a valid secret key")
        .public_key()
}

fn in_point() -> InPoint {
    InPoint {
        txid: TransactionId::all_zeros(),
        in_idx: 0,
    }
}

/// Units 0 and 1 are in the allowlist with `min_swap_in` 1_000 msats each;
/// default fee 3/1000 (0.30%), matching the reference Uniswap V2 fee. The
/// config isn't actually consulted by `process_input` (unlike
/// `process_output`), but it mirrors `output.rs`'s `amm()` so both test files
/// build the module identically.
fn amm() -> Amm {
    Amm::new(AmmConfigConsensus {
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
    })
}

fn pool01() -> PoolId {
    PoolId::new(unit(0), unit(1)).unwrap()
}

/// 1. `ClaimBalanceV0` for a non-existent balance returns `NoSuchBalance`.
#[tokio::test]
async fn claim_balance_for_nonexistent_balance_fails() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let owner = test_pubkey(1);

    let input = AmmInput::ClaimBalanceV0 {
        pubkey: owner,
        unit: unit(0),
    };

    let result = module.process_input(&mut dbtx, &input, in_point()).await;
    assert_eq!(result, Err(AmmInputError::NoSuchBalance));
}

/// 2. `ClaimBalanceV0` returns `amounts == {unit: stored}`, `pub_key ==
///    pubkey`, and DELETES the record.
#[tokio::test]
async fn claim_balance_returns_stored_amount_and_deletes_record() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let owner = test_pubkey(2);
    let key = BalanceKey {
        owner,
        unit: unit(0),
    };
    dbtx.insert_new_entry(
        &key,
        &BalanceEntry {
            amount: Amount::from_msats(12_345),
            tweak: [7u8; 16],
        },
    )
    .await;

    let input = AmmInput::ClaimBalanceV0 {
        pubkey: owner,
        unit: unit(0),
    };

    let result = module
        .process_input(&mut dbtx, &input, in_point())
        .await
        .expect("claiming an existing balance must succeed");

    assert_eq!(result.pub_key, owner);
    assert_eq!(
        result.amount.amounts.get(&unit(0)),
        Some(&Amount::from_msats(12_345))
    );
    assert_eq!(result.amount.amounts.len(), 1);
    assert!(result.amount.fees.is_empty());

    assert!(
        dbtx.get_value(&key).await.is_none(),
        "the balance record must be deleted on claim"
    );
}

/// 3. A second `ClaimBalanceV0` for the same key now returns `NoSuchBalance`
///    — the retry-safety property from spec §6.1.
#[tokio::test]
async fn second_claim_of_same_balance_fails() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let owner = test_pubkey(3);
    let key = BalanceKey {
        owner,
        unit: unit(0),
    };
    dbtx.insert_new_entry(
        &key,
        &BalanceEntry {
            amount: Amount::from_msats(500),
            tweak: [1u8; 16],
        },
    )
    .await;

    let input = AmmInput::ClaimBalanceV0 {
        pubkey: owner,
        unit: unit(0),
    };

    module
        .process_input(&mut dbtx, &input, in_point())
        .await
        .expect("first claim must succeed");

    let result = module.process_input(&mut dbtx, &input, in_point()).await;
    assert_eq!(result, Err(AmmInputError::NoSuchBalance));
}

/// 4. `WithdrawV0` for an unknown pool returns `NoSuchPool`.
#[tokio::test]
async fn withdraw_from_unknown_pool_fails() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();

    let input = AmmInput::WithdrawV0 {
        pool: pool01(),
        owner_pk: test_pubkey(4),
        shares: 100,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };

    let result = module.process_input(&mut dbtx, &input, in_point()).await;
    assert_eq!(result, Err(AmmInputError::NoSuchPool));
}

/// 5. `WithdrawV0` for an unknown position returns `NoSuchPosition`.
#[tokio::test]
async fn withdraw_with_unknown_position_fails() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000,
        },
    )
    .await;

    let input = AmmInput::WithdrawV0 {
        pool,
        owner_pk: test_pubkey(5),
        shares: 100,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };

    let result = module.process_input(&mut dbtx, &input, in_point()).await;
    assert_eq!(result, Err(AmmInputError::NoSuchPosition));
}

/// 6. `WithdrawV0` for more shares than held returns `InsufficientShares`.
#[tokio::test]
async fn withdraw_more_shares_than_held_fails() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner = test_pubkey(6);

    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000,
        },
    )
    .await;
    dbtx.insert_new_entry(
        &LpPositionKey { pool, owner },
        &LpPosition {
            shares: 100,
            tweak: [2u8; 16],
        },
    )
    .await;

    let input = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner,
        shares: 101,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };

    let result = module.process_input(&mut dbtx, &input, in_point()).await;
    assert_eq!(result, Err(AmmInputError::InsufficientShares));
}

/// 7. `WithdrawV0` of a partial position debits reserves and
///    `total_shares`, decrements the position, and KEEPS the record.
#[tokio::test]
async fn partial_withdraw_debits_reserves_and_keeps_position() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner = test_pubkey(7);

    let reserve_lo = Amount::from_msats(1_001_000);
    let reserve_hi = Amount::from_msats(1_001_000);
    let total_shares = 1_000_000;
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo,
            reserve_hi,
            total_shares,
        },
    )
    .await;
    dbtx.insert_new_entry(
        &LpPositionKey { pool, owner },
        &LpPosition {
            shares: 1_000,
            tweak: [3u8; 16],
        },
    )
    .await;

    let expected = math::burn_shares(reserve_lo.msats, reserve_hi.msats, total_shares, 500)
        .expect("burn must succeed for this fixture");

    let input = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner,
        shares: 500,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };

    let result = module
        .process_input(&mut dbtx, &input, in_point())
        .await
        .expect("partial withdraw must succeed");

    assert_eq!(result.pub_key, owner);
    assert_eq!(
        result.amount.amounts.get(&pool.lo()),
        Some(&Amount::from_msats(expected.da))
    );
    assert_eq!(
        result.amount.amounts.get(&pool.hi()),
        Some(&Amount::from_msats(expected.db))
    );

    let stored_pool = dbtx.get_value(&PoolKey(pool)).await.unwrap();
    assert_eq!(
        stored_pool.reserve_lo,
        Amount::from_msats(reserve_lo.msats - expected.da)
    );
    assert_eq!(
        stored_pool.reserve_hi,
        Amount::from_msats(reserve_hi.msats - expected.db)
    );
    assert_eq!(stored_pool.total_shares, expected.new_total_shares);

    let position = dbtx
        .get_value(&LpPositionKey { pool, owner })
        .await
        .expect("position must still exist after a partial withdraw");
    assert_eq!(position.shares, 500);
    assert_eq!(position.tweak, [3u8; 16]);
}

/// 8. `WithdrawV0` of the full position DELETES the record.
#[tokio::test]
async fn full_withdraw_deletes_position() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner = test_pubkey(8);

    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_001_000),
            reserve_hi: Amount::from_msats(1_001_000),
            total_shares: 1_000_000,
        },
    )
    .await;
    dbtx.insert_new_entry(
        &LpPositionKey { pool, owner },
        &LpPosition {
            shares: 1_000,
            tweak: [4u8; 16],
        },
    )
    .await;

    let input = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner,
        shares: 1_000,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };

    module
        .process_input(&mut dbtx, &input, in_point())
        .await
        .expect("full withdraw must succeed");

    assert!(
        dbtx.get_value(&LpPositionKey { pool, owner }).await.is_none(),
        "an emptied position must be deleted, not left as a zero-share record"
    );
}

/// 9. `WithdrawV0` whose payout is below `min_lo`/`min_hi` returns
///    `SlippageExceeded` and writes nothing.
#[tokio::test]
async fn withdraw_below_min_lo_hi_fails_and_writes_nothing() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner = test_pubkey(9);

    let seeded_pool = Pool {
        reserve_lo: Amount::from_msats(1_001_000),
        reserve_hi: Amount::from_msats(1_001_000),
        total_shares: 1_000_000,
    };
    dbtx.insert_new_entry(&PoolKey(pool), &seeded_pool).await;
    let seeded_position = LpPosition {
        shares: 1_000,
        tweak: [5u8; 16],
    };
    dbtx.insert_new_entry(&LpPositionKey { pool, owner }, &seeded_position)
        .await;

    let expected = math::burn_shares(
        seeded_pool.reserve_lo.msats,
        seeded_pool.reserve_hi.msats,
        seeded_pool.total_shares,
        500,
    )
    .unwrap();

    let input = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner,
        shares: 500,
        min_lo: Amount::from_msats(expected.da + 1),
        min_hi: Amount::ZERO,
    };

    let result = module.process_input(&mut dbtx, &input, in_point()).await;
    assert_eq!(result, Err(AmmInputError::SlippageExceeded));

    assert_eq!(dbtx.get_value(&PoolKey(pool)).await.unwrap(), seeded_pool);
    assert_eq!(
        dbtx.get_value(&LpPositionKey { pool, owner }).await.unwrap(),
        seeded_position
    );
}

/// 10. `WithdrawV0` whose payout floors to zero on BOTH legs is rejected.
#[tokio::test]
async fn withdraw_that_floors_to_zero_on_both_legs_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner = test_pubkey(10);

    // Mirrors math.rs's `burn_rejects_a_position_too_small_to_pay_anything`:
    // 1 share out of 1_000_000, pool 10 : 10 -> both legs floor to 0.
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(10),
            reserve_hi: Amount::from_msats(10),
            total_shares: 1_000_000,
        },
    )
    .await;
    dbtx.insert_new_entry(
        &LpPositionKey { pool, owner },
        &LpPosition {
            shares: 1,
            tweak: [6u8; 16],
        },
    )
    .await;

    let input = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner,
        shares: 1,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };

    let result = module.process_input(&mut dbtx, &input, in_point()).await;
    assert_eq!(
        result,
        Err(AmmInputError::Curve(
            math::CurveError::OutputRoundsToZero.to_string()
        ))
    );

    // Nothing was written.
    assert_eq!(
        dbtx.get_value(&LpPositionKey { pool, owner })
            .await
            .unwrap()
            .shares,
        1
    );
}

/// 11. A leg that floors to zero is OMITTED from `amounts`, not present as
///     an explicit zero (spec P3 — `Amounts` never stores zero entries).
#[tokio::test]
async fn zero_leg_is_omitted_from_amounts() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner = test_pubkey(11);

    // reserve_lo huge, reserve_hi tiny: `db` floors to 0 while `da` doesn't.
    let reserve_lo = Amount::from_msats(1_000_000_000);
    let reserve_hi = Amount::from_msats(10);
    let total_shares = 1_000_000;
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo,
            reserve_hi,
            total_shares,
        },
    )
    .await;
    dbtx.insert_new_entry(
        &LpPositionKey { pool, owner },
        &LpPosition {
            shares: 1,
            tweak: [8u8; 16],
        },
    )
    .await;

    let expected = math::burn_shares(reserve_lo.msats, reserve_hi.msats, total_shares, 1).unwrap();
    assert!(expected.da > 0);
    assert_eq!(expected.db, 0, "fixture must exercise the zero-leg path");

    let input = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner,
        shares: 1,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };

    let result = module
        .process_input(&mut dbtx, &input, in_point())
        .await
        .expect("a single non-zero leg must still succeed");

    assert_eq!(
        result.amount.amounts.get(&pool.lo()),
        Some(&Amount::from_msats(expected.da))
    );
    assert_eq!(
        result.amount.amounts.get(&pool.hi()),
        None,
        "the zero leg must be OMITTED, not present as an explicit zero"
    );
    assert_eq!(result.amount.amounts.len(), 1);

    // The pool is asymmetric (`reserve_lo != reserve_hi`), so a mutant that
    // swaps the two reserve debits (`reserve_lo -= db` / `reserve_hi -= da`)
    // would go undetected by the `amounts` assertions above alone. Pin the
    // stored reserves too, with the expected numbers hard-coded (not
    // recomputed from `burn_shares`, which is exactly what a debit-orientation
    // mutant would still agree with): 1_000_000_000 * 1 / 1_000_000 = 1_000,
    // 10 * 1 / 1_000_000 floors to 0.
    assert_eq!(expected.da, 1_000);
    let stored_pool = dbtx
        .get_value(&PoolKey(pool))
        .await
        .expect("pool must still exist after withdrawal");
    assert_eq!(
        stored_pool.reserve_lo,
        Amount::from_msats(1_000_000_000 - 1_000),
        "reserve_lo must be debited by da"
    );
    assert_eq!(
        stored_pool.reserve_hi,
        Amount::from_msats(10),
        "reserve_hi must be debited by db (which is 0 here), not by da"
    );
}

/// 12. `total_shares` never reaches zero even after every assigned position
///     is withdrawn — `MINIMUM_LIQUIDITY` remains unassigned and
///     unwithdrawable forever.
#[tokio::test]
async fn total_shares_never_reaches_zero_after_full_withdrawal() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner = test_pubkey(12);

    // Pool created by a single deposit of 1_000_000 : 1_000_000: total_shares
    // == 1_000_000, all but MINIMUM_LIQUIDITY assigned to `owner`.
    let total_shares = 1_000_000u64;
    let owner_shares = total_shares - MINIMUM_LIQUIDITY;
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares,
        },
    )
    .await;
    dbtx.insert_new_entry(
        &LpPositionKey { pool, owner },
        &LpPosition {
            shares: owner_shares,
            tweak: [9u8; 16],
        },
    )
    .await;

    let input = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner,
        shares: owner_shares,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };

    module
        .process_input(&mut dbtx, &input, in_point())
        .await
        .expect("withdrawing the entire assigned position must succeed");

    let stored_pool = dbtx
        .get_value(&PoolKey(pool))
        .await
        .expect("the Pool record itself is never deleted");
    assert_eq!(stored_pool.total_shares, MINIMUM_LIQUIDITY);
    assert!(stored_pool.total_shares > 0);

    assert!(
        dbtx.get_value(&LpPositionKey { pool, owner }).await.is_none(),
        "the emptied position must be deleted"
    );
}

/// 13. `WithdrawV0` whose payout is below `min_hi` ONLY (`min_lo` is zero)
///     still returns `SlippageExceeded` and writes nothing. The existing
///     `min_lo`/`min_hi` coverage (case 9) only ever trips `min_lo` — this
///     pins the second operand of `outcome.db < min_hi.msats` so a mutant
///     that mispairs it (e.g. checking `outcome.db < min_lo.msats`) fails
///     the suite.
#[tokio::test]
async fn withdraw_below_min_hi_only_fails_and_writes_nothing() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner = test_pubkey(13);

    let seeded_pool = Pool {
        reserve_lo: Amount::from_msats(1_001_000),
        reserve_hi: Amount::from_msats(1_001_000),
        total_shares: 1_000_000,
    };
    dbtx.insert_new_entry(&PoolKey(pool), &seeded_pool).await;
    let seeded_position = LpPosition {
        shares: 1_000,
        tweak: [13u8; 16],
    };
    dbtx.insert_new_entry(&LpPositionKey { pool, owner }, &seeded_position)
        .await;

    let expected = math::burn_shares(
        seeded_pool.reserve_lo.msats,
        seeded_pool.reserve_hi.msats,
        seeded_pool.total_shares,
        500,
    )
    .unwrap();

    let input = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner,
        shares: 500,
        min_lo: Amount::ZERO,
        min_hi: Amount::from_msats(expected.db + 1),
    };

    let result = module.process_input(&mut dbtx, &input, in_point()).await;
    assert_eq!(result, Err(AmmInputError::SlippageExceeded));

    assert_eq!(dbtx.get_value(&PoolKey(pool)).await.unwrap(), seeded_pool);
    assert_eq!(
        dbtx.get_value(&LpPositionKey { pool, owner }).await.unwrap(),
        seeded_position
    );
}

/// 14. `AmmInput::Default { variant, bytes }` is rejected as
///     `UnknownVariant`, mirroring `output.rs`'s
///     `default_output_variant_is_rejected_as_unknown`.
#[tokio::test]
async fn default_input_variant_is_rejected_as_unknown() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();

    let input = AmmInput::Default {
        variant: 42,
        bytes: vec![1, 2, 3],
    };

    let result = module.process_input(&mut dbtx, &input, in_point()).await;
    assert_eq!(result, Err(AmmInputError::UnknownVariant));
}

/// 15. `WithdrawV0` with `shares == 0` is rejected. Spec §7.3 requires
///     `0 < shares`; the implementation has no dedicated check for this —
///     it relies on `burn_shares`'s `shares == 0 -> CurveError::ZeroAmount`
///     guard. Pin the exact error so that guard stays covered even without
///     a dedicated variant.
#[tokio::test]
async fn withdraw_zero_shares_fails() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner = test_pubkey(14);

    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000,
        },
    )
    .await;
    dbtx.insert_new_entry(
        &LpPositionKey { pool, owner },
        &LpPosition {
            shares: 100,
            tweak: [14u8; 16],
        },
    )
    .await;

    let input = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner,
        shares: 0,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };

    let result = module.process_input(&mut dbtx, &input, in_point()).await;
    assert_eq!(
        result,
        Err(AmmInputError::Curve(math::CurveError::ZeroAmount.to_string()))
    );
}
