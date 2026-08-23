//! The AMM module server crate.
//!
//! Implements the `ServerModuleInit`/`ServerModule` skeleton and DKG config
//! generation (spec §11). `process_input`/`process_output` are stubs that
//! reject every variant — Tasks 7 and 8 fill in the real curve logic. `audit`
//! is empty — Task 9 implements it. `api_endpoints` is empty — Task 9 adds
//! them.

pub mod db;

use std::collections::BTreeMap;

use async_trait::async_trait;
use fedimint_amm_common::config::{
    AmmClientConfig, AmmConfig, AmmConfigConsensus, AmmConfigPrivate, UnitParams,
};
use fedimint_amm_common::types::{
    AmmConsensusItem, AmmInput, AmmInputError, AmmOutput, AmmOutputError, AmmOutputOutcome,
};
use fedimint_amm_common::{AmmCommonInit, AmmModuleTypes, MODULE_CONSENSUS_VERSION};
use fedimint_core::config::{
    ServerModuleConfig, ServerModuleConsensusConfig, TypedServerModuleConfig,
    TypedServerModuleConsensusConfig,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::{DatabaseTransaction, DatabaseVersion};
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    AmountUnit, ApiEndpoint, CoreConsensusVersion, InputMeta, ModuleConsensusVersion, ModuleInit,
    TransactionItemAmounts,
};
use fedimint_core::{Amount, InPoint, OutPoint, PeerId, push_db_pair_items};
use fedimint_server_core::config::PeerHandleOps;
use fedimint_server_core::migration::ServerModuleDbMigrationFn;
use fedimint_server_core::{
    ConfigGenModuleArgs, ServerModule, ServerModuleInit, ServerModuleInitArgs,
};
use futures::StreamExt;
use strum::IntoEnumIterator;

use crate::db::{
    BalanceEntry, BalancePrefix, DbKeyPrefix, LpPosition, LpPositionPrefix, Pool, PoolPrefix,
};

/// This module has no per-federation setup parameters exposed through
/// [`ConfigGenModuleArgs`] (which is a fixed `{ network, disable_base_fees }`
/// struct shared by every module on the pinned rev — there is no longer a
/// per-module `GenParams` hook to thread custom values through). The sensible
/// defaults below are therefore baked into the generator directly rather than
/// being a caller-supplied params type. `default_consensus_config_passes_validate`
/// (in this module's tests) stands in for the `validate()`-in-`trusted_dealer_gen`
/// check the brief asked for, since `trusted_dealer_gen`'s return type
/// (`BTreeMap<PeerId, ServerModuleConfig>`, no `Result`) gives no channel to
/// fail the ceremony without panicking, which the task's hard constraints
/// forbid. `distributed_gen` and `validate_config`, whose signatures do
/// return `anyhow::Result`, call `validate()` at runtime and fail on error.
fn default_consensus_config() -> AmmConfigConsensus {
    AmmConfigConsensus {
        units: BTreeMap::from([
            (
                AmountUnit::BITCOIN,
                UnitParams {
                    min_swap_in: Amount::from_msats(1_000),
                },
            ),
            (
                AmountUnit::new_custom(1),
                UnitParams {
                    min_swap_in: Amount::from_msats(10),
                },
            ),
        ]),
        default_fee_per_mille: 3,
        fee_overrides: BTreeMap::new(),
    }
}

/// Generates the module.
#[derive(Debug, Clone)]
pub struct AmmInit;

impl ModuleInit for AmmInit {
    type Common = AmmCommonInit;

    /// Dumps all database items for debugging.
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
                DbKeyPrefix::Pool => {
                    push_db_pair_items!(dbtx, PoolPrefix, PoolKey, Pool, items, "Amm Pool");
                }
                DbKeyPrefix::LpPosition => {
                    push_db_pair_items!(
                        dbtx,
                        LpPositionPrefix,
                        LpPositionKey,
                        LpPosition,
                        items,
                        "Amm LP Position"
                    );
                }
                DbKeyPrefix::Balance => {
                    push_db_pair_items!(
                        dbtx,
                        BalancePrefix,
                        BalanceKey,
                        BalanceEntry,
                        items,
                        "Amm Balance"
                    );
                }
            }
        }

        Box::new(items.into_iter())
    }
}

/// Implementation of server module non-consensus functions.
#[async_trait]
impl ServerModuleInit for AmmInit {
    type Module = Amm;

