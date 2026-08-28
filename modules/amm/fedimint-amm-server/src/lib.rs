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
    BALANCE_ENDPOINT, BALANCE_RECOVERY_ENDPOINT, BalanceRecoveryEntry, BalanceRecoveryResponse,
    BalanceRequest, LP_RECOVERY_ENDPOINT, LpRecoveryEntry, LpRecoveryResponse,
    MAX_RECOVERY_PAGE_SIZE, POOLS_ENDPOINT, PoolSummary, QUOTE_ENDPOINT, QuoteRequest,
    QuoteResponse, RecoveryPageRequest,
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
use fedimint_core::db::{
    DatabaseTransaction, DatabaseVersion, IDatabaseTransactionOpsCore,
    IDatabaseTransactionOpsCoreTyped, WithDecoders,
};
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    AmountUnit, Amounts, ApiEndpoint, ApiError, ApiVersion, CORE_CONSENSUS_VERSION,
    CoreConsensusVersion, InputMeta, ModuleConsensusVersion, ModuleInit,
    SupportedModuleApiVersions, TransactionItemAmounts, api_endpoint,
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

/// The sensible defaults below are baked into the generator rather than being
/// caller-supplied.
///
/// The pinned platform branch *does* offer a typed per-instance hook
/// ([`ServerModuleInit::Params`], alongside the federation-wide
/// `{ network, disable_base_fees }` of [`ConfigGenModuleArgs`]), so threading
/// the unit allowlist and fee schedule through config gen is now possible —
/// this module simply has not adopted it yet and declares `Params = ()`.
/// Changing that is a config-gen change, not a consensus one.
/// `default_consensus_config_passes_validate`
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

    /// The AMM takes no per-instance config-gen params — see the note on
    /// [`default_consensus_config`].
    type Params = ();

    /// Returns the version of this module.
    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        &[MODULE_CONSENSUS_VERSION]
    }

    /// Every endpoint in [`Amm::api_endpoints`] is declared at
    /// `ApiVersion::new(0, 0)`, so that is the only API version offered.
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
        _params: &Self::Params,
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
        _params: &Self::Params,
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

/// Output of [`quote_swap`]: the swap's computed output plus everything
/// resolved to compute it. `process_output`'s `SwapV0` arm and
/// `QUOTE_ENDPOINT` both need the oriented reserves (finding M6: previously
/// each re-derived `(reserve_in, reserve_out)` from `in_is_lo` independently,
/// a second copy of the same selection alongside the one inside this
/// function); `process_output` additionally needs `reserve_in_new` (finding
/// M5: previously recomputed there via a second `checked_add`, even though
/// this function had already computed and validated the identical sum on
/// the identical operands). All three are returned here instead, so there
/// is exactly one place that selects orientation and exactly one place that
/// computes `reserve_in_new` — the hand-copy-drift finding I1 was about.
struct SwapQuote {
    /// `amount_out` for this swap. Spec §7.4: this ONE binding must be used
    /// for both the reserve debit and the balance credit — never recompute
    /// it.
    dy: u64,
    /// Whether `unit_in` is `pool_id.lo()` (vs `.hi()`).
    in_is_lo: bool,
    /// The reserve `unit_in` was drawn from, before this swap.
    reserve_in: Amount,
    /// The reserve `unit_out` was drawn from, before this swap.
    reserve_out: Amount,
    /// `reserve_in.msats + amount_in.msats`, already validated `<=
    /// MAX_RESERVE`.
    reserve_in_new: u64,
}

