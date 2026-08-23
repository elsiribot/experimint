//! The AMM module client crate.
//!
//! `derivation` implements recovery-safe key derivation (spec §8, task 10).
//! This file wires up the `ClientModule`/`ClientModuleInit` impls, the
//! read-only operations (`pools`, `quote`), the two-transaction swap flow
//! (`swap`, backed by `swap::SwapStateMachine`), the single-transaction
//! `deposit`/`withdraw` flows (backed by `single_tx`), and `recover`.
//!
//! ## No curve arithmetic in the client
//!
//! `swap`'s `min_out` comes from a fresh call to `QUOTE_ENDPOINT`, which runs
//! the exact `fedimint_amm_common::math::amount_out` the server settles with
//! (spec §12) — never reimplemented here.
//!
//! `deposit`'s `min_shares` and `withdraw`'s `min_lo`/`min_hi` have no
//! equivalent dedicated endpoint (spec §12 only defines one for swaps), so
//! they are previewed by calling `fedimint_amm_common::math::mint_shares` /
//! `burn_shares` directly against reserves fetched from `POOLS_ENDPOINT`.
//! This is not a client-side reimplementation of the curve: `common::math` is
//! the one place spec §4 puts "all curve and share arithmetic ... so client
//! quotes and server settlement run the same code path", and these two
//! functions are exactly what `process_output`'s `DepositV0` arm and
//! `process_input`'s `WithdrawV0` arm call to settle. Calling the same pure
//! function locally cannot disagree with settlement any more than a network
//! round trip protects against pool state moving between the call and
//! landing.
//!
//! `min_shares` is a genuine tolerance bound, same as `swap`'s `min_out`.
//! `min_lo`/`min_hi` are not (fix pass 3, Important 6): `withdraw` sets them
//! equal to the preview's exact amounts, making a withdrawal exact-or-reject
//! rather than tolerance-banded — see that method's doc comment for why a
//! tolerance band there could never actually bind.

pub mod api;
pub mod db;
pub mod derivation;
pub mod single_tx;
pub mod swap;

use std::collections::BTreeMap;
use std::sync::Arc;

use fedimint_amm_common::config::AmmClientConfig;
use fedimint_amm_common::endpoints::{BalanceRequest, PoolSummary, QuoteRequest, QuoteResponse};
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_common::types::{AmmInput, AmmOutput};
use fedimint_amm_common::{AmmCommonInit, AmmModuleTypes, KIND, math};
use fedimint_api_client::api::DynModuleApi;
use fedimint_client_module::db::ClientModuleMigrationFn;
use fedimint_client_module::module::init::{
    ClientModuleInit, ClientModuleInitArgs, ClientModuleRecoverArgs,
};
use fedimint_client_module::module::recovery::NoModuleBackup;
use fedimint_client_module::module::{ClientContext, ClientModule, PrimaryModuleSupport};
use fedimint_client_module::sm::{Context, DynState, ModuleNotifier, State, StateTransition};
use fedimint_client_module::transaction::{
    ClientInput, ClientInputBundle, ClientInputSM, ClientOutput, ClientOutputBundle,
    ClientOutputSM, TransactionBuilder,
};
use fedimint_client_module::{DynGlobalClientContext, sm_enum_variant_translation};
use fedimint_core::core::{IntoDynInstance, ModuleInstanceId, ModuleKind, OperationId};
use fedimint_core::db::{
    Database, DatabaseTransaction, DatabaseVersion, IDatabaseTransactionOpsCoreTyped,
};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{
    AmountUnit, Amounts, ApiVersion, ModuleCommon, ModuleInit, MultiApiVersion,
};
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::{Amount, apply, async_trait_maybe_send, push_db_pair_items};
use fedimint_derive_secret::DerivableSecret;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use thiserror::Error;

use crate::api::{AmmFederationApi, for_each_balance_recovery_entry, for_each_lp_recovery_entry};
use crate::db::{DbKeyPrefix, LpPositionKey, LpPositionPrefixAll, LpPositionRecord};
use crate::derivation::{
    CHILD_LP, CHILD_SWAP, check_tweak, derive_keypair, grind_tweak, tweak_filter,
};
use crate::single_tx::{
    DepositCommon, DepositState, DepositStateMachine, WithdrawCommon, WithdrawState,
    WithdrawStateMachine,
};
use crate::swap::{SwapCommon, SwapState, SwapStateMachine};

/// Wrapper enum for all state machines in this module, mirroring
/// `fedimint-dummy-client`'s `DummyStateMachine` / `fedimint-lnv2-client`'s
/// `LightningClientStateMachines`.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum AmmClientStateMachines {
    Swap(SwapStateMachine),
    Deposit(DepositStateMachine),
    Withdraw(WithdrawStateMachine),
}

impl IntoDynInstance for AmmClientStateMachines {
    type DynType = DynState;

    fn into_dyn(self, instance_id: ModuleInstanceId) -> Self::DynType {
        DynState::from_typed(instance_id, self)
    }
}

impl State for AmmClientStateMachines {
    type ModuleContext = AmmClientContext;

    fn transitions(
        &self,
        context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        match self {
            AmmClientStateMachines::Swap(sm) => {
                sm_enum_variant_translation!(
                    sm.transitions(context, global_context),
                    AmmClientStateMachines::Swap
                )
            }
            AmmClientStateMachines::Deposit(sm) => {
                sm_enum_variant_translation!(
                    sm.transitions(context, global_context),
                    AmmClientStateMachines::Deposit
                )
            }
            AmmClientStateMachines::Withdraw(sm) => {
                sm_enum_variant_translation!(
                    sm.transitions(context, global_context),
                    AmmClientStateMachines::Withdraw
                )
            }
        }
    }

    fn operation_id(&self) -> OperationId {
        match self {
            AmmClientStateMachines::Swap(sm) => sm.operation_id(),
            AmmClientStateMachines::Deposit(sm) => sm.operation_id(),
            AmmClientStateMachines::Withdraw(sm) => sm.operation_id(),
        }
    }
}

