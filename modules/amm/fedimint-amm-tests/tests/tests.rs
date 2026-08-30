//! End-to-end integration tests for the `amm` module against a real,
//! consensus-driven federation running `mintv2` (BITCOIN), the test-only
//! `faucet` module (a second, custom `AmountUnit`), `amm` itself, and `dummy`
//! (used only to bootstrap BITCOIN into a wallet, exactly as
//! `fedimint-mintv2-tests`'s own `issue_ecash` helper does).
//!
//! `faucet` stands in for the second unit because it is *smaller*, not
//! because a second `mintv2` is impossible — see
//! `fedimint_amm_tests::faucet`'s module doc comment, and
//! `fedimint-usdt-tests`' `tests/full_topology_e2e.rs`, which runs this same
//! `amm` against a real second `mintv2` denominated in `USDT_UNIT`.
//!
//! No plan-of-record multi-hop test: a third hop needs a third tradable
//! unit, which needs a third issuing module of a third `ModuleKind` (spec
//! §3.2) — out of scope for this fixture. See the task report for the full
//! reasoning.

use anyhow::ensure;
use fedimint_amm_client::api::AmmFederationApi;
use fedimint_amm_client::db::{LpPositionKey, LpPositionRecord};
use fedimint_amm_client::{AmmClientModule, AmmRecoverySummary};
use fedimint_amm_common::endpoints::BalanceRequest;
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_common::types::{AmmInput, AmmOutput};
use fedimint_amm_tests::faucet::client::FaucetClientModule;
use fedimint_amm_tests::faucet::common::faucet_unit;
use fedimint_api_client::api::FederationApiExt as _;
use fedimint_client::secret::{PlainRootSecretStrategy, RootSecretStrategy};
use fedimint_client::{ClientHandleArc, RootSecret};
use fedimint_client_module::secret::DeriveableSecretClientExt;
use fedimint_client_module::transaction::{
    ClientInput, ClientInputBundle, ClientOutput, ClientOutputBundle, NeverClientStateMachine,
    TransactionBuilder,
};
use fedimint_core::Amount;
use fedimint_core::NumPeersExt as _;
use fedimint_core::core::{DynInput, DynOutput, IntoDynInstance, ModuleInstanceId, OperationId};
use fedimint_core::module::{AmountUnit, Amounts};
use fedimint_core::secp256k1::{Keypair, Secp256k1};
use fedimint_dummy_client::DummyClientModule;

/// Same fixed-seed pattern `fedimint-mintv2-tests` uses
/// (`root_secret(&SEND_SK)`): a deterministic, reproducible root secret so
/// recovery tests can reconstruct a client's derivations from the seed alone.
fn root_secret(bytes: &[u8; 64]) -> RootSecret {
    RootSecret::StandardDoubleDerive(PlainRootSecretStrategy::to_root_secret(bytes))
}

const LP_SK: [u8; 64] = [0x11; 64];
const TRADER_SK: [u8; 64] = [0x22; 64];
const OTHER_SK: [u8; 64] = [0x33; 64];

/// The one pool these tests trade: BITCOIN (lo, since `AmountUnit::BITCOIN`
/// is `0`) against `faucet_unit()` (hi, `1`) — see
/// `fedimint-amm-server::default_consensus_config`, which allowlists exactly
/// this pair by default.
fn pool_id() -> PoolId {
    PoolId::new(AmountUnit::BITCOIN, faucet_unit()).expect("BITCOIN != faucet_unit()")
}

/// Mints `amount` of BITCOIN into `client`'s wallet via `dummy`'s
/// unconditional "value from nothing" input — mirrors
/// `fedimint-mintv2-tests`'s own `issue_ecash` helper exactly.
async fn issue_btc(client: &ClientHandleArc, amount: Amount) -> anyhow::Result<()> {
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let dummy_input = dummy.create_input(amount);
    let operation_id = OperationId::new_random();

    let outpoint_range = client
        .finalize_and_submit_transaction(
            operation_id,
            "issue btc via dummy",
            |_| (),
            TransactionBuilder::new().with_inputs(dummy_input),
        )
        .await?;

    client
        .await_primary_bitcoin_module_outputs(operation_id, outpoint_range.into_iter().collect())
        .await?;

    Ok(())
}

/// Mints `amount` of `faucet_unit()` into `client`'s wallet via the test
/// faucet's bootstrap entry point.
async fn issue_faucet_unit(client: &ClientHandleArc, amount: Amount) -> anyhow::Result<()> {
    client
        .get_first_module::<FaucetClientModule>()?
        .mint(amount)
        .await
}

/// Funds `client` with both units and deposits into [`pool_id`], creating it
/// on the first call. Waits for both the deposit and the resulting change/
/// receive state machines to settle.
async fn create_pool(
    client: &ClientHandleArc,
    amount_btc: Amount,
    amount_unit1: Amount,
) -> anyhow::Result<()> {
    issue_btc(client, amount_btc).await?;
    issue_faucet_unit(client, amount_unit1).await?;

    let amm = client.get_first_module::<AmmClientModule>()?;
    let operation_id = amm.deposit(pool_id(), amount_btc, amount_unit1, 0).await?;
    amm.await_deposit(operation_id).await?;
    client.wait_for_all_active_state_machines().await?;

    Ok(())
}

/// Builds and submits a transaction containing a single raw output of an
/// arbitrary module, bypassing that module's own client wrapper — used to
/// construct transactions the real client API has no way to build (an
/// unreachable `min_out`, a manufactured recipient key) while still routing
/// funding through the real primary-module auto-balance machinery (spec P10):
/// the caller supplies `amounts` (the real per-unit value this output needs
/// funded), and `finalize_and_submit_transaction`'s own balancing pulls a
/// real input from whichever module is primary for that unit, exactly as it
/// would for a transaction built through the module's own client code.
async fn submit_output<O>(
    client: &ClientHandleArc,
    instance_id: ModuleInstanceId,
    output: O,
    amounts: Amounts,
    operation_type: &str,
) -> anyhow::Result<(OperationId, fedimint_core::OutPointRange)>
where
    O: IntoDynInstance<DynType = DynOutput> + 'static,
{
    let bundle: ClientOutputBundle<O, NeverClientStateMachine> =
        ClientOutputBundle::new_no_sm(vec![ClientOutput { output, amounts }]);
    let dyn_bundle = bundle.into_dyn(instance_id);
    let tx_builder = TransactionBuilder::new().with_outputs(dyn_bundle);
    let operation_id = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(operation_id, operation_type, |_| (), tx_builder)
        .await?;
    Ok((operation_id, range))
}

