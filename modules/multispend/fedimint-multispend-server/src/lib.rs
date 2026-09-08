//! The multispend module server crate.
//!
//! Implements the `ServerModuleInit`/`ServerModule` skeleton, DKG config
//! generation, the database schema (spec §3.2), `process_output` crediting an
//! account (spec §3.4), `audit` (spec §9), and the `account_balance` API
//! endpoint (spec §3.5).
//!
//! `verify_input`/`process_input` — the security-critical spend path — are
//! deliberately NOT implemented here; that is Task 5, landing alone so a
//! reviewer can give it full attention. `process_input` below is an
//! intentionally panicking placeholder: `ServerModule::process_input` has no
//! default implementation, so the trait cannot be satisfied without a body,
//! but nothing in this task calls it (there is no input-side test, and this
//! task builds no integration test crate).
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
    Amounts, ApiEndpoint, ApiVersion, CORE_CONSENSUS_VERSION, CoreConsensusVersion, InputMeta,
    ModuleConsensusVersion, ModuleInit, SupportedModuleApiVersions, TransactionItemAmounts,
    api_endpoint,
};
use fedimint_core::{Amount, InPoint, OutPoint, PeerId};
use fedimint_multispend_common::account::AccountId;
use fedimint_multispend_common::config::{
    MultispendClientConfig, MultispendConfig, MultispendConfigConsensus, MultispendConfigPrivate,
};
use fedimint_multispend_common::endpoints::{ACCOUNT_BALANCE_ENDPOINT, AccountBalance};
use fedimint_multispend_common::types::{
    MultispendConsensusItem, MultispendInput, MultispendInputError, MultispendOutput,
    MultispendOutputError, MultispendOutputOutcome,
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

    /// Task 5's job. `ServerModule::process_input` has no default
    /// implementation, so this placeholder exists purely to let the trait
    /// compile; nothing in Task 4 calls it. See this crate's top-level doc
    /// comment.
    async fn process_input<'a, 'b, 'c>(
        &'a self,
        _dbtx: &mut DatabaseTransaction<'c>,
        _input: &'b MultispendInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, MultispendInputError> {
        unimplemented!("process_input is Task 5's security-critical spend path")
    }

    async fn process_output<'a, 'b>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'b>,
        output: &'a MultispendOutput,
        _out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, MultispendOutputError> {
        let MultispendOutput::Credit { account, amounts } = output;

        // A zero credit would create an account record with no funds, which
        // the "an account exists exactly while it has a balance" invariant
        // relies on being impossible. `amounts.is_empty()` is NOT sufficient
        // to catch this: that only holds for `Amounts` values built through
        // its safe constructors (`new_bitcoin`, `new_custom`,
        // `checked_add_mut`), which filter zero entries, but this `amounts`
        // is decoded off the wire, and `Amounts`'s derived `Decodable`
        // enforces no such invariant — a hand-crafted, consensus-encoded
        // credit can carry an explicit zero-amount entry, making the map
        // non-empty while still being worth nothing. Checking that every
        // value is zero is vacuously true on an empty map too, so it covers
        // both shapes in one expression.
        if amounts.values().all(|amount| *amount == Amount::ZERO) {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use fedimint_core::Amount;
    use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::module::registry::ModuleDecoderRegistry;
    use fedimint_core::module::{
        AmountUnit, Amounts, ApiEndpointContext, ApiRequestErased, serde_json,
    };
    use fedimint_core::secp256k1::rand::rngs::OsRng;
    use fedimint_core::secp256k1::{Keypair, Secp256k1};
    use fedimint_core::{BitcoinHash, OutPoint, TransactionId};
    use fedimint_multispend_common::account::{Account, AccountId, AccountUnchecked};
    use fedimint_multispend_common::endpoints::{ACCOUNT_BALANCE_ENDPOINT, AccountBalance};
    use fedimint_multispend_common::types::{MultispendOutput, MultispendOutputError};
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
}
