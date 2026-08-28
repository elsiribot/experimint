//! `api_endpoints` — `QUOTE_ENDPOINT` (finding I1) and the two recovery
//! endpoints' pagination (finding I2).
//!
//! These call the real `ApiEndpoint<Amm>::handler` returned by
//! `Amm::api_endpoints()`, not a reimplementation of the endpoint logic, so a
//! bug in wiring (wrong path, wrong version, a handler that never got hooked
//! up) would be caught here too.

use std::collections::BTreeMap;

use fedimint_amm_common::config::{AmmConfigConsensus, UnitParams};
use fedimint_amm_common::endpoints::{
    BALANCE_ENDPOINT, BALANCE_RECOVERY_ENDPOINT, BalanceRecoveryResponse, BalanceRequest,
    LP_RECOVERY_ENDPOINT, LpRecoveryResponse, MAX_RECOVERY_PAGE_SIZE, POOLS_ENDPOINT, PoolSummary,
    QUOTE_ENDPOINT, QuoteRequest, QuoteResponse, RecoveryPageRequest,
};
use fedimint_amm_common::math;
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_common::types::AmmOutputError;
use fedimint_amm_server::Amm;
use fedimint_amm_server::db::{BalanceEntry, BalanceKey, LpPosition, LpPositionKey, Pool, PoolKey};
use fedimint_core::Amount;
use fedimint_core::db::mem_impl::MemDatabase;
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::module::{AmountUnit, ApiEndpointContext, ApiError, ApiRequestErased};
use fedimint_core::secp256k1::{Keypair, SECP256K1};
use fedimint_server_core::ServerModule;
use serde::Serialize;
use serde::de::DeserializeOwned;

fn db() -> Database {
    Database::new(MemDatabase::new(), ModuleDecoderRegistry::default())
}

fn unit(n: u64) -> AmountUnit {
    AmountUnit::new_custom(n)
}

/// Units 0 and 1 are in the allowlist with `min_swap_in` 1_000 msats each;
/// default fee 3/1000 (0.30%), matching `output.rs`'s `amm()`.
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

/// Calls the real handler for `path` out of `module.api_endpoints()` — not a
/// reimplementation — with `params`, and deserializes the JSON response into
/// `R`.
async fn call<R: DeserializeOwned>(
    module: &Amm,
    db: &Database,
    path: &str,
    params: impl Serialize,
) -> Result<R, ApiError> {
    let endpoint = module
        .api_endpoints()
        .into_iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("endpoint {path} must be registered"));
    let context = ApiEndpointContext::new(db.clone(), false, None);
    let request = ApiRequestErased::new(params);
    let value = (endpoint.handler)(module, context, request).await?;
    Ok(fedimint_core::module::serde_json::from_value(value)
        .expect("response must deserialize into the expected type"))
}