/// Like [`submit_output`], but for a single raw input the caller signs
/// itself (`keys`) — used for transactions authorised by a keypair the test
/// generated directly, with no wallet interaction needed to sign it.
async fn submit_input<I>(
    client: &ClientHandleArc,
    instance_id: ModuleInstanceId,
    input: I,
    keys: Vec<Keypair>,
    amounts: Amounts,
    operation_type: &str,
) -> anyhow::Result<(OperationId, fedimint_core::OutPointRange)>
where
    I: IntoDynInstance<DynType = DynInput> + 'static,
{
    let bundle: ClientInputBundle<I, NeverClientStateMachine> =
        ClientInputBundle::new_no_sm(vec![ClientInput {
            input,
            keys,
            amounts,
        }]);
    let dyn_bundle = bundle.into_dyn(instance_id);
    let tx_builder = TransactionBuilder::new().with_inputs(dyn_bundle);
    let operation_id = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(operation_id, operation_type, |_| (), tx_builder)
        .await?;
    Ok((operation_id, range))
}

/// Like [`submit_output`]/[`submit_input`] combined: one raw input (signed by
/// `keys`) and one raw output of a possibly different module, in the same
/// transaction — used for test 7 (§6.1's overpay-permitted assumption),
/// which needs `ClaimBalanceV0` (an `amm` input) and a receiving output
/// (a `faucet` output) to land atomically.
#[allow(clippy::too_many_arguments)]
async fn submit_input_and_output<I, O>(
    client: &ClientHandleArc,
    input_instance_id: ModuleInstanceId,
    input: I,
    keys: Vec<Keypair>,
    input_amounts: Amounts,
    output_instance_id: ModuleInstanceId,
    output: O,
    output_amounts: Amounts,
    operation_type: &str,
) -> anyhow::Result<(OperationId, fedimint_core::OutPointRange)>
where
    I: IntoDynInstance<DynType = DynInput> + 'static,
    O: IntoDynInstance<DynType = DynOutput> + 'static,
{
    let input_bundle: ClientInputBundle<I, NeverClientStateMachine> =
        ClientInputBundle::new_no_sm(vec![ClientInput {
            input,
            keys,
            amounts: input_amounts,
        }]);
    let output_bundle: ClientOutputBundle<O, NeverClientStateMachine> =
        ClientOutputBundle::new_no_sm(vec![ClientOutput {
            output,
            amounts: output_amounts,
        }]);
    let tx_builder = TransactionBuilder::new()
        .with_inputs(input_bundle.into_dyn(input_instance_id))
        .with_outputs(output_bundle.into_dyn(output_instance_id));
    let operation_id = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(operation_id, operation_type, |_| (), tx_builder)
        .await?;
    Ok((operation_id, range))
}

/// Reconstructs the `amm` module's own per-instance root secret for `client`
/// from the raw seed bytes it was constructed with, using the exact same
/// derivation chain `fedimint-client`'s builder applies internally for
/// `RootSecret::StandardDoubleDerive` (spec §8/P14). This lets a test
/// hand-derive the identical `recipient_pk`/`tweak` keyspace
/// [`fedimint_amm_client::derivation`] uses, entirely through public APIs
/// (`DeriveableSecretClientExt::derive_module_secret` is `pub` in
/// `fedimint_client_module::module::init`), without reaching into any
/// private field of [`AmmClientModule`].
///
/// `StandardDoubleDerive` is named for the two rounds of
/// `DerivableSecret::federation_key` it applies, both keyed on the same
/// `federation_id`, verified by reading the pinned
/// `fedimint-client/src/client/builder.rs`: `RootSecret::to_inner` calls
/// `get_default_client_secret`, which ends with one `federation_key` call
/// buried inside it (`fedimint-client-module/src/secret.rs`'s own doc
/// comment: "`.../<federation-id>/...`") and — critically — that call resets
/// the secret's `level()` back to `0` (`federation_key`'s own definition in
/// `crypto/derive-secret/src/lib.rs` always returns `level: 0`, regardless of
/// its input's level). `ClientBuilder::federation_root_secret`
/// (`builder.rs:1217-1221`) then applies a **second**, independent
/// `federation_key` call on top of that level-2 result before ever calling
/// `derive_module_secret` — which is exactly why `derive_module_secret`'s own
/// `assert_eq!(self.level(), 0)` never fires in the real client: the second
/// `federation_key` call resets the level back to `0` right before it runs.
/// An earlier version of this function skipped that second call and derived
/// the module secret directly off `get_default_client_secret`'s level-2
/// result, tripping that exact assertion (`left: 2, right: 0`) the moment
/// this function ran — a fixture bug, not a real client codepath, since a
/// real `AmmClientModule` only ever sees the correctly-leveled secret via
/// `ClientModuleInitArgs::module_root_secret()`.
fn amm_module_secret(
    client: &ClientHandleArc,
    seed_bytes: &[u8; 64],
) -> fedimint_derive_secret::DerivableSecret {
    let pre_root = PlainRootSecretStrategy::to_root_secret(seed_bytes);
    let federation_id = client.federation_id();
    let client_secret =
        fedimint_client_module::secret::get_default_client_secret(&pre_root, &federation_id);
    // The second `federation_key` round `StandardDoubleDerive` applies
    // (`ClientBuilder::federation_root_secret`) — resets `level()` back to
    // `0`, which is what makes the following `derive_module_secret` call
    // valid.
    let root_secret = client_secret.federation_key(&federation_id);
    let amm_instance_id = client
        .get_first_instance(&fedimint_amm_common::KIND)
        .expect("amm module is registered");
    root_secret.derive_module_secret(amm_instance_id)
}

fn fresh_keypair() -> Keypair {
    Keypair::new(&Secp256k1::new(), &mut rand::thread_rng())
}

/// Rounds `sats` up to the nearest amount `issue_btc` can mint without
/// losing dust.
///
/// `mintv2`'s client-held notes bottom out at denomination `2^9 = 512` msat
/// (`fedimint-mintv2-common::config::client_denominations`, `9..42` —
/// confirmed by reading the pinned source, not assumed): `represent_amount_with_fees`
/// (`fedimint-mintv2-client/src/lib.rs`) greedily represents a freshly
/// issued amount using only those denominations and silently drops whatever
/// is left over below the smallest one — not an error, the same
/// "surplus is forfeited" pattern spec §6.1 documents for `amm` itself, just
/// happening one layer down in `mintv2`. A test that issues `X` sats via
/// [`issue_btc`] and then immediately spends the *entire* `X` in one
/// transaction therefore needs `X` already aligned to that 512-msat grid, or
/// the spend comes up short and the primary module reports "Insufficient
/// funds" — exactly the failure this helper exists to prevent, confirmed by
/// reproducing it with `Amount::from_sats(100_000)` (100_000_000 msat, not a
/// multiple of 512: the wallet ended up with only 99_999_744 msat after
/// issuance) before this fix.
///
/// `Amount::from_sats(n)` is `n * 1000` msat, and `1000 = 2^3 * 125`, so
/// reaching the required `2^9` factor needs `n` itself to supply the missing
/// `2^6 = 64`: `n` must be a multiple of 64 sats. Rounding up here (rather
/// than requiring every call site to pick a pre-aligned literal) makes this
/// invariant impossible to accidentally violate again.
fn dust_free_sats(sats: u64) -> Amount {
    const MIN_SAT_MULTIPLE: u64 = 64;
    Amount::from_sats(sats.next_multiple_of(MIN_SAT_MULTIPLE))
}

