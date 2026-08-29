//! Server half of the test faucet.
//!
//! **Test-only. Never deploy this module to a real federation.** It has no
//! cryptography, no DKG key material, and no blind signatures — an attacker
//! (or, honestly, anyone) can mint themselves unlimited funds via
//! [`FaucetOutput`] for free. That is the entire point: it exists solely as a
//! funding counterparty so `fedimint-amm-tests` can exercise the real `amm`
//! module against a second issuing unit in a real, consensus-driven
//! federation. `fedimint-amm-{common,server,client}` do not and must not
//! depend on this crate.
//!
//! It exists because it is *smaller* than a second `mintv2`, not because a
//! second `mintv2` is impossible — see this module's `mod.rs` for that
//! correction.
//!
//! ## Why minting from nothing doesn't crash the federation
//!
//! `fedimint-server`'s consensus engine recomputes a global balance-sheet
//! audit after **every** consensus item and asserts `net_assets >= 0`,
//! panicking every guardian if it goes negative
//! (`fedimint-server/src/consensus/engine.rs:1049-1067`, confirmed by reading
//! the pinned source — this is not a "run it and see" claim). `Audit`
//! collapses every module's items into one `i64` scalar with no per-unit
//! separation at all (`fedimint-core/src/module/audit.rs`'s `AuditItem` has
//! no `AmountUnit` field), so a naive "mint credits a balance, audit reports
//! that balance as a liability" design would make the audit go negative the
//! moment the very first bootstrap mint in a test runs, since nothing offsets
//! it — panicking the entire test federation before any AMM test could even
//! start.
//!
//! The fix, verified against `fedimint-dummy-server`'s own audit
//! (`DummyInputAuditKey`/`DummyOutputAuditKey`, `+asset`/`-liability`,
//! `fedimint-dummy-server/src/lib.rs:237-256`): keep a **permanent**,
//! never-decremented record of every amount ever minted for free
//! ([`MintedAuditKey`], one row per `OutPoint`, written only by
//! [`FaucetOutput::MintV0`], reported as a **positive** item), separate from
//! the **live**, mutable spendable balance ([`BalanceKey`], credited by
//! either [`FaucetOutput`] variant and debited by [`FaucetInput`], reported
//! as a **negative** item). Immediately after a mint of `X` via `MintV0`, the
//! two cancel: `+X` (permanent) `- X` (still fully unspent) `= 0`. Once that
//! `X` is spent via a [`FaucetInput`] to fund some other module's output
//! (e.g. an `amm` `DepositV0` leg, which records its own new liability `-X`
//! the moment the reserve grows), this module's own contribution becomes `+X`
//! (permanent, unchanged) `- 0` (balance now empty) `= +X`, exactly
//! offsetting the `-X` that appeared elsewhere.
//!
//! [`FaucetOutput::ReceiveV0`] — the ordinary receive/change output
//! `create_final_inputs_and_outputs` builds — needs none of this: it declares
//! its real amount to core (`Amounts::new_custom(faucet_unit(), amount)`), so
//! by construction it only ever lands in a transaction whose matching input
//! or other-module output already funds it (P1, spec §3.1). It writes **no**
//! [`MintedAuditKey`] row; its `+amount` liability recorded by [`BalanceKey`]
//! is exactly offset by the `-amount` (or equivalent) that the transaction's
//! funding side records elsewhere, the same way any two-sided module balances
//! under the audit. Net effect: zero, always, regardless of how much is
//! minted via `MintV0` or when it is spent, and zero for every `ReceiveV0`
//! credit too, since those are never free in the first place — the same
//! invariant spec §7.4/§9.1 describes for `amm` itself ("never compute a
//! value twice"; here, "never let a credit disappear from the books without
//! either staying live or having been consumed by a matching liability
//! elsewhere").
//!
//! This mirrors `fedimint-dummy-server` exactly in spirit for the `MintV0`
//! case — dummy's `DummyInput` is *itself* an unconditional, unchecked "mint
//! from nothing" (`process_input` never checks a balance at all, see the
//! pinned source), permanently recorded as an asset. The only structural
//! difference is which side does the free minting: dummy's input creates
//! from nothing (fine for dummy, since nothing else in that module is a
//! checked, persisted, spendable ledger); this module's spec-mandated shape
//! needs a genuinely debit-checked, persisted balance, so the free side has
//! to be an output instead (P1, spec §3.1: "anything that creates a record
//! must be an output"), with the audit design above making that safe.

pub mod db;

use std::collections::BTreeMap;

