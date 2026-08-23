//! `process_output` — `SwapV0` and `DepositV0`. Spec §7, §7.4.
//!
//! Each case is its own `#[tokio::test]` with real assertions, so a failure
//! names the case directly rather than a table index.

use std::collections::BTreeMap;

use fedimint_amm_common::config::{AmmConfigConsensus, UnitParams};
use fedimint_amm_common::math::{self, MINIMUM_LIQUIDITY};
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_common::types::{AmmOutput, AmmOutputError};
use fedimint_amm_server::Amm;
use fedimint_amm_server::db::{BalanceKey, LpPositionKey, Pool, PoolKey};
use fedimint_core::db::mem_impl::MemDatabase;
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::module::AmountUnit;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::secp256k1::{self, Keypair, SECP256K1};
use fedimint_core::{Amount, BitcoinHash, OutPoint, TransactionId};
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

fn out_point() -> OutPoint {
    OutPoint {
        txid: TransactionId::all_zeros(),
        out_idx: 0,
    }
}

/// Units 0 and 1 are in the allowlist with `min_swap_in` 1_000 msats each;
/// default fee 3/1000 (0.30%), matching the reference Uniswap V2 fee.
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

/// 1. `SwapV0` into a fresh pool fails with `NoSuchPool`.
#[tokio::test]
async fn swap_into_fresh_pool_fails_with_no_such_pool() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();

    let output = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in: Amount::from_msats(10_000),
        min_out: Amount::ZERO,
        recipient_pk: test_pubkey(1),
        tweak: [0u8; 16],
    };

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
    let owner = test_pubkey(2);
    let pool = pool01();

    let da = Amount::from_msats(1_000_000);
    let db_amt = Amount::from_msats(1_000_000);

    let output = AmmOutput::DepositV0 {
        pool,
        amount_lo: da,
        amount_hi: db_amt,
        min_shares: 0,
        owner_pk: owner,
        tweak: [1u8; 16],
    };

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
    let output = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(1_000),
        amount_hi: Amount::from_msats(1_000),
        min_shares: 0,
        owner_pk: test_pubkey(3),
        tweak: [2u8; 16],
    };

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert!(result.is_err(), "expected rejection, got {result:?}");
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
    let first_owner = test_pubkey(4);
    let second_owner = test_pubkey(5);

    let first = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(1_000_000),
        amount_hi: Amount::from_msats(1_000_000),
        min_shares: 0,
        owner_pk: first_owner,
        tweak: [3u8; 16],
    };
    module
        .process_output(&mut dbtx, &first, out_point())
        .await
        .expect("first deposit must succeed");
    let pool_after_first = dbtx.get_value(&PoolKey(pool)).await.unwrap();

    // Second deposit at the same ratio would mint `100`, so `min_shares` set
    // above that must be rejected.
    let second = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(100),
        amount_hi: Amount::from_msats(100),
        min_shares: u64::MAX,
        owner_pk: second_owner,
        tweak: [4u8; 16],
    };
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
    let recipient = test_pubkey(6);
    let output = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in,
        min_out: Amount::ZERO,
        recipient_pk: recipient,
        tweak: [5u8; 16],
    };

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
    let recipient = test_pubkey(7);

    let output = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in,
        min_out: Amount::from_msats(dy + 1),
        recipient_pk: recipient,
        tweak: [6u8; 16],
    };

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

    let output = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(0),
        amount_in: Amount::from_msats(10_000),
        min_out: Amount::ZERO,
        recipient_pk: test_pubkey(8),
        tweak: [7u8; 16],
    };

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::IdenticalUnits));
}

/// 8. `SwapV0` with a unit outside `units` returns `UnknownUnit`.
#[tokio::test]
async fn swap_with_unit_outside_allowlist_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();

    let output = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(99),
        amount_in: Amount::from_msats(10_000),
        min_out: Amount::ZERO,
        recipient_pk: test_pubkey(9),
        tweak: [8u8; 16],
    };

    let result = module.process_output(&mut dbtx, &output, out_point()).await;
    assert_eq!(result, Err(AmmOutputError::UnknownUnit));
}

