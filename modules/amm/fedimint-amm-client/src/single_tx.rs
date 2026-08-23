//! State machines for the two single-transaction operations, `deposit` and
//! `withdraw` (spec §12: "Deposit and withdraw are single-transaction:
//! `Submitted -> Accepted | Rejected`"). Unlike [`crate::swap`], there is no
//! second transaction and nothing to re-read: `DepositV0` and `WithdrawV0`
//! either land or they don't, and the client already knows everything it
//! needs (the pool, the owner key, the amounts) up front.
//!
//! Both machines only exist to (a) let a caller await the outcome through the
//! usual operation-update mechanism, and (b) keep this crate's local
//! `LpPosition` cache (`crate::db`) in sync with what actually landed.

use fedimint_amm_common::pool_id::PoolId;
use fedimint_client_module::DynGlobalClientContext;
use fedimint_client_module::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_core::core::OperationId;
use fedimint_core::db::IDatabaseTransactionOpsCoreTyped;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::TransactionId;

use crate::AmmClientContext;
use crate::db::{LpPositionKey, LpPositionRecord};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct DepositStateMachine {
    pub common: DepositCommon,
    pub state: DepositState,
}

impl DepositStateMachine {
    fn update(&self, state: DepositState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct DepositCommon {
    pub operation_id: OperationId,
    pub pool: PoolId,
    pub owner_pk: PublicKey,
    /// Ground fresh per deposit (spec §8.3), stored so an accepted deposit
    /// can be recorded in the local LP-position cache without needing the
    /// module root secret (which this state machine has no access to).
    pub tweak: [u8; 16],
    /// A pre-submission preview of the shares this deposit will mint,
    /// computed via `fedimint_amm_common::math::mint_shares` against a
    /// snapshot of the pool's reserves (see `AmmClientModule::deposit`).
    /// Cached into the local `LpPositionRecord` on acceptance; see that
    /// type's doc comment for why it is informational only.
    pub expected_shares: u64,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum DepositState {
    Submitted { txid: TransactionId },
    Accepted,
    Rejected(String),
}

impl State for DepositStateMachine {
    type ModuleContext = AmmClientContext;

    fn transitions(
        &self,
        _context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        match &self.state {
            DepositState::Submitted { txid } => {
                let txid = *txid;
                let global = global_context.clone();
                vec![StateTransition::new(
                    async move { global.await_tx_accepted(txid).await },
                    |dbtx, result, old_state| Box::pin(transition_submitted(dbtx, result, old_state)),
                )]
            }
            DepositState::Accepted | DepositState::Rejected(_) => vec![],
        }
    }

    fn operation_id(&self) -> OperationId {
        self.common.operation_id
    }
}

async fn transition_submitted(
    dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
    result: Result<(), String>,
    old_state: DepositStateMachine,
) -> DepositStateMachine {
    match result {
        Ok(()) => {
            let key = LpPositionKey {
                pool: old_state.common.pool,
                owner_pk: old_state.common.owner_pk,
            };
            let record = LpPositionRecord {
                tweak: old_state.common.tweak,
                shares: old_state.common.expected_shares,
            };
            dbtx.module_tx().insert_entry(&key, &record).await;
            old_state.update(DepositState::Accepted)
        }
        Err(error) => old_state.update(DepositState::Rejected(error)),
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct WithdrawStateMachine {
    pub common: WithdrawCommon,
    pub state: WithdrawState,
}

impl WithdrawStateMachine {
    fn update(&self, state: WithdrawState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct WithdrawCommon {
    pub operation_id: OperationId,
    pub pool: PoolId,
    pub owner_pk: PublicKey,
    /// Shares this withdrawal declared burning. Declared client-side, unlike
    /// `ClaimBalanceV0` (spec §6.1) — `WithdrawV0` is not an all-or-nothing
    /// claim, so there is a real amount to record here, and the server
    /// checks it against the position's actual share count at settlement
    /// (spec §7.3) regardless of what this cache believes.
    pub shares: u64,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum WithdrawState {
    Submitted { txid: TransactionId },
    Accepted,
    Rejected(String),
}

impl State for WithdrawStateMachine {
    type ModuleContext = AmmClientContext;

    fn transitions(
        &self,
        _context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        match &self.state {
            WithdrawState::Submitted { txid } => {
                let txid = *txid;
                let global = global_context.clone();
                vec![StateTransition::new(
                    async move { global.await_tx_accepted(txid).await },
                    |dbtx, result, old_state| {
                        Box::pin(transition_withdraw_submitted(dbtx, result, old_state))
                    },
                )]
            }
            WithdrawState::Accepted | WithdrawState::Rejected(_) => vec![],
        }
    }

    fn operation_id(&self) -> OperationId {
        self.common.operation_id
    }
}

async fn transition_withdraw_submitted(
    dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
    result: Result<(), String>,
    old_state: WithdrawStateMachine,
) -> WithdrawStateMachine {
    match result {
        Ok(()) => {
            let key = LpPositionKey {
                pool: old_state.common.pool,
                owner_pk: old_state.common.owner_pk,
            };
            // Best-effort bookkeeping only (see `LpPositionRecord`'s doc
            // comment): if the cache is already gone or already stale this
            // just leaves it as-is rather than erroring, since the server
            // remains authoritative for what the position actually holds.
            let mut module_tx = dbtx.module_tx();
            if let Some(mut record) = module_tx.get_value(&key).await {
                if record.shares <= old_state.common.shares {
                    module_tx.remove_entry(&key).await;
                } else {
                    record.shares -= old_state.common.shares;
                    module_tx.insert_entry(&key, &record).await;
                }
            }
            old_state.update(WithdrawState::Accepted)
        }
        Err(error) => old_state.update(WithdrawState::Rejected(error)),
    }
}

#[cfg(test)]
mod tests {
    use fedimint_core::db::mem_impl::MemDatabase;
    use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::module::AmountUnit;
    use fedimint_core::module::registry::ModuleDecoderRegistry;
    use fedimint_core::secp256k1::{Keypair, SECP256K1};
    use fedimint_core::BitcoinHash as _;

    use super::*;

    fn pubkey(seed: u8) -> PublicKey {
        Keypair::from_seckey_slice(SECP256K1, &[seed; 32])
            .expect("a repeated non-zero byte is a valid secret key")
            .public_key()
    }

    fn pool() -> PoolId {
        PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).expect("distinct units")
    }

    #[tokio::test]
    async fn deposit_accepted_inserts_the_local_position() {
        let db = Database::new(MemDatabase::new(), ModuleDecoderRegistry::default());
        let mut dbtx = db.begin_transaction_nc().await;
        let mut sm_dbtx = ClientSMDatabaseTransaction::new(&mut dbtx, 0);

        let old_state = DepositStateMachine {
            common: DepositCommon {
                operation_id: OperationId::new_random(),
                pool: pool(),
                owner_pk: pubkey(1),
                tweak: [5u8; 16],
                expected_shares: 42,
            },
            state: DepositState::Submitted {
                txid: TransactionId::from_byte_array([0u8; 32]),
            },
        };

        let next = transition_submitted(&mut sm_dbtx, Ok(()), old_state.clone()).await;
        assert_eq!(next.state, DepositState::Accepted);

        let record = sm_dbtx
            .module_tx()
            .get_value(&LpPositionKey {
                pool: old_state.common.pool,
                owner_pk: old_state.common.owner_pk,
            })
            .await;
        assert_eq!(
            record,
            Some(LpPositionRecord {
                tweak: [5u8; 16],
                shares: 42,
            })
        );
    }

    #[tokio::test]
    async fn deposit_rejected_does_not_touch_the_local_position() {
        let db = Database::new(MemDatabase::new(), ModuleDecoderRegistry::default());
        let mut dbtx = db.begin_transaction_nc().await;
        let mut sm_dbtx = ClientSMDatabaseTransaction::new(&mut dbtx, 0);

        let old_state = DepositStateMachine {
            common: DepositCommon {
                operation_id: OperationId::new_random(),
                pool: pool(),
                owner_pk: pubkey(2),
                tweak: [6u8; 16],
                expected_shares: 7,
            },
            state: DepositState::Submitted {
                txid: TransactionId::from_byte_array([0u8; 32]),
            },
        };

        let next =
            transition_submitted(&mut sm_dbtx, Err("rejected".to_string()), old_state.clone()).await;
        assert_eq!(next.state, DepositState::Rejected("rejected".to_string()));

        let record = sm_dbtx
            .module_tx()
            .get_value(&LpPositionKey {
                pool: old_state.common.pool,
                owner_pk: old_state.common.owner_pk,
            })
            .await;
        assert_eq!(record, None);
    }

    #[tokio::test]
    async fn withdraw_accepted_decrements_a_partial_position() {
        let db = Database::new(MemDatabase::new(), ModuleDecoderRegistry::default());
        let key = LpPositionKey {
            pool: pool(),
            owner_pk: pubkey(3),
        };
        {
            // Written through the same module-instance-prefixed namespace
            // `ClientSMDatabaseTransaction::module_tx()` uses below (module
            // instance 0), so the read after the transition actually sees
            // it. `ClientSMDatabaseTransaction::new` requires a
            // `NonCommittable` transaction, which cannot itself be committed
            // — so this setup goes through `to_ref_with_prefix_module_id`
            // directly instead, on the real `Committable` transaction.
            let mut dbtx = db.begin_transaction().await;
            {
                let (mut prefixed, _) = dbtx.to_ref_with_prefix_module_id(0);
                prefixed
                    .insert_new_entry(
                        &key,
                        &LpPositionRecord {
                            tweak: [9u8; 16],
                            shares: 100,
                        },
                    )
                    .await;
            }
            dbtx.commit_tx().await;
        }

        let mut dbtx = db.begin_transaction_nc().await;
        let mut sm_dbtx = ClientSMDatabaseTransaction::new(&mut dbtx, 0);

        let old_state = WithdrawStateMachine {
            common: WithdrawCommon {
                operation_id: OperationId::new_random(),
                pool: key.pool,
                owner_pk: key.owner_pk,
                shares: 40,
            },
            state: WithdrawState::Submitted {
                txid: TransactionId::from_byte_array([0u8; 32]),
            },
        };

        let next = transition_withdraw_submitted(&mut sm_dbtx, Ok(()), old_state).await;
        assert_eq!(next.state, WithdrawState::Accepted);

        let record = sm_dbtx.module_tx().get_value(&key).await;
        assert_eq!(
            record,
            Some(LpPositionRecord {
                tweak: [9u8; 16],
                shares: 60,
            })
        );
    }

    #[tokio::test]
    async fn withdraw_accepted_removes_a_fully_drained_position() {
        let db = Database::new(MemDatabase::new(), ModuleDecoderRegistry::default());
        let key = LpPositionKey {
            pool: pool(),
            owner_pk: pubkey(4),
        };
        {
            let mut dbtx = db.begin_transaction().await;
            {
                let (mut prefixed, _) = dbtx.to_ref_with_prefix_module_id(0);
                prefixed
                    .insert_new_entry(
                        &key,
                        &LpPositionRecord {
                            tweak: [1u8; 16],
                            shares: 50,
                        },
                    )
                    .await;
            }
            dbtx.commit_tx().await;
        }

        let mut dbtx = db.begin_transaction_nc().await;
        let mut sm_dbtx = ClientSMDatabaseTransaction::new(&mut dbtx, 0);

        let old_state = WithdrawStateMachine {
            common: WithdrawCommon {
                operation_id: OperationId::new_random(),
                pool: key.pool,
                owner_pk: key.owner_pk,
                shares: 50,
            },
            state: WithdrawState::Submitted {
                txid: TransactionId::from_byte_array([0u8; 32]),
            },
        };

        let next = transition_withdraw_submitted(&mut sm_dbtx, Ok(()), old_state).await;
        assert_eq!(next.state, WithdrawState::Accepted);

        let record = sm_dbtx.module_tx().get_value(&key).await;
        assert_eq!(record, None, "a fully drained position must be removed, not left at 0");
    }

    #[test]
    fn state_machines_round_trip_through_encoding() {
        let deposit = DepositStateMachine {
            common: DepositCommon {
                operation_id: OperationId::new_random(),
                pool: pool(),
                owner_pk: pubkey(1),
                tweak: [1u8; 16],
                expected_shares: 10,
            },
            state: DepositState::Accepted,
        };
        let bytes = deposit.consensus_encode_to_vec();
        let decoded =
            DepositStateMachine::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default())
                .expect("round trip");
        assert_eq!(deposit, decoded);

        let withdraw = WithdrawStateMachine {
            common: WithdrawCommon {
                operation_id: OperationId::new_random(),
                pool: pool(),
                owner_pk: pubkey(2),
                shares: 5,
            },
            state: WithdrawState::Rejected("no".to_string()),
        };
        let bytes = withdraw.consensus_encode_to_vec();
        let decoded =
            WithdrawStateMachine::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default())
                .expect("round trip");
        assert_eq!(withdraw, decoded);
    }
}
