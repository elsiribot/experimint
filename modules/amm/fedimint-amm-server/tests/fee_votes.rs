//! The guardian-voted swap fee (spec §10, §11): the aggregator, the
//! consensus-item admission rules, the proposal diff, and the two endpoints.
//!
//! Same style as the other server tests: a bare `MemDatabase` plus the real
//! `ServerModule` methods and the real `ApiEndpoint<Amm>::handler` out of
//! `Amm::api_endpoints()`, never a reimplementation of either.

use std::collections::BTreeMap;

use fedimint_amm_common::config::{AmmConfigConsensus, UnitParams};
use fedimint_amm_common::endpoints::{
    FEE_VOTE_SUBMIT_ENDPOINT, FEE_VOTES_ENDPOINT, FeeVoteSubmitRequest, FeeVotesRequest,
    FeeVotesResponse, POOLS_ENDPOINT, PoolSummary, QUOTE_ENDPOINT, QuoteRequest, QuoteResponse,
};
use fedimint_amm_common::math;
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_common::types::AmmConsensusItem;
use fedimint_amm_server::Amm;
use fedimint_amm_server::db::{DesiredFeeKey, FeeVoteKey, Pool, PoolKey};
use fedimint_core::db::mem_impl::MemDatabase;
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::module::{AmountUnit, ApiAuth, ApiEndpointContext, ApiError, ApiRequestErased};
use fedimint_core::{Amount, NumPeers, PeerId};
use fedimint_server_core::ServerModule;
use serde::Serialize;
use serde::de::DeserializeOwned;

fn db() -> Database {
    Database::new(MemDatabase::new(), ModuleDecoderRegistry::default())
}

fn unit(n: u64) -> AmountUnit {
    AmountUnit::new_custom(n)
}

fn pool01() -> PoolId {
    PoolId::new(unit(0), unit(1)).expect("0 != 1")
}

fn pool02() -> PoolId {
    PoolId::new(unit(0), unit(2)).expect("0 != 2")
}

/// Band `[1, 50]` — the same shape `default_consensus_config` generates, so
/// these tests exercise the real admission rule rather than a wide-open one.
/// Config fee 3, i.e. the value the aggregator falls back to.
fn amm_with(num_peers: usize, our_peer_id: u16) -> Amm {
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
                (
                    unit(2),
                    UnitParams {
                        min_swap_in: Amount::from_msats(1_000),
                    },
                ),
            ]),
            default_fee_per_mille: 3,
            fee_overrides: BTreeMap::new(),
            min_fee_per_mille: 1,
            max_fee_per_mille: 50,
        },
        NumPeers::from(num_peers),
        PeerId::from(our_peer_id),
    )
}

fn amm() -> Amm {
    amm_with(4, 0)
}

/// Reserves large enough that `amount_out` is fee-sensitive at the msat
/// granularity these tests assert on.
async fn create_pool(db: &Database, pool: PoolId) {
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

async fn insert_votes(db: &Database, pool: PoolId, votes: &[(u16, u16)]) {
    let mut dbtx = db.begin_transaction().await;
    for (peer, fee) in votes {
        dbtx.insert_entry(
            &FeeVoteKey {
                pool,
                peer: PeerId::from(*peer),
            },
            fee,
        )
        .await;
    }
    dbtx.commit_tx().await;
}

async fn consensus_fee(module: &Amm, db: &Database, pool: PoolId) -> u16 {
    let mut dbtx = db.begin_transaction_nc().await;
    module.consensus_fee_for(&mut dbtx, pool).await
}

/// Calls the real handler for `path` out of `module.api_endpoints()`.
/// `authenticated` mirrors what the server itself computes: it sets
/// `has_auth`, the *verified* flag, not merely the presence of a password on
/// the request.
async fn call<R: DeserializeOwned>(
    module: &Amm,
    db: &Database,
    path: &str,
    authenticated: bool,
    params: impl Serialize,
) -> Result<R, ApiError> {
    let endpoint = module
        .api_endpoints()
        .into_iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("endpoint {path} must be registered"));
    let context = ApiEndpointContext::new(
        db.clone(),
        authenticated,
        authenticated.then(|| ApiAuth::new("pass".to_string())),
    );
    let request = ApiRequestErased::new(params);
    let value = (endpoint.handler)(module, context, request).await?;
    Ok(fedimint_core::module::serde_json::from_value(value)
        .expect("response must deserialize into the expected type"))
}

