//! Spec §14 audit lifecycle test — the highest-value test in Task 9.
//!
//! `audit` runs after every consensus item and feeds an `assert!` that halts
//! every guardian if the reported balance sheet goes negative (spec §9,
//! `fedimint-server/src/consensus/engine.rs:1032-1058`). This drives a full
//! lifecycle — deposit, deposit at a shifted ratio, swap A->B, swap B->A,
//! claim the A->B balance, partial withdraw, full withdraw — and checks
//! after EVERY step that the module's total reported liability matches a
//! value tracked independently by the test. The claim step (finding M7)
//! means all four of spec §9.1's conservation table rows (`SwapV0`,
//! `ClaimBalanceV0`, `DepositV0`, `WithdrawV0`) are exercised, not just
//! three of them.
//!
//! "Independently" means: never call `math::mint_shares` / `burn_shares` /
//! `amount_out` in this file to predict what the module *should* report,
//! and never peek at `Pool`/`BalanceEntry` rows to read off the answer.
//! Instead, each step's contribution to expected liability comes from
//! amounts the test itself chose (`amount_lo`/`amount_hi`/`amount_in` on the
//! way in) or from the `TransactionItemAmounts`/`InputMeta` the call under
//! test hands back (the "amount that moved," which is also literally what a
//! real client would use to mint or burn ecash notes — spec §9.1's
//! conservation table is defined in exactly these terms). The one place this
//! file relies on arithmetic beyond that is deposit share counts, and there
//! only via identities cheap enough to be self-evidently correct rather than
//! a reimplementation of the curve (see `assert_lifecycle`'s setup).

use std::collections::BTreeMap;