/// Mirrors [`dust_free_sats`]'s reasoning, but for the receive side and in
/// msats directly: floors `amount` down to the nearest multiple of
/// `mintv2`'s smallest client-held note denomination (`2^9 = 512` msat,
/// `fedimint-mintv2-common::config::client_denominations`, `9..42`). A swap's
/// settled `dy` is produced by integer AMM curve math
/// (`fedimint-amm-common::math::amount_out`) with no reason to land on that
/// grid, so crediting a wallet with an unaligned `dy` in BITCOIN forfeits its
/// remainder on reissue exactly as [`dust_free_sats`] describes for
/// issuance — this is what a test asserting `balance_after - balance_before
/// == dy` must account for whenever the swap settles into BITCOIN, since
/// (unlike [`dust_free_sats`]'s callers) the test does not control `dy`
/// directly.
fn mintv2_representable_floor(amount: Amount) -> Amount {
    const MINTV2_MIN_DENOMINATION_MSATS: u64 = 512;
    Amount::from_msats(
        (amount.msats / MINTV2_MIN_DENOMINATION_MSATS) * MINTV2_MIN_DENOMINATION_MSATS,
    )
}

// ---------------------------------------------------------------------------
// 1. Deposit creates a pool; POOLS_ENDPOINT reflects reserves and shares.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn deposit_creates_pool_and_pools_endpoint_reflects_it() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;
    let lp = fed.new_client().await;

    let amount_btc = Amount::from_sats(1_000_000);
    let amount_unit1 = Amount::from_sats(2_000_000);
    create_pool(&lp, amount_btc, amount_unit1).await?;

    let amm = lp.get_first_module::<AmmClientModule>()?;
    let pools = amm.pools().await?;
    let summary = pools
        .iter()
        .find(|p| p.pool == pool_id())
        .expect("pool exists after the first deposit");

    assert_eq!(summary.reserve_lo, amount_btc);
    assert_eq!(summary.reserve_hi, amount_unit1);
    assert!(
        summary.total_shares > 0,
        "first deposit must mint shares (minus MINIMUM_LIQUIDITY, still > 0 for these amounts)"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 2 & 3. Full swap round trip; the pre-Tx1 quote matches the settled `dy`.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn swap_round_trip_settles_and_matches_the_prior_quote() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;

    let lp = fed.new_client().await;
    create_pool(
        &lp,
        Amount::from_sats(10_000_000),
        Amount::from_sats(10_000_000),
    )
    .await?;

    let trader = fed.new_client().await;
    let amount_in = dust_free_sats(100_000);
    issue_btc(&trader, amount_in).await?;

    let amm = trader.get_first_module::<AmmClientModule>()?;

    // Quote taken before Tx1 (test 3's requirement) — the same call the
    // client's own `swap()` makes internally right before submitting Tx1,
    // so nothing else touches the pool in between in this single-actor test.
    let quote = amm
        .quote(AmountUnit::BITCOIN, faucet_unit(), amount_in)
        .await?;
    ensure!(quote.amount_out > Amount::ZERO, "quote must be non-zero");

    let balance_before = trader.get_balance_for_unit(faucet_unit()).await?;

    let operation_id = amm
        .swap(AmountUnit::BITCOIN, faucet_unit(), amount_in, 0)
        .await?;
    amm.await_swap(operation_id).await?;
    trader.wait_for_all_active_state_machines().await?;

    let balance_after = trader.get_balance_for_unit(faucet_unit()).await?;

    // Test 2: notes reissued, wallet balance increased by exactly the
    // settled `dy`. Test 3: that settled `dy` is exactly the pre-Tx1 quote.
    assert_eq!(balance_after - balance_before, quote.amount_out);

    Ok(())
}

