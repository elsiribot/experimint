//! State machine tracking a [`super::FaucetOutput`] this client is on the
//! receiving end of — either a mint ([`super::FaucetClientModule::mint`]) or
//! change credited back by `create_final_inputs_and_outputs`. Balance is
//! credited only on acceptance, mirroring
//! `fedimint-dummy-client::output_sm::DummyOutputStateMachine` exactly (this
//! file is a near-verbatim copy, minus the `AmountUnit` field: this module
//! only ever tracks one unit).

use fedimint_client_module::DynGlobalClientContext;
use fedimint_client_module::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_core::core::OperationId;
use fedimint_core::db::IDatabaseTransactionOpsCoreTyped;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{Amount, OutPoint};

use super::FaucetClientContext;
use super::db::FaucetClientFundsKey;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct FaucetOutputStateMachine {
    pub common: FaucetOutputSMCommon,
    pub state: FaucetOutputSMState,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct FaucetOutputSMCommon {
    pub operation_id: OperationId,
    pub out_point: OutPoint,
    pub amount: Amount,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum FaucetOutputSMState {
    Created,
    Accepted,
    Rejected,
}

impl FaucetOutputStateMachine {
    fn update(&self, state: FaucetOutputSMState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

impl State for FaucetOutputStateMachine {
    type ModuleContext = FaucetClientContext;

    fn transitions(
        &self,
        context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        match self.state {
            FaucetOutputSMState::Created => {
                let global = global_context.clone();
                let txid = self.common.out_point.txid;
                let balance_update_sender = context.balance_update_sender.clone();

                vec![StateTransition::new(
                    async move { global.await_tx_accepted(txid).await },
                    move |dbtx, result, old_state| {
                        Box::pin(Self::transition_created(
                            dbtx,
                            result,
                            old_state,
                            balance_update_sender.clone(),
                        ))
                    },
                )]
            }
            FaucetOutputSMState::Accepted | FaucetOutputSMState::Rejected => vec![],
        }
    }

    fn operation_id(&self) -> OperationId {
        self.common.operation_id
    }
}

impl FaucetOutputStateMachine {
    async fn transition_created(
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        result: Result<(), String>,
        old_state: Self,
        balance_update_sender: tokio::sync::watch::Sender<()>,
    ) -> Self {
        if result.is_ok() {
            let current = dbtx
                .module_tx()
                .get_value(&FaucetClientFundsKey)
                .await
                .unwrap_or(Amount::ZERO);

            dbtx.module_tx()
                .insert_entry(&FaucetClientFundsKey, &(current + old_state.common.amount))
                .await;

            dbtx.module_tx().on_commit(move || {
                balance_update_sender.send_replace(());
            });

            old_state.update(FaucetOutputSMState::Accepted)
        } else {
            old_state.update(FaucetOutputSMState::Rejected)
        }
    }
}
