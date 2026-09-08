//! The multispend module server crate.
//!
//! Implements the `ServerModuleInit`/`ServerModule` skeleton, DKG config
//! generation, the database schema (spec §3.2), `process_output` crediting an
//! account (spec §3.4), `audit` (spec §9), and the `account_balance` API
//! endpoint (spec §3.5).
//!
//! `verify_input`/`process_input` (spec §9.2) are the security-critical spend
//! path: `verify_input` (`verify_spend`) statelessly checks a t-of-n
//! signature witness against the key material carried inside the input,
//! bound to the txid; `process_input` (`process_spend`) then requires that
//! the account id names a funded record and debits it. Both halves are
//! required — see `verify_spend`'s doc comment for why the first alone is
//! forgeable. `process_input` returns `InputAuth::SelfVerified`: core
//! performs no authorization check of its own for this input.
//!
//! `consensus_proposal` returns `vec![]` and `process_consensus_item` is
//! unreachable: `MultispendConsensusItem` is uninhabited (spec — this module
//! contributes nothing to consensus beyond its transaction items).

pub mod db;

use std::collections::BTreeMap;

use async_trait::async_trait;
use fedimint_core::config::{
    ServerModuleConfig, ServerModuleConsensusConfig, TypedServerModuleConfig,
    TypedServerModuleConsensusConfig,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::{DatabaseTransaction, DatabaseVersion, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::encoding::Encodable;
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    Amounts, ApiEndpoint, ApiVersion, CORE_CONSENSUS_VERSION, CoreConsensusVersion, InputAuth,
    InputAuthCtx, InputMeta, ModuleConsensusVersion, ModuleInit, SupportedModuleApiVersions,
    TransactionItemAmounts, api_endpoint,
};
use fedimint_core::{InPoint, OutPoint, PeerId};
use fedimint_multispend_common::account::AccountId;
use fedimint_multispend_common::config::{
    MultispendClientConfig, MultispendConfig, MultispendConfigConsensus, MultispendConfigPrivate,
};
use fedimint_multispend_common::endpoints::{ACCOUNT_BALANCE_ENDPOINT, AccountBalance};
use fedimint_multispend_common::types::{
    MultispendConsensusItem, MultispendInput, MultispendInputError, MultispendOutput,
    MultispendOutputError, MultispendOutputOutcome, SpendWitness, amounts_are_well_formed,
};
use fedimint_multispend_common::{
    MODULE_CONSENSUS_VERSION, MultispendCommonInit, MultispendModuleTypes,
};
use fedimint_server_core::config::PeerHandleOps;
use fedimint_server_core::migration::ServerModuleDbMigrationFn;
use fedimint_server_core::{
    ConfigGenModuleArgs, ServerModule, ServerModuleInit, ServerModuleInitArgs,
};
use futures::StreamExt;
use strum::IntoEnumIterator;

use crate::db::{AccountKey, AccountPrefix, AccountState, DbKeyPrefix};

/// Generates the module.
#[derive(Debug, Clone)]
pub struct MultispendInit;

impl ModuleInit for MultispendInit {
    type Common = MultispendCommonInit;

    /// Dumps all database items for debugging.
    ///
    /// Dumps the `AccountBalance` view rather than the raw `AccountState`
    /// record: `AccountState` deliberately has no `Serialize` impl (it holds
    /// an `Amounts`, which has none at this rev — see `db::AccountState`'s
    /// doc comment), so it cannot go through `push_db_pair_items!`, which
    /// requires `Serialize` on the value type. `AccountBalance` is the
    /// serde-able view that already exists for the API endpoint, so this
    /// reuses it rather than inventing a second conversion.
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
                DbKeyPrefix::Account => {
                    let account_balances: BTreeMap<String, AccountBalance> = dbtx
                        .find_by_prefix(&AccountPrefix)
                        .await
                        .map(|(key, state): (AccountKey, AccountState)| {
                            (
                                key.consensus_encode_to_hex(),
                                AccountBalance {
                                    balance: state.balance.iter().map(|(u, a)| (*u, *a)).collect(),
                                    op_counter: state.op_counter,
                                },
                            )
                        })
                        .collect()
                        .await;
                    items.insert("Multispend Account".to_string(), Box::new(account_balances));
                }
            }
        }

        Box::new(items.into_iter())
    }
}

/// Implementation of server module non-consensus functions.
#[async_trait]
impl ServerModuleInit for MultispendInit {
    type Module = Multispend;