// ---------------------------------------------------------------------------
// The aggregator.
// ---------------------------------------------------------------------------

/// With no votes at all, the fee is the config value — NOT an error and not
/// zero. A freshly-DKG'd federation must be able to swap before any guardian
/// has voted, which is why `consensus_fee_for` returns `u16` rather than the
/// `Option` `fedimint-walletv2-server`'s `consensus_feerate` returns.
#[tokio::test]
async fn falls_back_to_the_config_fee_before_any_votes() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    assert_eq!(consensus_fee(&module, &db, pool01()).await, 3);
}

/// Below threshold the config fee still applies: a minority of guardians
/// cannot move the fee at all, not even part way.
#[tokio::test]
async fn below_threshold_votes_do_not_move_the_fee() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    // 4 peers => threshold 3. Two votes is one short.
    insert_votes(&db, pool01(), &[(0, 20), (1, 20)]).await;
    assert_eq!(consensus_fee(&module, &db, pool01()).await, 3);

    insert_votes(&db, pool01(), &[(2, 20)]).await;
    assert_eq!(consensus_fee(&module, &db, pool01()).await, 20);
}

/// One case of [`picks_the_threshold_index_of_the_sorted_votes`].
struct AggregationCase {
    total_peers: usize,
    /// Restated rather than derived, so a change to `NumPeers::threshold`
    /// fails here instead of silently re-deriving the expectations.
    threshold: usize,
    votes: &'static [(u16, u16)],
    expected_fee: u16,
}