use fedimint_amm_common::config::{AmmConfigConsensus, UnitParams};
use fedimint_amm_common::math::MINIMUM_LIQUIDITY;
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_common::types::{AmmInput, AmmOutput};
use fedimint_amm_server::Amm;
use fedimint_amm_server::db::{LpPositionKey, PoolKey};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::mem_impl::MemDatabase;
use fedimint_core::db::{Database, DatabaseTransaction, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::module::audit::Audit;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::module::{AmountUnit, Amounts};
use fedimint_core::secp256k1::{self, Keypair, SECP256K1};
use fedimint_core::{Amount, BitcoinHash, InPoint, OutPoint, TransactionId};
use fedimint_server_core::ServerModule;

const MODULE_INSTANCE_ID: ModuleInstanceId = 0;

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

fn in_point() -> InPoint {
    InPoint {
        txid: TransactionId::all_zeros(),
        in_idx: 0,
    }
}

/// `min_swap_in` of 1 msat so the test's small, hand-picked swap amounts are
/// never rejected as dust — this test is about audit conservation, not the
/// dust policy.
fn amm() -> Amm {
    Amm::new(AmmConfigConsensus {
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
        default_fee_per_mille: 3,
        fee_overrides: BTreeMap::new(),
    })
}

fn pool01() -> PoolId {
    PoolId::new(unit(0), unit(1)).unwrap()
}

/// Sums every unit's msats in an `Amounts` into one `i64` scalar — the same
/// collapse spec §9.1 makes for the audit itself ("Collapsing every unit
/// into one i64 msat scalar is lossy in general but safe here: the module
/// only ever *relocates* nominal quantity ... and never originates any").
/// This is the ONLY place this file interprets a `TransactionItemAmounts` —
/// it does no unit conversion and invents no exchange rate, it just adds up
/// msats the same way the audited items themselves are one flat `i64` scale.
fn total_msats(amounts: &Amounts) -> i64 {
    amounts
        .values()
        .map(|a| i64::try_from(a.msats).expect("far below i64::MAX in this test"))
        .sum()
}

/// Runs `audit` against the given (uncommitted) transaction and returns the
/// module's total reported liability: the negation of `net_assets`, since
/// every audited item is already stored negated (spec §9.1).
async fn total_liability(module: &Amm, dbtx: &mut DatabaseTransaction<'_>) -> i64 {
    let mut audit = Audit::default();
    module.audit(dbtx, &mut audit, MODULE_INSTANCE_ID).await;
    -audit
        .net_assets()
        .expect("no overflow: every value in this test is far below i64::MAX")
        .milli_sat
}

/// Drives the full spec §14 lifecycle at a caller-chosen scale and asserts,
/// after every step, that `total_liability` matches an expectation the test
/// tracks itself.
///
/// Deposit amounts are chosen so minted share counts are knowable from
/// arithmetic identities that hold for ANY correct curve implementation —
/// not from re-deriving `mint_shares`'s rounding behaviour:
///   - `da1 == db1 == n1` makes the first mint `isqrt(n1*n1) == n1` exactly
///     (isqrt of a perfect square is its root, unconditionally).
///   - Because that first deposit also makes `total_shares == reserve_lo ==
///     reserve_hi == n1`, the SECOND deposit's `via_lo`/`via_hi` terms
///     (`amount * total_shares / reserve`) reduce to `amount * 1` — exact,
///     with no floor division — so `minted = min(da2, db2)` exactly.
///
/// Everything after that (swap outputs, withdrawal payouts) is read only
/// from the call's own return value, never predicted.
async fn assert_lifecycle(n1: u64, da2: u64, db2: u64, swap1_in: u64, swap2_in: u64) {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();

    let owner1 = test_pubkey(1);
    let owner2 = test_pubkey(2);
    let swap_recipient_a = test_pubkey(3);
    let swap_recipient_b = test_pubkey(4);

    let mut expected: i64 = 0;
    assert_eq!(
        total_liability(&module, &mut dbtx).await,
        0,
        "empty database reports zero liability"
    );

    // --- Step 1: first deposit (creates the pool). ---
    let deposit1 = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(n1),
        amount_hi: Amount::from_msats(n1),
        min_shares: 0,
        owner_pk: owner1,
        tweak: [1u8; 16],
    };
    let outcome1 = module
        .process_output(&mut dbtx, &deposit1, out_point())
        .await
        .expect("first deposit must succeed");
    expected += total_msats(&outcome1.amounts);
    assert_eq!(
        total_liability(&module, &mut dbtx).await,
        expected,
        "after first deposit"
    );
    // `isqrt(n1*n1) == n1` unconditionally, so owner1 holds exactly
    // `n1 - MINIMUM_LIQUIDITY` shares — a fact about square numbers, not
    // about `mint_shares`'s implementation.
    let owner1_shares = n1 - MINIMUM_LIQUIDITY;

    // --- Step 2: second deposit at a shifted ratio. ---
    // `da2 != db2` by construction of the two call sites below, and the
    // pool's ratio is 1:1 after step 1, so this necessarily deposits at a
    // different ratio than the pool currently holds.
    let deposit2 = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(da2),
        amount_hi: Amount::from_msats(db2),
        min_shares: 0,
        owner_pk: owner2,
        tweak: [2u8; 16],
    };
    let outcome2 = module
        .process_output(&mut dbtx, &deposit2, out_point())
        .await
        .expect("second deposit must succeed");
    expected += total_msats(&outcome2.amounts);
    assert_eq!(
        total_liability(&module, &mut dbtx).await,
        expected,
        "after second deposit (shifted ratio)"
    );

    // --- Step 3: swap A -> B. ---
    let swap1 = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in: Amount::from_msats(swap1_in),
        min_out: Amount::ZERO,
        recipient_pk: swap_recipient_a,
        tweak: [3u8; 16],
    };
    let swap1_outcome = module
        .process_output(&mut dbtx, &swap1, out_point())
        .await
        .expect("swap A->B must succeed");
    // `process_output`'s `SwapV0` arm always returns `amount_in` as the
    // moved amount (see `lib.rs`): the output leg `dy` is an internal
    // transfer from reserve to balance and never appears here, matching
    // spec §9.1's conservation row for `SwapV0` (net zero for that leg).
    expected += total_msats(&swap1_outcome.amounts);
    assert_eq!(
        total_liability(&module, &mut dbtx).await,
        expected,
        "after swap A->B"
    );

    // --- Step 4: swap B -> A. ---
    let swap2 = AmmOutput::SwapV0 {
        unit_in: unit(1),
        unit_out: unit(0),
        amount_in: Amount::from_msats(swap2_in),
        min_out: Amount::ZERO,
        recipient_pk: swap_recipient_b,
        tweak: [4u8; 16],
    };
    let swap2_outcome = module
        .process_output(&mut dbtx, &swap2, out_point())
        .await
        .expect("swap B->A must succeed");
    expected += total_msats(&swap2_outcome.amounts);
    assert_eq!(
        total_liability(&module, &mut dbtx).await,
        expected,
        "after swap B->A"
    );

    // --- Step 4b: claim the balance created by swap A->B (M7: spec §9.1's
    // `ClaimBalanceV0` conservation row was otherwise never exercised by this
    // lifecycle). `process_input`'s own returned amount is used, exactly
    // like the withdrawal steps below -- never re-read from the stored
    // `BalanceEntry`.
    let claim = AmmInput::ClaimBalanceV0 {
        pubkey: swap_recipient_a,
        unit: unit(1),
    };
    let claim_meta = module
        .process_input(&mut dbtx, &claim, in_point())
        .await
        .expect("claiming swap A->B's balance must succeed");
    expected -= total_msats(&claim_meta.amount.amounts);
    assert_eq!(
        total_liability(&module, &mut dbtx).await,
        expected,
        "after claiming swap A->B's balance"
    );

    // --- Step 5: partial withdraw. ---
    // `owner1_shares` is known exactly (see above); split it so both a
    // partial and a full withdrawal are exercised.
    let partial_shares = owner1_shares / 2;
    assert!(
        partial_shares > 0 && partial_shares < owner1_shares,
        "test parameters must leave room for a genuine partial withdrawal"
    );
    let withdraw_partial = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner1,
        shares: partial_shares,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };
    let partial_meta = module
        .process_input(&mut dbtx, &withdraw_partial, in_point())
        .await
        .expect("partial withdraw must succeed");
    expected -= total_msats(&partial_meta.amount.amounts);
    assert_eq!(
        total_liability(&module, &mut dbtx).await,
        expected,
        "after partial withdraw"
    );

    // --- Step 6: full withdraw (of the remainder). ---
    let remaining_shares = owner1_shares - partial_shares;
    let withdraw_full = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner1,
        shares: remaining_shares,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };
    let full_meta = module
        .process_input(&mut dbtx, &withdraw_full, in_point())
        .await
        .expect("full withdraw must succeed");
    expected -= total_msats(&full_meta.amount.amounts);
    assert_eq!(
        total_liability(&module, &mut dbtx).await,
        expected,
        "after full withdraw"
    );

    // The full withdrawal must have consumed exactly the remaining shares:
    // owner1's position should now be gone.
    assert!(
        dbtx.get_value(&LpPositionKey {
            pool,
            owner: owner1
        })
        .await
        .is_none(),
        "owner1's position must be deleted after withdrawing every remaining share"
    );
    assert!(
        dbtx.get_value(&PoolKey(pool)).await.is_some(),
        "the pool itself must still exist (owner2's shares and MINIMUM_LIQUIDITY remain)"
    );
}