    /// The module has no per-instance config-gen params: it holds no key
    /// material and no tunable consensus parameters (the account key set is
    /// never stored — see `MultispendConfig`'s doc comment).
    type Params = ();

    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        &[MODULE_CONSENSUS_VERSION]
    }

    /// The only endpoint, `ACCOUNT_BALANCE_ENDPOINT`, is declared at
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

    async fn init(&self, _args: &ServerModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        Ok(Multispend)
    }

    /// There is no key material to generate — [`MultispendConfigPrivate`] is
    /// a unit struct, and [`MultispendConfigConsensus`] carries no tunable
    /// parameters — so every peer simply gets the same empty config.
    fn trusted_dealer_gen(
        &self,
        peers: &[PeerId],
        _args: &ConfigGenModuleArgs,
        _params: &Self::Params,
    ) -> BTreeMap<PeerId, ServerModuleConfig> {
        peers
            .iter()
            .map(|&peer| {
                let config = MultispendConfig {
                    private: MultispendConfigPrivate,
                    consensus: MultispendConfigConsensus,
                };
                (peer, config.to_erased())
            })
            .collect()
    }

    /// Same as [`Self::trusted_dealer_gen`]: no key material to generate.
    async fn distributed_gen(
        &self,
        _peers: &(dyn PeerHandleOps + Send + Sync),
        _args: &ConfigGenModuleArgs,
        _params: &Self::Params,
    ) -> anyhow::Result<ServerModuleConfig> {
        Ok(MultispendConfig {
            private: MultispendConfigPrivate,
            consensus: MultispendConfigConsensus,
        }
        .to_erased())
    }

    fn get_client_config(
        &self,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<MultispendClientConfig> {
        MultispendConfigConsensus::from_erased(config)?;
        Ok(MultispendClientConfig)
    }

    fn validate_config(
        &self,
        _identity: &PeerId,
        config: ServerModuleConfig,
    ) -> anyhow::Result<()> {
        let _config: MultispendConfig = config.to_typed()?;
        Ok(())
    }

    /// No prior version of this module has ever shipped, so there is nothing
    /// to migrate from yet.
    fn get_database_migrations(
        &self,
    ) -> BTreeMap<DatabaseVersion, ServerModuleDbMigrationFn<Multispend>> {
        BTreeMap::new()
    }
}

/// Add to an account's balance, creating its record if this is the first
/// credit, and count the operation.
///
/// Shared by `Multispend::process_output` and (per spec) Task 5's spend path,
/// which also credits — a spend moves value out of one account and, in the
/// general case, into others.
///
/// Has no guard of its own: `amounts` is trusted as-is, so every caller must
/// have already checked it with `amounts_are_well_formed` (or otherwise
/// guaranteed it has no zero-amount entries) before calling this.
pub(crate) async fn credit_account(
    dbtx: &mut DatabaseTransaction<'_>,
    account: AccountId,
    amounts: &Amounts,
) {
    let mut state = dbtx
        .get_value(&AccountKey(account))
        .await
        .unwrap_or(AccountState {
            balance: Amounts::ZERO,
            op_counter: 0,
        });

    state.balance = state
        .balance
        .checked_add(amounts)
        .expect("an account balance cannot overflow: total issuance is bounded");
    state.op_counter += 1;

    dbtx.insert_entry(&AccountKey(account), &state).await;
}

/// Half one of the obligation in spec §9.2: verify the witness against the
/// key material carried inside the input, bound to the txid.
///
/// Stateless by necessity — `verify_input` has no database access — which is
/// exactly why this alone is NOT sufficient. An attacker can supply their own
/// `Account` and their own signatures and pass this check. `process_input`
/// closes it by requiring the account id to name a funded record.
pub(crate) fn verify_spend(
    input: &MultispendInput,
    ctx: &InputAuthCtx<'_>,
) -> Result<(), MultispendInputError> {
    let MultispendInput::Spend { account, amounts } = input;

    // `Amounts` derives `Decodable` with no invariant enforcement, so a
    // crafted input can carry explicit zero-amount entries no safe
    // constructor would produce. `checked_add_mut` asserts per entry, and
    // `process_input` hands these straight back to core's `FundingVerifier`,
    // which calls it — so an unvalidated zero entry panics a debug node and
    // corrupts a release one. Task 4 added this helper for the same reason on
    // the output side; reject here, statelessly, before anything else.
    if !amounts_are_well_formed(amounts) {
        return Err(MultispendInputError::MalformedAmounts);
    }

    let witness = SpendWitness::decode(ctx.witness())?;

    let provided = witness.signatures.len() as u64;
    if provided < account.threshold() {
        return Err(MultispendInputError::ThresholdNotMet {
            provided,
            threshold: account.threshold(),
        });
    }

    for (&idx, sig) in &witness.signatures {
        let pub_key = account
            .pub_key_at(idx)
            .ok_or(MultispendInputError::SignatureIndexOutOfRange(idx))?;

        // `verify_schnorr` binds to the txid and cannot be pointed at another
        // message. Do not replace it with a hand-rolled check.
        if !ctx.verify_schnorr(pub_key, sig) {
            return Err(MultispendInputError::InvalidSignature(idx));
        }
    }

    Ok(())
}