/// The index taken is `threshold() - 1` of the ascending votes, for every
/// federation size — spelled out with hand-computed expectations rather than
/// by recomputing the index, so an off-by-one in the aggregator cannot cancel
/// against an off-by-one here.
#[tokio::test]
async fn picks_the_threshold_index_of_the_sorted_votes() {
    let cases = [
        // n=1 => max_evil 0 => threshold 1 => index 0 => the only vote.
        AggregationCase {
            total_peers: 1,
            threshold: 1,
            votes: &[(0, 7)],
            expected_fee: 7,
        },
        // n=4 => max_evil 1 => threshold 3 => index 2 of 4 sorted.
        AggregationCase {
            total_peers: 4,
            threshold: 3,
            votes: &[(0, 40), (1, 10), (2, 20), (3, 30)],
            expected_fee: 30,
        },
        // n=4 with exactly threshold-many votes => the largest of them.
        AggregationCase {
            total_peers: 4,
            threshold: 3,
            votes: &[(0, 10), (1, 30), (2, 20)],
            expected_fee: 30,
        },
        // n=7 => max_evil 2 => threshold 5 => index 4 of 7 sorted.
        AggregationCase {
            total_peers: 7,
            threshold: 5,
            votes: &[(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (5, 6), (6, 7)],
            expected_fee: 5,
        },
        // n=10 => max_evil 3 => threshold 7 => index 6 of 10 sorted.
        AggregationCase {
            total_peers: 10,
            threshold: 7,
            votes: &[
                (0, 10),
                (1, 9),
                (2, 8),
                (3, 7),
                (4, 6),
                (5, 5),
                (6, 4),
                (7, 3),
                (8, 2),
                (9, 1),
            ],
            expected_fee: 7,
        },
    ];

    for case in cases {
        assert_eq!(
            NumPeers::from(case.total_peers).threshold(),
            case.threshold,
            "the expectations below are written against this threshold"
        );

        let db = db();
        let module = amm_with(case.total_peers, 0);
        create_pool(&db, pool01()).await;
        insert_votes(&db, pool01(), case.votes).await;

        assert_eq!(
            consensus_fee(&module, &db, pool01()).await,
            case.expected_fee,
            "n={}",
            case.total_peers
        );
    }
}

/// The property the threshold index exists for, and the reason this is not a
/// plain `[len / 2]` median: with `n = 4` and `max_evil() = 1`, whatever the
/// single faulty guardian votes, the outcome stays inside the range the three
/// honest guardians voted. A `[len / 2]` median over the same votes lands on
/// the faulty value when it sorts low.
#[tokio::test]
async fn a_single_faulty_guardian_cannot_pull_the_fee_outside_the_honest_range() {
    let honest = [10u16, 20, 30];

    for faulty in [1u16, 15, 50] {
        let db = db();
        let module = amm();
        create_pool(&db, pool01()).await;
        insert_votes(
            &db,
            pool01(),
            &[(0, honest[0]), (1, honest[1]), (2, honest[2]), (3, faulty)],
        )
        .await;

        let fee = consensus_fee(&module, &db, pool01()).await;
        assert!(
            (honest[0]..=honest[2]).contains(&fee),
            "faulty vote {faulty} moved the fee to {fee}, outside the honest range \
             [{}, {}]",
            honest[0],
            honest[2]
        );
    }
}

/// Votes are per-pool: one pool's votes must never be read as another's. This
/// is what the `{ pool, peer }` field order of `FeeVoteKey` buys.
#[tokio::test]
async fn votes_are_scoped_to_their_own_pool() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;
    create_pool(&db, pool02()).await;

    insert_votes(&db, pool01(), &[(0, 40), (1, 40), (2, 40)]).await;

    assert_eq!(consensus_fee(&module, &db, pool01()).await, 40);
    assert_eq!(
        consensus_fee(&module, &db, pool02()).await,
        3,
        "pool02 has no votes of its own and must fall back to the config fee"
    );
}

/// The clamp is load bearing on the fallback branch: a DKG-time config fee is
/// under no obligation to sit inside a band, and the fee actually charged
/// must be inside it regardless of where it came from.
#[tokio::test]
async fn an_out_of_band_config_fee_is_clamped_into_the_band() {
    let db = db();
    let mut module = amm();
    module.cfg.default_fee_per_mille = 900;
    create_pool(&db, pool01()).await;

    assert_eq!(consensus_fee(&module, &db, pool01()).await, 50);

    module.cfg.default_fee_per_mille = 0;
    assert_eq!(consensus_fee(&module, &db, pool01()).await, 1);
}

/// The clamp on the vote branch is redundant with `process_consensus_item`'s
/// band check and exists so a weakening of that check cannot reach the fee
/// actually charged. Written directly into the table (bypassing
/// `process_consensus_item`) precisely because that is the scenario.
#[tokio::test]
async fn out_of_band_rows_are_clamped_even_if_they_reach_the_table() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;
    insert_votes(&db, pool01(), &[(0, 999), (1, 999), (2, 999)]).await;

    assert_eq!(consensus_fee(&module, &db, pool01()).await, 50);
}

// ---------------------------------------------------------------------------
// `process_consensus_item`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accepts_an_in_band_vote_for_an_existing_pool() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    let mut dbtx = db.begin_transaction().await;
    module
        .process_consensus_item(
            &mut dbtx.to_ref_nc(),
            AmmConsensusItem::FeeVoteV0 {
                pool: pool01(),
                fee_per_mille: 12,
            },
            PeerId::from(1),
        )
        .await
        .expect("an in-band vote for an existing pool must be accepted");
    assert_eq!(
        dbtx.get_value(&FeeVoteKey {
            pool: pool01(),
            peer: PeerId::from(1)
        })
        .await,
        Some(12)
    );
    dbtx.commit_tx().await;
}

