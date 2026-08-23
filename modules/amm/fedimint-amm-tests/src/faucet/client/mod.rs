//! Client half of the test faucet.
//!
//! **Test-only. Never deploy this module to a real federation** — see
//! `super::server`'s module doc comment.
//!
//! Structurally this is `fedimint-dummy-client` with the free-money side
//! moved from the input to the output (to match the server's spec-mandated
//! "output credits, input debits" shape — see `server.rs`'s module doc
//! comment for why), a single hardcoded unit instead of an arbitrary one, and
//! one addition: [`FaucetClientModule::mint`], the test-only bootstrap entry
//! point a real currency module doesn't need (a real mint's supply comes from
//! peg-ins or note issuance against a threshold signature, not a public
//! "give me money" call).
//!
//! Declares [`PrimaryModuleSupport::Selected`] for exactly
//! [`faucet_unit`] (spec P10, `fedimint-client-module/src/module/
//! mod.rs:924-934`): this is what lets `fedimint-amm-client`'s `swap`/
//! `deposit`/`withdraw` auto-fund and auto-receive-change for that unit
//! through the ordinary `finalize_and_submit_transaction` balancing path,
//! exactly as `mintv2` does for `AmountUnit::BITCOIN` — no `fedimint-amm-*`
//! code needs to know this module exists.

pub mod db;
mod input_sm;
mod output_sm;

use std::collections::BTreeMap;
use std::cmp::Ordering;
use std::sync::Arc;

use fedimint_client_module::db::ClientModuleMigrationFn;
use fedimint_client_module::module::init::{ClientModuleInit, ClientModuleInitArgs};
use fedimint_client_module::module::recovery::NoModuleBackup;
use fedimint_client_module::module::{
    ClientContext, ClientModule, OutPointRange, PrimaryModulePriority, PrimaryModuleSupport,
};
use fedimint_client_module::sm::{Context, DynState, ModuleNotifier, State, StateTransition};
use fedimint_client_module::transaction::{
    ClientInput, ClientInputBundle, ClientInputSM, ClientOutput, ClientOutputBundle, ClientOutputSM,
    TransactionBuilder,
};
use fedimint_client_module::{DynGlobalClientContext, sm_enum_variant_translation};
use fedimint_core::core::{IntoDynInstance, ModuleInstanceId, ModuleKind, OperationId};
use fedimint_core::db::{DatabaseTransaction, DatabaseVersion, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{
    AmountUnit, Amounts, ApiVersion, ModuleCommon, ModuleInit, MultiApiVersion,
};
use fedimint_core::secp256k1::{Keypair, Secp256k1};
use fedimint_core::util::BoxStream;
use fedimint_core::{Amount, OutPoint, apply, async_trait_maybe_send, push_db_pair_items};
use futures::StreamExt;
use strum::IntoEnumIterator;
use tokio::sync::watch;

use crate::faucet::common::{FaucetCommonInit, FaucetInput, FaucetModuleTypes, FaucetOutput, faucet_unit};
use db::{DbKeyPrefix, FaucetClientFundsKey, FaucetClientFundsPrefix};
use input_sm::{FaucetInputSMCommon, FaucetInputSMState, FaucetInputStateMachine};
use output_sm::{FaucetOutputSMCommon, FaucetOutputSMState, FaucetOutputStateMachine};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum FaucetClientStateMachines {
    Input(FaucetInputStateMachine),
    Output(FaucetOutputStateMachine),
}

impl State for FaucetClientStateMachines {
    type ModuleContext = FaucetClientContext;

    fn transitions(
        &self,
        context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        match self {
            FaucetClientStateMachines::Input(sm) => {
                sm_enum_variant_translation!(
                    sm.transitions(context, global_context),
                    FaucetClientStateMachines::Input
                )
            }
            FaucetClientStateMachines::Output(sm) => {
                sm_enum_variant_translation!(
                    sm.transitions(context, global_context),
                    FaucetClientStateMachines::Output
                )
            }
        }
    }

    fn operation_id(&self) -> OperationId {
        match self {
            FaucetClientStateMachines::Input(sm) => sm.operation_id(),
            FaucetClientStateMachines::Output(sm) => sm.operation_id(),
        }
    }
}

impl IntoDynInstance for FaucetClientStateMachines {
    type DynType = DynState;

    fn into_dyn(self, instance_id: ModuleInstanceId) -> Self::DynType {
        DynState::from_typed(instance_id, self)
    }
}

#[derive(Clone)]
pub struct FaucetClientContext {
    pub balance_update_sender: watch::Sender<()>,
}

impl std::fmt::Debug for FaucetClientContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaucetClientContext").finish_non_exhaustive()
    }
}