/// Resolves orientation, applies every swap admission check, and computes
/// the output via [`math::amount_out`] — the single function shared by
/// `process_output`'s `SwapV0` arm and `QUOTE_ENDPOINT` (finding I1), so a
/// quote can never disagree with settlement, and a client trusting a quote
/// cannot build a transaction that settlement then rejects for a reason the
/// quote never checked (finding M9: `min_swap_in`, the unit allowlist, and
/// the `MAX_RESERVE` cap on `reserve_in + amount_in` are all enforced here,
/// not just in `process_output`).
///
/// `unit_out` is not a separate parameter: `pool_id` must already have been
/// constructed as `PoolId::new(unit_in, unit_out)` by the caller (both call
/// sites do this), so `unit_out` is simply the side of `pool_id` that is not
/// `unit_in`.
///
/// `min_out` is `Some` only from `process_output`'s `SwapV0` arm —
/// `QUOTE_ENDPOINT` passes `None`, since a mere quote has no slippage
/// tolerance to check against. Checking it here, immediately after `dy` is
/// known and before the `MAX_RESERVE`-on-`reserve_in_new` check below,
/// restores the error priority settlement had before finding I1 folded both
/// checks into this one function (fix pass 2, Minor 4): a swap that both
/// misses slippage and would push the reserve past its cap reports
/// `SlippageExceeded`, not `ReserveCapExceeded`.
fn quote_swap(
    cfg: &AmmConfigConsensus,
    pool: &Pool,
    pool_id: PoolId,
    unit_in: AmountUnit,
    amount_in: Amount,
    min_out: Option<Amount>,
) -> Result<SwapQuote, AmmOutputError> {
    let params_in = cfg.units.get(&unit_in).ok_or(AmmOutputError::UnknownUnit)?;
    let in_is_lo = unit_in == pool_id.lo();
    let unit_out = if in_is_lo { pool_id.hi() } else { pool_id.lo() };
    if !cfg.units.contains_key(&unit_out) {
        return Err(AmmOutputError::UnknownUnit);
    }
    if amount_in < params_in.min_swap_in {
        return Err(AmmOutputError::BelowMinSwapIn);
    }

    let (reserve_in, reserve_out) = if in_is_lo {
        (pool.reserve_lo, pool.reserve_hi)
    } else {
        (pool.reserve_hi, pool.reserve_lo)
    };

    let fee = cfg.fee_for(pool_id);
    // Computed ONCE (spec §7.4): this is the same `dy` the caller uses for
    // both the reserve debit and the balance credit (settlement) or returns
    // directly (quote) — never recomputed.
    let dy = math::amount_out(reserve_in.msats, reserve_out.msats, amount_in.msats, fee)
        .map_err(map_curve_error)?;

    if let Some(min_out) = min_out
        && dy < min_out.msats
    {
        return Err(AmmOutputError::SlippageExceeded);
    }

    let reserve_in_new = reserve_in
        .msats
        .checked_add(amount_in.msats)
        .ok_or(AmmOutputError::ReserveCapExceeded)?;
    if reserve_in_new > math::MAX_RESERVE {
        return Err(AmmOutputError::ReserveCapExceeded);
    }

    Ok(SwapQuote {
        dy,
        in_is_lo,
        reserve_in,
        reserve_out,
        reserve_in_new,
    })
}

/// Clamps a client-requested recovery page size to `[1,
/// MAX_RECOVERY_PAGE_SIZE]` (finding I2). `None` requests the maximum. The
/// client-supplied value is never trusted directly — that would just move
/// the unpaginated-dump amplification from "no limit field" to "limit field
/// the server doesn't enforce".
fn recovery_page_limit(requested: Option<u32>) -> usize {
    requested
        .unwrap_or(MAX_RECOVERY_PAGE_SIZE)
        .clamp(1, MAX_RECOVERY_PAGE_SIZE) as usize
}

/// Ceiling on a client-supplied keyset cursor's length in bytes (fix pass 2
/// hardening, prompted by switching `RecoveryPageRequest::cursor` from a
/// fixed 8-byte `u64` to an opaque `Vec<u8>`, Important 2). Every real
/// cursor this module ever hands out is a `BalanceKey` or `LpPositionKey`
/// encoding — comfortably under 100 bytes — so this is generous, not tight;
/// its only job is to stop an unauthenticated caller from supplying an
/// arbitrarily large `cursor` purely to make the server allocate and copy
/// that much memory on every request, which the old fixed-size cursor could
/// never do. Same "never trust a client-supplied size directly" reasoning as
/// [`recovery_page_limit`].
const MAX_RECOVERY_CURSOR_LEN: usize = 256;

