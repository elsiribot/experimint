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
//! `deposit`/`withdraw`'s `min_shares`/`min_lo`/`min_hi` have no equivalent
//! dedicated endpoint (spec §12 only defines one for swaps), so they are
//! previewed by calling `fedimint_amm_common::math::mint_shares` /
//! `burn_shares` directly against reserves fetched from `POOLS_ENDPOINT`.
//! This is not a client-side reimplementation of the curve: `common::math` is
//! the one place spec §4 puts "all curve and share arithmetic ... so client
//! quotes and server settlement run the same code path", and these two
//! functions are exactly what `process_output`'s `DepositV0` arm and
//! `process_input`'s `WithdrawV0` arm call to settle. Calling the same pure
//! function locally cannot disagree with settlement any more than a network
//! round trip protects against pool state moving between the call and
//! landing — which is exactly what `min_shares`/`min_lo`/`min_hi` exist to
//! bound, the same as for swaps.

pub mod api;
pub mod db;
pub mod derivation;
pub mod single_tx;
pub mod swap;

use std::collections::BTreeMap;
use std::sync::Arc;

use fedimint_amm_common::config::AmmClientConfig;
use fedimint_amm_common::endpoints::{PoolSummary, QuoteRequest, QuoteResponse};
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_common::types::{AmmInput, AmmOutput};
use fedimint_amm_common::{AmmCommonInit, AmmModuleTypes, KIND, math};
use fedimint_api_client::api::DynModuleApi;
use fedimint_client_module::db::ClientModuleMigrationFn;
use fedimint_client_module::module::init::{ClientModuleInit, ClientModuleInitArgs};
use fedimint_client_module::module::recovery::NoModuleBackup;
use fedimint_client_module::module::{ClientContext, ClientModule, PrimaryModuleSupport};
use fedimint_client_module::sm::{Context, DynState, ModuleNotifier, State, StateTransition};
use fedimint_client_module::transaction::{
    ClientInput, ClientInputBundle, ClientInputSM, ClientOutput, ClientOutputBundle, ClientOutputSM,
    TransactionBuilder,
};
use fedimint_client_module::{DynGlobalClientContext, sm_enum_variant_translation};
use fedimint_core::core::{IntoDynInstance, ModuleInstanceId, ModuleKind, OperationId};
use fedimint_core::db::{
    Database, DatabaseTransaction, DatabaseVersion, IDatabaseTransactionOpsCoreTyped,
};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{AmountUnit, Amounts, ApiVersion, ModuleCommon, ModuleInit, MultiApiVersion};
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::{Amount, apply, async_trait_maybe_send, push_db_pair_items};
use fedimint_derive_secret::DerivableSecret;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use thiserror::Error;