    /// Returns the version of this module.
    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        &[MODULE_CONSENSUS_VERSION]
    }

    /// Initialize the module.
    async fn init(&self, args: &ServerModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        let cfg: AmmConfig = args.cfg().to_typed()?;
        Ok(Amm::new(cfg.consensus))
    }

    /// Generates configs for all peers in a trusted manner for testing.
    ///
    /// There is no key material to generate — [`AmmConfigPrivate`] is a unit
    /// struct — so this is pure config projection: every peer gets the same
    /// [`default_consensus_config`].
    fn trusted_dealer_gen(
        &self,
        peers: &[PeerId],
        _args: &ConfigGenModuleArgs,
    ) -> BTreeMap<PeerId, ServerModuleConfig> {
        let consensus = default_consensus_config();

        peers
            .iter()
            .map(|&peer| {
                let config = AmmConfig {
                    private: AmmConfigPrivate,
                    consensus: consensus.clone(),
                };
                (peer, config.to_erased())
            })
            .collect()
    }

    /// Generates configs for all peers in an untrusted manner.
    ///
    /// Same as [`Self::trusted_dealer_gen`]: no key material to generate, but
    /// here the return type is fallible, so we validate the config and fail
    /// the ceremony if it is somehow malformed.
    async fn distributed_gen(
        &self,
        _peers: &(dyn PeerHandleOps + Send + Sync),
        _args: &ConfigGenModuleArgs,
    ) -> anyhow::Result<ServerModuleConfig> {
        let consensus = default_consensus_config();
        consensus
            .validate()
            .map_err(|e| anyhow::anyhow!("AMM consensus config failed validation: {e}"))?;

        Ok(AmmConfig {
            private: AmmConfigPrivate,
            consensus,
        }
        .to_erased())
    }

    /// Converts the consensus config into the client config.
    fn get_client_config(
        &self,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<AmmClientConfig> {
        let config = AmmConfigConsensus::from_erased(config)?;

        Ok(AmmClientConfig {
            units: config.units,
            default_fee_per_mille: config.default_fee_per_mille,
            fee_overrides: config.fee_overrides,
        })
    }

    fn validate_config(
        &self,
        _identity: &PeerId,
        config: ServerModuleConfig,
    ) -> anyhow::Result<()> {
        let config: AmmConfig = config.to_typed()?;
        config
            .consensus
            .validate()
            .map_err(|e| anyhow::anyhow!("AMM consensus config failed validation: {e}"))
    }

    /// DB migrations to move from old to newer versions.
    fn get_database_migrations(&self) -> BTreeMap<DatabaseVersion, ServerModuleDbMigrationFn<Amm>> {
        BTreeMap::new()
    }
}

/// AMM module.
#[derive(Debug)]
pub struct Amm {
    pub cfg: AmmConfigConsensus,
}

impl Amm {
    /// Create new module instance.
    pub fn new(cfg: AmmConfigConsensus) -> Amm {
        Amm { cfg }
    }
}

/// Implementation of consensus for the server module.
#[async_trait]
impl ServerModule for Amm {
    type Common = AmmModuleTypes;
    type Init = AmmInit;

    async fn consensus_proposal(
        &self,
        _dbtx: &mut DatabaseTransaction<'_>,
    ) -> Vec<AmmConsensusItem> {
        // This module has no consensus items (spec §10): a `Balance` is
        // always claimable and no reserves are ever earmarked.
        Vec::new()
    }

    async fn process_consensus_item<'a, 'b>(
        &'a self,
        _dbtx: &mut DatabaseTransaction<'b>,
        _consensus_item: AmmConsensusItem,
        _peer_id: PeerId,
    ) -> anyhow::Result<()> {
        // WARNING: `process_consensus_item` should return an `Err` for items
        // that do not change any internal consensus state. We never expect to
        // receive one at all (spec §10), so unconditionally erroring is
        // correct and, unlike a panic, keeps the federation running.
        anyhow::bail!("AMM module does not process consensus items")
    }

    async fn process_input<'a, 'b, 'c>(
        &'a self,
        _dbtx: &mut DatabaseTransaction<'c>,
        _input: &'b AmmInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, AmmInputError> {
        // Stub for this task: Task 7 implements `ClaimBalanceV0`/`WithdrawV0`.
        Err(AmmInputError::UnknownVariant)
    }

    async fn process_output<'a, 'b>(
        &'a self,
        _dbtx: &mut DatabaseTransaction<'b>,
        _output: &'a AmmOutput,
        _out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, AmmOutputError> {
        // Stub for this task: Task 8 implements `SwapV0`/`DepositV0`.
        Err(AmmOutputError::UnknownVariant)
    }

    async fn output_status(
        &self,
        _dbtx: &mut DatabaseTransaction<'_>,
        _out_point: OutPoint,
    ) -> Option<AmmOutputOutcome> {
        None
    }

    async fn audit(
        &self,
        _dbtx: &mut DatabaseTransaction<'_>,
        _audit: &mut Audit,
        _module_instance_id: ModuleInstanceId,
    ) {
        // Empty for this task: Task 9 reports reserves and balances.
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        // Empty for this task: Task 9 adds endpoints.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::default_consensus_config;

    /// `trusted_dealer_gen` cannot itself signal a `validate()` failure (its
    /// return type has no `Result`), so its correctness rests on this
    /// hardcoded config always being valid. See the comment on
    /// `default_consensus_config`.
    #[test]
    fn default_consensus_config_passes_validate() {
        assert_eq!(default_consensus_config().validate(), Ok(()));
    }
}