/// Half two: the account must be one the module actually holds funds for.
pub(crate) async fn process_spend(
    dbtx: &mut DatabaseTransaction<'_>,
    input: &MultispendInput,
) -> Result<Amounts, MultispendInputError> {
    let MultispendInput::Spend { account, amounts } = input;

    let mut state = dbtx
        .get_value(&AccountKey(account.id()))
        .await
        .ok_or(MultispendInputError::UnknownAccount)?;

    state.balance = state
        .balance
        .checked_sub(amounts)
        .ok_or(MultispendInputError::InsufficientBalance)?;
    state.op_counter += 1;

    dbtx.insert_entry(&AccountKey(account.id()), &state).await;

    Ok(amounts.clone())
}

/// Multispend module.
#[derive(Debug)]
pub struct Multispend;

#[async_trait]
impl ServerModule for Multispend {
    type Common = MultispendModuleTypes;
    type Init = MultispendInit;

    /// This module contributes nothing to consensus beyond its transaction
    /// items (spec — see this crate's top-level doc comment).
    async fn consensus_proposal(
        &self,
        _dbtx: &mut DatabaseTransaction<'_>,
    ) -> Vec<MultispendConsensusItem> {
        vec![]
    }

    /// Unreachable: [`MultispendConsensusItem`] is uninhabited, so no value
    /// of this type can ever be constructed to call this with.
    async fn process_consensus_item<'a, 'b>(
        &'a self,
        _dbtx: &mut DatabaseTransaction<'b>,
        consensus_item: MultispendConsensusItem,
        _peer_id: PeerId,
    ) -> anyhow::Result<()> {
        match consensus_item {}
    }

    fn verify_input(
        &self,
        input: &MultispendInput,
        ctx: &InputAuthCtx<'_>,
    ) -> Result<(), MultispendInputError> {
        verify_spend(input, ctx)
    }

    async fn process_input<'a, 'b, 'c>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'c>,
        input: &'b MultispendInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, MultispendInputError> {
        let amounts = process_spend(dbtx, input).await?;

        Ok(InputMeta {
            amount: TransactionItemAmounts {
                amounts,
                fees: Amounts::ZERO,
            },
            // Authorization was checked in `verify_input`, bound to the txid.
            auth: InputAuth::SelfVerified,
        })
    }

    async fn process_output<'a, 'b>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'b>,
        output: &'a MultispendOutput,
        _out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, MultispendOutputError> {
        let MultispendOutput::Credit { account, amounts } = output;

        // A zero (or all-zero, or partially-zero) credit must be rejected
        // before it reaches `credit_account`. `amounts.is_empty()` alone is
        // NOT sufficient: that only holds for `Amounts` values built through
        // its safe constructors (`new_bitcoin`, `new_custom`,
        // `checked_add_mut`), which filter zero entries, but this `amounts`
        // is decoded off the wire, and `Amounts`'s derived `Decodable`
        // enforces no such invariant — a hand-crafted, consensus-encoded
        // credit can carry an explicit zero-amount entry for one unit
        // alongside a genuine positive amount for another, which is neither
        // empty nor all-zero. `Amounts::checked_add_mut` asserts per entry,
        // not on the total, so a mixed credit like that would panic a debug
        // node and persist a spurious zero entry on a release one.
        // `amounts_are_well_formed` checks both the empty/all-zero and the
        // mixed case in one expression.
        if !amounts_are_well_formed(amounts) {
            return Err(MultispendOutputError::ZeroCredit);
        }

        credit_account(dbtx, *account, amounts).await;

        Ok(TransactionItemAmounts {
            amounts: amounts.clone(),
            fees: Amounts::ZERO,
        })
    }

    #[allow(deprecated)]
    async fn output_status(
        &self,
        _dbtx: &mut DatabaseTransaction<'_>,
        _out_point: OutPoint,
    ) -> Option<MultispendOutputOutcome> {
        None
    }

    /// Spec §9: every account balance, per unit, negated — the module owes
    /// those funds back to whoever can produce a valid spend against the
    /// account. Mirrors the AMM's `audit`, which reports its own balance
    /// tables the same way.
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
                &AccountPrefix,
                |_, state: AccountState| {
                    state
                        .balance
                        .values()
                        .map(|amount| {
                            -i64::try_from(amount.msats).expect(
                                "an account balance cannot overflow: total issuance is bounded",
                            )
                        })
                        .sum()
                },
            )
            .await;
    }

    /// The module's only endpoint (spec §3.5): a read-only balance lookup by
    /// `AccountId`. Reads are deliberately unauthenticated — an `AccountId`
    /// is a hash, and the balance of an account whose id you already know is
    /// visible in consensus transactions anyway.
    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        vec![api_endpoint! {
            ACCOUNT_BALANCE_ENDPOINT,
            ApiVersion::new(0, 0),
            async |_module: &Multispend, context, account: AccountId| -> Option<AccountBalance> {
                let db = context.db();
                let mut dbtx = db.begin_transaction_nc().await;
                let balance = dbtx
                    .get_value(&AccountKey(account))
                    .await
                    .map(|state| AccountBalance {
                        balance: state.balance.iter().map(|(u, a)| (*u, *a)).collect(),
                        op_counter: state.op_counter,
                    });
                Ok(balance)
            }
        }]
    }
}