/// 9. `SwapV0` with `amount_in` below `min_swap_in` returns `BelowMinSwapIn`.
#[tokio::test]
async fn swap_below_min_swap_in_is_rejected() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();

    let output = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in: Amount::from_msats(500), // cfg's min_swap_in is 1_000
        min_out: Amount::ZERO,
        recipient_pk: test_pubkey(10),
        tweak: [9u8; 16],
    };

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

    let recipient = test_pubkey(11);
    let output = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in: Amount::from_msats(1_000),
        min_out: Amount::ZERO,
        recipient_pk: recipient,
        tweak: [10u8; 16],
    };

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

    let recipient = test_pubkey(12);
    let amount_in = Amount::from_msats(10_000_000);

    let first = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in,
        min_out: Amount::ZERO,
        recipient_pk: recipient,
        tweak: [11u8; 16],
    };
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

    let second = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in,
        min_out: Amount::ZERO,
        recipient_pk: recipient,
        tweak: [12u8; 16],
    };
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
    // The FIRST tweak wins on the shared balance record: an attacker who
    // credits someone else's `recipient_pk` with a garbage `tweak` must not
    // be able to overwrite the tweak the honest owner is relying on for
    // seed-only recovery (spec §8.2, §13).
    assert_eq!(balance_after_second.tweak, [11u8; 16]);
}

/// 12. A second `SwapV0` into an existing `(recipient_pk, unit)` balance,
///     carrying a DIFFERENT `tweak`, must NOT overwrite the stored tweak.
///     This is the attacker-controlled-tweak-overwrite defect (spec §13):
///     the pubkey and tweak on the wire are unverified, so anyone can
///     "credit" anyone else's balance with a garbage tweak. Preserving the
///     first tweak protects the victim's seed-only recovery even though the
///     attacker's transaction is otherwise a legitimate credit.
#[tokio::test]
async fn balance_tweak_is_preserved_across_accumulation() {
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

    let recipient = test_pubkey(13);
    let amount_in = Amount::from_msats(10_000_000);
    let tweak_a = [0xAAu8; 16];
    let tweak_b = [0xBBu8; 16];

    let first = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in,
        min_out: Amount::ZERO,
        recipient_pk: recipient,
        tweak: tweak_a,
    };
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

    // Attacker-controlled second swap: same recipient/unit, garbage tweak.
    let second = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in,
        min_out: Amount::ZERO,
        recipient_pk: recipient,
        tweak: tweak_b,
    };
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
        balance.tweak, tweak_a,
        "the FIRST tweak must be preserved, not overwritten by the second swap"
    );
}

/// 13. A second `DepositV0` into an existing `(pool, owner_pk)` LP position,
///     carrying a DIFFERENT `tweak`, must NOT overwrite the stored tweak.
///     Same defect as case 12, but for LP positions (spec §13).
#[tokio::test]
async fn lp_position_tweak_is_preserved_across_accumulation() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner = test_pubkey(14);
    let tweak_a = [0xCCu8; 16];
    let tweak_b = [0xDDu8; 16];

    let first = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(1_000_000),
        amount_hi: Amount::from_msats(1_000_000),
        min_shares: 0,
        owner_pk: owner,
        tweak: tweak_a,
    };
    module
        .process_output(&mut dbtx, &first, out_point())
        .await
        .expect("first deposit must succeed");
    let shares_after_first = dbtx
        .get_value(&LpPositionKey { pool, owner })
        .await
        .unwrap()
        .shares;

    // Attacker-controlled second deposit: same pool/owner, garbage tweak.
    let second = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(100),
        amount_hi: Amount::from_msats(100),
        min_shares: 0,
        owner_pk: owner,
        tweak: tweak_b,
    };
    let result = module
        .process_output(&mut dbtx, &second, out_point())
        .await
        .expect("second deposit must succeed");
    assert!(!result.amounts.is_empty());

    let position = dbtx.get_value(&LpPositionKey { pool, owner }).await.unwrap();
    assert!(
        position.shares > shares_after_first,
        "shares must accumulate: {shares_after_first} -> {}",
        position.shares
    );
    assert_eq!(
        position.tweak, tweak_a,
        "the FIRST tweak must be preserved, not overwritten by the second deposit"
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
    let owner = test_pubkey(15);

    let first = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(1_000_000),
        amount_hi: Amount::from_msats(1_000_000),
        min_shares: 0,
        owner_pk: owner,
        tweak: [0xEEu8; 16],
    };
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

    let second = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(100),
        amount_hi: Amount::from_msats(100),
        min_shares: 0,
        owner_pk: owner,
        tweak: [0xFFu8; 16],
    };
    // Must return Ok, not panic, despite `owner_pk` colliding with an
    // existing LP position.
    let result = module.process_output(&mut dbtx, &second, out_point()).await;
    assert!(
        result.is_ok(),
        "duplicate-owner deposit must not error, got {result:?}"
    );

    let position = dbtx.get_value(&LpPositionKey { pool, owner }).await.unwrap();
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