/// This module's state machines need no shared per-transition resources
/// beyond what `global_context` already provides (`module_api()`,
/// `claim_inputs`, `await_tx_accepted`), so this carries nothing.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct AmmClientContext;

impl Context for AmmClientContext {
    const KIND: Option<ModuleKind> = Some(KIND);
}

/// Payload logged to the operation log for each kind of operation this
/// module can start. Informational only — nothing reads it back to drive
/// behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AmmOperationMeta {
    Swap {
        unit_in: AmountUnit,
        unit_out: AmountUnit,
        amount_in: Amount,
    },
    Deposit {
        pool: PoolId,
        amount_lo: Amount,
        amount_hi: Amount,
    },
    Withdraw {
        pool: PoolId,
        owner_pk: PublicKey,
        shares: u64,
    },
    /// A balance claimed by [`AmmClientModule::recover`] rather than by a
    /// [`swap::SwapStateMachine`] Tx2 — same wire item (`ClaimBalanceV0`),
    /// different origin, kept distinct here purely for operation-log
    /// legibility.
    RecoveredClaim { pubkey: PublicKey, unit: AmountUnit },
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SlippageError {
    #[error("max_slippage_bps must be <= 10_000")]
    OutOfRange,
}

/// `amount` reduced by `max_slippage_bps` out of 10_000, floored — the
/// tolerance-to-`min_out` (and, by the same arithmetic, `min_shares`)
/// computation spec §12 calls for. `withdraw`'s `min_lo`/`min_hi` no longer
/// go through this function (fix pass 3, Important 6): a withdrawal is
/// exact-or-reject, not tolerance-banded — see [`AmmClientModule::withdraw`].
/// Pure basis-point arithmetic, not curve math: it has no notion of
/// reserves, fees, or pools, so it does not fall under the "no curve
/// arithmetic in the client" constraint (see crate-level docs).
pub fn min_after_slippage(amount: u64, max_slippage_bps: u64) -> Result<u64, SlippageError> {
    if max_slippage_bps > 10_000 {
        return Err(SlippageError::OutOfRange);
    }
    let retained_bps = 10_000u128 - u128::from(max_slippage_bps);
    // `amount <= u64::MAX` and `retained_bps <= 10_000 < 2^14`, so the
    // product is well under u128::MAX; the floored quotient is `<= amount <=
    // u64::MAX`, so the final cast is total and `unwrap_or` never actually
    // fires. `unwrap_or(0)`, not `unwrap_or(amount)` (fix pass 3, Minor): if
    // this ever were somehow reached, `0` is the fail-safe direction for a
    // slippage floor — it can only make the caller's transaction more likely
    // to be rejected as underfunded, never accept a worse deal than
    // requested, which `unwrap_or(amount)` (no floor at all) could.
    let min = u128::from(amount) * retained_bps / 10_000;
    Ok(u64::try_from(min).unwrap_or(0))
}

/// Fetches a quote via `fetch_quote` and derives `min_out` from it and
/// `max_slippage_bps` — spec §12's "the client computes `min_out` from a
/// fresh quote ... must re-quote immediately before submitting rather than
/// reuse a cached figure" (fix pass 3, Important 7a).
///
/// Generic over the quote-fetching closure (mirroring `api.rs`'s pagination
/// helpers) rather than tied to `AmmFederationApi`/`DynModuleApi` directly,
/// so this is unit-testable without a network client: a fake closure that
/// returns a different quote on every call proves this function asks fresh
/// each time — it has nowhere to cache a previous result even if it wanted
/// to, since it holds no state between calls. [`AmmClientModule::swap`]
/// calls this exact function rather than reusing a quote obtained any other
/// way (e.g. from a prior call to [`AmmClientModule::quote`]).
async fn fetch_min_out<F, Fut, E>(
    fetch_quote: F,
    unit_in: AmountUnit,
    unit_out: AmountUnit,
    amount_in: Amount,
    max_slippage_bps: u64,
) -> anyhow::Result<(Amount, QuoteResponse)>
where
    F: FnOnce(QuoteRequest) -> Fut,
    Fut: std::future::Future<Output = Result<QuoteResponse, E>>,
    E: Into<anyhow::Error>,
{
    let quote = fetch_quote(QuoteRequest {
        unit_in,
        unit_out,
        amount_in,
    })
    .await
    .map_err(Into::into)?;
    let min_out = Amount::from_msats(min_after_slippage(
        quote.amount_out.msats,
        max_slippage_bps,
    )?);
    Ok((min_out, quote))
}

/// The recovery matching predicate (spec §8, §8.2): whether `tweak` plausibly
/// belongs to `root` (the cheap [`check_tweak`] prefilter, spec's ~1-in-65536
/// tag) AND, only for the tweaks that pass, whether deriving `child` from it
/// actually reproduces `pubkey`.
///
/// The second check is what makes this safe against the "recovery-tweak
/// overwrite" threat (spec §13): an attacker can attach an arbitrary `tweak`
/// to a victim's real `pubkey` on a `Balance`/`LpPosition` record they credit
/// (the server cannot verify a pubkey was derived from a tweak, spec §7's
/// `DepositV0`/`SwapV0` comments), so a garbage tweak passing the filter by
/// chance must NOT be treated as a match — only a tweak that actually
/// re-derives the claimed pubkey may be.
pub fn matches_own_key(
    root: &DerivableSecret,
    filter: [u8; 32],
    child: u64,
    tweak: [u8; 16],
    pubkey: PublicKey,
) -> bool {
    check_tweak(tweak, filter) && derive_keypair(root, child, tweak).public_key() == pubkey
}

/// Result of [`AmmClientModule::recover`].
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct AmmRecoverySummary {
    pub balances_found: usize,
    pub balances_claimed: usize,
    pub positions_restored: usize,
    /// One entry per balance whose claim failed, formatted for logging/
    /// display. A failed claim does not roll back the LP positions already
    /// restored in the same call.
    pub claim_errors: Vec<String>,
}