#[tokio::test]
async fn rejects_votes_outside_the_configured_band() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    for fee in [0u16, 51, 999] {
        let mut dbtx = db.begin_transaction().await;
        let error = module
            .process_consensus_item(
                &mut dbtx.to_ref_nc(),
                AmmConsensusItem::FeeVoteV0 {
                    pool: pool01(),
                    fee_per_mille: fee,
                },
                PeerId::from(1),
            )
            .await
            .expect_err("a vote outside [1, 50] must be rejected");
        assert!(
            error.to_string().contains("outside the configured band"),
            "unexpected error for fee {fee}: {error}"
        );
    }
}

/// The band's own endpoints must be accepted — an off-by-one that excluded
/// them would make the configured band narrower than it reads.
#[tokio::test]
async fn accepts_votes_exactly_on_the_band_endpoints() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    for (peer, fee) in [(1u16, 1u16), (2, 50)] {
        let mut dbtx = db.begin_transaction().await;
        module
            .process_consensus_item(
                &mut dbtx.to_ref_nc(),
                AmmConsensusItem::FeeVoteV0 {
                    pool: pool01(),
                    fee_per_mille: fee,
                },
                PeerId::from(peer),
            )
            .await
            .expect("a vote on the band boundary must be accepted");
        dbtx.commit_tx().await;
    }
}

#[tokio::test]
async fn rejects_a_vote_for_a_pool_that_does_not_exist() {
    let db = db();
    let module = amm();

    let mut dbtx = db.begin_transaction().await;
    let error = module
        .process_consensus_item(
            &mut dbtx.to_ref_nc(),
            AmmConsensusItem::FeeVoteV0 {
                pool: pool01(),
                fee_per_mille: 12,
            },
            PeerId::from(1),
        )
        .await
        .expect_err("a vote naming a non-existent pool must be rejected");
    assert!(error.to_string().contains("pool that does not exist"));
}

/// `ServerModule::process_consensus_item`'s contract: an item that changes no
/// state must return `Err`, or a peer can pad every session for free.
/// Changing the vote and changing it back must both still be accepted.
#[tokio::test]
async fn rejects_a_redundant_vote_but_accepts_a_change() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    let vote = |fee| AmmConsensusItem::FeeVoteV0 {
        pool: pool01(),
        fee_per_mille: fee,
    };

    let mut dbtx = db.begin_transaction().await;
    module
        .process_consensus_item(&mut dbtx.to_ref_nc(), vote(12), PeerId::from(1))
        .await
        .expect("first vote");
    dbtx.commit_tx().await;

    let mut dbtx = db.begin_transaction().await;
    let error = module
        .process_consensus_item(&mut dbtx.to_ref_nc(), vote(12), PeerId::from(1))
        .await
        .expect_err("re-voting the same value changes nothing and must be rejected");
    assert!(error.to_string().contains("redundant"));

    let mut dbtx = db.begin_transaction().await;
    module
        .process_consensus_item(&mut dbtx.to_ref_nc(), vote(13), PeerId::from(1))
        .await
        .expect("changing the vote must be accepted");
    dbtx.commit_tx().await;

    let mut dbtx = db.begin_transaction().await;
    module
        .process_consensus_item(&mut dbtx.to_ref_nc(), vote(12), PeerId::from(1))
        .await
        .expect("changing back must be accepted");
    dbtx.commit_tx().await;
}

/// A different guardian voting the same value is a distinct row, not a
/// redundant item.
#[tokio::test]
async fn the_same_value_from_a_different_peer_is_not_redundant() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    let vote = AmmConsensusItem::FeeVoteV0 {
        pool: pool01(),
        fee_per_mille: 12,
    };

    for peer in [1u16, 2] {
        let mut dbtx = db.begin_transaction().await;
        module
            .process_consensus_item(&mut dbtx.to_ref_nc(), vote.clone(), PeerId::from(peer))
            .await
            .expect("each peer's first vote is its own row");
        dbtx.commit_tx().await;
    }
}

