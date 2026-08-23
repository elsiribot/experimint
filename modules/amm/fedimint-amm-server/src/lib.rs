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
use fedimint_amm_common::pool_id::PoolId;
use fedimint_amm_common::types::{
    AmmConsensusItem, AmmInput, AmmInputError, AmmOutput, AmmOutputError, AmmOutputOutcome,
};
use fedimint_amm_common::{AmmCommonInit, AmmModuleTypes, MODULE_CONSENSUS_VERSION, math};
use fedimint_core::config::{
    ServerModuleConfig, ServerModuleConsensusConfig, TypedServerModuleConfig,
    TypedServerModuleConsensusConfig,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::{DatabaseTransaction, DatabaseVersion, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    AmountUnit, Amounts, ApiEndpoint, CoreConsensusVersion, InputMeta, ModuleConsensusVersion,
    ModuleInit, TransactionItemAmounts,
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
    BalanceEntry, BalanceKey, BalancePrefix, DbKeyPrefix, LpPosition, LpPositionKey,
    LpPositionPrefix, Pool, PoolKey, PoolPrefix,
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

/// Maps a [`math::CurveError`] onto the wire-level [`AmmOutputError`]
/// taxonomy. `ReserveCapExceeded` gets its own dedicated variant — used by
/// both the swap and deposit paths — so a caller can distinguish "a reserve
/// would exceed `MAX_RESERVE`" from every other curve failure without string
/// matching on `Curve`'s payload.
fn map_curve_error(e: math::CurveError) -> AmmOutputError {
    match e {
        math::CurveError::ReserveCapExceeded => AmmOutputError::ReserveCapExceeded,
        other => AmmOutputError::Curve(other.to_string()),
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
        dbtx: &mut DatabaseTransaction<'b>,
        output: &'a AmmOutput,
        _out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, AmmOutputError> {
        match output {
            AmmOutput::SwapV0 {
                unit_in,
                unit_out,
                amount_in,
                min_out,
                recipient_pk,
                tweak,
            } => {
                if unit_in == unit_out {
                    return Err(AmmOutputError::IdenticalUnits);
                }
                let params_in = self
                    .cfg
                    .units
                    .get(unit_in)
                    .ok_or(AmmOutputError::UnknownUnit)?;
                if !self.cfg.units.contains_key(unit_out) {
                    return Err(AmmOutputError::UnknownUnit);
                }
                if *amount_in < params_in.min_swap_in {
                    return Err(AmmOutputError::BelowMinSwapIn);
                }
                let pool_id =
                    PoolId::new(*unit_in, *unit_out).ok_or(AmmOutputError::IdenticalUnits)?;
                let mut pool = dbtx
                    .get_value(&PoolKey(pool_id))
                    .await
                    .ok_or(AmmOutputError::NoSuchPool)?;

                // Orient reserves so `in` and `out` match the trader's
                // direction. `PoolId` sorts its pair, so `unit_in` may be
                // either `lo` or `hi` — determined once and used
                // consistently for both the read and the write-back below.
                let in_is_lo = *unit_in == pool_id.lo();
                let (reserve_in, reserve_out) = if in_is_lo {
                    (pool.reserve_lo, pool.reserve_hi)
                } else {
                    (pool.reserve_hi, pool.reserve_lo)
                };

                let fee = self.cfg.fee_for(pool_id);
                // Computed ONCE. This one binding is used for the reserve
                // debit AND the balance credit below — spec §7.4. Never
                // recompute it.
                let dy =
                    math::amount_out(reserve_in.msats, reserve_out.msats, amount_in.msats, fee)
                        .map_err(map_curve_error)?;

                if dy < min_out.msats {
                    return Err(AmmOutputError::SlippageExceeded);
                }

                let reserve_in_new = reserve_in
                    .msats
                    .checked_add(amount_in.msats)
                    .ok_or(AmmOutputError::ReserveCapExceeded)?;
                if reserve_in_new > math::MAX_RESERVE {
                    return Err(AmmOutputError::ReserveCapExceeded);
                }
                // `amount_out` guarantees `dy < reserve_out`, so this never
                // underflows.
                let reserve_out_new = reserve_out.msats - dy;

                if !math::k_non_decreasing(
                    reserve_in.msats,
                    reserve_out.msats,
                    reserve_in_new,
                    reserve_out_new,
                ) {
                    return Err(AmmOutputError::KInvariantViolated);
                }

                // A second SwapV0 into an existing (recipient_pk, unit_out)
                // ADDS to the balance rather than replacing it: anyone may
                // credit anyone's balance, so this must accumulate, not
                // overwrite. `checked_add` (not `saturating_add`): a
                // saturating clamp here would silently under-credit the
                // recipient while the reserve was already debited by the
                // full `dy`, breaking the §7.4 balance-sheet identity under
                // an adversarial accumulation. u64::MAX msats is far beyond
                // any single swap's `MAX_RESERVE`-bounded `dy`, so this is
                // not reachable in practice, but we still refuse to lose the
                // difference silently.
                //
                // `recipient_pk` and `tweak` are both unverified wire
                // fields — the server has no way to check that a pubkey was
                // actually derived from a tweak (that needs the client's
                // root secret). If the incoming `tweak` were written back
                // unconditionally, an attacker could credit a victim's
                // `recipient_pk` with a garbage `tweak` for the cost of one
                // `min_swap_in`, silently breaking the victim's seed-only
                // recovery (spec §8.2, §13) even though their funds remain
                // safe and spendable. So: preserve the EXISTING record's
                // tweak whenever one exists, and only take the incoming
                // tweak when creating a new record. An honest client
                // crediting the same pubkey twice necessarily supplies the
                // same tweak (the pubkey is derived from it), so this is a
                // no-op for honest use.
                //
                // This is computed BEFORE any write below: `checked_add` here
                // can still fail, and nothing may hit the database until
                // every fallible step — including this one — has succeeded.
                let bkey = BalanceKey {
                    owner: *recipient_pk,
                    unit: *unit_out,
                };
                let existing = dbtx.get_value(&bkey).await;
                let (credited, stored_tweak) = match existing {
                    Some(entry) => (
                        entry
                            .amount
                            .msats
                            .checked_add(dy)
                            .ok_or_else(|| AmmOutputError::Curve("balance overflow".to_string()))?,
                        entry.tweak,
                    ),
                    None => (dy, *tweak),
                };

                // All checks passed: now, and only now, write.
                if in_is_lo {
                    pool.reserve_lo = Amount::from_msats(reserve_in_new);
                    pool.reserve_hi = Amount::from_msats(reserve_out_new);
                } else {
                    pool.reserve_hi = Amount::from_msats(reserve_in_new);
                    pool.reserve_lo = Amount::from_msats(reserve_out_new);
                }
                dbtx.insert_entry(&PoolKey(pool_id), &pool).await;

                dbtx.insert_entry(
                    &bkey,
                    &BalanceEntry {
                        amount: Amount::from_msats(credited),
                        tweak: stored_tweak,
                    },
                )
                .await;

                Ok(TransactionItemAmounts {
                    amounts: Amounts::new_custom(*unit_in, *amount_in),
                    fees: Amounts::ZERO,
                })
            }
            AmmOutput::DepositV0 {
                pool,
                amount_lo,
                amount_hi,
                min_shares,
                owner_pk,
                tweak,
            } => {
                if !self.cfg.units.contains_key(&pool.lo())
                    || !self.cfg.units.contains_key(&pool.hi())
                {
                    return Err(AmmOutputError::UnknownUnit);
                }

                // Load or default: `total_shares == 0` on a fresh pool is
                // exactly the pool-creation branch `mint_shares` handles.
                let mut db_pool = dbtx.get_value(&PoolKey(*pool)).await.unwrap_or(Pool {
                    reserve_lo: Amount::ZERO,
                    reserve_hi: Amount::ZERO,
                    total_shares: 0,
                });

                let outcome = math::mint_shares(
                    db_pool.reserve_lo.msats,
                    db_pool.reserve_hi.msats,
                    db_pool.total_shares,
                    amount_lo.msats,
                    amount_hi.msats,
                )
                .map_err(map_curve_error)?;

                if outcome.to_owner < *min_shares {
                    return Err(AmmOutputError::SlippageExceeded);
                }

                // An `owner_pk` is expected to be freshly ground per deposit
                // (spec §8.3), so this should never collide — but `owner_pk`
                // is an attacker-controlled wire value with no freshness
                // enforced here. Accumulating shares (rather than
                // `insert_new_entry`, which would panic on a collision, or
                // overwriting, which would erase an existing depositor's
                // claim) keeps this path panic-free and loses no one's
                // funds regardless of input.
                //
                // As with the `BalanceEntry` above, `owner_pk` and `tweak`
                // are both unverified wire fields, so the incoming `tweak`
                // must NOT overwrite an existing record's tweak — only a
                // freshly-created record takes the incoming value. Otherwise
                // an attacker could grief a victim's LP position's
                // seed-only recovery the same way (spec §8.2, §13).
                //
                // This is computed BEFORE any write below: `checked_add` here
                // can still fail, and nothing may hit the database until
                // every fallible step — including this one — has succeeded.
                let lp_key = LpPositionKey {
                    pool: *pool,
                    owner: *owner_pk,
                };
                let existing = dbtx.get_value(&lp_key).await;
                let existing_shares = existing.as_ref().map_or(0, |p| p.shares);
                let shares = existing_shares
                    .checked_add(outcome.to_owner)
                    .ok_or_else(|| AmmOutputError::Curve("share overflow".to_string()))?;
                let stored_tweak = existing.map_or(*tweak, |p| p.tweak);

                // All checks passed: now, and only now, write.
                // `mint_shares` already verified reserve_{lo,hi} + amount_{lo,hi}
                // <= MAX_RESERVE, so these `checked_add`s cannot fail; the
                // `ok_or` is a non-panicking belt-and-suspenders rather than
                // a reachable error path.
                db_pool.reserve_lo = db_pool
                    .reserve_lo
                    .checked_add(*amount_lo)
                    .ok_or(AmmOutputError::ReserveCapExceeded)?;
                db_pool.reserve_hi = db_pool
                    .reserve_hi
                    .checked_add(*amount_hi)
                    .ok_or(AmmOutputError::ReserveCapExceeded)?;
                db_pool.total_shares = outcome.new_total_shares;
                dbtx.insert_entry(&PoolKey(*pool), &db_pool).await;

                dbtx.insert_entry(
                    &lp_key,
                    &LpPosition {
                        shares,
                        tweak: stored_tweak,
                    },
                )
                .await;

                let amounts = Amounts::ZERO
                    .checked_add_unit(*amount_lo, pool.lo())
                    .and_then(|a| a.checked_add_unit(*amount_hi, pool.hi()))
                    .ok_or(AmmOutputError::ReserveCapExceeded)?;

                Ok(TransactionItemAmounts {
                    amounts,
                    fees: Amounts::ZERO,
                })
            }
            AmmOutput::Default { .. } => Err(AmmOutputError::UnknownVariant),
        }
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
