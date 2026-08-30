//! The full experimint deployment topology, in one in-process federation,
//! with value moved across every leg of it.
//!
//! `README.md`'s "intended topology" is seven module instances --- `walletv2`,
//! `mintv2` (BITCOIN), `mintv2` ([`USDT_UNIT`]), `lnv2`, `usdt`, `amm`, `meta`
//! --- and nothing in this repo previously stood all seven up at once. Every
//! existing suite covers one module family against a minimal supporting cast:
//! `fedimint-amm-tests` trades against a purpose-built test-only `faucet`
//! module rather than a real second `mintv2` (so "the AMM trades real USDt"
//! was untested), and `fedimint-usdt-tests`' own dual-mint fixtures carry no
//! `amm`, `walletv2` or `lnv2` at all.
//!
//! Two tests live here, split by cost rather than by subject:
//!
//! - [`seven_instance_topology_stands_up`] is hermetic and fast: it proves the
//!   seven-instance federation config-gens, boots, and presents exactly the
//!   README topology to a client, including which `mintv2` instance is primary
//!   for which unit. It needs no external daemon.
//! - [`value_moves_across_the_full_topology`] is the end-to-end value flow:
//!   BTC in over `walletv2`, USDt in over `usdt`'s deposit-by-proof path
//!   against a real `anvil`, both legs seeded into an `amm` pool, a threshold
//!   of guardians voting the swap fee away from its DKG-time default, a swap
//!   in each direction settling at the quoted price *at that voted fee*, the
//!   LP position withdrawn, and the guardians' balance sheets checked at the
//!   end.
//!
//! # Unit alignment (load-bearing)
//!
//! `USDT_UNIT == AmountUnit::new_custom(1)`
//! (`fedimint-usdt-common/src/lib.rs`), the second `mintv2` instance is
//! config-gen'd with exactly that `amount_unit`, and `amm`'s default unit
//! allowlist is `{AmountUnit::BITCOIN, AmountUnit::new_custom(1)}`
//! (`fedimint-amm-server::default_consensus_config`). All three must agree or
//! the pool below cannot exist. USDt *balances* live in the second `mintv2`,
//! never in the `usdt` module --- `usdt` holds the on-chain peg and issues
//! into that mint.
//!
//! # Why `anvil` is required rather than optional
//!
//! The pre-existing `common::spawn_anvil` returns `Ok(None)` when the binary
//! is missing so its callers can skip. This file deliberately does not use
//! that affordance: a "USDt in" step that silently no-ops is
//! indistinguishable from a passing one, and the whole point of this test is
//! that the AMM trades real USDt. [`require_anvil`] fails loudly instead.
//! `anvil` ships in this repo's dev shell (`flake.nix` pulls in `foundry`).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use alloy::providers::{Provider as _, ProviderBuilder};
use anyhow::{Context as _, bail, ensure};
use common::MockEvmRpc;
use fedimint_amm_client::db::{LpPositionKey, LpPositionRecord};
use fedimint_amm_client::{AmmClientInit, AmmClientModule};
use fedimint_amm_common::endpoints::{
    FEE_VOTE_SUBMIT_ENDPOINT, FEE_VOTES_ENDPOINT, FeeVoteSubmitRequest, FeeVotesRequest,
    FeeVotesResponse, PoolSummary,
};
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_server::AmmInit;
use fedimint_api_client::api::FederationApiExt as _;
use fedimint_client::ClientHandleArc;
use fedimint_core::core::{ModuleInstanceId, ModuleKind};
use fedimint_core::module::audit::AuditSummary;
use fedimint_core::module::{AmountUnit, ApiAuth, ApiRequestErased};
use fedimint_core::runtime::{Instant, sleep};
use fedimint_core::{Amount, NumPeersExt as _, PeerId};
use fedimint_lnv2_client::LightningClientInit;
use fedimint_lnv2_server::LightningInit;
use fedimint_logging::LOG_TEST;
use fedimint_meta_client::MetaClientInit;
use fedimint_meta_server::MetaInit;
use fedimint_mintv2_client::MintClientInit as Mintv2ClientInit;
use fedimint_mintv2_common::KIND as MINTV2_KIND;
use fedimint_mintv2_common::config::MintGenParams;
use fedimint_mintv2_server::MintInit as Mintv2Init;
use fedimint_testing::btc::BitcoinTest;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use fedimint_usdt_client::{UsdtClientInit, UsdtClientModule};
use fedimint_usdt_common::{EvmAddress, USDT_UNIT, UsdtAmount, UsdtGenParams};
use fedimint_usdt_server::UsdtInit;
use fedimint_usdt_server::rpc::{AlloyEvmRpc, IServerEvmRpc};
use fedimint_walletv2_client::{WalletClientInit, WalletClientModule};
use fedimint_walletv2_server::{CONFIRMATION_FINALITY_DELAY, WalletInit};
use tracing::info;

/// `anvil`'s dev chain id, matching `common::spawn_anvil`'s `--chain-id`.
const ANVIL_CHAIN_ID: u64 = 31337;

/// The federation-wide admin password `fedimint-testing` hands every guardian
/// (`FederationTestBuilder::build` passes `ApiAuth::new("pass")` as both the
/// API and setup auth). Needed to read the `audit` endpoint, which is
/// admin-only.
const TESTING_API_AUTH: &str = "pass";

/// The pool these tests trade: BITCOIN against [`USDT_UNIT`] --- exactly the
/// pair `fedimint-amm-server::default_consensus_config` allowlists.
fn pool_id() -> PoolId {
    PoolId::new(AmountUnit::BITCOIN, USDT_UNIT).expect("BITCOIN != USDT_UNIT")
}

/// The smallest client-held `mintv2` note denomination, `2^9` msat
/// (`fedimint-mintv2-common::config::client_denominations`, `9..42`).
const MINTV2_MIN_DENOMINATION_MSATS: u64 = 512;