pub struct AmmClientModule {
    cfg: AmmClientConfig,
    db: Database,
    notifier: ModuleNotifier<AmmClientStateMachines>,
    client_ctx: ClientContext<Self>,
    module_api: DynModuleApi,
    root_secret: DerivableSecret,
}

impl std::fmt::Debug for AmmClientModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmmClientModule").finish_non_exhaustive()
    }
}

#[apply(async_trait_maybe_send!)]
impl ClientModule for AmmClientModule {
    type Init = AmmClientInit;
    type Common = AmmModuleTypes;
    type Backup = NoModuleBackup;
    type ModuleStateMachineContext = AmmClientContext;
    type States = AmmClientStateMachines;

    fn context(&self) -> Self::ModuleStateMachineContext {
        AmmClientContext
    }

    fn input_fee(
        &self,
        _amounts: &Amounts,
        input: &<Self::Common as ModuleCommon>::Input,
    ) -> Option<Amounts> {
        match input {
            AmmInput::ClaimBalanceV0 { .. } | AmmInput::WithdrawV0 { .. } => Some(Amounts::ZERO),
            // Spec §6: "fees is always empty" covers every item THIS client
            // recognizes. An unrecognized future variant has unknown
            // semantics, so `None` here (not a guessed zero) is correct —
            // mirrors the trait's own doc comment on `input_fee`.
            AmmInput::Default { .. } => None,
        }
    }

    fn output_fee(
        &self,
        _amounts: &Amounts,
        output: &<Self::Common as ModuleCommon>::Output,
    ) -> Option<Amounts> {
        match output {
            AmmOutput::SwapV0 { .. } | AmmOutput::DepositV0 { .. } => Some(Amounts::ZERO),
            AmmOutput::Default { .. } => None,
        }
    }

    /// This module never funds transactions itself (spec P10): it declares
    /// no `AmountUnit`s of its own, and every operation here relies on
    /// whichever `mintv2` instance is the client's registered primary module
    /// for the relevant unit to supply funding inputs and change/claim
    /// outputs. Not overriding this method would default to the same
    /// [`PrimaryModuleSupport::None`] (see the trait's default impl); it is
    /// spelled out here so the design choice is visible rather than implicit.
    fn supports_being_primary(&self) -> PrimaryModuleSupport {
        PrimaryModuleSupport::None
    }
}

impl AmmClientModule {
    /// Every `PoolId` with its reserves, `total_shares`, and effective fee
    /// (spec §12).
    pub async fn pools(&self) -> anyhow::Result<Vec<PoolSummary>> {
        Ok(self.module_api.amm_pools().await?)
    }

    /// A quote computed by the same `math::amount_out` settlement uses (spec
    /// §12) — never cache this across a submission; re-quote immediately
    /// before building a transaction instead (spec's "Slippage" guidance,
    /// followed by [`Self::swap`]).
    pub async fn quote(
        &self,
        unit_in: AmountUnit,
        unit_out: AmountUnit,
        amount_in: Amount,
    ) -> anyhow::Result<QuoteResponse> {
        Ok(self
            .module_api
            .amm_quote(QuoteRequest {
                unit_in,
                unit_out,
                amount_in,
            })
            .await?)
    }

    /// Every LP position this client has created or recovered, from the
    /// local cache (spec §8.3: positions fragment, one row per deposit).
    /// `LpPositionRecord::shares` is a best-effort cache; see its doc comment.
    pub async fn list_lp_positions(&self) -> Vec<(LpPositionKey, LpPositionRecord)> {
        let mut dbtx = self.db.begin_transaction_nc().await;
        dbtx.find_by_prefix(&LpPositionPrefixAll)
            .await
            .collect()
            .await
    }

    /// Sells `amount_in` of `unit_in` for `unit_out`, tolerating up to
    /// `max_slippage_bps` (out of 10_000) worse than the quote taken right
    /// before submission. Returns immediately after Tx1 is submitted; the
    /// operation (trackable via `operation_id` through the usual executor/
    /// notifier machinery) completes once [`swap::SwapStateMachine`] reaches
    /// [`SwapState::Done`] — two on-federation transactions, one operation
    /// (spec §12.1).
    pub async fn swap(
        &self,
        unit_in: AmountUnit,
        unit_out: AmountUnit,
        amount_in: Amount,
        max_slippage_bps: u64,
    ) -> anyhow::Result<OperationId> {
        anyhow::ensure!(unit_in != unit_out, "unit_in and unit_out must differ");
        anyhow::ensure!(
            self.cfg.units.contains_key(&unit_in) && self.cfg.units.contains_key(&unit_out),
            "unit not in this federation's allowlist"
        );

        let operation_id = OperationId::new_random();

        // Fresh quote, taken immediately before building Tx1 (spec §12).
        // Routed through `fetch_min_out` (fix pass 3, Important 7a) — a
        // function generic over the quote-fetching closure, exactly like
        // `api.rs`'s pagination helpers, so a test can prove it asks fresh
        // every call rather than reusing whatever `Self::quote` last
        // returned.
        let (min_out, _quote) = fetch_min_out(
            |request| self.module_api.amm_quote(request),
            unit_in,
            unit_out,
            amount_in,
            max_slippage_bps,
        )
        .await?;

        // Fresh key per swap (spec §8, §13.1): never reused or derived from
        // any other identity.
        let tweak = grind_tweak(&self.root_secret);
        let recipient_keypair = derive_keypair(&self.root_secret, CHILD_SWAP, tweak);
        let recipient_pk = recipient_keypair.public_key();

        let output = AmmOutput::SwapV0 {
            unit_in,
            unit_out,
            amount_in,
            min_out,
            recipient_pk,
            tweak,
        };

        let client_output = ClientOutput {
            output,
            amounts: Amounts::new_custom(unit_in, amount_in),
        };

        let common = SwapCommon {
            operation_id,
            unit_out,
            recipient_keypair,
        };

        let client_output_sm = ClientOutputSM {
            state_machines: Arc::new(move |range| {
                vec![AmmClientStateMachines::Swap(SwapStateMachine {
                    common: common.clone(),
                    state: SwapState::Tx1Submitted { txid: range.txid() },
                })]
            }),
        };

        let tx_builder =
            TransactionBuilder::new().with_outputs(self.client_ctx.make_client_outputs(
                ClientOutputBundle::new(vec![client_output], vec![client_output_sm]),
            ));

        self.client_ctx
            .finalize_and_submit_transaction(
                operation_id,
                KIND.as_str(),
                move |_| AmmOperationMeta::Swap {
                    unit_in,
                    unit_out,
                    amount_in,
                },
                tx_builder,
            )
            .await?;

        Ok(operation_id)
    }