/// A peer running a newer binary can legitimately send a variant this one
/// does not know. That must be an ordinary consensus rejection, never a
/// panic — the legacy wallet module panics here and is not the template.
#[tokio::test]
async fn rejects_an_unknown_consensus_item_variant_without_panicking() {
    let db = db();
    let module = amm();

    let mut dbtx = db.begin_transaction().await;
    let error = module
        .process_consensus_item(
            &mut dbtx.to_ref_nc(),
            AmmConsensusItem::Default {
                variant: 42,
                bytes: vec![1, 2, 3],
            },
            PeerId::from(1),
        )
        .await
        .expect_err("an unknown variant must be rejected");
    assert!(error.to_string().contains("unknown variant 42"));
}

// ---------------------------------------------------------------------------
// `consensus_proposal`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proposes_nothing_when_no_fee_has_been_desired() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    let mut dbtx = db.begin_transaction_nc().await;
    assert!(module.consensus_proposal(&mut dbtx).await.is_empty());
}

#[tokio::test]
async fn proposes_the_difference_and_then_stops() {
    let db = db();
    let module = amm_with(4, 2);
    create_pool(&db, pool01()).await;

    {
        let mut dbtx = db.begin_transaction().await;
        dbtx.insert_entry(&DesiredFeeKey(pool01()), &12u16).await;
        dbtx.commit_tx().await;
    }

    let mut dbtx = db.begin_transaction_nc().await;
    assert_eq!(
        module.consensus_proposal(&mut dbtx).await,
        vec![AmmConsensusItem::FeeVoteV0 {
            pool: pool01(),
            fee_per_mille: 12
        }]
    );
    drop(dbtx);

    // Once our own vote is recorded, there is no difference left to propose.
    insert_votes(&db, pool01(), &[(2, 12)]).await;
    let mut dbtx = db.begin_transaction_nc().await;
    assert!(module.consensus_proposal(&mut dbtx).await.is_empty());
}

/// The diff is against THIS guardian's recorded vote. Another peer having
/// voted the desired value already changes nothing about what we owe
/// consensus.
#[tokio::test]
async fn diffs_against_our_own_recorded_vote_not_another_peers() {
    let db = db();
    let module = amm_with(4, 2);
    create_pool(&db, pool01()).await;

    {
        let mut dbtx = db.begin_transaction().await;
        dbtx.insert_entry(&DesiredFeeKey(pool01()), &12u16).await;
        dbtx.commit_tx().await;
    }
    insert_votes(&db, pool01(), &[(0, 12), (1, 12)]).await;

    let mut dbtx = db.begin_transaction_nc().await;
    assert_eq!(
        module.consensus_proposal(&mut dbtx).await,
        vec![AmmConsensusItem::FeeVoteV0 {
            pool: pool01(),
            fee_per_mille: 12
        }]
    );
}

/// The no-salt design (see `AmmConsensusItem`'s doc comment) rests entirely
/// on this: a vote AlephBFT merged away is still a difference next session
/// and is proposed again. Modelled by moving the desired value `a -> b -> a`
/// while only `b` gets recorded.
#[tokio::test]
async fn re_proposes_a_vote_that_never_got_recorded() {
    let db = db();
    let module = amm_with(4, 2);
    create_pool(&db, pool01()).await;

    let set_desired = async |fee: u16| {
        let mut dbtx = db.begin_transaction().await;
        dbtx.insert_entry(&DesiredFeeKey(pool01()), &fee).await;
        dbtx.commit_tx().await;
    };

    set_desired(12).await;
    insert_votes(&db, pool01(), &[(2, 12)]).await;
    set_desired(13).await;
    insert_votes(&db, pool01(), &[(2, 13)]).await;
    // Back to 12; the item is byte-identical to the one already ordered this
    // session, so it is merged away and never reaches `FeeVoteKey`.
    set_desired(12).await;

    let mut dbtx = db.begin_transaction_nc().await;
    assert_eq!(
        module.consensus_proposal(&mut dbtx).await,
        vec![AmmConsensusItem::FeeVoteV0 {
            pool: pool01(),
            fee_per_mille: 12
        }],
        "a merged-away vote must still be a difference in the next session"
    );
}