/// `fedimint-amm-server::default_consensus_config`'s `default_fee_per_mille`:
/// what every pool charges until a threshold of guardians votes otherwise.
const CONFIG_DEFAULT_FEE_PER_MILLE: u16 = 3;

/// The fee this test's guardians vote. Inside `default_consensus_config`'s
/// `[1, 50]` band and an order of magnitude away from
/// [`CONFIG_DEFAULT_FEE_PER_MILLE`], so a swap priced at one is unmistakably
/// not priced at the other --- asserted below rather than assumed. Deliberately
/// the same gap `fedimint-amm-tests`'
/// `a_threshold_of_guardian_votes_changes_the_fee_a_swap_settles_at` votes
/// across, so any divergence between the two suites is a difference in
/// topology rather than in the numbers picked.
const VOTED_FEE_PER_MILLE: u16 = 30;

/// How long the fee-vote steps wait for a submission to be ordered and applied.
///
/// A vote only becomes a consensus item on the submitting guardian's next
/// `consensus_proposal`, so there is nothing deterministic to await: every
/// guardian converges on the same value, just not at the same instant. This is
/// longer than `fedimint-amm-tests` needs for the same steps because six other
/// module instances are proposing into the same sessions here.
const FEE_CONSENSUS_TIMEOUT: Duration = Duration::from_secs(120);

/// How long [`peg_in_btc`] lets `walletv2`'s client-side address scanner grind
/// before declaring it stuck. Generous: it is a ~1-in-65_536 search running in
/// an unoptimized test build, alongside three other in-process guardians.
const ADDRESS_GRIND_TIMEOUT: Duration = Duration::from_secs(180);

/// Floors `amount` to a value `mintv2` can represent exactly with its
/// client-held note denominations.
///
/// `represent_amount_with_fees` greedily represents an issued amount with
/// denominations at or above [`MINTV2_MIN_DENOMINATION_MSATS`] and silently
/// forfeits the remainder. Unlike `fedimint-amm-tests`, where the second unit
/// is a fee-free, denomination-free test faucet, *both* units here are held in
/// a real `mintv2` instance --- so any assertion that a wallet gained exactly
/// what the AMM paid out must account for this on both legs, not just the BTC
/// one.
///
/// The same floor applies to amounts this test *chooses* (pool seeding, swap
/// sizes): an amount off that grid cannot be spent down to the last msat
/// either, and the primary module reports "Insufficient funds" for the
/// shortfall.
fn mintv2_representable_floor(amount: Amount) -> Amount {
    Amount::from_msats(
        (amount.msats / MINTV2_MIN_DENOMINATION_MSATS) * MINTV2_MIN_DENOMINATION_MSATS,
    )
}

/// The seven-instance [`Fixtures`] of `README.md`'s intended topology.
///
/// `evm_rpc` is shared by every guardian so their independently-run EVM
/// pollers observe identical state, which is what lets their block-hash
/// observations reach consensus; `gen_params` carries the addresses of an
/// already-deployed EVM stack (the deposit account is
/// `CREATE2(account_factory, salt(claim_pk), ..)`, so the factory must exist
/// before config-gen bakes it into `UsdtConfigConsensus`).
///
/// Instance ids are assigned by `build_module_params_registry` in the server
/// init registry's own (kind-ordered) iteration order, with
/// `with_extra_module_instance`'s additions appended last --- *not* in the
/// positional order `fedimint-cli`'s `--module` flags produce. Nothing here
/// depends on the numeric ids; the units and kinds are what must line up.
fn full_topology_fixtures(evm_rpc: Arc<dyn IServerEvmRpc>, gen_params: UsdtGenParams) -> Fixtures {
    Fixtures::new_primary(Mintv2ClientInit, Mintv2Init)
        .with_extra_module_instance(
            MINTV2_KIND,
            MintGenParams {
                amount_unit: USDT_UNIT,
            },
        )
        .with_module(WalletClientInit, WalletInit)
        .with_module(LightningClientInit::default(), LightningInit)
        .with_module(
            UsdtClientInit,
            UsdtInit::with_evm_rpc(evm_rpc).with_gen_params(gen_params),
        )
        .with_module(AmmClientInit, AmmInit)
        .with_module(MetaClientInit, MetaInit)
}

/// The kind of every module instance the federation actually runs, as the
/// client sees it, sorted so it can be compared against an expected multiset.
async fn instance_kinds(client: &ClientHandleArc) -> Vec<String> {
    let mut kinds: Vec<String> = client
        .config()
        .await
        .modules
        .values()
        .map(|module| module.kind().to_string())
        .collect();
    kinds.sort();
    kinds
}

/// The seven kinds `README.md`'s topology runs, sorted to match
/// [`instance_kinds`]. `mintv2` appears twice --- that is the point.
fn expected_topology_kinds() -> Vec<String> {
    let mut kinds: Vec<String> = [
        fedimint_amm_common::KIND,
        fedimint_lnv2_common::KIND,
        fedimint_meta_common::KIND,
        MINTV2_KIND,
        MINTV2_KIND,
        fedimint_usdt_common::KIND,
        fedimint_walletv2_common::KIND,
    ]
    .iter()
    .map(ModuleKind::to_string)
    .collect();
    kinds.sort();
    kinds
}

/// Asserts the client resolved a distinct `mintv2` instance as the primary
/// module for each of the two units, and returns the two instance ids
/// (BITCOIN first).
///
/// This is the property the whole topology rests on: `mintv2`'s client
/// registers `PrimaryModuleSupport::selected(HIGH, [cfg.amount_unit])`, so
/// with two instances config-gen'd on different units the transaction
/// builder's per-unit auto-balancing routes each leg to the right mint. If
/// both units resolved to the same instance, every "USDt" balance below would
/// really be BTC.
async fn assert_two_distinct_primary_mints(
    client: &ClientHandleArc,
) -> anyhow::Result<(ModuleInstanceId, ModuleInstanceId)> {
    let config = client.config().await;

    let (btc_id, _) = client
        .primary_module_for_unit(AmountUnit::BITCOIN)
        .context("no primary module for BITCOIN")?;
    let (usdt_id, _) = client
        .primary_module_for_unit(USDT_UNIT)
        .context("no primary module for USDT_UNIT")?;

    ensure!(
        btc_id != usdt_id,
        "BITCOIN and USDT_UNIT must resolve to different mintv2 instances, both resolved to {btc_id}"
    );
    for (unit, id) in [("BITCOIN", btc_id), ("USDT_UNIT", usdt_id)] {
        let kind = config.modules[&id].kind();
        ensure!(
            *kind == MINTV2_KIND,
            "the primary module for {unit} must be mintv2, got {kind}"
        );
    }

    Ok((btc_id, usdt_id))
}