use crate::api::{AmmFederationApi, for_each_balance_recovery_entry, for_each_lp_recovery_entry};
use crate::db::{DbKeyPrefix, LpPositionKey, LpPositionPrefixAll, LpPositionRecord};
use crate::derivation::{CHILD_LP, CHILD_SWAP, check_tweak, derive_keypair, grind_tweak, tweak_filter};
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
/// tolerance-to-`min_out` (and, by the same arithmetic, `min_shares`/
/// `min_lo`/`min_hi`) computation spec §12 calls for. Pure basis-point
/// arithmetic, not curve math: it has no notion of reserves, fees, or pools,
/// so it does not fall under the "no curve arithmetic in the client"
/// constraint (see crate-level docs).
pub fn min_after_slippage(amount: u64, max_slippage_bps: u64) -> Result<u64, SlippageError> {
    if max_slippage_bps > 10_000 {
        return Err(SlippageError::OutOfRange);
    }
    let retained_bps = 10_000u128 - u128::from(max_slippage_bps);
    // `amount <= u64::MAX` and `retained_bps <= 10_000 < 2^14`, so the
    // product is well under u128::MAX; the floored quotient is `<= amount <=
    // u64::MAX`, so the final cast is total.
    let min = u128::from(amount) * retained_bps / 10_000;
    Ok(u64::try_from(min).unwrap_or(amount))
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
        dbtx.find_by_prefix(&LpPositionPrefixAll).await.collect().await
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
        let quote = self
            .module_api
            .amm_quote(QuoteRequest {
                unit_in,
                unit_out,
                amount_in,
            })
            .await?;
        let min_out = Amount::from_msats(min_after_slippage(quote.amount_out.msats, max_slippage_bps)?);

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

        let tx_builder = TransactionBuilder::new().with_outputs(self.client_ctx.make_client_outputs(
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

        let tx_builder = TransactionBuilder::new().with_outputs(self.client_ctx.make_client_outputs(
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

    /// Burns `shares` of the local LP position keyed by `(pool, owner_pk)`,
    /// tolerating up to `max_slippage_bps` less on either side than a preview
    /// computed against the pool's current reserves.
    pub async fn withdraw(
        &self,
        pool: PoolId,
        owner_pk: PublicKey,
        shares: u64,
        max_slippage_bps: u64,
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
        let min_lo = Amount::from_msats(min_after_slippage(preview.da, max_slippage_bps)?);
        let min_hi = Amount::from_msats(min_after_slippage(preview.db, max_slippage_bps)?);

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
                | SwapState::Tx1Accepted
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
    pub async fn recover(&self) -> anyhow::Result<AmmRecoverySummary> {
        let filter = tweak_filter(&self.root_secret);

        let mut own_balances = Vec::new();
        for_each_balance_recovery_entry(
            |req| self.module_api.amm_balance_recovery_page(req),
            |entry| {
                if matches_own_key(&self.root_secret, filter, CHILD_SWAP, entry.tweak, entry.pubkey) {
                    own_balances.push(*entry);
                }
            },
        )
        .await?;

        let mut own_positions = Vec::new();
        for_each_lp_recovery_entry(
            |req| self.module_api.amm_lp_recovery_page(req),
            |entry| {
                if matches_own_key(&self.root_secret, filter, CHILD_LP, entry.tweak, entry.pubkey) {
                    own_positions.push(*entry);
                }
            },
        )
        .await?;

        // Restore LP positions first: even if a subsequent claim below
        // fails, positions found so far are not lost.
        {
            let mut dbtx = self.db.begin_transaction().await;
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
            match self
                .claim_recovered_balance(entry.tweak, entry.unit, entry.amount)
                .await
            {
                Ok(()) => balances_claimed += 1,
                Err(error) => claim_errors.push(format!("{:?}/{:?}: {error}", entry.pubkey, entry.unit)),
            }
        }

        Ok(AmmRecoverySummary {
            balances_found: own_balances.len(),
            balances_claimed,
            positions_restored: own_positions.len(),
            claim_errors,
        })
    }

    /// Claims a single balance found by [`Self::recover`]. Unlike
    /// [`swap::SwapStateMachine`]'s Tx2, this re-derives the recipient
    /// keypair from the recovered tweak rather than carrying it in state,
    /// since there is no ongoing swap operation to carry it — recovery finds
    /// balances that predate this client run entirely.
    async fn claim_recovered_balance(
        &self,
        tweak: [u8; 16],
        unit: AmountUnit,
        amount: Amount,
    ) -> anyhow::Result<()> {
        let keypair = derive_keypair(&self.root_secret, CHILD_SWAP, tweak);
        let pubkey = keypair.public_key();

        let input = AmmInput::ClaimBalanceV0 { pubkey, unit };
        let client_input = ClientInput {
            input,
            keys: vec![keypair],
            amounts: Amounts::new_custom(unit, amount),
        };

        let operation_id = OperationId::new_random();
        let tx_builder = TransactionBuilder::new().with_inputs(
            self.client_ctx
                .make_client_inputs(ClientInputBundle::new_no_sm(vec![client_input])),
        );

        let range = self
            .client_ctx
            .finalize_and_submit_transaction(
                operation_id,
                KIND.as_str(),
                move |_| AmmOperationMeta::RecoveredClaim { pubkey, unit },
                tx_builder,
            )
            .await?;

        self.client_ctx
            .transaction_updates(operation_id)
            .await
            .await_tx_accepted(range.txid())
            .await
            .map_err(|error| anyhow::anyhow!("recovered claim rejected: {error}"))
    }
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
        assert_eq!(min_after_slippage(1_000, 10_001), Err(SlippageError::OutOfRange));
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

        assert!(!matches_own_key(&root, filter, CHILD_SWAP, tweak, unrelated_pubkey));
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
        assert!(!matches_own_key(&root_a, filter_a, CHILD_SWAP, tweak_b, pubkey_b));
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

        assert!(matches_own_key(&root, filter, CHILD_SWAP, tweak, swap_pubkey));
        assert!(!matches_own_key(&root, filter, CHILD_LP, tweak, swap_pubkey));
    }
}