/// Finding I1: `QUOTE_ENDPOINT` and `process_output`'s `SwapV0` arm now share
/// `quote_swap`, so a quote can never disagree with settlement. This checks
/// BOTH swap directions against an asymmetric pool (`reserve_lo !=
/// reserve_hi`), with expected `amount_out`/`price_impact_per_mille` values
/// hard-coded from independently worked-out Uniswap V2 arithmetic — not
/// derived by calling `quote_swap` or `math::amount_out` — so an inverted
/// orientation inside the shared helper cannot cancel out against an
/// inverted expectation here.
#[tokio::test]
async fn quote_endpoint_matches_settlement_in_both_directions() {
    let db = db();
    let module = amm();
    let pool = pool01();

    // Same asymmetric pool as `output.rs`'s worked-example tests:
    // reserve_lo (unit 0) = 1_000_000_000, reserve_hi (unit 1) = 1_000_000.
    {
        let mut dbtx = db.begin_transaction().await;
        dbtx.insert_new_entry(
            &PoolKey(pool),
            &Pool {
                reserve_lo: Amount::from_msats(1_000_000_000),
                reserve_hi: Amount::from_msats(1_000_000),
                total_shares: 1_000_000_000,
            },
        )
        .await;
        dbtx.commit_tx().await;
    }

    // Direction 1: lo -> hi. Reference vector (also used in
    // `fedimint-amm-common`'s `amount_out_matches_reference_vector` and
    // `price_impact_matches_worked_example`): amount_in 10_000_000, fee
    // 3/1000 -> amount_out 9_871, price_impact_per_mille 13.
    let lo_to_hi: QuoteResponse = call(
        &module,
        &db,
        QUOTE_ENDPOINT,
        QuoteRequest {
            unit_in: unit(0),
            unit_out: unit(1),
            amount_in: Amount::from_msats(10_000_000),
        },
    )
    .await
    .expect("lo->hi quote must succeed");
    assert_eq!(lo_to_hi.amount_out, Amount::from_msats(9_871));
    assert_eq!(lo_to_hi.price_impact_per_mille, 13);

    // Direction 2: hi -> lo. Hand-computed in `output.rs`'s
    // `swap_in_hi_direction_moves_reserves_and_credits_balance_by_the_same_dy`:
    // amount_in 10_000, fee 3/1000 -> amount_out 9_871_580. price_impact
    // worked out separately below (not by calling the module under test):
    // ratio = floor(1000 * 9_871_580 * 1_000_000 / (10_000 * 1_000_000_000))
    //       = floor(9_871_580_000_000_000 / 10_000_000_000_000) = 987
    // impact = 1000 - 987 = 13.
    let hi_to_lo: QuoteResponse = call(
        &module,
        &db,
        QUOTE_ENDPOINT,
        QuoteRequest {
            unit_in: unit(1),
            unit_out: unit(0),
            amount_in: Amount::from_msats(10_000),
        },
    )
    .await
    .expect("hi->lo quote must succeed");
    assert_eq!(hi_to_lo.amount_out, Amount::from_msats(9_871_580));
    assert_eq!(hi_to_lo.price_impact_per_mille, 13);
}

/// Finding M9, fix pass 2 Minor 7: the quote endpoint now enforces the same
/// admission checks settlement does. `amount_in` below `min_swap_in` (1_000
/// for both units in this config) must be rejected rather than silently
/// quoted, with the exact `AmmOutputError::BelowMinSwapIn` reason — not just
/// "some error or other".
#[tokio::test]
async fn quote_endpoint_rejects_amount_below_min_swap_in() {
    let db = db();
    let module = amm();
    let pool = pool01();

    let mut dbtx = db.begin_transaction().await;
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000_000,
        },
    )
    .await;
    dbtx.commit_tx().await;

    let result: Result<QuoteResponse, ApiError> = call(
        &module,
        &db,
        QUOTE_ENDPOINT,
        QuoteRequest {
            unit_in: unit(0),
            unit_out: unit(1),
            amount_in: Amount::from_msats(500), // below the 1_000 min_swap_in
        },
    )
    .await;
    let err = result.expect_err("a below-min_swap_in quote must be rejected");
    assert_eq!(err.message, AmmOutputError::BelowMinSwapIn.to_string());
}

/// Fix pass 2, Minor 7: `quote_swap` also enforces the unit allowlist —
/// `unit_out` not being a unit this federation trades must be rejected with
/// `AmmOutputError::UnknownUnit`, even when a `Pool` record happens to exist
/// for that pair (e.g. left over from a config change).
#[tokio::test]
async fn quote_endpoint_rejects_unit_not_in_allowlist() {
    let db = db();
    let module = amm();
    let unknown_unit = unit(2); // not in `amm()`'s config
    let pool = PoolId::new(unit(0), unknown_unit).unwrap();

    let mut dbtx = db.begin_transaction().await;
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000_000,
        },
    )
    .await;
    dbtx.commit_tx().await;

    let result: Result<QuoteResponse, ApiError> = call(
        &module,
        &db,
        QUOTE_ENDPOINT,
        QuoteRequest {
            unit_in: unit(0),
            unit_out: unknown_unit,
            amount_in: Amount::from_msats(10_000),
        },
    )
    .await;
    let err = result.expect_err("a quote naming a unit outside the allowlist must be rejected");
    assert_eq!(err.message, AmmOutputError::UnknownUnit.to_string());
}