impl Context for FaucetClientContext {
    const KIND: Option<ModuleKind> = None;
}

pub struct FaucetClientModule {
    key: Keypair,
    notifier: ModuleNotifier<FaucetClientStateMachines>,
    client_ctx: ClientContext<Self>,
    balance_update_sender: watch::Sender<()>,
}

impl std::fmt::Debug for FaucetClientModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaucetClientModule").finish_non_exhaustive()
    }
}

#[apply(async_trait_maybe_send!)]
impl ClientModule for FaucetClientModule {
    type Init = FaucetClientInit;
    type Common = FaucetModuleTypes;
    type Backup = NoModuleBackup;
    type ModuleStateMachineContext = FaucetClientContext;
    type States = FaucetClientStateMachines;

    fn context(&self) -> Self::ModuleStateMachineContext {
        FaucetClientContext {
            balance_update_sender: self.balance_update_sender.clone(),
        }
    }

    fn input_fee(
        &self,
        _amount: &Amounts,
        _input: &<Self::Common as ModuleCommon>::Input,
    ) -> Option<Amounts> {
        Some(Amounts::ZERO)
    }

    fn output_fee(
        &self,
        _amount: &Amounts,
        _output: &<Self::Common as ModuleCommon>::Output,
    ) -> Option<Amounts> {
        Some(Amounts::ZERO)
    }

    /// Exactly one unit, unlike `fedimint-dummy-client`'s `Any` (spec P10):
    /// this module never wants to be picked as the funding source for
    /// `AmountUnit::BITCOIN` or anything else `mintv2` already owns in these
    /// tests.
    fn supports_being_primary(&self) -> PrimaryModuleSupport {
        PrimaryModuleSupport::selected(PrimaryModulePriority::LOW, [faucet_unit()])
    }