/// Proposing a vote every peer must reject would be a permanent proposal
/// loop, so a desired fee for a pool that does not exist is skipped instead.
#[tokio::test]
async fn does_not_propose_a_vote_for_a_pool_that_does_not_exist() {
    let db = db();
    let module = amm();

    let mut dbtx = db.begin_transaction().await;
    dbtx.insert_entry(&DesiredFeeKey(pool01()), &12u16).await;
    dbtx.commit_tx().await;

    let mut dbtx = db.begin_transaction_nc().await;
    assert!(module.consensus_proposal(&mut dbtx).await.is_empty());
}

// ---------------------------------------------------------------------------
// Endpoints.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn submit_endpoint_requires_verified_guardian_auth() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    let request = FeeVoteSubmitRequest {
        pool: pool01(),
        fee_per_mille: 12,
    };

    call::<()>(&module, &db, FEE_VOTE_SUBMIT_ENDPOINT, false, request)
        .await
        .expect_err("an unauthenticated caller must not be able to set a guardian's vote");

    let mut dbtx = db.begin_transaction_nc().await;
    assert_eq!(dbtx.get_value(&DesiredFeeKey(pool01())).await, None);
    drop(dbtx);

    call::<()>(&module, &db, FEE_VOTE_SUBMIT_ENDPOINT, true, request)
        .await
        .expect("an authenticated guardian may set its own vote");

    let mut dbtx = db.begin_transaction_nc().await;
    assert_eq!(dbtx.get_value(&DesiredFeeKey(pool01())).await, Some(12));
}

#[tokio::test]
async fn submit_endpoint_rejects_out_of_band_and_unknown_pools() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    let out_of_band = call::<()>(
        &module,
        &db,
        FEE_VOTE_SUBMIT_ENDPOINT,
        true,
        FeeVoteSubmitRequest {
            pool: pool01(),
            fee_per_mille: 51,
        },
    )
    .await
    .expect_err("a fee outside the band must be rejected at submit time");
    assert!(out_of_band.message.contains("within [1, 50]"));

    let unknown_pool = call::<()>(
        &module,
        &db,
        FEE_VOTE_SUBMIT_ENDPOINT,
        true,
        FeeVoteSubmitRequest {
            pool: pool02(),
            fee_per_mille: 12,
        },
    )
    .await
    .expect_err("a vote for a pool that does not exist must be rejected at submit time");
    assert!(unknown_pool.message.contains("no such pool"));

    let mut dbtx = db.begin_transaction_nc().await;
    assert_eq!(dbtx.get_value(&DesiredFeeKey(pool01())).await, None);
    assert_eq!(dbtx.get_value(&DesiredFeeKey(pool02())).await, None);
}

/// Re-submitting the value already desired must succeed and change nothing,
/// so a guardian UI can submit unconditionally. (The *consensus* item for an
/// unchanged vote is separately rejected as redundant; that is a different
/// layer.)
#[tokio::test]
async fn submit_endpoint_is_idempotent() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    let request = FeeVoteSubmitRequest {
        pool: pool01(),
        fee_per_mille: 12,
    };
    for _ in 0..3 {
        call::<()>(&module, &db, FEE_VOTE_SUBMIT_ENDPOINT, true, request)
            .await
            .expect("re-submitting the desired value must succeed");
    }

    let mut dbtx = db.begin_transaction_nc().await;
    assert_eq!(dbtx.get_value(&DesiredFeeKey(pool01())).await, Some(12));
}