/// Base case: reserves comfortably above every edge case, pool ratio 1:1.
#[tokio::test]
async fn audit_liability_tracks_lifecycle_at_a_typical_ratio() {
    assert_lifecycle(1_000_000, 5_000, 3_000, 10_000, 8_000).await;
}

/// Spec §14: repeat at a 1_000_000:1 ratio. `n1` is chosen so
/// `isqrt(n1*n1) == n1` still holds (any perfect square does), while
/// `da1 == db1 == n1` alone would only give a 1:1 pool — so instead this
/// picks the two deposit legs independently: `da1 = 1_000_000_000`,
/// `db1 = 1_000`, giving a 1_000_000:1 ratio, and `isqrt(da1 * db1) ==
/// isqrt(1000^2 * 1000^2) == 1000 * 1000 == 1_000_000` exactly (again a
/// perfect-square identity, not curve-specific behaviour).
#[tokio::test]
async fn audit_liability_tracks_lifecycle_at_a_million_to_one_ratio() {
    let db = db();
    let mut dbtx = db.begin_transaction_nc().await;
    let module = amm();
    let pool = pool01();
    let owner1 = test_pubkey(11);
    let owner2 = test_pubkey(12);
    let swap_recipient_a = test_pubkey(13);
    let swap_recipient_b = test_pubkey(14);

    let mut expected: i64 = 0;

    // Step 1: first deposit, 1_000_000:1 ratio.
    // isqrt(1_000_000_000 * 1_000) == isqrt(1000^2 * 1000^2) == 1_000_000.
    let (da1, db1) = (1_000_000_000u64, 1_000u64);
    let deposit1 = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(da1),
        amount_hi: Amount::from_msats(db1),
        min_shares: 0,
        owner_pk: owner1,
        tweak: [1u8; 16],
    };
    let outcome1 = module
        .process_output(&mut dbtx, &deposit1, out_point())
        .await
        .expect("first deposit must succeed");
    expected += total_msats(&outcome1.amounts);
    assert_eq!(total_liability(&module, &mut dbtx).await, expected);
    let total_shares_after_1 = 1_000_000u64; // isqrt(da1 * db1), see doc comment
    let owner1_shares = total_shares_after_1 - MINIMUM_LIQUIDITY;

    // Step 2: second deposit at a shifted ratio (5000:1, vs the pool's
    // 1_000_000:1). After step 1, total_shares == 1_000_000, reserve_lo ==
    // 1_000_000_000, reserve_hi == 1_000, so:
    //   via_lo = da2 * 1_000_000 / 1_000_000_000 = da2 / 1000 -- exact
    //            because da2 is chosen as a multiple of 1000 below.
    //   via_hi = db2 * 1_000_000 / 1_000 = db2 * 1000           -- always
    //            exact, no floor loss possible.
    let (da2, db2) = (5_000u64, 1u64);
    let deposit2 = AmmOutput::DepositV0 {
        pool,
        amount_lo: Amount::from_msats(da2),
        amount_hi: Amount::from_msats(db2),
        min_shares: 0,
        owner_pk: owner2,
        tweak: [2u8; 16],
    };
    let outcome2 = module
        .process_output(&mut dbtx, &deposit2, out_point())
        .await
        .expect("second deposit must succeed");
    expected += total_msats(&outcome2.amounts);
    assert_eq!(total_liability(&module, &mut dbtx).await, expected);

    // Step 3: swap A->B. The B (hi) side only holds ~1_000 units against a
    // ~1_000_005_000-unit A side, so the input must be a healthy multiple of
    // the spot-price threshold (~1_000_000 lo per 1 hi) or the output floors
    // to zero.
    let swap1 = AmmOutput::SwapV0 {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in: Amount::from_msats(5_000_000),
        min_out: Amount::ZERO,
        recipient_pk: swap_recipient_a,
        tweak: [3u8; 16],
    };
    let swap1_outcome = module
        .process_output(&mut dbtx, &swap1, out_point())
        .await
        .expect("swap A->B must succeed");
    expected += total_msats(&swap1_outcome.amounts);
    assert_eq!(total_liability(&module, &mut dbtx).await, expected);

    // Step 4: swap B->A. The B side is thin, so even a tiny input yields a
    // large A-side output — well within reserve_lo, which is now ~1e9.
    let swap2 = AmmOutput::SwapV0 {
        unit_in: unit(1),
        unit_out: unit(0),
        amount_in: Amount::from_msats(2),
        min_out: Amount::ZERO,
        recipient_pk: swap_recipient_b,
        tweak: [4u8; 16],
    };
    let swap2_outcome = module
        .process_output(&mut dbtx, &swap2, out_point())
        .await
        .expect("swap B->A must succeed");
    expected += total_msats(&swap2_outcome.amounts);
    assert_eq!(total_liability(&module, &mut dbtx).await, expected);

    // Step 4b: claim the balance created by swap A->B (M7).
    let claim = AmmInput::ClaimBalanceV0 {
        pubkey: swap_recipient_a,
        unit: unit(1),
    };
    let claim_meta = module
        .process_input(&mut dbtx, &claim, in_point())
        .await
        .expect("claiming swap A->B's balance must succeed");
    expected -= total_msats(&claim_meta.amount.amounts);
    assert_eq!(total_liability(&module, &mut dbtx).await, expected);

    // Step 5: partial withdraw.
    let partial_shares = owner1_shares / 2;
    let withdraw_partial = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner1,
        shares: partial_shares,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };
    let partial_meta = module
        .process_input(&mut dbtx, &withdraw_partial, in_point())
        .await
        .expect("partial withdraw must succeed");
    expected -= total_msats(&partial_meta.amount.amounts);
    assert_eq!(total_liability(&module, &mut dbtx).await, expected);

    // Step 6: full withdraw of the remainder.
    let remaining_shares = owner1_shares - partial_shares;
    let withdraw_full = AmmInput::WithdrawV0 {
        pool,
        owner_pk: owner1,
        shares: remaining_shares,
        min_lo: Amount::ZERO,
        min_hi: Amount::ZERO,
    };
    let full_meta = module
        .process_input(&mut dbtx, &withdraw_full, in_point())
        .await
        .expect("full withdraw must succeed");
    expected -= total_msats(&full_meta.amount.amounts);
    assert_eq!(total_liability(&module, &mut dbtx).await, expected);
}

/// Spec §14: repeat at minimum viable reserves — just above
/// `MINIMUM_LIQUIDITY` (1000). `n1 = 1010` keeps the pool's very first
/// deposit barely past the minimum while still leaving 10 shares for
/// owner1, enough to exercise a genuine partial withdrawal.
#[tokio::test]
async fn audit_liability_tracks_lifecycle_at_minimum_viable_reserves() {
    assert_lifecycle(1_010, 6, 4, 100, 50).await;
}