/// Spawns `anvil`, failing (rather than skipping) if it is unavailable.
///
/// See this file's module doc comment: a skipped USDt leg would make this
/// suite's central claim unfalsifiable.
async fn require_anvil() -> anyhow::Result<common::AnvilHandle> {
    common::spawn_anvil().await?.context(
        "anvil is required by this test and was not found; it is provided by this repo's dev \
         shell (`nix develop`), or set FM_ANVIL_BASE_EXECUTABLE to an anvil binary",
    )
}

/// Drives the federation's `walletv2` consensus past block count zero, which
/// it must reach before any UTXO sent to a federation address is tracked.
async fn initialize_wallet_consensus(
    client: &ClientHandleArc,
    bitcoin: &Arc<dyn BitcoinTest>,
) -> anyhow::Result<()> {
    bitcoin.mine_blocks(1 + CONFIRMATION_FINALITY_DELAY).await;
    await_consensus_block_count(client, 1).await
}

async fn await_consensus_block_count(
    client: &ClientHandleArc,
    block_count: u64,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if client
            .get_first_module::<WalletClientModule>()?
            .block_count()
            .await?
            >= block_count
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("walletv2 consensus never reached block count {block_count}");
        }
        sleep(Duration::from_secs(1)).await;
    }
}

/// Pegs `amount` of on-chain BTC into `client` over `walletv2`, returning the
/// client's BITCOIN e-cash balance once the peg-in has been claimed and
/// reissued by the primary mint.
///
/// The event-log position is read *before* the address is derived, as
/// `WalletClientModule::receive`'s doc comment requires: `await_receive` only
/// considers payments recorded at or after that position, so reading it later
/// could race past the very peg-in being waited on.
async fn peg_in_btc(
    client: &ClientHandleArc,
    bitcoin: &Arc<dyn BitcoinTest>,
    amount: bitcoin::Amount,
) -> anyhow::Result<Amount> {
    let wallet = client.get_first_module::<WalletClientModule>()?;
    let position = client.get_next_event_log_id().await;

    // `receive` waits, without a bound of its own, for the client's background
    // scanner to grind out a valid address index --- only ~1 in 65_536
    // qualifies (`fedimint-walletv2-common::is_potential_receive`), so this is
    // genuinely CPU-bound work, and an unbounded wait on it would report a
    // stall as an indefinitely-running test rather than a failure.
    let address = tokio::time::timeout(ADDRESS_GRIND_TIMEOUT, wallet.receive())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "walletv2's background scanner produced no valid address index within \
                 {ADDRESS_GRIND_TIMEOUT:?}"
            )
        })?;

    bitcoin.send_and_mine_block(&address, amount).await;
    bitcoin.mine_blocks(CONFIRMATION_FINALITY_DELAY).await;

    wallet.await_receive(position).await?;
    client.wait_for_all_active_state_machines().await?;

    client.get_balance_for_btc().await
}

/// The pool's current reserves and fee, or an error if it does not exist.
async fn pool_summary(amm: &AmmClientModule) -> anyhow::Result<PoolSummary> {
    amm.pools()
        .await?
        .into_iter()
        .find(|p| p.pool == pool_id())
        .context("the BITCOIN/USDT_UNIT pool does not exist")
}

/// One `FEE_VOTE_SUBMIT_ENDPOINT` call against `peer`'s own admin API.
///
/// `request_admin` is the only path that reaches an endpoint gated on
/// *verified* guardian auth: `FEE_VOTE_SUBMIT_ENDPOINT` calls
/// `fedimint_core::net::auth::check_auth`, not `request_auth()`, so a request
/// that merely carries a password field does not pass. It targets exactly the
/// one peer the admin API was built for, which is what makes "a threshold of
/// distinct guardians voted" a statement about distinct guardians.
///
/// `auth` is a parameter so the negative case can be exercised with the same
/// request the positive one uses.
async fn try_submit_fee_vote(
    fed: &FederationTest,
    peer: PeerId,
    amm_instance: ModuleInstanceId,
    fee_per_mille: u16,
    auth: ApiAuth,
) -> anyhow::Result<()> {
    fed.new_admin_api(peer)
        .await?
        .with_module(amm_instance)
        .request_admin::<()>(
            FEE_VOTE_SUBMIT_ENDPOINT,
            ApiRequestErased::new(FeeVoteSubmitRequest {
                pool: pool_id(),
                fee_per_mille,
            }),
            auth,
        )
        .await
        .map_err(Into::into)
}

/// Records `fee_per_mille` as `peer`'s desired fee for [`pool_id`], retrying
/// while the receiving guardian has not yet created the pool.
///
/// The endpoint rejects a vote for a pool the *receiving* guardian does not
/// hold yet. A client observing its deposit as accepted has seen a threshold
/// of guardians apply it, not all of them --- guardians apply an ordered
/// session at slightly different wall-clock moments --- so a submit racing
/// that lag is expected rather than a fault, and retrying it is not a flake
/// mask. Any other rejection fails immediately.
async fn submit_fee_vote(
    fed: &FederationTest,
    peer: PeerId,
    amm_instance: ModuleInstanceId,
    fee_per_mille: u16,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + FEE_CONSENSUS_TIMEOUT;
    loop {
        match try_submit_fee_vote(
            fed,
            peer,
            amm_instance,
            fee_per_mille,
            ApiAuth::new(TESTING_API_AUTH.to_string()),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) => ensure!(
                error.to_string().contains("no such pool") && Instant::now() < deadline,
                "guardian {peer} rejected the fee vote: {error}"
            ),
        }
        sleep(Duration::from_millis(300)).await;
    }
}