    /// Adds liquidity to `pool` (creating it if this is the first deposit),
    /// tolerating up to `max_slippage_bps` fewer shares than a preview
    /// computed against the pool's current reserves (see crate-level docs on
    /// why this preview is not "curve arithmetic in the client").
    pub async fn deposit(
        &self,
        pool: PoolId,
        amount_lo: Amount,
        amount_hi: Amount,
        max_slippage_bps: u64,
    ) -> anyhow::Result<OperationId> {
        anyhow::ensure!(
            self.cfg.units.contains_key(&pool.lo()) && self.cfg.units.contains_key(&pool.hi()),
            "unit not in this federation's allowlist"
        );

        let operation_id = OperationId::new_random();

        let pools = self.module_api.amm_pools().await?;
        let (reserve_lo, reserve_hi, total_shares) = pools
            .iter()
            .find(|p| p.pool == pool)
            .map(|p| (p.reserve_lo.msats, p.reserve_hi.msats, p.total_shares))
            .unwrap_or((0, 0, 0));

        let preview = math::mint_shares(
            reserve_lo,
            reserve_hi,
            total_shares,
            amount_lo.msats,
            amount_hi.msats,
        )?;
        let min_shares = min_after_slippage(preview.to_owner, max_slippage_bps)?;

        let tweak = grind_tweak(&self.root_secret);
        let owner_keypair = derive_keypair(&self.root_secret, CHILD_LP, tweak);
        let owner_pk = owner_keypair.public_key();

        let output = AmmOutput::DepositV0 {
            pool,
            amount_lo,
            amount_hi,
            min_shares,
            owner_pk,
            tweak,
        };

        let amounts = Amounts::ZERO
            .checked_add_unit(amount_lo, pool.lo())
            .and_then(|a| a.checked_add_unit(amount_hi, pool.hi()))
            .ok_or_else(|| anyhow::anyhow!("deposit amount overflow"))?;

        let client_output = ClientOutput { output, amounts };

        let common = DepositCommon {
            operation_id,
            pool,
            owner_pk,
            tweak,
            expected_shares: preview.to_owner,
        };

        let client_output_sm = ClientOutputSM {
            state_machines: Arc::new(move |range| {
                vec![AmmClientStateMachines::Deposit(DepositStateMachine {
                    common: common.clone(),
                    state: DepositState::Submitted { txid: range.txid() },
                })]
            }),
        };

        let tx_builder =
            TransactionBuilder::new().with_outputs(self.client_ctx.make_client_outputs(
                ClientOutputBundle::new(vec![client_output], vec![client_output_sm]),
            ));

        self.client_ctx
            .finalize_and_submit_transaction(
                operation_id,
                KIND.as_str(),
                move |_| AmmOperationMeta::Deposit {
                    pool,
                    amount_lo,
                    amount_hi,
                },
                tx_builder,
            )
            .await?;

        Ok(operation_id)
    }

    /// Burns `shares` of the local LP position keyed by `(pool, owner_pk)`.
    ///
    /// **Exact-or-reject, not slippage-tolerant** (fix pass 3, Important 6).
    /// There used to be a `max_slippage_bps` parameter here, but it could
    /// never be the binding constraint: this method declares the
    /// transaction's output amounts as the preview computed below, while
    /// `min_lo`/`min_hi` were set to a tolerance *under* that same preview.
    /// The server settles `WithdrawV0` at the pool's reserves at settlement
    /// time, not at preview time — so if any concurrent operation on this
    /// pool (most commonly a swap) moves reserves between the preview above
    /// and settlement, one of two things happened: settlement below the
    /// preview on either leg made `input < output` for that unit, and core
    /// rejects the whole transaction with an opaque `UnbalancedTransaction`
    /// regardless of what `min_lo`/`min_hi` said (spec P5); settlement above
    /// the preview instead silently forfeited the surplus, since the
    /// declared output amounts — not the tolerance band — are what the
    /// client actually asks core to mint back. Either way the tolerance
    /// parameter did nothing.
    ///
    /// So `min_lo`/`min_hi` are set equal to the declared amounts here: a
    /// reserve move before settlement is rejected up front with a clear
    /// `SlippageExceeded` (spec §7.3) instead of an opaque
    /// `UnbalancedTransaction` from core, and nothing is ever silently
    /// under- or over-paid. Spec §12.1 documents this: a withdrawal that
    /// loses this race should simply be retried against a fresh preview.
    pub async fn withdraw(
        &self,
        pool: PoolId,
        owner_pk: PublicKey,
        shares: u64,
    ) -> anyhow::Result<OperationId> {
        let record = {
            let mut dbtx = self.db.begin_transaction_nc().await;
            dbtx.get_value(&LpPositionKey { pool, owner_pk }).await
        }
        .ok_or_else(|| anyhow::anyhow!("no locally known LP position for this pool/owner_pk"))?;

        anyhow::ensure!(shares > 0, "shares must be non-zero");

        let owner_keypair = derive_keypair(&self.root_secret, CHILD_LP, record.tweak);
        anyhow::ensure!(
            owner_keypair.public_key() == owner_pk,
            "stored tweak does not derive to the requested owner_pk"
        );

        let pools = self.module_api.amm_pools().await?;
        let summary = pools
            .iter()
            .find(|p| p.pool == pool)
            .ok_or_else(|| anyhow::anyhow!("no such pool"))?;

        let preview = math::burn_shares(
            summary.reserve_lo.msats,
            summary.reserve_hi.msats,
            summary.total_shares,
            shares,
        )?;
        // Exact, not tolerance-reduced (see this method's doc comment): a
        // withdrawal is exact-or-reject.
        let min_lo = Amount::from_msats(preview.da);
        let min_hi = Amount::from_msats(preview.db);

        let input = AmmInput::WithdrawV0 {
            pool,
            owner_pk,
            shares,
            min_lo,
            min_hi,
        };

        let amounts = Amounts::ZERO
            .checked_add_unit(Amount::from_msats(preview.da), pool.lo())
            .and_then(|a| a.checked_add_unit(Amount::from_msats(preview.db), pool.hi()))
            .ok_or_else(|| anyhow::anyhow!("withdraw amount overflow"))?;

        let client_input = ClientInput {
            input,
            keys: vec![owner_keypair],
            amounts,
        };

        let operation_id = OperationId::new_random();
        let common = WithdrawCommon {
            operation_id,
            pool,
            owner_pk,
            shares,
        };

        let client_input_sm = ClientInputSM {
            state_machines: Arc::new(move |range| {
                vec![AmmClientStateMachines::Withdraw(WithdrawStateMachine {
                    common: common.clone(),
                    state: WithdrawState::Submitted { txid: range.txid() },
                })]
            }),
        };

        let tx_builder = TransactionBuilder::new().with_inputs(self.client_ctx.make_client_inputs(
            ClientInputBundle::new(vec![client_input], vec![client_input_sm]),
        ));

        self.client_ctx
            .finalize_and_submit_transaction(
                operation_id,
                KIND.as_str(),
                move |_| AmmOperationMeta::Withdraw {
                    pool,
                    owner_pk,
                    shares,
                },
                tx_builder,
            )
            .await?;

        Ok(operation_id)
    }