// ---------------------------------------------------------------------------
// 2b. A successful swap settling into real, mintv2-verified BITCOIN notes
//     (review Important 2). Every other successful swap in this suite trades
//     BITCOIN -> faucet_unit(), so `unit_out` is always the free-minting
//     faucet's own unit and no successful Tx2 ever exercises mintv2's own
//     `create_final_inputs_and_outputs`/`process_output` funding path. This
//     is the one swap that does.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn swap_into_bitcoin_settles_real_mintv2_notes() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;

    let lp = fed.new_client().await;
    create_pool(
        &lp,
        Amount::from_sats(10_000_000),
        Amount::from_sats(10_000_000),
    )
    .await?;

    let trader = fed.new_client().await;
    let amount_in = Amount::from_sats(100_000);
    issue_faucet_unit(&trader, amount_in).await?;
    trader.wait_for_all_active_state_machines().await?;

    let amm = trader.get_first_module::<AmmClientModule>()?;
    let quote = amm
        .quote(faucet_unit(), AmountUnit::BITCOIN, amount_in)
        .await?;
    ensure!(quote.amount_out > Amount::ZERO, "quote must be non-zero");

    let balance_before = trader.get_balance_for_btc().await?;

    let operation_id = amm
        .swap(faucet_unit(), AmountUnit::BITCOIN, amount_in, 0)
        .await?;
    amm.await_swap(operation_id).await?;
    trader.wait_for_all_active_state_machines().await?;

    let balance_after = trader.get_balance_for_btc().await?;

    // The settled `dy` is real, consensus-verified BITCOIN — reissued into
    // spendable `mintv2` notes exactly like any other BTC receive — but only
    // the portion of it aligned to mintv2's 512-msat denomination floor
    // survives reissue (`mintv2_representable_floor`'s doc comment); asserting
    // equality against the raw quote would be wrong for the same reason
    // `dust_free_sats` exists on the issuance side.
    assert_eq!(
        balance_after - balance_before,
        mintv2_representable_floor(quote.amount_out),
        "the wallet must gain exactly the mintv2-representable portion of the settled dy"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 4. `min_out` violation: Tx1 rejected, wallet unchanged.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn min_out_violation_rejects_tx1_and_leaves_the_wallet_unchanged() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;

    let lp = fed.new_client().await;
    create_pool(
        &lp,
        Amount::from_sats(10_000_000),
        Amount::from_sats(10_000_000),
    )
    .await?;

    let trader = fed.new_client().await;
    let amount_in = Amount::from_sats(100_000);
    issue_faucet_unit(&trader, amount_in).await?;
    trader.wait_for_all_active_state_machines().await?;

    let balance_before = trader.get_balance_for_unit(faucet_unit()).await?;

    let amm_instance_id = trader
        .get_first_instance(&fedimint_amm_common::KIND)
        .expect("amm module is registered");

    // A raw `SwapV0` with an unreachable `min_out`: the real `swap()` public
    // API always re-quotes fresh and could never build this itself (that is
    // the whole point of `fetch_min_out` — see `fedimint-amm-client`'s own
    // `fetch_min_out_asks_a_fresh_quote_every_call_rather_than_caching`
    // test), so this is the only way to exercise `AmmOutputError::
    // SlippageExceeded` deterministically rather than racing two concurrent
    // swaps against each other. Funded through the real primary-module
    // auto-balance path (`submit_output`'s doc comment), so this still
    // exercises the wallet's genuine optimistic-debit/refund-on-reject state
    // machine, not a hand-signed bypass.
    let output = AmmOutput::new_swap_v0(
        &fresh_keypair(),
        faucet_unit(),
        AmountUnit::BITCOIN,
        amount_in,
        Amount::from_msats(u64::MAX),
        [0; 16],
    );

    let (operation_id, range) = submit_output(
        &trader,
        amm_instance_id,
        output,
        Amounts::new_custom(faucet_unit(), amount_in),
        "malicious min_out swap",
    )
    .await?;

    let error = trader
        .transaction_updates(operation_id)
        .await
        .await_tx_accepted(range.txid())
        .await
        .expect_err("a swap whose min_out no real pool could ever pay must be rejected");
    assert!(
        error.contains("min_out"),
        "expected rejection to name AmmOutputError::SlippageExceeded (\"output below \
         min_out, or shares below min_shares\"), got: {error}"
    );

    // The wallet's optimistic debit (made when the transaction was built)
    // must be refunded once the rejection is observed.
    trader.wait_for_all_active_state_machines().await?;
    let balance_after = trader.get_balance_for_unit(faucet_unit()).await?;
    assert_eq!(
        balance_before, balance_after,
        "a rejected Tx1 must not change the wallet's balance"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 5. A claim for a non-existent balance is rejected.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn claim_for_a_non_existent_balance_is_rejected() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;
    let client = fed.new_client().await;

    let amm_instance_id = client
        .get_first_instance(&fedimint_amm_common::KIND)
        .expect("amm module is registered");

    // A fresh keypair that has never received a `SwapV0` credit, so no
    // `Balance` row exists under it. Fully self-signed (the test holds the
    // key), so this needs no interaction with any wallet at all.
    let claimant = fresh_keypair();
    let input = AmmInput::ClaimBalanceV0 {
        pubkey: claimant.public_key(),
        unit: faucet_unit(),
    };

    let (operation_id, range) = submit_input(
        &client,
        amm_instance_id,
        input,
        vec![claimant],
        Amounts::ZERO,
        "claim a non-existent balance",
    )
    .await?;

    let error = client
        .transaction_updates(operation_id)
        .await
        .await_tx_accepted(range.txid())
        .await
        .expect_err("claiming a balance that was never credited must be rejected");
    assert!(
        error.contains("no balance for this key and unit"),
        "expected rejection to name AmmInputError::NoSuchBalance (\"no balance for this key \
         and unit\"), got: {error}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 6. Withdraw returns both legs as spendable value.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn withdraw_returns_both_legs_as_spendable_value() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;
    let lp = fed.new_client().await;

    let amount_btc = Amount::from_sats(1_000_000);
    let amount_unit1 = Amount::from_sats(1_000_000);
    create_pool(&lp, amount_btc, amount_unit1).await?;

    let amm = lp.get_first_module::<AmmClientModule>()?;
    let (key, record): (LpPositionKey, LpPositionRecord) = amm
        .list_lp_positions()
        .await
        .into_iter()
        .next()
        .expect("the deposit above created exactly one LP position");

    // `lp` holds the pool's only position, so what it withdraws must be
    // reflected exactly in how far the pool's own reserves fall — a 1-msat-
    // per-leg withdraw would still pass `> before`, but not this.
    let pools_before = amm.pools().await?;
    let pool_before = pools_before
        .iter()
        .find(|p| p.pool == pool_id())
        .expect("pool exists");
    let reserve_lo_before_pool = pool_before.reserve_lo;
    let reserve_hi_before_pool = pool_before.reserve_hi;

    let btc_before = lp.get_balance_for_btc().await?;
    let unit1_before = lp.get_balance_for_unit(faucet_unit()).await?;

    let operation_id = amm.withdraw(key.pool, key.owner_pk, record.shares).await?;
    amm.await_withdraw(operation_id).await?;
    lp.wait_for_all_active_state_machines().await?;

    let btc_after = lp.get_balance_for_btc().await?;
    let unit1_after = lp.get_balance_for_unit(faucet_unit()).await?;

    // `MINIMUM_LIQUIDITY` shares are permanently burned on pool creation
    // (spec §7.1), so even the sole LP withdrawing every share it owns
    // leaves a tiny residual behind — the pool still exists afterward, just
    // with smaller reserves.
    let pools_after = amm.pools().await?;
    let pool_after = pools_after
        .iter()
        .find(|p| p.pool == pool_id())
        .expect("MINIMUM_LIQUIDITY keeps the pool alive after a sole LP's full withdrawal");
    let reserve_lo_after_pool = pool_after.reserve_lo;
    let reserve_hi_after_pool = pool_after.reserve_hi;

    // The BTC leg is reissued into spendable `mintv2` notes exactly like a
    // swap settling into BITCOIN would (see
    // `mintv2_representable_floor`'s doc comment): only the portion of what
    // left the pool that is aligned to the 512-msat denomination floor
    // survives reissue. `MINIMUM_LIQUIDITY`'s rounding makes the raw
    // withdrawn amount essentially never land on that grid, so comparing
    // against the unfloored reserve delta fails even though nothing is
    // wrong — it must be floored the same way here.
    assert_eq!(
        btc_after - btc_before,
        mintv2_representable_floor(reserve_lo_before_pool - reserve_lo_after_pool),
        "the BITCOIN credited to the wallet must equal exactly the mintv2-representable \
         portion of what left the pool's reserve_lo"
    );
    assert_eq!(
        unit1_after - unit1_before,
        reserve_hi_before_pool - reserve_hi_after_pool,
        "the faucet_unit() credited to the wallet must equal exactly what left the pool's reserve_hi"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 7. Overpay-permitted standing assumption (spec §6.1). Tripwire for
//    CORE_CONSENSUS_VERSION ever dropping below 2.1.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn overpay_between_tx1_and_tx2_forfeits_only_the_surplus() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;

    let lp = fed.new_client().await;
    create_pool(
        &lp,
        Amount::from_sats(10_000_000),
        Amount::from_sats(10_000_000),
    )
    .await?;

    let trader = fed.new_client().await;
    let amount_in = dust_free_sats(100_000);
    issue_btc(&trader, amount_in).await?;

    let amm_instance_id = trader
        .get_first_instance(&fedimint_amm_common::KIND)
        .expect("amm module is registered");
    let faucet_instance_id = trader
        .get_first_instance(&fedimint_amm_tests::faucet::common::KIND)
        .expect("faucet module is registered");

    // The recipient this test controls directly (no wallet interaction
    // needed to sign its later claim).
    let recipient = fresh_keypair();

    // Tx1: a real swap crediting `recipient`'s balance, funded through the
    // wallet's own real BTC (auto-balanced via mintv2, exactly like
    // `AmmClientModule::swap` builds it internally).
    let (operation_id, range) = submit_output(
        &trader,
        amm_instance_id,
        AmmOutput::new_swap_v0(
            &recipient,
            AmountUnit::BITCOIN,
            faucet_unit(),
            amount_in,
            Amount::ZERO,
            [0; 16],
        ),
        Amounts::new_custom(AmountUnit::BITCOIN, amount_in),
        "swap Tx1 for the overpay test",
    )
    .await?;
    trader
        .transaction_updates(operation_id)
        .await
        .await_tx_accepted(range.txid())
        .await
        .map_err(|e| anyhow::anyhow!("Tx1 rejected: {e}"))?;

    // The exact `dy` Tx1 settled for — read directly off `BALANCE_ENDPOINT`
    // (spec §12) rather than recomputed, so this test does not depend on
    // reproducing `quote_swap`'s own arithmetic.
    let amm_api = &trader.get_first_module::<AmmClientModule>()?.api;
    let original_dy = amm_api
        .amm_balance(BalanceRequest {
            pubkey: recipient.public_key(),
            unit: faucet_unit(),
        })
        .await?
        .expect("Tx1 credited recipient's balance");

    // A second party credits the SAME recipient_pk between Tx1 and Tx2 —
    // the "gift" spec §6.1 says a claim must still capture rather than be
    // blocked by. Since outputs now carry a proof of possession, a gift
    // requires the recipient's cooperation: the output itself is signed by
    // the recipient's key (which this test holds) even though a different
    // client funds and submits the transaction.
    let other = fed.join_client_with_db(
        fedimint_core::db::mem_impl::MemDatabase::new().into(),
        root_secret(&OTHER_SK),
    );
    let other = other.await;
    let gift_amount = dust_free_sats(500_000);
    issue_btc(&other, gift_amount).await?;
    let other_amm_instance_id = other
        .get_first_instance(&fedimint_amm_common::KIND)
        .expect("amm module is registered");
    let (gift_operation_id, gift_range) = submit_output(
        &other,
        other_amm_instance_id,
        AmmOutput::new_swap_v0(
            &recipient,
            AmountUnit::BITCOIN,
            faucet_unit(),
            gift_amount,
            Amount::ZERO,
            [0; 16],
        ),
        Amounts::new_custom(AmountUnit::BITCOIN, gift_amount),
        "gift credit landing between Tx1 and Tx2",
    )
    .await?;
    other
        .transaction_updates(gift_operation_id)
        .await
        .await_tx_accepted(gift_range.txid())
        .await
        .map_err(|e| anyhow::anyhow!("gift swap rejected: {e}"))?;

    let grown_balance = amm_api
        .amm_balance(BalanceRequest {
            pubkey: recipient.public_key(),
            unit: faucet_unit(),
        })
        .await?
        .expect("balance still exists, now larger");
    assert!(
        grown_balance > original_dy,
        "the gift must have grown the balance beyond the original dy"
    );

    // Tx2, built for the ORIGINAL `dy` (as if the gift never happened): a
    // `ClaimBalanceV0` input (sweeps the whole, now-larger, balance — spec
    // §6.1: claims are all-or-nothing) plus an output receiving exactly the
    // original `dy` in `faucet_unit()`.
    let receiver = fresh_keypair();
    let (tx2_operation_id, tx2_range) = submit_input_and_output(
        &trader,
        amm_instance_id,
        AmmInput::ClaimBalanceV0 {
            pubkey: recipient.public_key(),
            unit: faucet_unit(),
        },
        vec![recipient],
        Amounts::new_custom(faucet_unit(), original_dy),
        faucet_instance_id,
        fedimint_amm_tests::faucet::common::FaucetOutput::ReceiveV0 {
            amount: original_dy,
            pub_key: receiver.public_key(),
        },
        Amounts::new_custom(faucet_unit(), original_dy),
        "Tx2 built for the pre-gift dy",
    )
    .await?;

    trader
        .transaction_updates(tx2_operation_id)
        .await
        .await_tx_accepted(tx2_range.txid())
        .await
        .map_err(|e| anyhow::anyhow!("Tx2 rejected: {e}"))?;

    // Succeeded despite the balance having grown past what Tx2 declared —
    // this is the tripwire: it fails loudly the moment
    // `CORE_CONSENSUS_VERSION` ever drops below 2.1 (spec §6.1, P5/P6),
    // since `verify_funding` would then require exact per-unit equality.
    let remaining = amm_api
        .amm_balance(BalanceRequest {
            pubkey: recipient.public_key(),
            unit: faucet_unit(),
        })
        .await?;
    assert_eq!(
        remaining, None,
        "ClaimBalanceV0 always sweeps the entire record — the surplus is forfeited under \
         core's overpay rule (P5/P6), not left behind as residue"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 8. Recovery: a swap and a deposit, seed-only recovery finds and claims
//    both.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn recovery_finds_and_claims_an_unclaimed_balance_and_an_lp_position() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;

    let lp_for_pool = fed.new_client().await;
    create_pool(
        &lp_for_pool,
        Amount::from_sats(10_000_000),
        Amount::from_sats(10_000_000),
    )
    .await?;

    let client = fed
        .join_client_with_db(
            fedimint_core::db::mem_impl::MemDatabase::new().into(),
            root_secret(&LP_SK),
        )
        .await;

    // A deposit: a normal, single-transaction LP position that is
    // permanently recoverable server-side (no "claim" step, unlike Balance).
    let amount_btc = Amount::from_sats(1_000_000);
    let amount_unit1 = Amount::from_sats(1_000_000);
    issue_btc(&client, amount_btc).await?;
    issue_faucet_unit(&client, amount_unit1).await?;
    let amm = client.get_first_module::<AmmClientModule>()?;
    let deposit_op = amm.deposit(pool_id(), amount_btc, amount_unit1, 0).await?;
    amm.await_deposit(deposit_op).await?;
    client.wait_for_all_active_state_machines().await?;

    // A swap left deliberately unclaimed: Tx1 only, built by hand with a
    // `recipient_pk` genuinely derived from this client's own seed (via the
    // same derivation `fedimint-amm-client` uses internally — see
    // `amm_module_secret`'s doc comment), so the later recovery scan's
    // `matches_own_key` check finds it. `AmmClientModule::swap`'s own Tx2
    // would run automatically the moment Tx1 is accepted, which would leave
    // nothing to recover — this is the reason for building Tx1 by hand
    // rather than calling `swap()`.
    let amm_root = amm_module_secret(&client, &LP_SK);
    let tweak = fedimint_amm_client::derivation::grind_tweak(&amm_root);
    let recipient_keypair = fedimint_amm_client::derivation::derive_keypair(
        &amm_root,
        fedimint_amm_client::derivation::CHILD_SWAP,
        tweak,
    );

    let swap_amount_in = dust_free_sats(50_000);
    issue_btc(&client, swap_amount_in).await?;
    let amm_instance_id = client
        .get_first_instance(&fedimint_amm_common::KIND)
        .expect("amm module is registered");
    let (operation_id, range) = submit_output(
        &client,
        amm_instance_id,
        AmmOutput::new_swap_v0(
            &recipient_keypair,
            AmountUnit::BITCOIN,
            faucet_unit(),
            swap_amount_in,
            Amount::ZERO,
            tweak,
        ),
        Amounts::new_custom(AmountUnit::BITCOIN, swap_amount_in),
        "unclaimed swap for the recovery test",
    )
    .await?;
    client
        .transaction_updates(operation_id)
        .await
        .await_tx_accepted(range.txid())
        .await
        .map_err(|e| anyhow::anyhow!("Tx1 rejected: {e}"))?;

    // Wipe client state: a brand new, empty database, recovered from the
    // seed alone.
    let recovering = fed
        .recover_client_with_db(
            fedimint_core::db::mem_impl::MemDatabase::new().into(),
            root_secret(&LP_SK),
        )
        .await;
    recovering.wait_for_all_recoveries().await?;

    let recovered = fed
        .open_client_with_db(recovering.db().clone(), root_secret(&LP_SK))
        .await;

    // The LP position must already have been restored during the recovery
    // scan itself (spec §8.2: positions are found and restored inline, no
    // extra claim step needed).
    let amm = recovered.get_first_module::<AmmClientModule>()?;
    let positions = amm.list_lp_positions().await;
    assert!(
        !positions.is_empty(),
        "the deposit's LP position must be found by seed-only recovery"
    );

    // The balance is persisted by recovery, then claimed once the module
    // actually starts (see `AmmClientModule::start`'s doc comment) — an
    // explicit `recover()` call claims it immediately rather than waiting
    // for the background sweep.
    let summary: AmmRecoverySummary = amm.recover().await?;
    assert!(
        summary.claim_errors.is_empty(),
        "no claim should fail: {:?}",
        summary.claim_errors
    );

    let recipient_pk = recipient_keypair.public_key();

    // `summary.balances_found >= 1` alone is racy for exactly the reason
    // `summary.balances_claimed` is (see below): `recover()`'s own scan runs
    // against the live server, so if the background sweep already claimed
    // this balance before that scan executed, the now-removed `Balance`
    // record simply is not there to find, and `balances_found` legitimately
    // reports `0` — not a sign recovery is broken. Distinguish the two cases
    // the same way: a `0` here is only acceptable if the balance is
    // independently confirmed gone (i.e. something — the scan or the
    // background sweep, using only seed-derived material — must have found
    // and claimed it already).
    assert!(
        summary.balances_found >= 1
            || amm
                .api
                .amm_balance(BalanceRequest {
                    pubkey: recipient_pk,
                    unit: faucet_unit(),
                })
                .await?
                .is_none(),
        "the unclaimed swap's Balance must be found by the recovery scan (or already claimed \
         by a background sweep that itself only knows about it because an earlier scan found it)"
    );

    // `AmmClientModule::start`'s own background sweep and this explicit
    // `recover()` call both race to claim the very same recovered balance —
    // both ultimately call the module's own `claim_pending_balances`, and the
    // background sweep's first attempt starts the moment `open_client_with_db`
    // above constructs the module, with no guarantee it loses the race
    // against this explicit call. Whichever wins performs the actual claim;
    // the other legitimately reports `balances_claimed: 0` for its own call
    // once it discovers there was nothing left for it to do (a benign race
    // `claim_pending_balances`'s own doc comment describes and tolerates, not
    // an error — confirmed above via `claim_errors.is_empty()`).
    // `summary.balances_claimed` from this one call therefore cannot tell
    // "recovery is broken" apart from "the background sweep already won" —
    // what actually proves "claimable from the seed alone" is the `Balance`
    // record vanishing from `BALANCE_ENDPOINT` (claimed by *something*, using
    // only material this seed can reconstruct), so poll for that directly
    // instead, bounded so a genuine regression still fails the test rather
    // than hanging.
    let mut claimed = summary.balances_claimed >= 1;
    for _ in 0..50 {
        if claimed {
            break;
        }
        fedimint_core::runtime::sleep(std::time::Duration::from_millis(100)).await;
        claimed = amm
            .api
            .amm_balance(BalanceRequest {
                pubkey: recipient_pk,
                unit: faucet_unit(),
            })
            .await?
            .is_none();
    }
    assert!(
        claimed,
        "the unclaimed swap's Balance must become claimed from the seed alone, whether by \
         this explicit call or the module's own background sweep"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 9. Concurrent clients on one seed: smoke test against a real federation.
//
// `fedimint_amm_client::derivation::concurrent_clients_on_one_seed_do_not_
// collide` (`derivation.rs:167-173`) already unit-tests the actual
// non-collision property directly: 100 tweaks drawn from one seed via
// `grind_tweak`, all distinct. This test does not add another proof of that
// — two draws can't meaningfully raise confidence a 100-draw unit test
// hasn't already established — it only exercises the same derivation
// end-to-end against a real, consensus-driven federation with two real
// client processes trading concurrently.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_clients_on_one_seed_swap_without_colliding() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;

    let lp = fed.new_client().await;
    create_pool(
        &lp,
        Amount::from_sats(10_000_000),
        Amount::from_sats(10_000_000),
    )
    .await?;

    // Two independent client processes (separate databases), same seed —
    // the exact scenario the random-tweak derivation (spec §8) exists for:
    // a counter-based scheme would have both derive "the next" key and
    // collide, since neither has any way to know about the other.
    let client_a = fed
        .join_client_with_db(
            fedimint_core::db::mem_impl::MemDatabase::new().into(),
            root_secret(&TRADER_SK),
        )
        .await;
    let client_b = fed
        .join_client_with_db(
            fedimint_core::db::mem_impl::MemDatabase::new().into(),
            root_secret(&TRADER_SK),
        )
        .await;

    let amount_in = dust_free_sats(100_000);
    issue_btc(&client_a, amount_in).await?;
    issue_btc(&client_b, amount_in).await?;

    let pools_before = client_a
        .get_first_module::<AmmClientModule>()?
        .pools()
        .await?;
    let pool_before = pools_before
        .iter()
        .find(|p| p.pool == pool_id())
        .expect("pool exists");
    let reserve_lo_before = pool_before.reserve_lo;
    let reserve_hi_before = pool_before.reserve_hi;

    let amm_a = client_a.get_first_module::<AmmClientModule>()?;
    let amm_b = client_b.get_first_module::<AmmClientModule>()?;

    // Both swaps trade the same direction against the same pool
    // concurrently, so whichever lands second genuinely settles at a worse
    // price than its own pre-Tx1 quote — real slippage caused by the other
    // swap's price impact, not a bug. `max_slippage_bps: 0` (exact quote
    // required) would make the second-place swap's own `min_out` reject its
    // own Tx1 deterministically; verified by running this test with `0`
    // before this fix, which failed with `AmmOutputError::SlippageExceeded`
    // ("output below min_out") every time, never a key collision. A tolerance
    // large enough to comfortably cover the two-actor price impact (measured
    // at ~196 bps for this reserve/`amount_in` pair) lets both swaps land so
    // the test can actually exercise what it is named for: that concurrent
    // clients on one seed derive non-colliding recipient keys, not zero-slippage
    // pricing under concurrent load.
    let max_slippage_bps = 1_000;

    // Under a genuine `recipient_pk` collision the winner sweeps both
    // `dy_a` and `dy_b`, and the loser's Tx2 — finding a balance that no
    // longer matches what it expects — retries unboundedly, by design (spec
    // §6.3: a `Balance` is permanently claimable, never abandoned). That
    // would make the plain `try_join!` below hang forever rather than fail,
    // so bound it: a timeout here is the collision failing loudly instead of
    // the test suite stalling.
    let collision_timeout = std::time::Duration::from_secs(60);
    let (op_a, op_b) = tokio::time::timeout(collision_timeout, async {
        tokio::try_join!(
            amm_a.swap(
                AmountUnit::BITCOIN,
                faucet_unit(),
                amount_in,
                max_slippage_bps
            ),
            amm_b.swap(
                AmountUnit::BITCOIN,
                faucet_unit(),
                amount_in,
                max_slippage_bps
            ),
        )
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("Tx1 submission did not complete within {collision_timeout:?}")
    })??;
    tokio::time::timeout(collision_timeout, async {
        tokio::try_join!(amm_a.await_swap(op_a), amm_b.await_swap(op_b))
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "swaps did not settle within {collision_timeout:?} — a real recipient-key \
             collision manifests exactly this way (the loser's Tx2 retries unbounded), so \
             treat this timeout as a collision, not a slow CI machine"
        )
    })??;
    client_a.wait_for_all_active_state_machines().await?;
    client_b.wait_for_all_active_state_machines().await?;

    let balance_a = client_a.get_balance_for_unit(faucet_unit()).await?;
    let balance_b = client_b.get_balance_for_unit(faucet_unit()).await?;
    assert!(
        balance_a > Amount::ZERO,
        "client A's swap must have succeeded"
    );
    assert!(
        balance_b > Amount::ZERO,
        "client B's swap must have succeeded"
    );

    // Both trades landed independently. `reserve_lo` alone is not enough to
    // rule out a collision: both Tx1s land regardless of `recipient_pk`, so
    // `reserve_lo_after == before + 2 * amount_in` would hold even if the
    // winner's Tx2 swept both `dy_a` and `dy_b` and the loser's Tx2 were
    // still retrying. `reserve_hi` (the leg the swaps actually paid out from,
    // via each client's own `recipient_pk`) is what distinguishes the two
    // wallets genuinely having settled independently from one having
    // silently absorbed the other's share.
    let pools_after = amm_a.pools().await?;
    let pool_after = pools_after
        .iter()
        .find(|p| p.pool == pool_id())
        .expect("pool exists");
    let reserve_lo_after = pool_after.reserve_lo;
    let reserve_hi_after = pool_after.reserve_hi;
    assert_eq!(
        reserve_lo_after,
        reserve_lo_before + amount_in + amount_in,
        "both swaps' BITCOIN legs must have landed"
    );
    assert_eq!(
        reserve_hi_before - reserve_hi_after,
        balance_a + balance_b,
        "the faucet_unit() the pool paid out must equal exactly what both wallets received, \
         not more collected by one wallet at the other's expense"
    );

    Ok(())
}

/// Sanity check that both fixtures crates actually agree on which module is
/// primary for which unit (spec P10) — not a task-required test, but cheap
/// insurance the other nine implicitly depend on.
#[tokio::test(flavor = "multi_thread")]
async fn faucet_is_primary_for_its_unit_and_mintv2_for_bitcoin() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;
    let client = fed.new_client().await;

    let (btc_module_id, _) = client
        .primary_module_for_unit(AmountUnit::BITCOIN)
        .expect("a primary module for BITCOIN must exist");
    ensure!(
        Some(btc_module_id) == client.get_first_instance(&fedimint_mintv2_common::KIND),
        "mintv2 must be primary for BITCOIN"
    );

    let (unit1_module_id, _) = client
        .primary_module_for_unit(faucet_unit())
        .expect("a primary module for faucet_unit() must exist");
    ensure!(
        Some(unit1_module_id)
            == client.get_first_instance(&fedimint_amm_tests::faucet::common::KIND),
        "the test faucet must be primary for faucet_unit(), not dummy's wildcard match"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 11. The guardian-voted swap fee (spec §10, §11): a threshold of guardians
//     moves the fee, and the swap that follows settles at the VOTED fee, not
//     at the DKG-time config default.
// ---------------------------------------------------------------------------

/// The fee this test votes. Inside `default_consensus_config`'s `[1, 50]`
/// band and far enough from the config default of 3 that the resulting `dy`
/// differs by far more than any rounding — asserted below rather than assumed.
const VOTED_FEE_PER_MILLE: u16 = 30;

/// Records `fee_per_mille` as `peer`'s desired fee for [`pool_id`] through the
/// real guardian-authenticated endpoint, over that guardian's own admin API.
///
/// `request_admin` is the only path that reaches an endpoint gated on verified
/// guardian auth: it targets exactly the one peer the admin API was built for
/// and attaches the password `fedimint-testing` configures its in-process
/// federation with (`federation.rs`, `ApiAuth::new("pass")`).
///
/// Retried to a deadline because the endpoint rejects a vote for a pool the
/// *receiving guardian* has not created yet. A client observing its deposit
/// as accepted has seen a threshold of guardians apply it, not all of them —
/// guardians apply an ordered session at slightly different wall-clock
/// moments — so a submit racing that lag is expected, not a fault. (The usdt
/// suite polls each peer to convergence for the same reason.)
async fn submit_fee_vote(
    fed: &fedimint_testing::federation::FederationTest,
    peer: fedimint_core::PeerId,
    amm_instance_id: ModuleInstanceId,
    fee_per_mille: u16,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let result = fed
            .new_admin_api(peer)
            .await?
            .with_module(amm_instance_id)
            .request_admin::<()>(
                fedimint_amm_common::endpoints::FEE_VOTE_SUBMIT_ENDPOINT,
                fedimint_core::module::ApiRequestErased::new(
                    fedimint_amm_common::endpoints::FeeVoteSubmitRequest {
                        pool: pool_id(),
                        fee_per_mille,
                    },
                ),
                fedimint_core::module::ApiAuth::new("pass".to_string()),
            )
            .await;

        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                ensure!(
                    e.to_string().contains("no such pool") && std::time::Instant::now() < deadline,
                    "guardian {peer} rejected the fee vote: {e}"
                );
            }
        }

        fedimint_core::task::sleep_in_test(
            "waiting for the guardian to have applied the pool-creating deposit",
            std::time::Duration::from_millis(300),
        )
        .await;
    }
}

/// Polls `POOLS_ENDPOINT` until it reports `expected` for [`pool_id`].
///
/// Votes reach consensus asynchronously — a guardian's submission only becomes
/// a consensus item on its next `consensus_proposal` — so there is nothing to
/// await deterministically here; every guardian converges on the same value,
/// just not at the same instant.
async fn await_reported_fee(
    client: &ClientHandleArc,
    expected: u16,
) -> anyhow::Result<fedimint_amm_common::endpoints::PoolSummary> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let amm = client.get_first_module::<AmmClientModule>()?;
        let summary = amm
            .pools()
            .await?
            .into_iter()
            .find(|p| p.pool == pool_id())
            .expect("the pool exists");
        if summary.fee_per_mille == expected {
            return Ok(summary);
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "POOLS_ENDPOINT never reported fee {expected} (last saw {})",
            summary.fee_per_mille
        );
        fedimint_core::task::sleep_in_test(
            "waiting for the voted fee to reach consensus",
            std::time::Duration::from_millis(500),
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_threshold_of_guardian_votes_changes_the_fee_a_swap_settles_at() -> anyhow::Result<()> {
    let fed = fedimint_amm_tests::fixtures::new_federation().await;

    let lp = fed.new_client().await;
    create_pool(
        &lp,
        Amount::from_sats(10_000_000),
        Amount::from_sats(10_000_000),
    )
    .await?;

    let amm_instance_id = lp
        .get_first_instance(&fedimint_amm_common::KIND)
        .expect("amm module is registered");

    // Before any vote, the reported fee is the DKG-time default.
    let before_votes = lp
        .get_first_module::<AmmClientModule>()?
        .pools()
        .await?
        .into_iter()
        .find(|p| p.pool == pool_id())
        .expect("the pool exists");
    assert_eq!(
        before_votes.fee_per_mille, 3,
        "an unvoted pool must report the config default"
    );

    let peers: Vec<_> = fed.online_peer_ids().collect();
    let threshold = peers.as_slice().to_num_peers().threshold();
    ensure!(
        peers.len() == 4 && threshold == 3,
        "this test's below-threshold/at-threshold split is written for 4 peers with threshold 3"
    );

    // Below threshold: two guardians voting must NOT move the fee.
    for &peer in peers.iter().take(threshold - 1) {
        submit_fee_vote(&fed, peer, amm_instance_id, VOTED_FEE_PER_MILLE).await?;
    }
    // Give those two votes time to be ordered before concluding nothing moved.
    // A false pass here would mean the assertion ran before consensus, not
    // that a minority failed to move the fee, so this waits for the votes to
    // be visible in their own right first.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let votes: fedimint_amm_common::endpoints::FeeVotesResponse = lp
            .api()
            .with_module(amm_instance_id)
            .request_current_consensus(
                fedimint_amm_common::endpoints::FEE_VOTES_ENDPOINT.to_string(),
                fedimint_core::module::ApiRequestErased::new(
                    fedimint_amm_common::endpoints::FeeVotesRequest { pool: pool_id() },
                ),
            )
            .await?;
        if votes.votes.len() == threshold - 1 {
            assert_eq!(
                votes.effective_fee_per_mille, 3,
                "a minority of guardians must not move the fee at all"
            );
            break;
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "only {} of {} below-threshold votes were ever recorded",
            votes.votes.len(),
            threshold - 1
        );
        fedimint_core::task::sleep_in_test(
            "waiting for the below-threshold votes to be ordered",
            std::time::Duration::from_millis(500),
        )
        .await;
    }

    // The threshold-th vote is what moves it.
    submit_fee_vote(
        &fed,
        peers[threshold - 1],
        amm_instance_id,
        VOTED_FEE_PER_MILLE,
    )
    .await?;
    let summary = await_reported_fee(&lp, VOTED_FEE_PER_MILLE).await?;

    // The swap must settle at the voted fee. `dy` is read off the pool's own
    // reserves rather than a wallet balance, so no mintv2 denomination floor
    // sits between the assertion and what the curve actually paid out.
    let trader = fed.new_client().await;
    let amount_in = dust_free_sats(100_000);
    issue_btc(&trader, amount_in).await?;

    let expected_at_voted_fee = fedimint_amm_common::math::amount_out(
        summary.reserve_lo.msats,
        summary.reserve_hi.msats,
        amount_in.msats,
        VOTED_FEE_PER_MILLE,
    )
    .expect("a valid swap against these reserves");
    let expected_at_config_fee = fedimint_amm_common::math::amount_out(
        summary.reserve_lo.msats,
        summary.reserve_hi.msats,
        amount_in.msats,
        3,
    )
    .expect("a valid swap against these reserves");
    ensure!(
        expected_at_voted_fee < expected_at_config_fee,
        "the test is vacuous unless the two fees produce different outputs"
    );

    let amm = trader.get_first_module::<AmmClientModule>()?;
    let quote = amm
        .quote(AmountUnit::BITCOIN, faucet_unit(), amount_in)
        .await?;
    assert_eq!(
        quote.amount_out,
        Amount::from_msats(expected_at_voted_fee),
        "QUOTE_ENDPOINT must price at the voted fee"
    );

    let operation_id = amm
        .swap(AmountUnit::BITCOIN, faucet_unit(), amount_in, 0)
        .await?;
    amm.await_swap(operation_id).await?;
    trader.wait_for_all_active_state_machines().await?;

    let after = amm
        .pools()
        .await?
        .into_iter()
        .find(|p| p.pool == pool_id())
        .expect("the pool exists");
    let settled_dy = summary.reserve_hi - after.reserve_hi;

    assert_eq!(
        settled_dy,
        Amount::from_msats(expected_at_voted_fee),
        "settlement must charge the voted fee, not the config default"
    );
    assert_ne!(
        settled_dy,
        Amount::from_msats(expected_at_config_fee),
        "settlement must not still be charging the config default"
    );
    assert_eq!(
        after.reserve_lo,
        summary.reserve_lo + amount_in,
        "the BITCOIN leg must have landed in full"
    );
    assert_eq!(
        after.fee_per_mille, VOTED_FEE_PER_MILLE,
        "the reported fee must still be the voted one after the swap"
    );

    Ok(())
}