use async_trait::async_trait;
use fedimint_core::config::{
    ServerModuleConfig, ServerModuleConsensusConfig, TypedServerModuleConfig,
};
use fedimint_core::core::{ModuleInstanceId, ModuleKind};
use fedimint_core::db::{DatabaseTransaction, DatabaseVersion, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    Amounts, ApiEndpoint, CORE_CONSENSUS_VERSION, CoreConsensusVersion, InputMeta,
    ModuleConsensusVersion, ModuleInit, SupportedModuleApiVersions, TransactionItemAmounts,
};
use fedimint_core::{
    Amount, InPoint, OutPoint, PeerId, plugin_types_trait_impl_config, push_db_pair_items,
};
use fedimint_server_core::config::PeerHandleOps;
use fedimint_server_core::migration::ServerModuleDbMigrationFn;
use fedimint_server_core::{
    ConfigGenModuleArgs, ServerModule, ServerModuleInit, ServerModuleInitArgs,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;

use crate::faucet::common::{
    FaucetClientConfig, FaucetCommonInit, FaucetConsensusItem, FaucetInput, FaucetInputError,
    FaucetModuleTypes, FaucetOutput, FaucetOutputError, FaucetOutputOutcome,
    MODULE_CONSENSUS_VERSION, faucet_unit,
};
use crate::faucet::server::db::{
    BalanceKey, BalancePrefix, DbKeyPrefix, MintedAuditKey, MintedAuditPrefix,
};

/// Contains all the configuration for the server. This module holds no key
/// material — [`FaucetConfigPrivate`] is a unit struct — and no consensus
/// parameters, so `FaucetConfigConsensus` is one too.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaucetConfig {
    pub private: FaucetConfigPrivate,
    pub consensus: FaucetConfigConsensus,
}

#[derive(Clone, Debug, Serialize, Deserialize, Decodable, Encodable)]
pub struct FaucetConfigConsensus;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaucetConfigPrivate;

plugin_types_trait_impl_config!(
    FaucetCommonInit,
    FaucetConfig,
    FaucetConfigPrivate,
    FaucetConfigConsensus,
    FaucetClientConfig
);

/// Generates the module.
#[derive(Debug, Clone)]
pub struct FaucetInit;

impl ModuleInit for FaucetInit {
    type Common = FaucetCommonInit;

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
                DbKeyPrefix::Balance => {
                    push_db_pair_items!(
                        dbtx,
                        BalancePrefix,
                        BalanceKey,
                        Amount,
                        items,
                        "Faucet Balance"
                    );
                }
                DbKeyPrefix::MintedAudit => {
                    push_db_pair_items!(
                        dbtx,
                        MintedAuditPrefix,
                        MintedAuditKey,
                        Amount,
                        items,
                        "Faucet Minted Audit"
                    );
                }
            }
        }

        Box::new(items.into_iter())
    }
}

#[async_trait]
impl ServerModuleInit for FaucetInit {
    type Module = Faucet;