/// Fix pass 3, Important 5: `BALANCE_ENDPOINT` is a point lookup — this
/// checks it returns exactly the stored amount for a key that exists, `None`
/// for one that doesn't, and does not confuse two different `(pubkey, unit)`
/// keys with each other.
#[tokio::test]
async fn balance_endpoint_looks_up_a_single_stored_balance() {
    let db = db();
    let module = amm();

    let pubkey = Keypair::from_seckey_slice(SECP256K1, &[7u8; 32])
        .expect("nonzero bytes are a valid secret key")
        .public_key();

    {
        let mut dbtx = db.begin_transaction().await;
        dbtx.insert_new_entry(
            &BalanceKey {
                owner: pubkey,
                unit: unit(0),
            },
            &BalanceEntry {
                amount: Amount::from_msats(4_242),
                tweak: [7u8; 16],
            },
        )
        .await;
        dbtx.commit_tx().await;
    }

    let found: Option<Amount> = call(
        &module,
        &db,
        BALANCE_ENDPOINT,
        BalanceRequest {
            pubkey,
            unit: unit(0),
        },
    )
    .await
    .expect("lookup itself must succeed");
    assert_eq!(found, Some(Amount::from_msats(4_242)));

    // Same pubkey, different unit: must not find the unit(0) balance.
    let wrong_unit: Option<Amount> = call(
        &module,
        &db,
        BALANCE_ENDPOINT,
        BalanceRequest {
            pubkey,
            unit: unit(1),
        },
    )
    .await
    .expect("lookup itself must succeed");
    assert_eq!(wrong_unit, None);

    // A pubkey with no stored balance at all.
    let other_pubkey = Keypair::from_seckey_slice(SECP256K1, &[8u8; 32])
        .expect("nonzero bytes are a valid secret key")
        .public_key();
    let not_found: Option<Amount> = call(
        &module,
        &db,
        BALANCE_ENDPOINT,
        BalanceRequest {
            pubkey: other_pubkey,
            unit: unit(0),
        },
    )
    .await
    .expect("lookup itself must succeed");
    assert_eq!(not_found, None);
}

/// Final-review finding T1: `POOLS_ENDPOINT` must report the pool's fee
/// OVERRIDE, not `default_fee_per_mille` — the other half of "fee_overrides
/// must reach settlement", since a client previewing a swap via this
/// endpoint would otherwise be quoted the wrong fee for an overridden pool.
#[tokio::test]
async fn pools_endpoint_reports_the_fee_override_not_the_default() {
    let db = db();
    let pool = pool01();
    let module = Amm::new(AmmConfigConsensus {
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
    });

    let mut dbtx = db.begin_transaction().await;
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo: Amount::from_msats(1_000_000),
            reserve_hi: Amount::from_msats(1_000_000),
            total_shares: 1_000_000,
        },
    )
    .await;
    dbtx.commit_tx().await;

    let pools: Vec<PoolSummary> = call(&module, &db, POOLS_ENDPOINT, ())
        .await
        .expect("POOLS_ENDPOINT must succeed");
    assert_eq!(pools.len(), 1);
    assert_eq!(
        pools[0].fee_per_mille, 100,
        "must report the pool's fee override (100), not default_fee_per_mille (3)"
    );
}