    /// Waits for a [`Self::swap`] operation to reach a terminal state.
    pub async fn await_swap(&self, operation_id: OperationId) -> anyhow::Result<()> {
        let mut stream = self.notifier.subscribe(operation_id).await;
        while let Some(state) = stream.next().await {
            let AmmClientStateMachines::Swap(sm) = state else {
                continue;
            };
            match sm.state {
                SwapState::Done => return Ok(()),
                SwapState::Tx1Rejected(error) => anyhow::bail!("Tx1 rejected: {error}"),
                SwapState::Tx2Rejected(error) => anyhow::bail!("Tx2 rejected: {error}"),
                SwapState::Tx1Submitted { .. }
                | SwapState::Tx1Accepted { .. }
                | SwapState::Tx2Failed { .. }
                | SwapState::Tx2Submitted { .. } => {}
            }
        }
        anyhow::bail!(
            "swap operation {} stream ended without reaching a terminal state",
            operation_id.fmt_short()
        )
    }

    /// Waits for a [`Self::deposit`] operation to reach a terminal state.
    pub async fn await_deposit(&self, operation_id: OperationId) -> anyhow::Result<()> {
        let mut stream = self.notifier.subscribe(operation_id).await;
        while let Some(state) = stream.next().await {
            let AmmClientStateMachines::Deposit(sm) = state else {
                continue;
            };
            match sm.state {
                DepositState::Accepted => return Ok(()),
                DepositState::Rejected(error) => anyhow::bail!("deposit rejected: {error}"),
                DepositState::Submitted { .. } => {}
            }
        }
        anyhow::bail!(
            "deposit operation {} stream ended without reaching a terminal state",
            operation_id.fmt_short()
        )
    }

    /// Waits for a [`Self::withdraw`] operation to reach a terminal state.
    pub async fn await_withdraw(&self, operation_id: OperationId) -> anyhow::Result<()> {
        let mut stream = self.notifier.subscribe(operation_id).await;
        while let Some(state) = stream.next().await {
            let AmmClientStateMachines::Withdraw(sm) = state else {
                continue;
            };
            match sm.state {
                WithdrawState::Accepted => return Ok(()),
                WithdrawState::Rejected(error) => anyhow::bail!("withdraw rejected: {error}"),
                WithdrawState::Submitted { .. } => {}
            }
        }
        anyhow::bail!(
            "withdraw operation {} stream ended without reaching a terminal state",
            operation_id.fmt_short()
        )
    }

    /// Scans both recovery endpoints (spec §8.2, §12: "Recovery is a table
    /// scan, not a history replay"), restores every LP position this seed
    /// owns into the local cache, and claims every balance this seed owns.
    ///
    /// For each row, [`check_tweak`] runs first as a cheap prefilter; only
    /// the ~1-in-65536 tweaks that pass it pay for a real key derivation
    /// (spec §8) — see [`matches_own_key`].
    ///
    /// This inherent method and [`AmmClientInit::recover`] (the framework's
    /// seed-restore hook, fix pass 3, Important 4) both delegate to
    /// [`recover_with`], so restoring from seed through either path runs the
    /// identical logic.
    ///
    /// **Racing an in-flight swap is benign.** If a `SwapStateMachine` on
    /// this same client is concurrently claiming a balance this scan also
    /// found, one of the two claim attempts loses the race and its
    /// `ClaimBalanceV0` is rejected with `NoSuchBalance` (spec §6.1: claims
    /// are all-or-nothing, and a second claim for an already-claimed key
    /// finds nothing). That surfaces here as one entry in `claim_errors`,
    /// not as a lost balance — the swap state machine's own claim, or this
    /// call's, whichever won, still completes the transfer.
    pub async fn recover(&self) -> anyhow::Result<AmmRecoverySummary> {
        recover_with(
            &self.db,
            &self.module_api,
            &self.root_secret,
            &self.client_ctx,
        )
        .await
    }
}

