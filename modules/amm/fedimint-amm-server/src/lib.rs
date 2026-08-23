//! The AMM module server crate.
//!
//! Implements the `ServerModuleInit`/`ServerModule` skeleton and DKG config
//! generation (spec §11). `process_output` (Task 7) and `process_input`
//! (Task 8) implement the real curve logic. `audit` is empty — Task 9
//! implements it. `api_endpoints` is empty — Task 9 adds them.

pub mod db;

use std::collections::BTreeMap;

use async_trait::async_trait;
use fedimint_amm_common::config::{
    AmmClientConfig, AmmConfig, AmmConfigConsensus, AmmConfigPrivate, UnitParams,
};
use fedimint_amm_common::endpoints::{
    BALANCE_RECOVERY_ENDPOINT, BalanceRecoveryEntry, LP_RECOVERY_ENDPOINT, LpRecoveryEntry,
    POOLS_ENDPOINT, PoolSummary, QUOTE_ENDPOINT, QuoteRequest, QuoteResponse,
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
    AmountUnit, Amounts, ApiEndpoint, ApiError, ApiVersion, CoreConsensusVersion, InputMeta,
    ModuleConsensusVersion, ModuleInit, TransactionItemAmounts, public_api_endpoint,
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

/// Maps a [`math::CurveError`] onto the wire-level [`AmmInputError`]
/// taxonomy, mirroring [`map_curve_error`] for the input side. `burn_shares`
/// can independently report `InsufficientShares` (its own `shares >
/// total_shares` guard) — give that its dedicated variant too, even though
/// `process_input`'s own `shares > position.shares` check is expected to
/// catch every reachable case first.
fn map_curve_error_input(e: math::CurveError) -> AmmInputError {
    match e {
        math::CurveError::InsufficientShares => AmmInputError::InsufficientShares,
        other => AmmInputError::Curve(other.to_string()),
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
        dbtx: &mut DatabaseTransaction<'c>,
        input: &'b AmmInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, AmmInputError> {
        match input {
            AmmInput::ClaimBalanceV0 { pubkey, unit } => {
                // Full claim, no amount field (spec §6.1): the record is
                // removed unconditionally and its stored amount is what we
                // report. One read (via `remove_entry`), one binding, used
                // once. A second claim for the same key finds nothing and
                // returns `NoSuchBalance` — the retry-safety property.
                let key = BalanceKey {
                    owner: *pubkey,
                    unit: *unit,
                };
                let entry = dbtx
                    .remove_entry(&key)
                    .await
                    .ok_or(AmmInputError::NoSuchBalance)?;

                Ok(InputMeta {
                    amount: TransactionItemAmounts {
                        amounts: Amounts::new_custom(*unit, entry.amount),
                        fees: Amounts::ZERO,
                    },
                    pub_key: *pubkey,
                })
            }
            AmmInput::WithdrawV0 {
                pool,
                owner_pk,
                shares,
                min_lo,
                min_hi,
            } => {
                let mut db_pool = dbtx
                    .get_value(&PoolKey(*pool))
                    .await
                    .ok_or(AmmInputError::NoSuchPool)?;

                let lp_key = LpPositionKey {
                    pool: *pool,
                    owner: *owner_pk,
                };
                let mut position = dbtx
                    .get_value(&lp_key)
                    .await
                    .ok_or(AmmInputError::NoSuchPosition)?;

                // Checked against the POSITION's shares, not the pool's
                // `total_shares` — `burn_shares` only guards the latter, and
                // this is the check spec §7.3 actually wants: you may only
                // burn what you hold.
                if *shares > position.shares {
                    return Err(AmmInputError::InsufficientShares);
                }

                // Computed ONCE. `da`/`db` are the same bindings used for
                // both the reserve debit below and the returned `amounts` —
                // spec §7.4. Never recompute them.
                let outcome = math::burn_shares(
                    db_pool.reserve_lo.msats,
                    db_pool.reserve_hi.msats,
                    db_pool.total_shares,
                    *shares,
                )
                .map_err(map_curve_error_input)?;

                if outcome.da < min_lo.msats || outcome.db < min_hi.msats {
                    return Err(AmmInputError::SlippageExceeded);
                }

                // `Amounts` never stores an explicit zero entry (P3):
                // `checked_add_unit` already skips a zero amount, so a leg
                // that floored to zero is omitted here rather than inserted
                // as zero. `pool.lo() != pool.hi()`, so neither `checked_add`
                // can overflow from summing into the same slot twice.
                let amounts = Amounts::ZERO
                    .checked_add_unit(Amount::from_msats(outcome.da), pool.lo())
                    .and_then(|a| a.checked_add_unit(Amount::from_msats(outcome.db), pool.hi()))
                    .ok_or_else(|| AmmInputError::Curve("amount overflow".to_string()))?;

                // All checks passed: now, and only now, write.
                // `burn_shares` only succeeds when `shares <= total_shares`,
                // and `da`/`db` are that same fraction of `reserve_lo`/
                // `reserve_hi`, so `da <= reserve_lo` and `db <= reserve_hi`:
                // these never underflow.
                db_pool.reserve_lo = Amount::from_msats(db_pool.reserve_lo.msats - outcome.da);
                db_pool.reserve_hi = Amount::from_msats(db_pool.reserve_hi.msats - outcome.db);
                db_pool.total_shares = outcome.new_total_shares;
                dbtx.insert_entry(&PoolKey(*pool), &db_pool).await;

                position.shares -= *shares;
                if position.shares == 0 {
                    dbtx.remove_entry(&lp_key).await;
                } else {
                    dbtx.insert_entry(&lp_key, &position).await;
                }

                Ok(InputMeta {
                    amount: TransactionItemAmounts {
                        amounts,
                        fees: Amounts::ZERO,
                    },
                    pub_key: *owner_pk,
                })
            }
            AmmInput::Default { .. } => Err(AmmInputError::UnknownVariant),
        }
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

    /// Spec §9.1. Reserves and outstanding balances are both liabilities —
    /// obligations to hand notes back to someone — so each is reported as
    /// its own negated item, one `add_items` call per quantity, so no
    /// single item mixes units. `LpPosition` is deliberately NOT reported:
    /// shares are a claim against reserves already counted above, and
    /// reporting them too would double-count the same liability.
    ///
    /// This runs after every consensus item (P7) with its result feeding an
    /// `assert!` that halts every guardian if it goes negative, so it must
    /// never panic. `Pool.reserve_lo`/`reserve_hi` are provably `<=
    /// MAX_RESERVE < i64::MAX` — Task 7 rejects any write that would push a
    /// reserve past that cap — so `i64::try_from` on them is total in
    /// practice and the `.expect` is justified by a real, code-enforced
    /// invariant.
    ///
    /// `Balance.amount`, by contrast, has no such cap: it accumulates via
    /// `checked_add` against `u64::MAX` (see the `SwapV0` comment in
    /// `process_output`), so it is only bounded by the total value that
    /// could ever move through this pair over the federation's lifetime —
    /// the same order-of-magnitude, supply-based argument that justifies
    /// `MAX_RESERVE` itself (2^58 msats is ~2.88e6 BTC), not a hard
    /// type-level guarantee. We still use the same `.expect` here, matching
    /// spec §9.1 verbatim, because there is no total conversion that is
    /// actually *safer*: `calculate_net_assets` sums every item with
    /// `checked_add` behind its own `.expect`, so silently clamping an
    /// out-of-range balance to `i64::MAX`/`i64::MIN` would not avoid a
    /// panic, only move it into the sum — precisely the failure mode this
    /// function must avoid. See the report for this task for more detail.
    async fn audit(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        audit: &mut Audit,
        module_instance_id: ModuleInstanceId,
    ) {
        audit
            .add_items(dbtx, module_instance_id, &PoolPrefix, |_, pool: Pool| {
                -i64::try_from(pool.reserve_lo.msats).expect("bounded by MAX_RESERVE (spec §7.1)")
            })
            .await;
        audit
            .add_items(dbtx, module_instance_id, &PoolPrefix, |_, pool: Pool| {
                -i64::try_from(pool.reserve_hi.msats).expect("bounded by MAX_RESERVE (spec §7.1)")
            })
            .await;
        audit
            .add_items(
                dbtx,
                module_instance_id,
                &BalancePrefix,
                |_, balance: BalanceEntry| {
                    -i64::try_from(balance.amount.msats)
                        .expect("bounded by MAX_RESERVE (spec §9.1)")
                },
            )
            .await;
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        vec![
            public_api_endpoint! {
                POOLS_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Amm, context, _params: ()| -> Vec<PoolSummary> {
                    let db = context.db();
                    let mut dbtx = db.begin_transaction_nc().await;
                    let pools: Vec<_> = dbtx.find_by_prefix(&PoolPrefix).await.collect().await;
                    Ok(pools
                        .into_iter()
                        .map(|(key, pool)| PoolSummary {
                            pool: key.0,
                            reserve_lo: pool.reserve_lo,
                            reserve_hi: pool.reserve_hi,
                            total_shares: pool.total_shares,
                            fee_per_mille: module.cfg.fee_for(key.0),
                        })
                        .collect())
                }
            },
            public_api_endpoint! {
                QUOTE_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Amm, context, request: QuoteRequest| -> QuoteResponse {
                    let QuoteRequest { unit_in, unit_out, amount_in } = request;

                    let pool_id = PoolId::new(unit_in, unit_out).ok_or_else(|| {
                        ApiError::bad_request("unit_in and unit_out must differ".to_string())
                    })?;

                    let db = context.db();
                    let mut dbtx = db.begin_transaction_nc().await;
                    let pool = dbtx
                        .get_value(&PoolKey(pool_id))
                        .await
                        .ok_or_else(|| ApiError::not_found("no such pool".to_string()))?;

                    let in_is_lo = unit_in == pool_id.lo();
                    let (reserve_in, reserve_out) = if in_is_lo {
                        (pool.reserve_lo, pool.reserve_hi)
                    } else {
                        (pool.reserve_hi, pool.reserve_lo)
                    };

                    let fee = module.cfg.fee_for(pool_id);
                    // The same function `process_output` settles with (spec
                    // §12): a quote can never disagree with settlement.
                    let amount_out = math::amount_out(
                        reserve_in.msats,
                        reserve_out.msats,
                        amount_in.msats,
                        fee,
                    )
                    .map_err(|e| ApiError::bad_request(e.to_string()))?;

                    let price_impact_per_mille = math::price_impact_per_mille(
                        reserve_in.msats,
                        reserve_out.msats,
                        amount_in.msats,
                        amount_out,
                    );

                    Ok(QuoteResponse {
                        amount_out: Amount::from_msats(amount_out),
                        price_impact_per_mille,
                    })
                }
            },
            public_api_endpoint! {
                BALANCE_RECOVERY_ENDPOINT,
                ApiVersion::new(0, 0),
                async |_module: &Amm, context, _params: ()| -> Vec<BalanceRecoveryEntry> {
                    let db = context.db();
                    let mut dbtx = db.begin_transaction_nc().await;
                    let entries: Vec<_> =
                        dbtx.find_by_prefix(&BalancePrefix).await.collect().await;
                    Ok(entries
                        .into_iter()
                        .map(|(key, entry)| BalanceRecoveryEntry {
                            tweak: entry.tweak,
                            pubkey: key.owner,
                            unit: key.unit,
                            amount: entry.amount,
                        })
                        .collect())
                }
            },
            public_api_endpoint! {
                LP_RECOVERY_ENDPOINT,
                ApiVersion::new(0, 0),
                async |_module: &Amm, context, _params: ()| -> Vec<LpRecoveryEntry> {
                    let db = context.db();
                    let mut dbtx = db.begin_transaction_nc().await;
                    let entries: Vec<_> =
                        dbtx.find_by_prefix(&LpPositionPrefix).await.collect().await;
                    Ok(entries
                        .into_iter()
                        .map(|(key, position)| LpRecoveryEntry {
                            tweak: position.tweak,
                            pool: key.pool,
                            pubkey: key.owner,
                            shares: position.shares,
                        })
                        .collect())
                }
            },
        ]
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