#[tokio::test]
async fn fee_votes_endpoint_reports_votes_and_the_aggregate_unauthenticated() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;
    insert_votes(&db, pool01(), &[(0, 10), (1, 20), (2, 30)]).await;

    let response: FeeVotesResponse = call(
        &module,
        &db,
        FEE_VOTES_ENDPOINT,
        false,
        FeeVotesRequest { pool: pool01() },
    )
    .await
    .expect("the votes endpoint is public");

    assert_eq!(
        response.votes,
        BTreeMap::from([
            (PeerId::from(0), 10),
            (PeerId::from(1), 20),
            (PeerId::from(2), 30),
        ])
    );
    assert_eq!(response.effective_fee_per_mille, 30);
    assert_eq!(response.min_fee_per_mille, 1);
    assert_eq!(response.max_fee_per_mille, 50);
}

/// Absence of a vote is "has not voted", not "voted the default" — the
/// reported aggregate must still be the config fallback.
#[tokio::test]
async fn fee_votes_endpoint_reports_an_empty_map_before_any_votes() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    let response: FeeVotesResponse = call(
        &module,
        &db,
        FEE_VOTES_ENDPOINT,
        false,
        FeeVotesRequest { pool: pool01() },
    )
    .await
    .expect("the votes endpoint is public");

    assert!(response.votes.is_empty());
    assert_eq!(response.effective_fee_per_mille, 3);
}

/// Both new endpoints must actually be wired into `api_endpoints()` under
/// the names `common` publishes — a handler that exists but is never
/// registered, or registered at a different path, is unreachable.
#[test]
fn the_new_endpoints_are_registered_under_their_published_paths() {
    let paths: Vec<&str> = amm().api_endpoints().iter().map(|e| e.path).collect();
    assert!(paths.contains(&FEE_VOTE_SUBMIT_ENDPOINT));
    assert!(paths.contains(&FEE_VOTES_ENDPOINT));
}

// ---------------------------------------------------------------------------
// The voted fee is the fee everything reports and settles at.
// ---------------------------------------------------------------------------

/// `POOLS_ENDPOINT` is the only channel through which the client learns the
/// fee, and `QUOTE_ENDPOINT` shares `quote_swap` with settlement. Both must
/// follow the votes, not the config — the two together are what stop the
/// displayed fee from diverging from what a swap actually costs.
#[tokio::test]
async fn pools_and_quote_endpoints_both_follow_the_votes() {
    let db = db();
    let module = amm();
    create_pool(&db, pool01()).await;

    let amount_in = Amount::from_msats(10_000_000);
    let quote_request = QuoteRequest {
        unit_in: unit(0),
        unit_out: unit(1),
        amount_in,
    };

    let before: Vec<PoolSummary> = call(&module, &db, POOLS_ENDPOINT, false, ())
        .await
        .expect("pools");
    assert_eq!(before[0].fee_per_mille, 3);
    let quote_before: QuoteResponse = call(&module, &db, QUOTE_ENDPOINT, false, quote_request)
        .await
        .expect("quote");

    insert_votes(&db, pool01(), &[(0, 30), (1, 30), (2, 30)]).await;

    let after: Vec<PoolSummary> = call(&module, &db, POOLS_ENDPOINT, false, ())
        .await
        .expect("pools");
    assert_eq!(after[0].fee_per_mille, 30);
    let quote_after: QuoteResponse = call(&module, &db, QUOTE_ENDPOINT, false, quote_request)
        .await
        .expect("quote");

    // Independent expectations, computed straight from `math::amount_out` at
    // the two fees, so a quote that silently kept using the config fee cannot
    // pass by accident.
    assert_eq!(
        quote_before.amount_out,
        Amount::from_msats(
            math::amount_out(1_000_000_000, 1_000_000, amount_in.msats, 3).expect("valid swap")
        )
    );
    assert_eq!(
        quote_after.amount_out,
        Amount::from_msats(
            math::amount_out(1_000_000_000, 1_000_000, amount_in.msats, 30).expect("valid swap")
        )
    );
    assert!(
        quote_after.amount_out < quote_before.amount_out,
        "a higher voted fee must return less"
    );
}