/// Shared body of [`AmmClientModule::recover`] and
/// [`AmmClientInit::recover`] (fix pass 3, Important 4): free-standing and
/// parameterized rather than an inherent method, since
/// `ClientModuleInit::recover` runs from [`fedimint_client_module::module::
/// init::ClientModuleRecoverArgs`] and has no constructed [`AmmClientModule`]
/// to call methods on — only the pieces of it that `ClientModuleRecoverArgs`
/// exposes (`db()`, `module_api()`, `module_root_secret()`, `context()`,
/// documented as usable immediately), which is exactly this function's
/// parameter list.
async fn recover_with(
    db: &Database,
    module_api: &DynModuleApi,
    root_secret: &DerivableSecret,
    client_ctx: &ClientContext<AmmClientModule>,
) -> anyhow::Result<AmmRecoverySummary> {
    let filter = tweak_filter(root_secret);

    let mut own_balances = Vec::new();
    for_each_balance_recovery_entry(
        |req| module_api.amm_balance_recovery_page(req),
        |entry| {
            if matches_own_key(root_secret, filter, CHILD_SWAP, entry.tweak, entry.pubkey) {
                own_balances.push(*entry);
            }
        },
    )
    .await?;

    let mut own_positions = Vec::new();
    for_each_lp_recovery_entry(
        |req| module_api.amm_lp_recovery_page(req),
        |entry| {
            if matches_own_key(root_secret, filter, CHILD_LP, entry.tweak, entry.pubkey) {
                own_positions.push(*entry);
            }
        },
    )
    .await?;

    // Restore LP positions first: even if a subsequent claim below fails,
    // positions found so far are not lost.
    {
        let mut dbtx = db.begin_transaction().await;
        for entry in &own_positions {
            dbtx.insert_entry(
                &LpPositionKey {
                    pool: entry.pool,
                    owner_pk: entry.pubkey,
                },
                &LpPositionRecord {
                    tweak: entry.tweak,
                    shares: entry.shares,
                },
            )
            .await;
        }
        dbtx.commit_tx().await;
    }

    let mut balances_claimed = 0usize;
    let mut claim_errors = Vec::new();
    for entry in &own_balances {
        // Re-read immediately before claiming (fix pass 3, Minor), via the
        // point-lookup endpoint (Important 5) rather than the scan-time
        // `entry.amount`: that amount was read as long ago as one full
        // paginated scan, during which a gift credited to this pubkey (spec
        // §6.1: anyone may credit anyone's balance) would otherwise be
        // forfeited. `Ok(None)` means the balance is already gone — most
        // likely an in-flight `SwapStateMachine`'s own Tx2 won a race against
        // this scan, which is benign (see this function's doc comment) — so
        // it is skipped rather than attempted or reported as an error.
        match module_api
            .amm_balance(BalanceRequest {
                pubkey: entry.pubkey,
                unit: entry.unit,
            })
            .await
        {
            Ok(None) => {}
            Ok(Some(amount)) => {
                match claim_recovered_balance(
                    client_ctx,
                    root_secret,
                    entry.tweak,
                    entry.unit,
                    amount,
                )
                .await
                {
                    Ok(()) => balances_claimed += 1,
                    Err(error) => {
                        claim_errors.push(format!("{:?}/{:?}: {error}", entry.pubkey, entry.unit));
                    }
                }
            }
            Err(error) => claim_errors.push(format!(
                "{:?}/{:?}: failed to re-read balance before claiming: {error}",
                entry.pubkey, entry.unit
            )),
        }
    }

    Ok(AmmRecoverySummary {
        balances_found: own_balances.len(),
        balances_claimed,
        positions_restored: own_positions.len(),
        claim_errors,
    })
}

/// Claims a single balance found by [`recover_with`]. Unlike
/// [`swap::SwapStateMachine`]'s Tx2, this re-derives the recipient keypair
/// from the recovered tweak rather than carrying it in state, since there is
/// no ongoing swap operation to carry it — recovery finds balances that
/// predate this client run entirely.
async fn claim_recovered_balance(
    client_ctx: &ClientContext<AmmClientModule>,
    root_secret: &DerivableSecret,
    tweak: [u8; 16],
    unit: AmountUnit,
    amount: Amount,
) -> anyhow::Result<()> {
    let keypair = derive_keypair(root_secret, CHILD_SWAP, tweak);
    let pubkey = keypair.public_key();

    let input = AmmInput::ClaimBalanceV0 { pubkey, unit };
    let client_input = ClientInput {
        input,
        keys: vec![keypair],
        amounts: Amounts::new_custom(unit, amount),
    };

    let operation_id = OperationId::new_random();
    let tx_builder = TransactionBuilder::new().with_inputs(
        client_ctx.make_client_inputs(ClientInputBundle::new_no_sm(vec![client_input])),
    );

    let range = client_ctx
        .finalize_and_submit_transaction(
            operation_id,
            KIND.as_str(),
            move |_| AmmOperationMeta::RecoveredClaim { pubkey, unit },
            tx_builder,
        )
        .await?;

    client_ctx
        .transaction_updates(operation_id)
        .await
        .await_tx_accepted(range.txid())
        .await
        .map_err(|error| anyhow::anyhow!("recovered claim rejected: {error}"))
}

#[derive(Debug, Clone)]
pub struct AmmClientInit;

impl ModuleInit for AmmClientInit {
    type Common = AmmCommonInit;

    async fn dump_database(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        prefix_names: Vec<String>,
    ) -> Box<dyn Iterator<Item = (String, Box<dyn erased_serde::Serialize + Send>)> + '_> {
        let mut items: BTreeMap<String, Box<dyn erased_serde::Serialize + Send>> = BTreeMap::new();
        let filtered_prefixes = DbKeyPrefix::iter().filter(|f| {
            prefix_names.is_empty() || prefix_names.contains(&f.to_string().to_lowercase())
        });