/// Fix pass 2, Minor 7: `quote_swap` also enforces `MAX_RESERVE` on
/// `reserve_in + amount_in`, not just on each value individually — a quote
/// that would push the reserve over the cap must be rejected with
/// `AmmOutputError::ReserveCapExceeded`, matching what settlement would do.
#[tokio::test]
async fn quote_endpoint_rejects_amount_that_would_exceed_max_reserve() {
    let db = db();
    let module = amm();
    let pool = pool01();

    // Individually within MAX_RESERVE, but the sum is not. `reserve_hi` is
    // also kept close to `MAX_RESERVE` (rather than small) so `amount_out`
    // does not separately reject with `OutputRoundsToZero`: a small
    // `reserve_out` next to a huge `reserve_in` floors the output to 0
    // before the `reserve_in_new` check this test targets is ever reached.
    let reserve_lo = Amount::from_msats(math::MAX_RESERVE - 100);
    let reserve_hi = Amount::from_msats(math::MAX_RESERVE - 100);
    let amount_in = Amount::from_msats(2_000);

    let mut dbtx = db.begin_transaction().await;
    dbtx.insert_new_entry(
        &PoolKey(pool),
        &Pool {
            reserve_lo,
            reserve_hi,
            total_shares: 1_000_000_000,
        },
    )
    .await;
    dbtx.commit_tx().await;

    let result: Result<QuoteResponse, ApiError> = call(
        &module,
        &db,
        QUOTE_ENDPOINT,
        QuoteRequest {
            unit_in: unit(0),
            unit_out: unit(1),
            amount_in,
        },
    )
    .await;
    let err = result.expect_err("a quote that would push reserve_in past MAX_RESERVE must fail");
    assert_eq!(err.message, AmmOutputError::ReserveCapExceeded.to_string());
}

/// Finding I2: `BALANCE_RECOVERY_ENDPOINT` pages through every row exactly
/// once, with no duplicates or gaps, and the server-side page-size cap is
/// enforced even when the client requests a larger one.
#[tokio::test]
async fn balance_recovery_paginates_every_row_exactly_once() {
    let db = db();
    let module = amm();

    // One more row than the max page size, so at least two pages are forced
    // even when the client asks for the biggest page the server allows.
    let row_count = MAX_RECOVERY_PAGE_SIZE as usize + 5;
    let mut expected_pubkeys = Vec::with_capacity(row_count);
    {
        let mut dbtx = db.begin_transaction().await;
        for i in 0..row_count {
            // `test_pubkey` only takes a `u8` seed; derive distinct keys from
            // more than 256 rows by hashing the index into the seed byte via
            // a keypair built from a full 32-byte secret instead.
            let mut sk_bytes = [0u8; 32];
            sk_bytes[0..8].copy_from_slice(&(i as u64 + 1).to_be_bytes());
            let pk = Keypair::from_seckey_slice(SECP256K1, &sk_bytes)
                .expect("nonzero index-derived bytes are a valid secret key")
                .public_key();
            expected_pubkeys.push(pk);
            dbtx.insert_new_entry(
                &BalanceKey {
                    owner: pk,
                    unit: unit(0),
                },
                &BalanceEntry {
                    amount: Amount::from_msats(1_000 + i as u64),
                    tweak: [i as u8; 16],
                },
            )
            .await;
        }
        dbtx.commit_tx().await;
    }
    expected_pubkeys.sort();

    // Client asks for an oversized limit; the server must clamp it rather
    // than trust it (finding I2), so this still comes back paginated.
    let mut collected_pubkeys = Vec::new();
    let mut cursor: Option<Vec<u8>> = None;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages <= row_count + 1, "pagination did not terminate");
        let page: BalanceRecoveryResponse = call(
            &module,
            &db,
            BALANCE_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: cursor.clone(),
                limit: Some(u32::MAX),
            },
        )
        .await
        .expect("balance recovery page must succeed");

        assert!(
            page.entries.len() <= MAX_RECOVERY_PAGE_SIZE as usize,
            "a client-requested limit above MAX_RECOVERY_PAGE_SIZE must be clamped server-side"
        );
        collected_pubkeys.extend(page.entries.iter().map(|e| e.pubkey));

        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    assert!(pages >= 2, "the fixture must force at least two pages");
    assert_eq!(
        collected_pubkeys.len(),
        row_count,
        "every row must be returned exactly once: no duplicates, no gaps"
    );
    collected_pubkeys.sort();
    assert_eq!(
        collected_pubkeys, expected_pubkeys,
        "the set of pubkeys collected across pages must equal the set inserted"
    );
}

