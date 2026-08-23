//! State machine tracking a [`super::FaucetInput`] this client has spent.
//! Balance is subtracted immediately on build and refunded on rejection —
//! mirrors `fedimint-dummy-client::input_sm::DummyInputStateMachine` exactly
//! (near-verbatim copy, minus the `AmountUnit` field).

use fedimint_client_module::DynGlobalClientContext;
use fedimint_client_module::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_core::core::OperationId;
use fedimint_core::db::IDatabaseTransactionOpsCoreTyped;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{Amount, OutPoint};

use super::FaucetClientContext;
use super::db::FaucetClientFundsKey;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct FaucetInputStateMachine {
    pub common: FaucetInputSMCommon,
    pub state: FaucetInputSMState,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct FaucetInputSMCommon {
    pub operation_id: OperationId,
    pub out_point: OutPoint,
    pub amount: Amount,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum FaucetInputSMState {
    Created,
    Accepted,
    Refunded,
}

impl FaucetInputStateMachine {
    fn update(&self, state: FaucetInputSMState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

impl State for FaucetInputStateMachine {
    type ModuleContext = FaucetClientContext;

    fn transitions(
        &self,
        context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        match self.state {
            FaucetInputSMState::Created => {
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
            FaucetInputSMState::Accepted | FaucetInputSMState::Refunded => vec![],
        }
    }

    fn operation_id(&self) -> OperationId {
        self.common.operation_id
    }
}

impl FaucetInputStateMachine {
    async fn transition_created(
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        result: Result<(), String>,
        old_state: Self,
        balance_update_sender: tokio::sync::watch::Sender<()>,
    ) -> Self {
        if result.is_ok() {
            old_state.update(FaucetInputSMState::Accepted)
        } else {
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

            old_state.update(FaucetInputSMState::Refunded)
        }
    }
}