/// Blocks until `FEE_VOTES_ENDPOINT` reports exactly `expected` recorded votes
/// for [`pool_id`], returning that response.
///
/// Waiting for the votes to be *visible in their own right* is what makes the
/// below-threshold assertion meaningful: an unvoted-fee reading taken before
/// the minority's votes were ordered would pass because nothing had happened
/// yet, not because a minority cannot move the fee.
async fn await_recorded_fee_votes(
    client: &ClientHandleArc,
    amm_instance: ModuleInstanceId,
    expected: usize,
) -> anyhow::Result<FeeVotesResponse> {
    let deadline = Instant::now() + FEE_CONSENSUS_TIMEOUT;
    loop {
        let votes: FeeVotesResponse = client
            .api()
            .with_module(amm_instance)
            .request_current_consensus(
                FEE_VOTES_ENDPOINT.to_string(),
                ApiRequestErased::new(FeeVotesRequest { pool: pool_id() }),
            )
            .await?;
        if votes.votes.len() == expected {
            return Ok(votes);
        }
        ensure!(
            Instant::now() < deadline,
            "only {} of {expected} fee votes were ever recorded",
            votes.votes.len()
        );
        sleep(Duration::from_millis(500)).await;
    }
}

/// Blocks until `POOLS_ENDPOINT` reports `expected` as [`pool_id`]'s effective
/// fee, returning the summary that reported it.
async fn await_reported_fee(amm: &AmmClientModule, expected: u16) -> anyhow::Result<PoolSummary> {
    let deadline = Instant::now() + FEE_CONSENSUS_TIMEOUT;
    loop {
        let summary = pool_summary(amm).await?;
        if summary.fee_per_mille == expected {
            return Ok(summary);
        }
        ensure!(
            Instant::now() < deadline,
            "POOLS_ENDPOINT never reported fee {expected} (last saw {})",
            summary.fee_per_mille
        );
        sleep(Duration::from_millis(500)).await;
    }
}

/// Asserts `quoted_out` is exactly what the constant-product curve pays out
/// of `pool`'s reserves at the fee the federation *currently* charges for it.
///
/// The fee comes from `POOLS_ENDPOINT`'s `fee_per_mille` --- i.e. from
/// `Amm::consensus_fee_for`, the aggregate of the guardians' votes --- rather
/// than being hard-coded to `default_consensus_config`'s
/// [`CONFIG_DEFAULT_FEE_PER_MILLE`]. Both swaps below run after the guardians
/// have voted [`VOTED_FEE_PER_MILLE`], so this reads the voted fee; hard-coding
/// the config default here would instead make the check disagree with a
/// federation that has exercised its own fee governance.
///
/// This is the *consistency* half of the fee assertions. It cannot by itself
/// prove the federation charges the voted fee, since a server that reported
/// and charged the same wrong number would satisfy it --- which is why the
/// call sites additionally pin settlement against
/// `math::amount_out(.., VOTED_FEE_PER_MILLE)` computed test-side, and against
/// its inequality with the config-default result.
///
/// Combined with the settlement assertions at the call sites, this pins the
/// module's core invariant from both ends: `quote_swap` is one shared
/// function serving `QUOTE_ENDPOINT` and `process_output`'s `SwapV0` arm, so
/// a quote can never disagree with settlement --- and the quote itself is the
/// curve at the effective fee, not some other number that merely happens to
/// be self-consistent.
fn assert_quote_is_curve_at_effective_fee(
    pool: &PoolSummary,
    unit_in: AmountUnit,
    amount_in: Amount,
    quoted_out: Amount,
) -> anyhow::Result<()> {
    let (reserve_in, reserve_out) = if unit_in == pool.pool.lo() {
        (pool.reserve_lo, pool.reserve_hi)
    } else {
        (pool.reserve_hi, pool.reserve_lo)
    };
    let expected = fedimint_amm_common::math::amount_out(
        reserve_in.msats,
        reserve_out.msats,
        amount_in.msats,
        pool.fee_per_mille,
    )?;
    ensure!(
        quoted_out.msats == expected,
        "quote {quoted_out} disagrees with the curve at the federation's effective fee of {}/1000 \
         (expected {expected} msats)",
        pool.fee_per_mille
    );
    Ok(())
}

/// Every online guardian's `audit` summary, keyed by peer.
async fn audit_every_guardian(
    fed: &FederationTest,
) -> anyhow::Result<BTreeMap<PeerId, AuditSummary>> {
    let auth = ApiAuth::new(TESTING_API_AUTH.to_string());
    let mut summaries = BTreeMap::new();
    for peer in fed.online_peer_ids() {
        let api = fed.new_admin_api(peer).await?;
        summaries.insert(peer, api.audit(auth.clone()).await?);
    }
    Ok(summaries)
}

// ---------------------------------------------------------------------------
// 1. The topology itself.
// ---------------------------------------------------------------------------

/// `UsdtGenParams` for the hermetic fixture: the module's own all-zero
/// placeholder addresses, which [`MockEvmRpc`] is scripted against.
fn usdt_gen_params_for_mock() -> UsdtGenParams {
    UsdtGenParams {
        chain_id: ANVIL_CHAIN_ID,
        ..UsdtGenParams::default()
    }
}