/// Fix pass 2, Important 2: a keyset cursor must survive a row being
/// deleted below it between pages. Inserts 3 rows (so a page size of 1
/// forces multiple pages), fetches page 1 (returns the first row in key
/// order), deletes that first row (simulating a concurrent
/// `ClaimBalanceV0` landing between the two page fetches), then fetches
/// page 2. The second row was never returned by page 1 and still exists,
/// so it MUST come back on page 2.
///
/// Confirmed (fix pass 2 report) that the equivalent scenario against the
/// prior `u64` row-offset cursor fails here: with an offset cursor,
/// `.skip(1)` on page 2 skips what used to be row 2, because deleting row
/// 1 shifts every later row left by one — so row 2 is silently dropped by
/// every page. A keyset cursor resumes from the last KEY returned, which a
/// deletion elsewhere in the table cannot shift.
#[tokio::test]
async fn balance_recovery_keyset_cursor_survives_deletion_below_cursor() {
    let db = db();
    let module = amm();

    let pubkeys: Vec<_> = (1u8..=3)
        .map(|seed| {
            Keypair::from_seckey_slice(SECP256K1, &[seed; 32])
                .unwrap()
                .public_key()
        })
        .collect();
    let mut sorted_pubkeys = pubkeys.clone();
    sorted_pubkeys.sort();

    {
        let mut dbtx = db.begin_transaction().await;
        for (i, pk) in pubkeys.iter().enumerate() {
            dbtx.insert_new_entry(
                &BalanceKey {
                    owner: *pk,
                    unit: unit(0),
                },
                &BalanceEntry {
                    amount: Amount::from_msats(1_000 + i as u64),
                    tweak: [i as u8; 16],
                },
            )
            .await;
        }
        dbtx.commit_tx().await;
    }

    // Page 1: limit 1, no cursor -> returns the first row in key order.
    let page1: BalanceRecoveryResponse = call(
        &module,
        &db,
        BALANCE_RECOVERY_ENDPOINT,
        RecoveryPageRequest {
            cursor: None,
            limit: Some(1),
        },
    )
    .await
    .expect("page 1 must succeed");
    assert_eq!(page1.entries.len(), 1);
    assert_eq!(page1.entries[0].pubkey, sorted_pubkeys[0]);
    let cursor = page1.next_cursor.expect("more rows remain");

    // Delete the row already returned (below the cursor), simulating a
    // concurrent ClaimBalanceV0 landing between the two page fetches.
    {
        let mut dbtx = db.begin_transaction().await;
        dbtx.remove_entry(&BalanceKey {
            owner: sorted_pubkeys[0],
            unit: unit(0),
        })
        .await;
        dbtx.commit_tx().await;
    }

    // Page 2 must return the second row (sorted_pubkeys[1]) — it was never
    // returned by page 1 and still exists.
    let page2: BalanceRecoveryResponse = call(
        &module,
        &db,
        BALANCE_RECOVERY_ENDPOINT,
        RecoveryPageRequest {
            cursor: Some(cursor),
            limit: Some(1),
        },
    )
    .await
    .expect("page 2 must succeed");
    assert_eq!(
        page2.entries.len(),
        1,
        "row sorted_pubkeys[1] still exists and was never returned by page 1"
    );
    assert_eq!(
        page2.entries[0].pubkey, sorted_pubkeys[1],
        "a keyset cursor must not skip a row that a deletion below the cursor would have shifted \
         under an offset cursor"
    );
}