impl Multispend {
    /// Test-only wrapper around [`verify_spend`] so it can be exercised
    /// without a configured `Multispend` instance. Calls the exact same
    /// logic `ServerModule::verify_input` calls; there is no second
    /// implementation to keep in sync.
    #[cfg(test)]
    pub(crate) fn test_verify_input(
        input: &MultispendInput,
        ctx: &InputAuthCtx<'_>,
    ) -> Result<(), MultispendInputError> {
        verify_spend(input, ctx)
    }

    /// Test-only wrapper around [`process_spend`]. See
    /// [`Self::test_verify_input`]'s doc comment.
    #[cfg(test)]
    pub(crate) async fn test_process_input(
        dbtx: &mut DatabaseTransaction<'_>,
        input: &MultispendInput,
    ) -> Result<Amounts, MultispendInputError> {
        process_spend(dbtx, input).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use fedimint_core::Amount;
    use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::module::registry::ModuleDecoderRegistry;
    use fedimint_core::module::{
        AmountUnit, Amounts, ApiEndpointContext, ApiRequestErased, InputAuth, InputAuthCtx,
        serde_json,
    };
    use fedimint_core::secp256k1::rand::rngs::OsRng;
    use fedimint_core::secp256k1::{Keypair, Secp256k1};
    use fedimint_core::{BitcoinHash, InPoint, OutPoint, TransactionId};
    use fedimint_multispend_common::account::{Account, AccountId, AccountUnchecked};
    use fedimint_multispend_common::endpoints::{ACCOUNT_BALANCE_ENDPOINT, AccountBalance};
    use fedimint_multispend_common::types::{
        MultispendInput, MultispendInputError, MultispendOutput, MultispendOutputError,
        SpendWitness,
    };
    use fedimint_server_core::ServerModule as _;

    use crate::Multispend;
    use crate::db::AccountKey;

    fn account(n: usize, threshold: u64) -> Account {
        let secp = Secp256k1::new();
        let pub_keys = (0..n)
            .map(|_| Keypair::new(&secp, &mut OsRng).public_key())
            .collect();
        Account::try_from(AccountUnchecked {
            pub_keys,
            threshold,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn credit_creates_then_accumulates() {
        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let account = account(3, 2);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(1000),
        )
        .await;
        dbtx.commit_tx().await;

        let mut dbtx = db.begin_transaction().await;
        let state = dbtx
            .get_value(&AccountKey(account.id()))
            .await
            .expect("created");
        assert_eq!(
            state.balance.get(&AmountUnit::BITCOIN),
            Some(&Amount::from_msats(1000))
        );
        assert_eq!(
            state.op_counter, 1,
            "the creating credit counts as an operation"
        );

        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(500),
        )
        .await;
        let state = dbtx
            .get_value(&AccountKey(account.id()))
            .await
            .expect("still there");
        assert_eq!(
            state.balance.get(&AmountUnit::BITCOIN),
            Some(&Amount::from_msats(1500))
        );
        assert_eq!(state.op_counter, 2);
    }

    #[tokio::test]
    async fn credit_keeps_units_separate() {
        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let account = account(2, 1);
        let other = AmountUnit::new_custom(1);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(1000),
        )
        .await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_custom(other, Amount::from_msats(7)),
        )
        .await;

        let state = dbtx.get_value(&AccountKey(account.id())).await.unwrap();
        assert_eq!(
            state.balance.get(&AmountUnit::BITCOIN),
            Some(&Amount::from_msats(1000))
        );
        assert_eq!(state.balance.get(&other), Some(&Amount::from_msats(7)));
    }