    type Params = ();

    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        &[MODULE_CONSENSUS_VERSION]
    }

    fn supported_api_versions(&self) -> SupportedModuleApiVersions {
        SupportedModuleApiVersions::from_raw(
            (CORE_CONSENSUS_VERSION.major, CORE_CONSENSUS_VERSION.minor),
            (
                MODULE_CONSENSUS_VERSION.major,
                MODULE_CONSENSUS_VERSION.minor,
            ),
            &[(0, 0)],
        )
    }

    async fn init(&self, args: &ServerModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        let _cfg: FaucetConfig = args.cfg().to_typed()?;
        Ok(Faucet)
    }

    /// No key material and no consensus parameters, so every peer gets the
    /// same trivial config — mirrors `fedimint-dummy-server` exactly.
    fn trusted_dealer_gen(
        &self,
        peers: &[PeerId],
        _args: &ConfigGenModuleArgs,
        _params: &Self::Params,
    ) -> BTreeMap<PeerId, ServerModuleConfig> {
        peers
            .iter()
            .map(|&peer| {
                let config = FaucetConfig {
                    private: FaucetConfigPrivate,
                    consensus: FaucetConfigConsensus,
                };
                (peer, config.to_erased())
            })
            .collect()
    }

    async fn distributed_gen(
        &self,
        _peers: &(dyn PeerHandleOps + Send + Sync),
        _args: &ConfigGenModuleArgs,
        _params: &Self::Params,
    ) -> anyhow::Result<ServerModuleConfig> {
        Ok(FaucetConfig {
            private: FaucetConfigPrivate,
            consensus: FaucetConfigConsensus,
        }
        .to_erased())
    }

    fn get_client_config(
        &self,
        _config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<FaucetClientConfig> {
        Ok(FaucetClientConfig)
    }

    fn validate_config(
        &self,
        _identity: &PeerId,
        _config: ServerModuleConfig,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_database_migrations(
        &self,
    ) -> BTreeMap<DatabaseVersion, ServerModuleDbMigrationFn<Faucet>> {
        BTreeMap::new()
    }
}

/// The test faucet's server module. Holds no state of its own — every value
/// it needs lives in the database, keyed per-instance by the framework.
#[derive(Debug)]
pub struct Faucet;

#[async_trait]
impl ServerModule for Faucet {
    type Common = FaucetModuleTypes;
    type Init = FaucetInit;

    async fn consensus_proposal(
        &self,
        _dbtx: &mut DatabaseTransaction<'_>,
    ) -> Vec<FaucetConsensusItem> {
        Vec::new()
    }

    async fn process_consensus_item<'a, 'b>(
        &'a self,
        _dbtx: &mut DatabaseTransaction<'b>,
        _consensus_item: FaucetConsensusItem,
        _peer_id: PeerId,
    ) -> anyhow::Result<()> {
        anyhow::bail!("The test faucet module does not use consensus items");
    }

    /// Debits `input.pub_key`'s stored balance, rejecting if it is
    /// insufficient. Declares real backing to core — see this file's module
    /// doc comment and [`FaucetInput`]'s doc comment for why that is safe.
    async fn process_input<'a, 'b, 'c>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'c>,
        input: &'b FaucetInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, FaucetInputError> {
        let key = BalanceKey(input.pub_key);
        let balance = dbtx.get_value(&key).await.unwrap_or(Amount::ZERO);

        let remaining = balance
            .checked_sub(input.amount)
            .ok_or(FaucetInputError::InsufficientBalance)?;

        if remaining == Amount::ZERO {
            dbtx.remove_entry(&key).await;
        } else {
            dbtx.insert_entry(&key, &remaining).await;
        }

        Ok(InputMeta {
            amount: TransactionItemAmounts {
                amounts: Amounts::new_custom(faucet_unit(), input.amount),
                fees: Amounts::ZERO,
            },
            pub_key: input.pub_key,
        })
    }

    /// Unconditionally credits `pub_key`'s stored balance by `amount` — this
    /// IS the faucet (see [`FaucetOutput`]'s doc comment). [`FaucetOutput::
    /// MintV0`] declares **no** backing to core and writes a permanent
    /// [`MintedAuditKey`] row; [`FaucetOutput::ReceiveV0`] declares real
    /// backing and writes none. See this file's module doc comment for why
    /// both stay solvent under the global audit assert.
    async fn process_output<'a, 'b>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'b>,
        output: &'a FaucetOutput,
        out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, FaucetOutputError> {
        let (amount, pub_key, declared) = match output {
            FaucetOutput::MintV0 { amount, pub_key } => (*amount, *pub_key, Amounts::ZERO),
            FaucetOutput::ReceiveV0 { amount, pub_key } => (
                *amount,
                *pub_key,
                Amounts::new_custom(faucet_unit(), *amount),
            ),
            FaucetOutput::Default { .. } => return Err(FaucetOutputError::UnknownVariant),
        };

        let key = BalanceKey(pub_key);
        let balance = dbtx.get_value(&key).await.unwrap_or(Amount::ZERO);

        let credited = balance
            .checked_add(amount)
            .ok_or(FaucetOutputError::BalanceOverflow)?;

        // Permanent record first (see module doc comment), `MintV0` only:
        // both writes must land together, but if only one could, recording
        // the permanent asset without the live balance would merely make
        // this module look solvent-and-then-some, never insolvent — the safe
        // direction to err in, though in practice `dbtx` here commits both or
        // neither. `ReceiveV0` never writes this table at all — it declares
        // real backing instead, so adding a permanent unbacked-asset row for
        // it would double-count the same value the funding side already
        // accounts for.
        if let FaucetOutput::MintV0 { .. } = output {
            dbtx.insert_entry(&MintedAuditKey(out_point), &amount).await;
        }
        dbtx.insert_entry(&key, &credited).await;

        Ok(TransactionItemAmounts {
            amounts: declared,
            fees: Amounts::ZERO,
        })
    }

    async fn output_status(
        &self,
        _dbtx: &mut DatabaseTransaction<'_>,
        _out_point: OutPoint,
    ) -> Option<FaucetOutputOutcome> {
        None
    }

    /// Spec-mirroring shape (`fedimint-amm-server::Amm::audit`,
    /// `fedimint-dummy-server::Dummy::audit`): report the permanent minted
    /// total as a positive item and the live outstanding balance as a
    /// negative one. See this file's module doc comment for why exactly
    /// these two tables, with exactly these signs, keep the global audit
    /// solvent.
    async fn audit(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        audit: &mut Audit,
        module_instance_id: ModuleInstanceId,
    ) {
        audit
            .add_items(
                dbtx,
                module_instance_id,
                &MintedAuditPrefix,
                |_, v: Amount| v.msats as i64,
            )
            .await;
        audit
            .add_items(dbtx, module_instance_id, &BalancePrefix, |_, v: Amount| {
                -(v.msats as i64)
            })
            .await;
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        Vec::new()
    }
}