        for table in filtered_prefixes {
            match table {
                DbKeyPrefix::LpPosition => {
                    push_db_pair_items!(
                        dbtx,
                        LpPositionPrefixAll,
                        LpPositionKey,
                        LpPositionRecord,
                        items,
                        "Amm LP Position"
                    );
                }
                DbKeyPrefix::ExternalReservedStart
                | DbKeyPrefix::CoreInternalReservedStart
                | DbKeyPrefix::CoreInternalReservedEnd => {}
            }
        }

        Box::new(items.into_iter())
    }
}

#[apply(async_trait_maybe_send!)]
impl ClientModuleInit for AmmClientInit {
    type Module = AmmClientModule;

    /// `MultiApiVersion::try_from_iter` only fails on two input elements
    /// sharing a major version (checked by reading
    /// `fedimint-core/src/module/version.rs`); a single-element literal array
    /// cannot trigger that, so the `.expect` here is unreachable rather than
    /// hopeful. There is no infallible constructor for a non-empty
    /// `MultiApiVersion` in the pinned API, and `ClientModuleInit::
    /// supported_api_versions` returns `MultiApiVersion` directly (no
    /// `Result`) — this mirrors `fedimint-dummy-client` and
    /// `fedimint-lnv2-client`'s identical call at the same trait method,
    /// which is the pinned source's own idiom for exactly this situation.
    fn supported_api_versions(&self) -> MultiApiVersion {
        MultiApiVersion::try_from_iter([ApiVersion { major: 0, minor: 0 }])
            .expect("a single-element array cannot contain two conflicting major versions")
    }

    async fn init(&self, args: &ClientModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        Ok(AmmClientModule {
            cfg: args.cfg().clone(),
            db: args.db().clone(),
            notifier: args.notifier().clone(),
            client_ctx: args.context(),
            module_api: args.module_api().clone(),
            root_secret: args.module_root_secret().clone(),
        })
    }

    /// Wires [`recover_with`] into the framework's seed-restore hook (fix
    /// pass 3, Important 4). Before this, the default
    /// `ClientModuleInit::recover` ran instead — it only logs "Module does
    /// not support recovery, completing without doing anything"
    /// (`fedimint-client-module/src/module/init.rs:392-403`, verified against
    /// the pinned source) — so every seed restore
    /// (`fedimint-client/src/client/builder.rs:830-856`) silently recovered
    /// nothing from this module. [`AmmClientModule::recover`] is kept as a
    /// public method too (e.g. for an already-running client to re-scan on
    /// demand), and both now share [`recover_with`].
    ///
    /// `Backup = NoModuleBackup` (see [`ClientModule`] impl above), so
    /// `_snapshot` is always `None` here and carries no information this
    /// function could use — recovery is a full table scan regardless (spec
    /// §8.2), not a snapshot-assisted resume.
    async fn recover(
        &self,
        args: &ClientModuleRecoverArgs<Self>,
        _snapshot: Option<&NoModuleBackup>,
    ) -> anyhow::Result<Option<Amount>> {
        let summary = recover_with(
            args.db(),
            args.module_api(),
            args.module_root_secret(),
            &args.context(),
        )
        .await?;

        tracing::info!(
            balances_found = summary.balances_found,
            balances_claimed = summary.balances_claimed,
            positions_restored = summary.positions_restored,
            claim_errors = summary.claim_errors.len(),
            "AMM module recovery complete",
        );

        // Unlike a single-denomination module (e.g. mintv2's notes), this
        // module's recovered balances and positions span whichever
        // federation-configured `AmountUnit`s they happened to be in —
        // there is no single native unit to sum them into one `Amount`, so
        // `None` is the honest answer the trait's own doc comment
        // anticipates ("`None` for modules that can't determine the amount
        // at recovery-completion time"), not a placeholder.
        Ok(None)
    }

    fn get_database_migrations(&self) -> BTreeMap<DatabaseVersion, ClientModuleMigrationFn> {
        BTreeMap::new()
    }
}

#[cfg(test)]
mod tests {
    use fedimint_derive_secret::DerivableSecret;

    use super::*;

    #[test]
    fn min_after_slippage_zero_tolerance_keeps_the_full_amount() {
        assert_eq!(min_after_slippage(1_000, 0), Ok(1_000));
    }

    #[test]
    fn min_after_slippage_full_tolerance_allows_zero() {
        assert_eq!(min_after_slippage(1_000, 10_000), Ok(0));
    }

    #[test]
    fn min_after_slippage_applies_the_stated_percentage() {
        // 1% of 1000 = 10, so 100 bps of tolerance keeps 990.
        assert_eq!(min_after_slippage(1_000, 100), Ok(990));
    }

    #[test]
    fn min_after_slippage_floors_rather_than_rounds() {
        // 3 * 9_999 / 10_000 = 2.9997, must floor to 2, not round to 3.
        assert_eq!(min_after_slippage(3, 1), Ok(2));
    }

    #[test]
    fn min_after_slippage_rejects_bps_above_ten_thousand() {
        assert_eq!(
            min_after_slippage(1_000, 10_001),
            Err(SlippageError::OutOfRange)
        );
    }

    #[test]
    fn min_after_slippage_handles_amount_zero() {
        assert_eq!(min_after_slippage(0, 5_000), Ok(0));
    }

    fn root(seed: &[u8; 32]) -> DerivableSecret {
        DerivableSecret::new_root(seed, b"fedimint-amm-client matches_own_key tests")
    }

    #[test]
    fn matches_own_key_accepts_a_genuinely_ground_tweak() {
        let root = root(&[1u8; 32]);
        let filter = tweak_filter(&root);
        let tweak = grind_tweak(&root);
        let pubkey = derive_keypair(&root, CHILD_SWAP, tweak).public_key();

        assert!(matches_own_key(&root, filter, CHILD_SWAP, tweak, pubkey));
    }