    /// Balances a transaction's `faucet_unit()` leg — mirrors
    /// `fedimint-dummy-client`'s implementation exactly (see that module for
    /// the reasoning behind the optimistic-debit-refund-on-reject and
    /// credit-only-on-accept timing), except the wire types here carry no
    /// `unit` field since there is only ever one.
    async fn create_final_inputs_and_outputs(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        operation_id: OperationId,
        unit: AmountUnit,
        input_amount: Amount,
        output_amount: Amount,
    ) -> anyhow::Result<(
        ClientInputBundle<FaucetInput, FaucetClientStateMachines>,
        ClientOutputBundle<FaucetOutput, FaucetClientStateMachines>,
    )> {
        dbtx.ensure_isolated().expect("must be isolated");
        debug_assert_eq!(
            unit,
            faucet_unit(),
            "registered as primary only for faucet_unit()"
        );

        match input_amount.cmp(&output_amount) {
            Ordering::Less => {
                let missing_input_amount = output_amount.saturating_sub(input_amount);

                let our_funds = get_funds(dbtx).await;
                if our_funds < missing_input_amount {
                    return Err(anyhow::format_err!("Insufficient funds"));
                }

                let updated = our_funds.saturating_sub(missing_input_amount);
                dbtx.insert_entry(&FaucetClientFundsKey, &updated).await;

                let sender = self.balance_update_sender.clone();
                dbtx.on_commit(move || sender.send_replace(()));

                let input = ClientInput {
                    input: FaucetInput {
                        amount: missing_input_amount,
                        pub_key: self.key.public_key(),
                    },
                    amounts: Amounts::new_custom(unit, missing_input_amount),
                    keys: vec![self.key],
                };

                let input_sm = ClientInputSM {
                    state_machines: Arc::new(move |out_point_range: OutPointRange| {
                        out_point_range
                            .into_iter()
                            .map(|out_point| {
                                FaucetClientStateMachines::Input(FaucetInputStateMachine {
                                    common: FaucetInputSMCommon {
                                        operation_id,
                                        out_point,
                                        amount: missing_input_amount,
                                    },
                                    state: FaucetInputSMState::Created,
                                })
                            })
                            .collect()
                    }),
                };

                Ok((
                    ClientInputBundle::new(vec![input], vec![input_sm]),
                    ClientOutputBundle::new(vec![], vec![]),
                ))
            }
            Ordering::Equal => Ok((
                ClientInputBundle::new(vec![], vec![]),
                ClientOutputBundle::new(vec![], vec![]),
            )),
            Ordering::Greater => {
                let missing_output_amount = input_amount.saturating_sub(output_amount);

                let output = ClientOutput {
                    output: FaucetOutput {
                        amount: missing_output_amount,
                        pub_key: self.key.public_key(),
                    },
                    amounts: Amounts::new_custom(unit, missing_output_amount),
                };

                let output_sm = ClientOutputSM {
                    state_machines: Arc::new(move |range: OutPointRange| {
                        range
                            .into_iter()
                            .map(|out_point| {
                                FaucetClientStateMachines::Output(FaucetOutputStateMachine {
                                    common: FaucetOutputSMCommon {
                                        operation_id,
                                        out_point,
                                        amount: missing_output_amount,
                                    },
                                    state: FaucetOutputSMState::Created,
                                })
                            })
                            .collect()
                    }),
                };

                Ok((
                    ClientInputBundle::new(vec![], vec![]),
                    ClientOutputBundle::new(vec![output], vec![output_sm]),
                ))
            }
        }
    }

    async fn await_primary_module_output(
        &self,
        operation_id: OperationId,
        out_point: OutPoint,
    ) -> anyhow::Result<()> {
        let mut stream = self.notifier.subscribe(operation_id).await;

        loop {
            let FaucetClientStateMachines::Output(output_sm) = stream
                .next()
                .await
                .expect("Stream should not end before reaching final state")
            else {
                continue;
            };

            if output_sm.common.out_point != out_point {
                continue;
            }

            match output_sm.state {
                FaucetOutputSMState::Created => {}
                FaucetOutputSMState::Accepted => return Ok(()),
                FaucetOutputSMState::Rejected => {
                    return Err(anyhow::anyhow!("Transaction was rejected"));
                }
            }
        }
    }

    async fn get_balance(&self, dbtx: &mut DatabaseTransaction<'_>, _unit: AmountUnit) -> Amount {
        get_funds(dbtx).await
    }

    async fn get_balances(&self, dbtx: &mut DatabaseTransaction<'_>) -> Amounts {
        Amounts::new_custom(faucet_unit(), get_funds(dbtx).await)
    }

    async fn subscribe_balance_changes(&self) -> BoxStream<'static, ()> {
        Box::pin(tokio_stream::wrappers::WatchStream::new(
            self.balance_update_sender.subscribe(),
        ))
    }
}