    /// Calls `ACCOUNT_BALANCE_ENDPOINT`'s handler directly against an
    /// isolated in-memory database — `ApiEndpoint::handler` is public
    /// exactly to make this possible without standing up a federation.
    async fn account_balance(db: &Database, account: AccountId) -> Option<AccountBalance> {
        let module = Multispend;
        let endpoint = module
            .api_endpoints()
            .into_iter()
            .find(|e| e.path == ACCOUNT_BALANCE_ENDPOINT)
            .expect("account_balance is registered");

        let context = ApiEndpointContext::new(db.clone(), false, None);
        let response = (endpoint.handler)(&module, context, ApiRequestErased::new(account))
            .await
            .expect("request succeeds");
        serde_json::from_value(response).expect("response decodes as Option<AccountBalance>")
    }

    #[tokio::test]
    async fn account_balance_endpoint_distinguishes_unfunded_from_zero() {
        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let account = account(2, 1);

        assert_eq!(
            account_balance(&db, account.id()).await,
            None,
            "an account that has never been credited must not read as a zero balance"
        );

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(1000),
        )
        .await;
        dbtx.commit_tx().await;

        assert_eq!(
            account_balance(&db, account.id()).await,
            Some(AccountBalance {
                balance: BTreeMap::from([(AmountUnit::BITCOIN, Amount::from_msats(1000))]),
                op_counter: 1,
            })
        );
    }

    /// `Amounts` derives `Decodable` on its wrapped `BTreeMap` with no custom
    /// impl, so nothing on the wire stops a hand-crafted zero-amount entry
    /// from surviving decode, unlike `new_custom`/`checked_add_mut`, which
    /// filter/assert against ever inserting one. Build the adversarial value
    /// the way it would actually arrive: consensus-encode a raw
    /// `BTreeMap<AmountUnit, Amount>` containing an explicit zero entry, then
    /// decode it back as `Amounts` — this is exactly what happens to a
    /// transaction that comes in over the wire, and it must not be
    /// mistakeable for `Amounts::ZERO`'s empty map.
    #[tokio::test]
    async fn wire_decoded_zero_credit_is_rejected() {
        let modules = ModuleDecoderRegistry::default();
        let mut raw = BTreeMap::new();
        raw.insert(AmountUnit::new_custom(9), Amount::ZERO);
        let bytes = raw.consensus_encode_to_vec();
        let amounts = Amounts::consensus_decode_whole(&bytes, &modules)
            .expect("a BTreeMap<AmountUnit, Amount> we just encoded decodes as Amounts");
        assert_eq!(
            amounts.len(),
            1,
            "the explicit zero entry must survive decoding, or this test isn't reproducing \
             the wire shape the bug depends on"
        );

        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let account = account(2, 1);
        let module = Multispend;
        let output = MultispendOutput::Credit {
            account: account.id(),
            amounts,
        };
        let out_point = OutPoint {
            txid: TransactionId::all_zeros(),
            out_idx: 0,
        };

        let mut dbtx = db.begin_transaction().await;
        let result = module
            .process_output(&mut dbtx.to_ref_nc(), &output, out_point)
            .await;
        assert_eq!(result, Err(MultispendOutputError::ZeroCredit));
        dbtx.commit_tx().await;

        assert_eq!(
            account_balance(&db, account.id()).await,
            None,
            "a rejected zero credit must not create an account record"
        );
    }

    /// The `.all(|amount| *amount == Amount::ZERO)` guard this replaces only
    /// catches an all-zero (or empty) `amounts`. A *mixed* credit — a
    /// genuine positive amount for one unit alongside an explicit
    /// zero-amount entry for another — sails right through it, because
    /// `.all(zero)` is false as soon as any entry is non-zero. That mixed
    /// value still isn't safe: `Amounts::checked_add_mut` asserts per entry,
    /// not on the total, so the zero entry panics a debug node and persists
    /// a spurious zero-valued entry into `AccountState.balance` on a release
    /// one. Build it the way it would actually arrive on the wire — same
    /// technique as `wire_decoded_zero_credit_is_rejected` above — since
    /// `Amounts::new_custom`/`new_bitcoin` both filter zero entries and so
    /// cannot construct this shape directly.
    #[tokio::test]
    async fn wire_decoded_mixed_zero_credit_is_rejected() {
        let modules = ModuleDecoderRegistry::default();
        let mut raw = BTreeMap::new();
        raw.insert(AmountUnit::BITCOIN, Amount::from_msats(5));
        raw.insert(AmountUnit::new_custom(9), Amount::ZERO);
        let bytes = raw.consensus_encode_to_vec();
        let amounts = Amounts::consensus_decode_whole(&bytes, &modules)
            .expect("a BTreeMap<AmountUnit, Amount> we just encoded decodes as Amounts");
        assert_eq!(
            amounts.len(),
            2,
            "both the genuine and the explicit zero entry must survive decoding, or this \
             test isn't reproducing the wire shape the bug depends on"
        );

        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let account = account(2, 1);
        let module = Multispend;
        let output = MultispendOutput::Credit {
            account: account.id(),
            amounts,
        };
        let out_point = OutPoint {
            txid: TransactionId::all_zeros(),
            out_idx: 0,
        };

        let mut dbtx = db.begin_transaction().await;
        let result = module
            .process_output(&mut dbtx.to_ref_nc(), &output, out_point)
            .await;
        assert_eq!(result, Err(MultispendOutputError::ZeroCredit));
        dbtx.commit_tx().await;

        assert_eq!(
            account_balance(&db, account.id()).await,
            None,
            "a rejected mixed credit must not create an account record"
        );
    }

    /// An account plus the keypairs behind it, in the same ascending order
    /// the account's indices use.
    fn account_with_keys(n: usize, threshold: u64) -> (Account, Vec<Keypair>) {
        let secp = Secp256k1::new();
        let mut kps: Vec<Keypair> = (0..n).map(|_| Keypair::new(&secp, &mut OsRng)).collect();
        kps.sort_by_key(|kp| kp.public_key());

        let pub_keys = kps.iter().map(|kp| kp.public_key()).collect();
        let account = Account::try_from(AccountUnchecked {
            pub_keys,
            threshold,
        })
        .unwrap();
        (account, kps)
    }

    fn sign_at(ctx: &InputAuthCtx<'_>, kps: &[Keypair], idxs: &[u16]) -> Vec<u8> {
        let secp = Secp256k1::new();
        let signatures = idxs
            .iter()
            .map(|&i| {
                (
                    i,
                    secp.sign_schnorr(&ctx.txid_message(), &kps[usize::from(i)]),
                )
            })
            .collect::<BTreeMap<_, _>>();
        SpendWitness { signatures }.encode_to_vec()
    }

    const TXID: [u8; 32] = [0x33; 32];

    fn ctx_for(witness: &[u8]) -> InputAuthCtx<'_> {
        InputAuthCtx::new(TransactionId::from_byte_array(TXID), 0, witness)
    }

    fn spend(account: &Account) -> MultispendInput {
        MultispendInput::Spend {
            account: account.clone(),
            amounts: Amounts::new_bitcoin_msats(500),
        }
    }

    #[test]
    fn accepts_exactly_threshold_signatures() {
        let (account, kps) = account_with_keys(3, 2);
        let witness = sign_at(&ctx_for(&[]), &kps, &[0, 2]);

        Multispend::test_verify_input(&spend(&account), &ctx_for(&witness))
            .expect("2 of 3 is enough");
    }

    #[test]
    fn accepts_more_than_threshold_signatures() {
        let (account, kps) = account_with_keys(3, 2);
        let witness = sign_at(&ctx_for(&[]), &kps, &[0, 1, 2]);

        Multispend::test_verify_input(&spend(&account), &ctx_for(&witness))
            .expect("any t of n, and more than t is still fine");
    }

    #[test]
    fn rejects_below_threshold() {
        let (account, kps) = account_with_keys(3, 2);
        let witness = sign_at(&ctx_for(&[]), &kps, &[1]);

        assert_eq!(
            Multispend::test_verify_input(&spend(&account), &ctx_for(&witness)),
            Err(MultispendInputError::ThresholdNotMet {
                provided: 1,
                threshold: 2
            })
        );
    }

    #[test]
    fn rejects_a_signature_over_a_different_txid() {
        let (account, kps) = account_with_keys(3, 2);

        // Sign a different transaction entirely.
        let other = InputAuthCtx::new(TransactionId::from_byte_array([0x44; 32]), 0, &[]);
        let witness = sign_at(&other, &kps, &[0, 1]);

        assert_eq!(
            Multispend::test_verify_input(&spend(&account), &ctx_for(&witness)),
            Err(MultispendInputError::InvalidSignature(0)),
            "this is the detach-and-reattach attack; it must not verify"
        );
    }

    #[test]
    fn rejects_a_signature_by_a_non_member() {
        let (account, _) = account_with_keys(3, 2);
        let (_, outsiders) = account_with_keys(3, 2);
        let witness = sign_at(&ctx_for(&[]), &outsiders, &[0, 1]);

        assert_eq!(
            Multispend::test_verify_input(&spend(&account), &ctx_for(&witness)),
            Err(MultispendInputError::InvalidSignature(0)),
            "an outsider's signature must be rejected as invalid at the index it was \
             supplied for, not merely rejected for some reason or other -- `is_err()` alone \
             would also pass for a decode or threshold failure"
        );
    }

    #[test]
    fn rejects_an_out_of_range_index() {
        let (account, kps) = account_with_keys(3, 2);
        let secp = Secp256k1::new();
        let sig = secp.sign_schnorr(&ctx_for(&[]).txid_message(), &kps[0]);

        let witness = SpendWitness {
            signatures: BTreeMap::from([(0u16, sig), (9u16, sig)]),
        }
        .encode_to_vec();

        assert_eq!(
            Multispend::test_verify_input(&spend(&account), &ctx_for(&witness)),
            Err(MultispendInputError::SignatureIndexOutOfRange(9))
        );
    }

    #[test]
    fn rejects_a_malformed_witness() {
        let (account, _) = account_with_keys(3, 2);
        assert_eq!(
            Multispend::test_verify_input(&spend(&account), &ctx_for(&[0xde, 0xad])),
            Err(MultispendInputError::MalformedWitness)
        );
    }

    /// `Amounts` derives `Decodable` with no invariant enforcement, so a
    /// crafted `MultispendInput::Spend` can carry an explicit zero-amount
    /// entry that no safe constructor (`new_bitcoin`, `new_custom`) would
    /// ever produce -- `Amounts::new_custom` filters zeros, so building the
    /// value that way would prove nothing. Build it the way it would
    /// actually arrive on the wire: consensus-encode a raw
    /// `BTreeMap<AmountUnit, Amount>` with an explicit zero entry, then
    /// decode it back as `Amounts`.
    #[test]
    fn rejects_malformed_amounts() {
        let (account, _) = account_with_keys(3, 2);

        let modules = ModuleDecoderRegistry::default();
        let mut raw = BTreeMap::new();
        raw.insert(AmountUnit::BITCOIN, Amount::from_msats(5));
        raw.insert(AmountUnit::new_custom(1), Amount::ZERO);
        let bytes = raw.consensus_encode_to_vec();
        let amounts = Amounts::consensus_decode_whole(&bytes, &modules)
            .expect("a BTreeMap<AmountUnit, Amount> we just encoded decodes as Amounts");
        assert_eq!(
            amounts.len(),
            2,
            "both the genuine and the explicit zero entry must survive decoding, or this \
             test isn't reproducing the wire shape the bug depends on"
        );

        let input = MultispendInput::Spend {
            account: account.clone(),
            amounts,
        };

        assert_eq!(
            Multispend::test_verify_input(&input, &ctx_for(&[])),
            Err(MultispendInputError::MalformedAmounts)
        );
    }

    #[tokio::test]
    async fn process_input_rejects_a_forged_account() {
        // The attack the two-sided obligation exists to stop: an attacker
        // builds their OWN account, signs with their OWN keys -- so
        // verify_input passes -- and tries to spend. process_input must
        // refuse, because that account id has no funded record.
        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let (funded, _) = account_with_keys(3, 2);
        let (forged, forged_kps) = account_with_keys(1, 1);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            funded.id(),
            &Amounts::new_bitcoin_msats(10_000),
        )
        .await;

        let witness = sign_at(&ctx_for(&[]), &forged_kps, &[0]);
        Multispend::test_verify_input(&spend(&forged), &ctx_for(&witness))
            .expect("the attacker's own signatures verify against their own account");

        let err = Multispend::test_process_input(&mut dbtx.to_ref_nc(), &spend(&forged))
            .await
            .expect_err("a self-declared account must not be spendable");
        assert_eq!(err, MultispendInputError::UnknownAccount);

        // A mutant that materializes a default record for an unknown account
        // (rather than genuinely refusing it) would still be caught here:
        // `checked_sub` on that phantom zero balance would independently
        // return `InsufficientBalance` for the 500-msat spend, so the
        // error-variant assertion above cannot by itself distinguish "the
        // account must exist" from "the account must have funds". Assert
        // directly that no record was created.
        assert!(
            dbtx.get_value(&AccountKey(forged.id())).await.is_none(),
            "a forged, unfunded account must not gain a database record just by being spent \
             against"
        );
    }

    #[tokio::test]
    async fn process_input_debits_and_counts() {
        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let (account, _) = account_with_keys(3, 2);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(10_000),
        )
        .await;

        Multispend::test_process_input(&mut dbtx.to_ref_nc(), &spend(&account))
            .await
            .expect("a funded account can be spent");

        let state = dbtx.get_value(&AccountKey(account.id())).await.unwrap();
        assert_eq!(
            state.balance.get(&AmountUnit::BITCOIN),
            Some(&Amount::from_msats(9_500))
        );
        assert_eq!(state.op_counter, 2, "credit then spend");
    }

    #[tokio::test]
    async fn process_input_rejects_an_overdraft() {
        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let (account, _) = account_with_keys(3, 2);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(100),
        )
        .await;

        let err = Multispend::test_process_input(&mut dbtx.to_ref_nc(), &spend(&account))
            .await
            .expect_err("500 > 100");
        assert_eq!(err, MultispendInputError::InsufficientBalance);
    }

    #[tokio::test]
    async fn process_input_rejects_spending_a_unit_the_account_lacks() {
        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let (account, _) = account_with_keys(2, 1);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(10_000),
        )
        .await;

        let input = MultispendInput::Spend {
            account: account.clone(),
            amounts: Amounts::new_custom(AmountUnit::new_custom(1), Amount::from_msats(1)),
        };

        let err = Multispend::test_process_input(&mut dbtx.to_ref_nc(), &input)
            .await
            .expect_err("a BTC balance must not fund a USDT spend");
        assert_eq!(err, MultispendInputError::InsufficientBalance);
    }

    /// Every other spend test goes through `test_process_input`, which
    /// returns only the debited `Amounts` -- nothing exercises the
    /// `InputMeta` assembly `ServerModule::process_input` actually returns.
    /// Call the real trait method (`Multispend` needs no configuration to
    /// construct, so there's nothing awkward about it) and check both fields
    /// a mutant could silently flip: swapping `InputAuth::SelfVerified` for
    /// `InputAuth::Key(..)` would hand authorization back to core, which
    /// performs none of its own for an input this module claims to have
    /// already verified; a nonzero `fees` would silently start charging
    /// spends.
    #[tokio::test]
    async fn process_input_reports_self_verified_auth_and_zero_fees() {
        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let (account, _) = account_with_keys(3, 2);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(10_000),
        )
        .await;

        let module = Multispend;
        let in_point = InPoint {
            txid: TransactionId::from_byte_array(TXID),
            in_idx: 0,
        };
        let meta = module
            .process_input(&mut dbtx.to_ref_nc(), &spend(&account), in_point)
            .await
            .expect("a funded account can be spent");

        assert_eq!(
            meta.auth,
            InputAuth::SelfVerified,
            "process_input must keep asserting SelfVerified -- core performs no \
             authorization check of its own for this input, so verify_input must remain \
             the only gate"
        );
        assert_eq!(
            meta.amount.fees,
            Amounts::ZERO,
            "a spend must not carry a fee"
        );
    }

    /// `checked_sub` in `process_spend` removes units that hit zero, so
    /// spending an account down to exactly zero stores
    /// `AccountState { balance: Amounts::ZERO, op_counter }` rather than
    /// deleting the record. This is intentional, consensus-relevant
    /// behaviour, not an oversight -- document it so a future reader doesn't
    /// "fix" it into deleting drained accounts.
    #[tokio::test]
    async fn draining_an_account_to_zero_keeps_its_record() {
        let db = Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            Default::default(),
        );
        let (account, _) = account_with_keys(3, 2);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(500),
        )
        .await;

        Multispend::test_process_input(&mut dbtx.to_ref_nc(), &spend(&account))
            .await
            .expect("spending the exact balance is allowed");

        let state = dbtx.get_value(&AccountKey(account.id())).await.expect(
            "a drained account must still have a record: an unfunded account and a \
                 drained one are different states, and deleting the record would erase the \
                 op_counter history",
        );
        assert_eq!(
            state.balance,
            Amounts::ZERO,
            "the balance must be exactly zero, not merely absent"
        );
        assert_eq!(state.op_counter, 2, "credit then spend");
    }
}