/// Finding I2, mirrored for `LP_RECOVERY_ENDPOINT`.
#[tokio::test]
async fn lp_recovery_paginates_every_row_exactly_once() {
    let db = db();
    let module = amm();
    let pool = pool01();

    let row_count = MAX_RECOVERY_PAGE_SIZE as usize + 5;
    let mut expected_pubkeys = Vec::with_capacity(row_count);
    {
        let mut dbtx = db.begin_transaction().await;
        for i in 0..row_count {
            let mut sk_bytes = [0u8; 32];
            sk_bytes[0..8].copy_from_slice(&(i as u64 + 1).to_be_bytes());
            let pk = Keypair::from_seckey_slice(SECP256K1, &sk_bytes)
                .expect("nonzero index-derived bytes are a valid secret key")
                .public_key();
            expected_pubkeys.push(pk);
            dbtx.insert_new_entry(
                &LpPositionKey { pool, owner: pk },
                &LpPosition {
                    shares: 1 + i as u64,
                    tweak: [i as u8; 16],
                },
            )
            .await;
        }
        dbtx.commit_tx().await;
    }
    expected_pubkeys.sort();

    let mut collected_pubkeys = Vec::new();
    let mut cursor: Option<Vec<u8>> = None;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages <= row_count + 1, "pagination did not terminate");
        let page: LpRecoveryResponse = call(
            &module,
            &db,
            LP_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: cursor.clone(),
                limit: None, // omitted -> server defaults to the max page size
            },
        )
        .await
        .expect("LP recovery page must succeed");

        assert!(page.entries.len() <= MAX_RECOVERY_PAGE_SIZE as usize);
        collected_pubkeys.extend(page.entries.iter().map(|e| e.pubkey));

        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    assert!(pages >= 2, "the fixture must force at least two pages");
    assert_eq!(collected_pubkeys.len(), row_count);
    collected_pubkeys.sort();
    assert_eq!(collected_pubkeys, expected_pubkeys);
}

/// BLOCKING bug: an unauthenticated `cursor` used to be trusted to already
/// lie inside the endpoint's own `DbKeyPrefix` byte range. It never was —
/// `cursor` is exactly what the client sends on a recovery request, no
/// authentication required — so a cursor of `vec![]`, one from the OTHER
/// recovery endpoint, or one below/above/outside this table's prefix swept
/// `raw_find_by_range` into a different table's rows (or an inverted
/// range) and panicked the handler on `.expect()`/the range assertion. Each
/// case below is reproduced against **both** recovery endpoints and must
/// fail against current (pre-fix) code — as an internal `.expect()` panic
/// for four of the five, and as a wrong-but-not-crashing empty response for
/// the fifth (`..._cursor_above_prefix`; see its doc comment). Post-fix
/// every one of them must come back as a clean `Err(ApiError)`, never a
/// panic and never silently-wrong data.
mod recovery_cursor_hardening {
    use super::*;

    fn pool_id() -> PoolId {
        pool01()
    }