/// Builds the half-open `[start, end)` raw byte range for one page of a
/// keyset-cursor scan over a single-table `DbKeyPrefix` byte (fix pass 2,
/// Important 2 and Important 3).
///
/// `end` is one past `db_prefix`: every real key in the table starts with
/// `db_prefix`, and any byte string starting with `db_prefix + 1` therefore
/// compares strictly greater than all of them regardless of what follows.
/// This bounds the whole table without needing a "maximum key" of the
/// table's actual key type — which, for a key containing a
/// `secp256k1::PublicKey`, cannot be constructed at all through the public
/// API without an actual valid curve point, so `find_by_range`'s typed
/// `Range<K>` (the pinned `fedimint-core`'s native range-start scan) is not
/// usable for these composite keys and `raw_find_by_range`'s untyped byte
/// range is used instead.
///
/// `start` is normally `cursor` — the exact bytes the previous page
/// returned as `next_cursor`, themselves exactly what
/// `DatabaseKeyPrefix::to_bytes` produces for a key of this table — with
/// one extra `0x00` byte appended. Appending a byte to `X` always sorts
/// strictly after `X` in byte-lexicographic order (a string that is a
/// proper prefix of another always sorts first), and there is no real key
/// strictly between the two: resuming here excludes exactly the cursor's
/// own row and nothing else, independent of whether this table's key
/// encoding is fixed- or variable-length. `None` starts from the first key
/// in the table.
///
/// BLOCKING SECURITY FIX: `cursor` is unauthenticated, attacker-controlled
/// input — it is NOT guaranteed to already lie inside `[db_prefix,
/// db_prefix + 1)`, and a prior version of this function trusted it
/// completely. `cursor = Some(vec![])` produced `start = [0x00]`, below
/// every real `DbKeyPrefix` (`Pool` 0x01, `LpPosition` 0x02, `Balance`
/// 0x03), so the scan swept in rows from a lower-prefixed table; those
/// then failed to decode as this table's key type and panicked the caller's
/// `.expect()` on a zero-byte, unauthenticated request. `start` is
/// therefore always clamped into `[prefix_start, prefix_end]` here,
/// regardless of what `cursor` contains, so the returned range can never
/// reach outside this table no matter what the client sends — the bounds
/// come from `db_prefix` (server-controlled), never from `cursor` alone.
/// Both `RECOVERY_ENDPOINT` handlers additionally reject a cursor that
/// fails to decode as this table's own key type before ever calling this
/// function, so in practice this clamp is defense in depth rather than the
/// only thing standing between a bad cursor and a cross-table scan.
fn recovery_range(db_prefix: u8, cursor: Option<&[u8]>) -> (Vec<u8>, Vec<u8>) {
    let prefix_start = vec![db_prefix];
    let prefix_end = vec![db_prefix + 1];
    let start = match cursor {
        Some(last_key_bytes) => {
            let mut candidate = last_key_bytes.to_vec();
            candidate.push(0);
            candidate.clamp(prefix_start.clone(), prefix_end.clone())
        }
        None => prefix_start,
    };
    (start, prefix_end)
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
                max_lo,
                max_hi,
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

                // Two-sided (fix pass 4, Important 3): below `min_lo`/
                // `min_hi` catches settlement worse than the client's
                // preview; above `max_lo`/`max_hi` catches settlement better
                // than the preview, which core's overpay rule (P5/P6) would
                // otherwise let through and silently forfeit, since the
                // client's declared transaction outputs are fixed at the
                // preview regardless of what actually settles here.
                if outcome.da < min_lo.msats
                    || outcome.db < min_hi.msats
                    || outcome.da > max_lo.msats
                    || outcome.db > max_hi.msats
                {
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
                let pool_id =
                    PoolId::new(*unit_in, *unit_out).ok_or(AmmOutputError::IdenticalUnits)?;
                let mut pool = dbtx
                    .get_value(&PoolKey(pool_id))
                    .await
                    .ok_or(AmmOutputError::NoSuchPool)?;

                // Resolves orientation, the admission checks (unit
                // allowlist, `min_swap_in`, `MAX_RESERVE`), and the curve
                // call itself — the exact function `QUOTE_ENDPOINT` calls
                // (finding I1), so a quote can never disagree with
                // settlement.
                let quote = quote_swap(
                    &self.cfg,
                    &pool,
                    pool_id,
                    *unit_in,
                    *amount_in,
                    Some(*min_out),
                )?;
                let dy = quote.dy;

                // Findings Minor 4/5/6: orientation, `reserve_in_new`, and
                // the `min_out` slippage check all now live in `quote_swap`
                // (see its doc comment) — reused here via `SwapQuote`
                // rather than re-selected or recomputed.
                let reserve_in_new = quote.reserve_in_new;
                // `amount_out` guarantees `dy < reserve_out`, so this never
                // underflows.
                let reserve_out_new = quote.reserve_out.msats - dy;

                if !math::k_non_decreasing(
                    quote.reserve_in.msats,
                    quote.reserve_out.msats,
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
                // an adversarial accumulation.
                //
                // Finding I4: bounded against `MAX_RESERVE`, not just
                // `u64::MAX`. `audit()`'s `.expect` on `BalanceEntry.amount`
                // asserts it never exceeds `i64::MAX`, and `MAX_RESERVE <
                // i64::MAX` is the only cap this module enforces anywhere —
                // so THIS is the check that makes that `.expect` true rather
                // than aspirational. Rejecting here (an ordinary consensus
                // error) rather than clamping is required: `audit`'s
                // `net_assets` sums every item behind its own `.expect`
                // (spec §9.2), so a saturating clamp would not avoid the
                // panic, only relocate it into the sum.
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
                // This is computed BEFORE any write below: both the
                // `checked_add` and the `MAX_RESERVE` check can still fail,
                // and nothing may hit the database until every fallible step
                // — including this one — has succeeded.
                let bkey = BalanceKey {
                    owner: *recipient_pk,
                    unit: *unit_out,
                };
                let existing = dbtx.get_value(&bkey).await;
                let (credited, stored_tweak) = match existing {
                    Some(entry) => {
                        let credited = entry
                            .amount
                            .msats
                            .checked_add(dy)
                            .ok_or(AmmOutputError::ReserveCapExceeded)?;
                        if credited > math::MAX_RESERVE {
                            return Err(AmmOutputError::ReserveCapExceeded);
                        }
                        (credited, entry.tweak)
                    }
                    // `dy < reserve_out <= MAX_RESERVE` (guaranteed by
                    // `amount_out` plus the `MAX_RESERVE` cap already
                    // enforced on every stored reserve), so a fresh record's
                    // amount is bounded without a separate check.
                    None => (dy, *tweak),
                };

                // All checks passed: now, and only now, write.
                if quote.in_is_lo {
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
    /// `Balance.amount` is bounded the same way (finding I4): `process_output`'s
    /// `SwapV0` arm rejects any accumulation that would push a stored
    /// balance's amount above `MAX_RESERVE`, returning
    /// `AmmOutputError::ReserveCapExceeded` (a consensus-level rejection, not
    /// a panic) rather than writing an out-of-range value. Before that fix
    /// this comment claimed the same guarantee while the code only checked
    /// against `u64::MAX` — the `.expect` below was consequently unjustified
    /// (true in practice, not true by construction). It is now enforced at
    /// the one place that ever writes `BalanceEntry.amount`, so both
    /// `.expect`s below rest on an equally real, code-enforced invariant, and
    /// neither is a saturating clamp: `calculate_net_assets` sums every item
    /// with `checked_add` behind its own `.expect` (spec §9.2), so a clamp
    /// here would not avoid a panic, only relocate it into the sum.
    async fn audit(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        audit: &mut Audit,
        module_instance_id: ModuleInstanceId,
    ) {
        audit
            .add_items(dbtx, module_instance_id, &PoolPrefix, |_, pool: Pool| {
                -i64::try_from(pool.reserve_lo.msats)
                    .expect("Pool.reserve_lo is bounded by MAX_RESERVE (spec §7.1)")
            })
            .await;
        audit
            .add_items(dbtx, module_instance_id, &PoolPrefix, |_, pool: Pool| {
                -i64::try_from(pool.reserve_hi.msats)
                    .expect("Pool.reserve_hi is bounded by MAX_RESERVE (spec §7.1)")
            })
            .await;
        audit
            .add_items(
                dbtx,
                module_instance_id,
                &BalancePrefix,
                |_, balance: BalanceEntry| {
                    -i64::try_from(balance.amount.msats).expect(
                        "BalanceEntry.amount is bounded by MAX_RESERVE: process_output's SwapV0 \
                         arm rejects any credit that would exceed it (finding I4)",
                    )
                },
            )
            .await;
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        vec![
            api_endpoint! {
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
            api_endpoint! {
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

                    // The exact function `process_output`'s `SwapV0` arm
                    // settles with (finding I1): orientation, the admission
                    // checks, and the curve call cannot drift apart between
                    // quote and settlement (finding M9). `min_out` is `None`:
                    // a quote has no slippage tolerance of its own to check.
                    let quote = quote_swap(&module.cfg, &pool, pool_id, unit_in, amount_in, None)
                        .map_err(|e| ApiError::bad_request(e.to_string()))?;

                    let price_impact_per_mille = math::price_impact_per_mille(
                        quote.reserve_in.msats,
                        quote.reserve_out.msats,
                        amount_in.msats,
                        quote.dy,
                    );

                    Ok(QuoteResponse {
                        amount_out: Amount::from_msats(quote.dy),
                        price_impact_per_mille,
                    })
                }
            },
            api_endpoint! {
                BALANCE_ENDPOINT,
                ApiVersion::new(0, 0),
                async |_module: &Amm, context, request: BalanceRequest| -> Option<Amount> {
                    // Point lookup (fix pass 3, Important 5): a single
                    // `get_value` on the exact key, not a paginated scan —
                    // see `BALANCE_ENDPOINT`'s doc comment for why the scan
                    // was the wrong tool for a caller that already knows
                    // `(pubkey, unit)`.
                    let db = context.db();
                    let mut dbtx = db.begin_transaction_nc().await;
                    let balance = dbtx
                        .get_value(&BalanceKey {
                            owner: request.pubkey,
                            unit: request.unit,
                        })
                        .await
                        .map(|entry| entry.amount);
                    Ok(balance)
                }
            },
            api_endpoint! {
                BALANCE_RECOVERY_ENDPOINT,
                ApiVersion::new(0, 0),
                async |_module: &Amm, context, request: RecoveryPageRequest| -> BalanceRecoveryResponse {
                    if request.cursor.as_ref().is_some_and(|c| c.len() > MAX_RECOVERY_CURSOR_LEN) {
                        return Err(ApiError::bad_request("cursor too large".to_string()));
                    }

                    let db = context.db();
                    let mut dbtx = db.begin_transaction_nc().await;
                    let limit = recovery_page_limit(request.limit);

                    // Fix pass 2, Important 2: a keyset cursor, not a row
                    // offset — resumes from the last KEY returned rather
                    // than a row count, so it stays correct under concurrent
                    // deletion. `Balance` rows are deleted routinely (every
                    // `ClaimBalanceV0`), and the previous `.skip(cursor)`
                    // counted positions: a deletion below the cursor shifted
                    // every later row left by one, so the next page silently
                    // dropped whatever row landed on the boundary.
                    // `balance_recovery_keyset_cursor_survives_deletion_below_cursor`
                    // (tests/endpoints.rs) reproduces this against the prior
                    // offset implementation and fails there; it passes here.
                    //
                    // Also genuinely bounds the scan (Important 3: the old
                    // comment here claimed this without it being true —
                    // `find_by_prefix().skip(n)` still polls and decodes,
                    // including a full secp256k1 `PublicKey` parse, every
                    // row up to the cursor on every call, so an
                    // unauthenticated caller replaying a cursor pinned at
                    // table size paid O(table size) per request; only
                    // allocation and response size were actually bounded).
                    // `raw_find_by_range` seeks directly to `start` in every
                    // store this module runs against — confirmed by reading
                    // `MemDatabase::raw_find_by_range` (`BTreeMap::range`,
                    // `fedimint-core/src/db/mem_impl.rs`) and RocksDB's
                    // (`IteratorMode::From` plus `set_iterate_range`,
                    // `fedimint-rocksdb/src/lib.rs`) at the pinned rev —
                    // neither iterates from the start of the table, so
                    // per-request work is O(limit), not O(cursor).
                    //
                    // BLOCKING SECURITY FIX: `cursor` is unauthenticated
                    // input and is validated BEFORE it is ever used as a
                    // scan boundary — a cursor that does not decode as a
                    // `BalanceKey` (garbage, an empty cursor, or one
                    // recycled from `LP_RECOVERY_ENDPOINT`) is rejected
                    // outright, rather than trusted to already lie inside
                    // this table's byte range. `recovery_range` also
                    // clamps the range server-side as defense in depth (see
                    // its doc comment), but this check is what turns a bad
                    // cursor into a clean 400 instead of a scan that could
                    // otherwise return a wrong (if not out-of-range) page.
                    let decoders = dbtx.decoders().clone();
                    if let Some(cursor) = request.cursor.as_deref() {
                        <BalanceKey as fedimint_core::db::DatabaseKey>::from_bytes(
                            cursor, &decoders,
                        )
                        .map_err(|e| {
                            ApiError::bad_request(format!("malformed recovery cursor: {e}"))
                        })?;
                    }
                    let (start, end) =
                        recovery_range(DbKeyPrefix::Balance as u8, request.cursor.as_deref());
                    // Keeps the raw `key_bytes` alongside the decoded value,
                    // rather than decoding and later re-encoding a
                    // `BalanceKey` for `next_cursor`: `key_bytes` already
                    // *is* what `to_bytes()` would produce, since it came
                    // straight off the wire from `raw_find_by_range`.
                    let raw_rows: Vec<(Vec<u8>, Vec<u8>)> = dbtx
                        .raw_find_by_range(start.as_slice()..end.as_slice())
                        .await?
                        .take(limit + 1)
                        .collect()
                        .await;

                    // Every row here came from the range above, which is
                    // now clamped to the `Balance` prefix regardless of
                    // what `cursor` was, so in the absence of a server bug
                    // every row really was written by this module as a
                    // `BalanceKey` -> `BalanceEntry` pair. A decode failure
                    // is therefore a server-side invariant violation, not
                    // something a client can trigger — but per this
                    // module's hard rule against panicking on anything
                    // reachable from a request, it still surfaces as a
                    // clean API error rather than an `.expect()` panic (a
                    // remote, unauthenticated caller reaching exactly this
                    // panic was the blocking bug this code replaces).
                    let mut rows: Vec<(Vec<u8>, BalanceKey, BalanceEntry)> =
                        Vec::with_capacity(raw_rows.len());
                    for (key_bytes, value_bytes) in raw_rows {
                        let key =
                            <BalanceKey as fedimint_core::db::DatabaseKey>::from_bytes(
                                &key_bytes, &decoders,
                            )
                            .map_err(|e| {
                                ApiError::server_error(format!(
                                    "corrupt Balance key in database: {e}"
                                ))
                            })?;
                        let value =
                            <BalanceEntry as fedimint_core::db::DatabaseValue>::from_bytes(
                                &value_bytes,
                                &decoders,
                            )
                            .map_err(|e| {
                                ApiError::server_error(format!(
                                    "corrupt Balance value in database: {e}"
                                ))
                            })?;
                        rows.push((key_bytes, key, value));
                    }

                    let has_more = rows.len() > limit;
                    if has_more {
                        rows.truncate(limit);
                    }
                    let next_cursor = has_more
                        .then(|| rows.last())
                        .flatten()
                        .map(|(key_bytes, _, _)| key_bytes.clone());

                    Ok(BalanceRecoveryResponse {
                        entries: rows
                            .into_iter()
                            .map(|(_, key, entry)| BalanceRecoveryEntry {
                                tweak: entry.tweak,
                                pubkey: key.owner,
                                unit: key.unit,
                                amount: entry.amount,
                            })
                            .collect(),
                        next_cursor,
                    })
                }
            },
            api_endpoint! {
                LP_RECOVERY_ENDPOINT,
                ApiVersion::new(0, 0),
                async |_module: &Amm, context, request: RecoveryPageRequest| -> LpRecoveryResponse {
                    if request.cursor.as_ref().is_some_and(|c| c.len() > MAX_RECOVERY_CURSOR_LEN) {
                        return Err(ApiError::bad_request("cursor too large".to_string()));
                    }

                    let db = context.db();
                    let mut dbtx = db.begin_transaction_nc().await;
                    let limit = recovery_page_limit(request.limit);

                    // See `BALANCE_RECOVERY_ENDPOINT` above — same keyset
                    // cursor, mirrored for `LpPosition` (fix pass 2,
                    // Important 2 and Important 3; blocking security fix:
                    // the cursor is validated before use and every `.expect`
                    // below is a clean API error instead, for the exact
                    // same reasons as `BALANCE_RECOVERY_ENDPOINT`).
                    let decoders = dbtx.decoders().clone();
                    if let Some(cursor) = request.cursor.as_deref() {
                        <LpPositionKey as fedimint_core::db::DatabaseKey>::from_bytes(
                            cursor, &decoders,
                        )
                        .map_err(|e| {
                            ApiError::bad_request(format!("malformed recovery cursor: {e}"))
                        })?;
                    }
                    let (start, end) =
                        recovery_range(DbKeyPrefix::LpPosition as u8, request.cursor.as_deref());
                    let raw_rows: Vec<(Vec<u8>, Vec<u8>)> = dbtx
                        .raw_find_by_range(start.as_slice()..end.as_slice())
                        .await?
                        .take(limit + 1)
                        .collect()
                        .await;

                    let mut rows: Vec<(Vec<u8>, LpPositionKey, LpPosition)> =
                        Vec::with_capacity(raw_rows.len());
                    for (key_bytes, value_bytes) in raw_rows {
                        let key =
                            <LpPositionKey as fedimint_core::db::DatabaseKey>::from_bytes(
                                &key_bytes, &decoders,
                            )
                            .map_err(|e| {
                                ApiError::server_error(format!(
                                    "corrupt LpPosition key in database: {e}"
                                ))
                            })?;
                        let value =
                            <LpPosition as fedimint_core::db::DatabaseValue>::from_bytes(
                                &value_bytes,
                                &decoders,
                            )
                            .map_err(|e| {
                                ApiError::server_error(format!(
                                    "corrupt LpPosition value in database: {e}"
                                ))
                            })?;
                        rows.push((key_bytes, key, value));
                    }

                    let has_more = rows.len() > limit;
                    if has_more {
                        rows.truncate(limit);
                    }
                    let next_cursor = has_more
                        .then(|| rows.last())
                        .flatten()
                        .map(|(key_bytes, _, _)| key_bytes.clone());

                    Ok(LpRecoveryResponse {
                        entries: rows
                            .into_iter()
                            .map(|(_, key, position)| LpRecoveryEntry {
                                tweak: position.tweak,
                                pool: key.pool,
                                pubkey: key.owner,
                                shares: position.shares,
                            })
                            .collect(),
                        next_cursor,
                    })
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