    /// A tweak that passes the cheap filter but was never actually used to
    /// derive `pubkey` must be rejected — this is precisely the
    /// "recovery-tweak overwrite" scenario spec §13 describes: an attacker
    /// can attach ANY tweak to a victim's real pubkey on a record they
    /// credit, and the filter alone (a 16-bit tag with no cryptographic
    /// binding to the pubkey) cannot tell that apart from a real one.
    #[test]
    fn matches_own_key_rejects_a_filter_passing_tweak_paired_with_the_wrong_pubkey() {
        let root = root(&[2u8; 32]);
        let filter = tweak_filter(&root);
        let tweak = grind_tweak(&root); // passes the filter by construction
        let unrelated_pubkey = derive_keypair(&root, CHILD_LP, [0u8; 16]).public_key();

        assert!(!matches_own_key(
            &root,
            filter,
            CHILD_SWAP,
            tweak,
            unrelated_pubkey
        ));
    }

    #[test]
    fn matches_own_key_rejects_a_tweak_from_a_different_seed() {
        let root_a = root(&[3u8; 32]);
        let root_b = root(&[4u8; 32]);
        let filter_a = tweak_filter(&root_a);
        let tweak_b = grind_tweak(&root_b);
        let pubkey_b = derive_keypair(&root_b, CHILD_SWAP, tweak_b).public_key();

        // Vanishingly unlikely for `tweak_b` to also pass `root_a`'s filter,
        // but even if it did, the pubkey re-derivation under `root_a` would
        // not match `pubkey_b` (derived under `root_b`).
        assert!(!matches_own_key(
            &root_a, filter_a, CHILD_SWAP, tweak_b, pubkey_b
        ));
    }

    /// The two child ids must never be interchangeable: a balance's
    /// `recipient_pk` (spec §8.1, `CHILD_SWAP`) must not be mistaken for an
    /// LP position's `owner_pk` (`CHILD_LP`), even from the same tweak.
    #[test]
    fn matches_own_key_is_specific_to_the_requested_child_id() {
        let root = root(&[5u8; 32]);
        let filter = tweak_filter(&root);
        let tweak = grind_tweak(&root);
        let swap_pubkey = derive_keypair(&root, CHILD_SWAP, tweak).public_key();

        assert!(matches_own_key(
            &root,
            filter,
            CHILD_SWAP,
            tweak,
            swap_pubkey
        ));
        assert!(!matches_own_key(
            &root,
            filter,
            CHILD_LP,
            tweak,
            swap_pubkey
        ));
    }

    /// Fix pass 3, Important 7a: [`swap`] must derive `min_out` from a quote
    /// taken fresh for that call, never a cached one. [`fetch_min_out`] is
    /// the exact function [`AmmClientModule::swap`] calls, generic over the
    /// quote-fetching closure so it can be exercised here without a network
    /// client: a fake closure returning a different quote on each of two
    /// calls produces two different `min_out`s, and — via the call counter —
    /// each call is proven to invoke the closure exactly once, rather than
    /// reusing a value from a previous call.
    #[tokio::test]
    async fn fetch_min_out_asks_a_fresh_quote_every_call_rather_than_caching() {
        use std::cell::Cell;

        let calls = Cell::new(0u32);
        let fetch = |amount_out_msats: u64| {
            calls.set(calls.get() + 1);
            async move {
                Ok::<_, anyhow::Error>(QuoteResponse {
                    amount_out: Amount::from_msats(amount_out_msats),
                    price_impact_per_mille: 0,
                })
            }
        };

        let unit_in = AmountUnit::new_custom(0);
        let unit_out = AmountUnit::new_custom(1);
        let amount_in = Amount::from_msats(1_000);

        let (min_out_first, _) = fetch_min_out(|_req| fetch(100), unit_in, unit_out, amount_in, 0)
            .await
            .expect("first fetch succeeds");
        let (min_out_second, _) = fetch_min_out(|_req| fetch(200), unit_in, unit_out, amount_in, 0)
            .await
            .expect("second fetch succeeds");

        assert_eq!(calls.get(), 2, "each call must fetch its own fresh quote");
        assert_eq!(min_out_first, Amount::from_msats(100));
        assert_eq!(min_out_second, Amount::from_msats(200));
        assert_ne!(
            min_out_first, min_out_second,
            "a stale/cached quote would make both calls agree despite different underlying quotes"
        );
    }

    /// [`fetch_min_out`] must apply the caller's slippage tolerance to
    /// whatever the fetched quote says, not some other figure.
    #[tokio::test]
    async fn fetch_min_out_applies_slippage_to_the_fetched_quote() {
        let unit_in = AmountUnit::new_custom(0);
        let unit_out = AmountUnit::new_custom(1);
        let amount_in = Amount::from_msats(1_000);

        let (min_out, quote) = fetch_min_out(
            |_req| async {
                Ok::<_, anyhow::Error>(QuoteResponse {
                    amount_out: Amount::from_msats(1_000),
                    price_impact_per_mille: 0,
                })
            },
            unit_in,
            unit_out,
            amount_in,
            100, // 1%
        )
        .await
        .expect("fetch succeeds");

        assert_eq!(quote.amount_out, Amount::from_msats(1_000));
        assert_eq!(min_out, Amount::from_msats(990));
    }

    /// Fix pass 3, Important 7b: [`AmmClientModule::swap`] must ground a
    /// fresh `recipient_pk` for every call (spec §8, §13.1) — this exercises
    /// the exact primitive (`grind_tweak` + `derive_keypair(_, CHILD_SWAP,
    /// _)`) that `swap`'s body calls inline with no caching in between, so
    /// two calls on the same root secret must not collide.
    #[test]
    fn swaps_recipient_key_derivation_is_fresh_every_call() {
        let root = root(&[6u8; 32]);
        let first = derive_keypair(&root, CHILD_SWAP, grind_tweak(&root)).public_key();
        let second = derive_keypair(&root, CHILD_SWAP, grind_tweak(&root)).public_key();
        assert_ne!(
            first, second,
            "recipient_pk must not be reused across swaps"
        );
    }
}