    /// Seeds one row of each table so a wrongly-unclamped scan has
    /// something from a lower prefix to trip over: a `Pool` row (prefix
    /// `0x01`) and an `LpPosition` row (prefix `0x02`). `BALANCE_RECOVERY`
    /// tests need both lower prefixes covered; `LP_RECOVERY` tests need the
    /// `Pool` row.
    async fn seed_lower_prefix_rows(db: &Database) {
        let pool = pool_id();
        let mut dbtx = db.begin_transaction().await;
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
            &LpPositionKey {
                pool,
                owner: test_owner_pubkey(),
            },
            &LpPosition {
                shares: 1,
                tweak: [0u8; 16],
            },
        )
        .await;
        dbtx.commit_tx().await;
    }

    fn test_owner_pubkey() -> fedimint_core::secp256k1::PublicKey {
        Keypair::from_seckey_slice(SECP256K1, &[9u8; 32])
            .expect("nonzero bytes are a valid secret key")
            .public_key()
    }

    /// `cursor = Some(vec![])`: pre-fix, `start = [0x00]`, below every real
    /// `DbKeyPrefix` (`Pool` 0x01, `LpPosition` 0x02, `Balance` 0x03), so
    /// the scan sweeps in every table. Against `BALANCE_RECOVERY_ENDPOINT`
    /// the first foreign row it hits (the seeded `Pool` row) fails to
    /// decode as `BalanceKey` and panics.
    #[tokio::test]
    async fn balance_recovery_empty_cursor() {
        let db = db();
        seed_lower_prefix_rows(&db).await;
        let module = amm();
        let result: Result<BalanceRecoveryResponse, ApiError> = call(
            &module,
            &db,
            BALANCE_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: Some(vec![]),
                limit: Some(10),
            },
        )
        .await;
        assert!(
            result.is_err(),
            "an empty cursor must be rejected cleanly, not accepted"
        );
    }

    /// `cursor` below the endpoint's own prefix: for `BALANCE_RECOVERY`
    /// (prefix `0x03`), a one-byte cursor equal to `LpPosition`'s prefix
    /// (`0x02`) makes `start = [0x02, 0x00]`, which still sits below
    /// `Balance` and sweeps in the seeded `LpPosition` row, which fails to
    /// decode as `BalanceKey`.
    #[tokio::test]
    async fn balance_recovery_cursor_below_prefix() {
        let db = db();
        seed_lower_prefix_rows(&db).await;
        let module = amm();
        let result: Result<BalanceRecoveryResponse, ApiError> = call(
            &module,
            &db,
            BALANCE_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: Some(vec![0x02]),
                limit: Some(10),
            },
        )
        .await;
        assert!(
            result.is_err(),
            "a cursor below this endpoint's own prefix must be rejected cleanly"
        );
    }

    /// `cursor` above every real prefix (`0xFF`): `start = [0xFF, 0x00]`
    /// sorts above `end = [0x04]` for `BALANCE_RECOVERY`, an inverted
    /// range. Confirmed this does NOT panic against `MemDatabase`
    /// specifically — its backing `imbl::OrdMap::range` tolerates an
    /// inverted bound and just yields nothing, unlike
    /// `std::collections::BTreeMap::range`, which panics on the identical
    /// input once the map is non-empty (checked directly, outside this
    /// module). Pre-fix this endpoint therefore silently returns an empty
    /// page for a client-supplied cursor that makes no sense, instead of
    /// rejecting it — still a real defect (wrong behaviour, masking a
    /// malformed request as "no more results"), just not the crash the
    /// other cases below reproduce. Included for completeness of the five
    /// cursor-hardening cases; a real RocksDB backend's behaviour on an
    /// inverted range is untested here.
    #[tokio::test]
    async fn balance_recovery_cursor_above_prefix() {
        let db = db();
        {
            let mut dbtx = db.begin_transaction().await;
            dbtx.insert_new_entry(
                &BalanceKey {
                    owner: test_owner_pubkey(),
                    unit: unit(0),
                },
                &BalanceEntry {
                    amount: Amount::from_msats(1),
                    tweak: [0u8; 16],
                },
            )
            .await;
            dbtx.commit_tx().await;
        }
        let module = amm();
        let result: Result<BalanceRecoveryResponse, ApiError> = call(
            &module,
            &db,
            BALANCE_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: Some(vec![0xFF]),
                limit: Some(10),
            },
        )
        .await;
        assert!(
            result.is_err(),
            "a cursor above every real prefix must be rejected cleanly, not panic the range scan"
        );
    }

    /// Multi-byte garbage that is not any real cursor this module ever
    /// handed out, chosen (like the empty-cursor case) to sort below every
    /// real prefix so it sweeps in the seeded `Pool`/`LpPosition` rows.
    #[tokio::test]
    async fn balance_recovery_garbage_cursor() {
        let db = db();
        seed_lower_prefix_rows(&db).await;
        let module = amm();
        let result: Result<BalanceRecoveryResponse, ApiError> = call(
            &module,
            &db,
            BALANCE_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: Some(vec![0x00, 0xDE, 0xAD, 0xBE, 0xEF]),
                limit: Some(10),
            },
        )
        .await;
        assert!(
            result.is_err(),
            "garbage cursor bytes must be rejected cleanly, not panic"
        );
    }

    /// A cursor over `MAX_RECOVERY_CURSOR_LEN` (256) bytes. Unlike the four
    /// cases above, this is already rejected pre-fix (the length check runs
    /// before any range scan) — included for completeness of the "each of
    /// the five" cursor-hardening cases, not because it demonstrates the
    /// blocking bug.
    #[tokio::test]
    async fn balance_recovery_over_long_cursor() {
        let db = db();
        let module = amm();
        let result: Result<BalanceRecoveryResponse, ApiError> = call(
            &module,
            &db,
            BALANCE_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: Some(vec![0u8; 257]),
                limit: Some(10),
            },
        )
        .await;
        assert!(
            result.is_err(),
            "an over-long cursor must be rejected cleanly"
        );
    }

    // --- Same five, mirrored against LP_RECOVERY_ENDPOINT (prefix 0x02).
    // The only real row available at a LOWER prefix is `Pool` (0x01), so
    // these seed just that.

    async fn seed_pool_row(db: &Database) {
        let mut dbtx = db.begin_transaction().await;
        dbtx.insert_new_entry(
            &PoolKey(pool_id()),
            &Pool {
                reserve_lo: Amount::from_msats(1_000_000),
                reserve_hi: Amount::from_msats(1_000_000),
                total_shares: 1_000_000,
            },
        )
        .await;
        dbtx.commit_tx().await;
    }

    #[tokio::test]
    async fn lp_recovery_empty_cursor() {
        let db = db();
        seed_pool_row(&db).await;
        let module = amm();
        let result: Result<LpRecoveryResponse, ApiError> = call(
            &module,
            &db,
            LP_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: Some(vec![]),
                limit: Some(10),
            },
        )
        .await;
        assert!(
            result.is_err(),
            "an empty cursor must be rejected cleanly, not accepted"
        );
    }

    /// Below `LpPosition`'s own prefix (`0x02`): a one-byte cursor equal to
    /// `Pool`'s prefix (`0x01`) makes `start = [0x01, 0x00]`, sweeping in
    /// the seeded `Pool` row, which fails to decode as `LpPositionKey`.
    #[tokio::test]
    async fn lp_recovery_cursor_below_prefix() {
        let db = db();
        seed_pool_row(&db).await;
        let module = amm();
        let result: Result<LpRecoveryResponse, ApiError> = call(
            &module,
            &db,
            LP_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: Some(vec![0x01]),
                limit: Some(10),
            },
        )
        .await;
        assert!(
            result.is_err(),
            "a cursor below this endpoint's own prefix must be rejected cleanly"
        );
    }

    /// Mirrors `balance_recovery_cursor_above_prefix`: against
    /// `MemDatabase` this does not panic (silently returns an empty page
    /// instead of rejecting the cursor) — included for completeness.
    #[tokio::test]
    async fn lp_recovery_cursor_above_prefix() {
        let db = db();
        {
            let mut dbtx = db.begin_transaction().await;
            dbtx.insert_new_entry(
                &LpPositionKey {
                    pool: pool_id(),
                    owner: test_owner_pubkey(),
                },
                &LpPosition {
                    shares: 1,
                    tweak: [0u8; 16],
                },
            )
            .await;
            dbtx.commit_tx().await;
        }
        let module = amm();
        let result: Result<LpRecoveryResponse, ApiError> = call(
            &module,
            &db,
            LP_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: Some(vec![0xFF]),
                limit: Some(10),
            },
        )
        .await;
        assert!(
            result.is_err(),
            "a cursor above every real prefix must be rejected cleanly, not panic the range scan"
        );
    }

    #[tokio::test]
    async fn lp_recovery_garbage_cursor() {
        let db = db();
        seed_pool_row(&db).await;
        let module = amm();
        let result: Result<LpRecoveryResponse, ApiError> = call(
            &module,
            &db,
            LP_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: Some(vec![0x00, 0xDE, 0xAD, 0xBE, 0xEF]),
                limit: Some(10),
            },
        )
        .await;
        assert!(
            result.is_err(),
            "garbage cursor bytes must be rejected cleanly, not panic"
        );
    }

    #[tokio::test]
    async fn lp_recovery_over_long_cursor() {
        let db = db();
        let module = amm();
        let result: Result<LpRecoveryResponse, ApiError> = call(
            &module,
            &db,
            LP_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor: Some(vec![0u8; 257]),
                limit: Some(10),
            },
        )
        .await;
        assert!(
            result.is_err(),
            "an over-long cursor must be rejected cleanly"
        );
    }
}
