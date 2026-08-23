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
    BALANCE_RECOVERY_ENDPOINT, BalanceRecoveryResponse, LP_RECOVERY_ENDPOINT, LpRecoveryResponse,
    MAX_RECOVERY_PAGE_SIZE, QUOTE_ENDPOINT, QuoteRequest, QuoteResponse, RecoveryPageRequest,
};
use fedimint_amm_common::pool_id::PoolId;
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
    let context = ApiEndpointContext::new(db.clone(), false);
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

/// Finding M9: the quote endpoint now enforces the same admission checks
/// settlement does. `amount_in` below `min_swap_in` (1_000 for both units in
/// this config) must be rejected rather than silently quoted.
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
    assert!(
        result.is_err(),
        "a below-min_swap_in quote must be rejected, got {result:?}"
    );
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
    let mut cursor = 0u64;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages <= row_count + 1, "pagination did not terminate");
        let page: BalanceRecoveryResponse = call(
            &module,
            &db,
            BALANCE_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor,
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
            Some(next) => cursor = next,
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
    let mut cursor = 0u64;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages <= row_count + 1, "pagination did not terminate");
        let page: LpRecoveryResponse = call(
            &module,
            &db,
            LP_RECOVERY_ENDPOINT,
            RecoveryPageRequest {
                cursor,
                limit: None, // omitted -> server defaults to the max page size
            },
        )
        .await
        .expect("LP recovery page must succeed");

        assert!(page.entries.len() <= MAX_RECOVERY_PAGE_SIZE as usize);
        collected_pubkeys.extend(page.entries.iter().map(|e| e.pubkey));

        match page.next_cursor {
            Some(next) => cursor = next,
            None => break,
        }
    }

    assert!(pages >= 2, "the fixture must force at least two pages");
    assert_eq!(collected_pubkeys.len(), row_count);
    collected_pubkeys.sort();
    assert_eq!(collected_pubkeys, expected_pubkeys);
}