/// Config-gens and boots all seven instances, hermetically (a scriptable
/// `MockEvmRpc` stands in for the EVM chain, and `fedimint-testing`'s default
/// `FakeBitcoinTest` for the Bitcoin one --- `FM_TEST_USE_REAL_DAEMONS` is
/// deliberately not set anywhere in this file).
///
/// Nothing here spends anything; the point is that the seven-instance
/// federation is a thing that exists, that a client can join it, and that the
/// two `mintv2` instances are genuinely distinguishable by unit.
#[tokio::test(flavor = "multi_thread")]
async fn seven_instance_topology_stands_up() -> anyhow::Result<()> {
    let mock = Arc::new(MockEvmRpc::new());
    mock.set_chain_id(ANVIL_CHAIN_ID);
    mock.set_block_number(100);

    let fed = full_topology_fixtures(mock.clone(), usdt_gen_params_for_mock())
        .new_fed_builder(0)
        .disable_mint_fees()
        .build()
        .await;
    let client = fed.new_client().await;

    assert_eq!(
        instance_kinds(&client).await,
        expected_topology_kinds(),
        "the federation must run exactly README.md's seven instances, two mintv2 among them"
    );

    assert_two_distinct_primary_mints(&client).await?;

    // Each module's client also has to have initialized against its own
    // consensus config --- a kind appearing in the config proves config-gen
    // ran, not that the module came up.
    let _ = client.get_first_module::<WalletClientModule>()?;
    let _ = client.get_first_module::<UsdtClientModule>()?;
    let amm = client.get_first_module::<AmmClientModule>()?;

    // Quoting the BITCOIN/USDT_UNIT pair before any deposit must fail as "no
    // such pool" --- i.e. `QUOTE_ENDPOINT` got past `PoolId::new` and reached
    // the pool lookup, rather than inventing a price. (That the pair is
    // tradable at all is proved by
    // [`value_moves_across_the_full_topology`] actually creating the pool.)
    let error = amm
        .quote(AmountUnit::BITCOIN, USDT_UNIT, Amount::from_sats(1))
        .await
        .expect_err("no pool exists yet")
        .to_string();
    assert!(
        error.contains("no such pool"),
        "expected a missing-pool rejection, got: {error}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 2. Value across every leg.
// ---------------------------------------------------------------------------

/// BTC in over `walletv2`, USDt in over `usdt`, both seeded into the `amm`
/// pool, swapped in both directions at the quoted price, withdrawn, audited.
#[tokio::test(flavor = "multi_thread")]
async fn value_moves_across_the_full_topology() -> anyhow::Result<()> {
    let anvil = require_anvil().await?;

    // --- EVM side: a real 4337 stack, deployed before config-gen so the
    // federation's usdt config can name the real factory it will derive
    // deposit accounts from.
    let usdt_holder = common::anvil_account_1_address()?;
    let stack = common::deploy_4337_stack(&anvil, usdt_holder, UsdtAmount(50_000_000)).await?;
    let evm_rpc: Arc<dyn IServerEvmRpc> = AlloyEvmRpc::new(anvil.url())?
        .with_broadcaster(common::ANVIL_ACCOUNT_0_PRIVATE_KEY)?
        .with_entry_point(stack.entry_point)
        .into_dyn();
    let account_factory =
        fedimint_usdt_server::factory_bytecode::derive_account_factory(stack.entry_point);
    let gen_params = UsdtGenParams {
        usdt_contract: stack.usdt,
        chain_id: ANVIL_CHAIN_ID,
        confirmation_depth: 1,
        entry_point: stack.entry_point,
        account_factory,
        simple_account_impl: fedimint_usdt_server::factory_bytecode::derive_simple_account_impl(
            account_factory,
        ),
        // The broadcaster is anvil's default-funded account 0, so any
        // positive minimum is trivially met (zero is rejected by config
        // validation).
        broadcaster_min_balance_wei: 1,
        // No Chainlink feed on anvil: the all-zero address disables the feed
        // and falls back to `AlloyEvmRpc`'s static ETH price.
        eth_usd_price_feed: EvmAddress([0u8; 20]),
        price_feed_max_staleness_secs: 14_400,
        residual_recovery_recipient: EvmAddress([0u8; 20]),
    };

    let fixtures = full_topology_fixtures(evm_rpc, gen_params);
    let bitcoin = fixtures.bitcoin();
    let fed = fixtures
        .new_fed_builder(0)
        .disable_mint_fees()
        .build()
        .await;

    let client = fed.new_client().await;
    assert_eq!(instance_kinds(&client).await, expected_topology_kinds());
    let (btc_mint, usdt_mint) = assert_two_distinct_primary_mints(&client).await?;
    // A multi-minute test whose every assertion is an equality between two
    // computed values is hard to audit from a pass/fail alone; each step
    // reports the numbers it actually measured.
    info!(
        target: LOG_TEST,
        instances = ?instance_kinds(&client).await,
        btc_mint,
        usdt_mint,
        "seven-instance federation up"
    );

    // --- Step 2: BTC in, over walletv2, landing as unit-0 e-cash in the
    // first mint.
    initialize_wallet_consensus(&client, &bitcoin).await?;
    let btc_balance = peg_in_btc(&client, &bitcoin, bitcoin::Amount::from_int_btc(1)).await?;
    info!(target: LOG_TEST, %btc_balance, "BTC pegged in over walletv2");
    assert!(
        btc_balance > Amount::ZERO,
        "the peg-in must have been reissued as BITCOIN e-cash"
    );
    assert_eq!(
        client.get_balance_for_unit(AmountUnit::BITCOIN).await?,
        btc_balance,
        "the peg-in must land in the BITCOIN-denominated mint ({btc_mint}), not the USDt one"
    );
    assert_eq!(
        client.get_balance_for_unit(USDT_UNIT).await?,
        Amount::ZERO,
        "a BTC peg-in must not credit the USDT_UNIT mint ({usdt_mint})"
    );

    // --- Step 3: USDt in, over the usdt module's deposit-by-proof path,
    // landing as unit-1 e-cash in the second mint.
    let usdt = client.get_first_module::<UsdtClientModule>()?;
    common::await_usdt_ready(&usdt, Duration::from_secs(120)).await?;
    let (_claim_keypair, deposit_account) = usdt.allocate_deposit().await?;

    let deposit_amount = UsdtAmount(40_000_000);
    common::transfer_erc20_from_account_1(&anvil, stack.usdt, deposit_account, deposit_amount)
        .await
        .context("failed to fund the counterfactual deposit account with USDT")?;

    // anvil auto-mines one block per transaction and then sits still, so the
    // chain head has to be pushed past the funding transfer before a
    // confirmation-deep read can see it.
    let mine_provider = ProviderBuilder::new().connect_http(anvil.url().parse()?);
    for _ in 0..5u32 {
        mine_provider
            .raw_request::<_, String>("evm_mine".into(), ())
            .await
            .context("failed to mine an anvil block past the funding transfer")?;
    }

    common::credit_deposit_via_anvil_proof(&usdt, &anvil, 0, Duration::from_secs(300)).await?;

    // The deposit is credited and minted in one transaction, net of the live
    // gas-derived deposit fee, but issuance settles asynchronously.
    let usdt_balance = await_nonzero_balance(&client, USDT_UNIT, Duration::from_secs(120)).await?;
    assert!(
        usdt_balance < Amount::from_msats(deposit_amount.0),
        "the deposit fee must have been charged (got the full {deposit_amount:?})"
    );
    info!(
        target: LOG_TEST,
        deposited = deposit_amount.0,
        %usdt_balance,
        deposit_fee = deposit_amount.0 - usdt_balance.msats,
        "USDt deposited by proof and minted into the unit-1 mint"
    );
    assert_eq!(
        client.get_balance_for_btc().await?,
        btc_balance,
        "crediting USDt must not touch the BITCOIN mint"
    );

    // --- Step 4: seed the pool with both assets.
    let amm = client.get_first_module::<AmmClientModule>()?;
    let seed_btc = mintv2_representable_floor(Amount::from_sats(10_000_000));
    let seed_usdt = mintv2_representable_floor(Amount::from_msats(usdt_balance.msats / 2));
    ensure!(
        btc_balance >= seed_btc && usdt_balance >= seed_usdt,
        "both legs must be fundable from the wallet ({btc_balance} BTC, {usdt_balance} USDt)"
    );

    let deposit_op = amm.deposit(pool_id(), seed_btc, seed_usdt, 0).await?;
    amm.await_deposit(deposit_op).await?;
    client.wait_for_all_active_state_machines().await?;

    let seeded = pool_summary(&amm).await?;
    info!(
        target: LOG_TEST,
        reserve_lo = %seeded.reserve_lo,
        reserve_hi = %seeded.reserve_hi,
        total_shares = seeded.total_shares,
        fee_per_mille = seeded.fee_per_mille,
        "AMM pool seeded with both assets"
    );
    assert_eq!(seeded.reserve_lo, seed_btc, "the pool's BITCOIN leg");
    assert_eq!(seeded.reserve_hi, seed_usdt, "the pool's USDT_UNIT leg");
    assert!(
        seeded.total_shares > 0,
        "the first deposit must mint shares"
    );
    assert_eq!(
        seeded.fee_per_mille, CONFIG_DEFAULT_FEE_PER_MILLE,
        "a pool no guardian has voted on must report the DKG-time config fee"
    );

    // --- Step 5: the guardians vote the swap fee away from its config
    // default. The pool has to exist first: a vote naming a pool the receiving
    // guardian does not hold is rejected outright.
    let amm_instance = client
        .get_first_instance(&fedimint_amm_common::KIND)
        .context("amm module is registered")?;
    let peers: Vec<PeerId> = fed.online_peer_ids().collect();
    let threshold = peers.as_slice().to_num_peers().threshold();
    ensure!(
        peers.len() == 4 && threshold == 3,
        "this step's below-threshold/at-threshold split is written for 4 peers with threshold 3, \
         got {} peers with threshold {threshold}",
        peers.len()
    );

    // The submit endpoint gates on verified guardian auth, so an unauthorized
    // caller must not be able to price other people's swaps. Checked here and
    // not only in `fedimint-amm-server`'s unit suite because only a real
    // federation exercises the actual authentication layer rather than a
    // hand-built `ApiEndpointContext`.
    let unauthorized = try_submit_fee_vote(
        &fed,
        peers[0],
        amm_instance,
        VOTED_FEE_PER_MILLE,
        ApiAuth::new("not-the-federation-password".to_string()),
    )
    .await
    .expect_err("a fee vote under a bogus password must be rejected");
    info!(target: LOG_TEST, %unauthorized, "unauthenticated fee vote rejected");

    // Below threshold: `threshold - 1` guardians voting must not move the fee.
    // Without this the at-threshold assertion below could be satisfied by an
    // implementation that let any single guardian set the fee.
    for &peer in peers.iter().take(threshold - 1) {
        submit_fee_vote(&fed, peer, amm_instance, VOTED_FEE_PER_MILLE).await?;
    }
    let minority = await_recorded_fee_votes(&client, amm_instance, threshold - 1).await?;
    info!(
        target: LOG_TEST,
        votes = ?minority.votes,
        effective_fee_per_mille = minority.effective_fee_per_mille,
        band = ?(minority.min_fee_per_mille, minority.max_fee_per_mille),
        "below-threshold fee votes recorded"
    );
    assert_eq!(
        minority.effective_fee_per_mille, CONFIG_DEFAULT_FEE_PER_MILLE,
        "a minority of guardians must not move the fee at all"
    );
    assert_eq!(
        pool_summary(&amm).await?.fee_per_mille,
        CONFIG_DEFAULT_FEE_PER_MILLE,
        "POOLS_ENDPOINT must still report the config fee below threshold"
    );

    // The threshold-th vote is what moves it.
    submit_fee_vote(
        &fed,
        peers[threshold - 1],
        amm_instance,
        VOTED_FEE_PER_MILLE,
    )
    .await?;
    let voted = await_reported_fee(&amm, VOTED_FEE_PER_MILLE).await?;
    info!(
        target: LOG_TEST,
        config_fee_per_mille = CONFIG_DEFAULT_FEE_PER_MILLE,
        voted_fee_per_mille = voted.fee_per_mille,
        voters = threshold,
        of_peers = peers.len(),
        "threshold of guardians moved the swap fee"
    );
    assert_eq!(
        voted.reserve_lo, seeded.reserve_lo,
        "voting a fee must not move the pool's BITCOIN reserve"
    );
    assert_eq!(
        voted.reserve_hi, seeded.reserve_hi,
        "voting a fee must not move the pool's USDT_UNIT reserve"
    );

    // --- Step 6a: BTC -> USDt, settling at the quoted price, at the VOTED fee.
    //
    // `max_slippage_bps: 0` demands the swap settle at exactly the quote the
    // client takes immediately before submitting; this is a single-actor test,
    // so nothing else can move the pool in between.
    let btc_in = mintv2_representable_floor(Amount::from_sats(100_000));

    // Computed test-side from the pool's own reserves, so the comparison never
    // routes through a number the server chose. The inequality is what stops
    // the voted-fee assertion from being satisfied by a federation that
    // ignored the vote entirely.
    let at_voted_fee = Amount::from_msats(fedimint_amm_common::math::amount_out(
        voted.reserve_lo.msats,
        voted.reserve_hi.msats,
        btc_in.msats,
        VOTED_FEE_PER_MILLE,
    )?);
    let at_config_fee = Amount::from_msats(fedimint_amm_common::math::amount_out(
        voted.reserve_lo.msats,
        voted.reserve_hi.msats,
        btc_in.msats,
        CONFIG_DEFAULT_FEE_PER_MILLE,
    )?);
    ensure!(
        at_voted_fee < at_config_fee,
        "the fee assertions are vacuous unless {VOTED_FEE_PER_MILLE} and \
         {CONFIG_DEFAULT_FEE_PER_MILLE} per-mille produce different outputs (both gave \
         {at_voted_fee})"
    );

    let quote_btc_usdt = amm.quote(AmountUnit::BITCOIN, USDT_UNIT, btc_in).await?;
    ensure!(
        quote_btc_usdt.amount_out > Amount::ZERO,
        "quote must be non-zero"
    );
    assert_quote_is_curve_at_effective_fee(
        &voted,
        AmountUnit::BITCOIN,
        btc_in,
        quote_btc_usdt.amount_out,
    )?;
    assert_eq!(
        quote_btc_usdt.amount_out, at_voted_fee,
        "QUOTE_ENDPOINT must price at the voted fee"
    );

    let usdt_before = client.get_balance_for_unit(USDT_UNIT).await?;
    let swap_op = amm.swap(AmountUnit::BITCOIN, USDT_UNIT, btc_in, 0).await?;
    amm.await_swap(swap_op).await?;
    client.wait_for_all_active_state_machines().await?;
    let usdt_after = client.get_balance_for_unit(USDT_UNIT).await?;

    // Settlement is read off the pool's reserves, not off the wallet: both
    // units are held in a real `mintv2` here, so a wallet delta carries the
    // denomination floor and cannot be compared to an exact curve result.
    let after_first_swap = pool_summary(&amm).await?;
    let settled_dy = voted.reserve_hi - after_first_swap.reserve_hi;
    info!(
        target: LOG_TEST,
        %btc_in,
        quoted = %quote_btc_usdt.amount_out,
        %settled_dy,
        %at_voted_fee,
        %at_config_fee,
        fee_per_mille = after_first_swap.fee_per_mille,
        received = %(usdt_after - usdt_before),
        "BTC -> USDt swap settled"
    );

    assert_eq!(
        settled_dy, at_voted_fee,
        "settlement must charge the voted fee of {VOTED_FEE_PER_MILLE}/1000"
    );
    assert_ne!(
        settled_dy, at_config_fee,
        "settlement must not still be charging the config default of \
         {CONFIG_DEFAULT_FEE_PER_MILLE}/1000"
    );
    assert_eq!(
        after_first_swap.reserve_lo,
        voted.reserve_lo + btc_in,
        "the BITCOIN leg must have landed in the pool in full"
    );
    assert_eq!(
        after_first_swap.fee_per_mille, VOTED_FEE_PER_MILLE,
        "the voted fee must survive a swap"
    );

    // What reaches the wallet is that settled `dy` minus only what `mintv2`
    // cannot represent.
    assert_eq!(
        usdt_after - usdt_before,
        mintv2_representable_floor(quote_btc_usdt.amount_out),
        "a BTC -> USDt swap must settle at exactly the quoted price"
    );

    // --- Step 6b: USDt -> BTC, likewise, against the reserves the first swap
    // left behind.
    let usdt_in = mintv2_representable_floor(Amount::from_msats(usdt_after.msats / 4));

    // The reverse direction is priced off `reserve_hi` in and `reserve_lo`
    // out, so it is a different arithmetic path through the curve and gets its
    // own voted-versus-config comparison rather than inheriting 6a's.
    let back_at_voted_fee = Amount::from_msats(fedimint_amm_common::math::amount_out(
        after_first_swap.reserve_hi.msats,
        after_first_swap.reserve_lo.msats,
        usdt_in.msats,
        VOTED_FEE_PER_MILLE,
    )?);
    let back_at_config_fee = Amount::from_msats(fedimint_amm_common::math::amount_out(
        after_first_swap.reserve_hi.msats,
        after_first_swap.reserve_lo.msats,
        usdt_in.msats,
        CONFIG_DEFAULT_FEE_PER_MILLE,
    )?);
    ensure!(
        back_at_voted_fee < back_at_config_fee,
        "the fee assertions are vacuous unless {VOTED_FEE_PER_MILLE} and \
         {CONFIG_DEFAULT_FEE_PER_MILLE} per-mille produce different outputs (both gave \
         {back_at_voted_fee})"
    );

    let quote_usdt_btc = amm.quote(USDT_UNIT, AmountUnit::BITCOIN, usdt_in).await?;
    ensure!(
        quote_usdt_btc.amount_out > Amount::ZERO,
        "quote must be non-zero"
    );
    assert_quote_is_curve_at_effective_fee(
        &after_first_swap,
        USDT_UNIT,
        usdt_in,
        quote_usdt_btc.amount_out,
    )?;
    assert_eq!(
        quote_usdt_btc.amount_out, back_at_voted_fee,
        "QUOTE_ENDPOINT must price the reverse direction at the voted fee too"
    );

    let btc_before = client.get_balance_for_btc().await?;
    let swap_back_op = amm.swap(USDT_UNIT, AmountUnit::BITCOIN, usdt_in, 0).await?;
    amm.await_swap(swap_back_op).await?;
    client.wait_for_all_active_state_machines().await?;
    let btc_after = client.get_balance_for_btc().await?;

    let after_second_swap = pool_summary(&amm).await?;
    let settled_dx = after_first_swap.reserve_lo - after_second_swap.reserve_lo;
    info!(
        target: LOG_TEST,
        %usdt_in,
        quoted = %quote_usdt_btc.amount_out,
        %settled_dx,
        at_voted_fee = %back_at_voted_fee,
        at_config_fee = %back_at_config_fee,
        fee_per_mille = after_second_swap.fee_per_mille,
        received = %(btc_after - btc_before),
        "USDt -> BTC swap settled"
    );

    assert_eq!(
        settled_dx, back_at_voted_fee,
        "the reverse swap must charge the voted fee of {VOTED_FEE_PER_MILLE}/1000"
    );
    assert_ne!(
        settled_dx, back_at_config_fee,
        "the reverse swap must not still be charging the config default of \
         {CONFIG_DEFAULT_FEE_PER_MILLE}/1000"
    );
    assert_eq!(
        btc_after - btc_before,
        mintv2_representable_floor(quote_usdt_btc.amount_out),
        "a USDt -> BTC swap must settle at exactly the quoted price"
    );

    // --- Step 7: withdraw the LP position; the accounting closes.
    let (position_key, position): (LpPositionKey, LpPositionRecord) = amm
        .list_lp_positions()
        .await
        .into_iter()
        .next()
        .context("the deposit above created exactly one LP position")?;

    let before_withdraw = pool_summary(&amm).await?;
    let btc_pre_withdraw = client.get_balance_for_btc().await?;
    let usdt_pre_withdraw = client.get_balance_for_unit(USDT_UNIT).await?;

    let withdraw_op = amm
        .withdraw(position_key.pool, position_key.owner_pk, position.shares)
        .await?;
    amm.await_withdraw(withdraw_op).await?;
    client.wait_for_all_active_state_machines().await?;

    // `MINIMUM_LIQUIDITY` shares are burned on pool creation, so even the sole
    // LP withdrawing everything it owns leaves the pool alive with a residue.
    let after_withdraw = pool_summary(&amm).await?;
    info!(
        target: LOG_TEST,
        shares_burned = position.shares,
        btc_out = %(client.get_balance_for_btc().await? - btc_pre_withdraw),
        usdt_out = %(client.get_balance_for_unit(USDT_UNIT).await? - usdt_pre_withdraw),
        residual_lo = %after_withdraw.reserve_lo,
        residual_hi = %after_withdraw.reserve_hi,
        "LP position withdrawn"
    );
    assert_eq!(
        client.get_balance_for_btc().await? - btc_pre_withdraw,
        mintv2_representable_floor(before_withdraw.reserve_lo - after_withdraw.reserve_lo),
        "the BITCOIN credited must equal what left the pool's reserve_lo"
    );
    assert_eq!(
        client.get_balance_for_unit(USDT_UNIT).await? - usdt_pre_withdraw,
        mintv2_representable_floor(before_withdraw.reserve_hi - after_withdraw.reserve_hi),
        "the USDT_UNIT credited must equal what left the pool's reserve_hi"
    );

    // --- Step 8: the guardians' balance sheets.
    let audits = audit_every_guardian(&fed).await?;
    let final_pool = pool_summary(&amm).await?;

    for (peer, summary) in &audits {
        // The AMM's liability is exactly what it still owes: both pool
        // reserves plus any unclaimed swap balance. Every swap above ran to
        // completion, so the only remainder is the pool itself.
        let amm_summary = summary
            .module_summaries
            .get(&amm_instance)
            .with_context(|| format!("guardian {peer} reported no amm summary"))?;
        let expected = -i64::try_from(final_pool.reserve_lo.msats + final_pool.reserve_hi.msats)
            .expect("reserves are bounded by MAX_RESERVE");
        info!(
            target: LOG_TEST,
            %peer,
            amm_net_assets = amm_summary.net_assets,
            federation_net_assets = summary.net_assets,
            "guardian balance sheet"
        );
        assert_eq!(
            amm_summary.net_assets, expected,
            "guardian {peer}'s amm liability must equal its remaining reserves"
        );

        // The federation-wide sheet must not be short: every e-cash liability
        // is matched by a declared asset (walletv2's UTXOs for BITCOIN, the
        // usdt module's credited-and-unswept plus pooled balance for USDt).
        assert!(
            summary.net_assets >= 0,
            "guardian {peer} is insolvent on its own books: {}",
            summary.net_assets
        );
    }

    let distinct: BTreeSet<_> = audits.values().map(|s| s.net_assets).collect();
    assert_eq!(
        distinct.len(),
        1,
        "every guardian must agree on the federation's net assets, got {distinct:?}"
    );

    Ok(())
}

/// Polls `client`'s balance in `unit` until it is non-zero, returning it.
async fn await_nonzero_balance(
    client: &ClientHandleArc,
    unit: AmountUnit,
    timeout: Duration,
) -> anyhow::Result<Amount> {
    let deadline = Instant::now() + timeout;
    loop {
        let balance = client.get_balance_for_unit(unit).await?;
        if balance > Amount::ZERO {
            return Ok(balance);
        }
        if Instant::now() >= deadline {
            bail!("balance for {unit:?} never became non-zero within {timeout:?}");
        }
        sleep(Duration::from_millis(500)).await;
    }
}