impl FaucetClientModule {
    /// **Test-only bootstrap.** Mints `amount` of `faucet_unit()` to this
    /// client's own key and waits for the transaction to be accepted before
    /// returning — a real currency module has no equivalent (its supply
    /// comes from a peg-in or a threshold-signed issuance, never a public
    /// "give me money" call). Every integration test that needs to fund a
    /// wallet in the faucet's unit starts here, exactly as
    /// `fedimint-mintv2-tests`'s own tests start every scenario with
    /// `issue_ecash` (`fedimint_dummy_client::DummyClientModule::
    /// create_input`).
    ///
    /// Builds the mint output directly (bypassing
    /// `create_final_inputs_and_outputs`) with `amounts: Amounts::ZERO`, for
    /// the same reason `server.rs`'s `process_output` declares no backing:
    /// if this call instead declared the real amount, `finalize_transaction`
    /// would see an apparent imbalance for `faucet_unit()` and re-enter this
    /// same module's own `create_final_inputs_and_outputs` to "fix" it —
    /// which would try to *spend* `amount` from a wallet that, before this
    /// call, has none.
    ///
    /// Waits for completion via the notifier, matching on `operation_id`
    /// alone (mirroring `AmmClientModule::await_swap`/`await_deposit`) —
    /// **not** via the `OutPointRange` `finalize_and_submit_transaction`
    /// returns. That range is documented (`Client::finalize_transaction`'s
    /// own comment) as "the range of outputs that will be added to the
    /// transaction in order to balance it" — i.e. only auto-balance-added
    /// change outputs, "empty in case the transaction is already balanced".
    /// This transaction has no imbalance to balance (its one output declares
    /// `Amounts::ZERO`), so that range is always empty here; looping over it
    /// to decide when to stop waiting (an earlier version of this method did
    /// exactly that) silently skips waiting altogether; the explicit output
    /// this method itself added was never a "balancing" output in the first
    /// place, so it isn't in that range either.
    pub async fn mint(&self, amount: Amount) -> anyhow::Result<()> {
        let operation_id = OperationId::new_random();
        let pub_key = self.key.public_key();

        let client_output = ClientOutput {
            output: FaucetOutput { amount, pub_key },
            amounts: Amounts::ZERO,
        };

        let client_output_sm = ClientOutputSM {
            state_machines: Arc::new(move |range: OutPointRange| {
                range
                    .into_iter()
                    .map(|out_point| {
                        FaucetClientStateMachines::Output(FaucetOutputStateMachine {
                            common: FaucetOutputSMCommon {
                                operation_id,
                                out_point,
                                amount,
                            },
                            state: FaucetOutputSMState::Created,
                        })
                    })
                    .collect()
            }),
        };

        let tx_builder =
            TransactionBuilder::new().with_outputs(self.client_ctx.make_client_outputs(
                ClientOutputBundle::new(vec![client_output], vec![client_output_sm]),
            ));

        self.client_ctx
            .finalize_and_submit_transaction(operation_id, "ammfaucet mint", move |_| (), tx_builder)
            .await?;

        let mut stream = self.notifier.subscribe(operation_id).await;
        loop {
            let FaucetClientStateMachines::Output(output_sm) = stream
                .next()
                .await
                .expect("Stream should not end before reaching final state")
            else {
                continue;
            };

            match output_sm.state {
                FaucetOutputSMState::Created => {}
                FaucetOutputSMState::Accepted => return Ok(()),
                FaucetOutputSMState::Rejected => {
                    return Err(anyhow::anyhow!("mint transaction was rejected"));
                }
            }
        }
    }
}

async fn get_funds(dbtx: &mut DatabaseTransaction<'_>) -> Amount {
    dbtx.get_value(&FaucetClientFundsKey)
        .await
        .unwrap_or(Amount::ZERO)
}

#[derive(Debug, Clone)]
pub struct FaucetClientInit;

impl ModuleInit for FaucetClientInit {
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
                DbKeyPrefix::ClientFunds => {
                    push_db_pair_items!(
                        dbtx,
                        FaucetClientFundsPrefix,
                        FaucetClientFundsKey,
                        Amount,
                        items,
                        "Faucet Funds"
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
impl ClientModuleInit for FaucetClientInit {
    type Module = FaucetClientModule;

    fn supported_api_versions(&self) -> MultiApiVersion {
        MultiApiVersion::try_from_iter([ApiVersion { major: 0, minor: 0 }])
            .expect("no version conflicts")
    }

    async fn init(&self, args: &ClientModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        Ok(FaucetClientModule {
            key: args
                .module_root_secret()
                .clone()
                .to_secp_key(&Secp256k1::new()),
            notifier: args.notifier().clone(),
            client_ctx: args.context(),
            balance_update_sender: watch::channel(()).0,
        })
    }

    fn get_database_migrations(&self) -> BTreeMap<DatabaseVersion, ClientModuleMigrationFn> {
        BTreeMap::new()
    }
}
