//! The swap state machine (spec §3.1, §6, §6.1, §12).
//!
//! A swap is two transactions because core processes all inputs, then
//! signatures, then all outputs — an input can never observe a sibling
//! output's effects within the same transaction (spec §3.1). Tx1 mints a
//! `Balance` via `SwapV0`; Tx2 destroys it via `ClaimBalanceV0`. Between the
//! two, the balance sits in federation state, permanently claimable, with no
//! deadline and no refund path (spec §6.3) — so this state machine has no
//! timeout state and no way to abandon a swap. A crash at any point is
//! resumed exactly where it left off when the executor replays this state
//! machine's persisted `state` on restart.
//!
//! ```text
//! Tx1Submitted -> Tx1Accepted -> Tx2Submitted -> Done
//!      |                                  |
//!      v                                  v
//! Tx1Rejected                       Tx2Rejected
//! ```

use fedimint_api_client::api::DynModuleApi;
use fedimint_client_module::DynGlobalClientContext;
use fedimint_client_module::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_client_module::transaction::{ClientInput, ClientInputBundle};
use fedimint_core::core::OperationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{AmountUnit, Amounts};
use fedimint_core::secp256k1::{Keypair, PublicKey};
use fedimint_core::util::backoff_util::api_networking_backoff;
use fedimint_core::{Amount, TransactionId};
use fedimint_amm_common::types::AmmInput;
use tracing::warn;

use crate::AmmClientContext;
use crate::api::{AmmFederationApi, find_balance_recovery_entry};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SwapStateMachine {
    pub common: SwapCommon,
    pub state: SwapState,
}

impl SwapStateMachine {
    fn update(&self, state: SwapState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SwapCommon {
    pub operation_id: OperationId,
    /// The unit `recipient_keypair` will hold a `Balance` in once Tx1 lands.
    pub unit_out: AmountUnit,
    /// Ground fresh per swap (spec §8, §13.1): the sole key that can build
    /// Tx2's `ClaimBalanceV0`. Stored directly (mirroring
    /// `fedimint-lnv2-client`'s `SendSMCommon::refund_keypair`) rather than
    /// re-derived from the tweak each time, since the state machine has no
    /// access to the module root secret — only `crate::AmmClientModule` does.
    pub recipient_keypair: Keypair,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum SwapState {
    Tx1Submitted { txid: TransactionId },
    Tx1Rejected(String),
    Tx1Accepted,
    Tx2Submitted { txid: TransactionId },
    Tx2Rejected(String),
    Done,
}

impl State for SwapStateMachine {
    type ModuleContext = AmmClientContext;

    fn transitions(
        &self,
        _context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        match &self.state {
            SwapState::Tx1Submitted { txid } => {
                let txid = *txid;
                let global = global_context.clone();
                vec![StateTransition::new(
                    async move { global.await_tx_accepted(txid).await },
                    |_dbtx, result: Result<(), String>, old_state: Self| {
                        Box::pin(async move {
                            match result {
                                Ok(()) => old_state.update(SwapState::Tx1Accepted),
                                Err(error) => old_state.update(SwapState::Tx1Rejected(error)),
                            }
                        })
                    },
                )]
            }
            SwapState::Tx1Accepted => {
                let recipient_pk = self.common.recipient_keypair.public_key();
                let unit_out = self.common.unit_out;
                // `module_api()` is fetched lazily inside the trigger future
                // rather than eagerly here, so merely calling `transitions()`
                // (e.g. to inspect how many transitions a state schedules,
                // done in this module's tests) never touches it.
                let global_for_trigger = global_context.clone();
                let global_for_transition = global_context.clone();
                vec![StateTransition::new(
                    async move {
                        await_own_balance(global_for_trigger.module_api(), recipient_pk, unit_out)
                            .await
                    },
                    move |dbtx, balance, old_state| {
                        Box::pin(transition_tx1_accepted(
                            dbtx,
                            balance,
                            global_for_transition.clone(),
                            old_state,
                        ))
                    },
                )]
            }
            SwapState::Tx2Submitted { txid } => {
                let txid = *txid;
                let global = global_context.clone();
                vec![StateTransition::new(
                    async move { global.await_tx_accepted(txid).await },
                    |_dbtx, result: Result<(), String>, old_state: Self| {
                        Box::pin(async move {
                            match result {
                                Ok(()) => old_state.update(SwapState::Done),
                                Err(error) => old_state.update(SwapState::Tx2Rejected(error)),
                            }
                        })
                    },
                )]
            }
            SwapState::Tx1Rejected(_) | SwapState::Tx2Rejected(_) | SwapState::Done => vec![],
        }
    }

    fn operation_id(&self) -> OperationId {
        self.common.operation_id
    }
}

/// Re-reads the balance `recipient_pk` holds in `unit`, immediately before
/// building Tx2 (spec §6.1): a gift credited to `recipient_pk` after Tx1
/// landed must be captured, not forfeited, and the only way to observe it is
/// a fresh read — there is no way to derive it locally, since the exact
/// amount Tx1 settled at depends on pool state at the time of settlement,
/// which this client cannot recompute without curve arithmetic it is not
/// allowed to reimplement (see crate-level docs).
///
/// Returns `None` if no balance is found at all. Given Tx1 already landed
/// (this only runs from [`SwapState::Tx1Accepted`]), the one legitimate way
/// for that to happen is a crash that struck after Tx2 was already accepted
/// by consensus but before this state machine recorded reaching
/// [`SwapState::Tx2Submitted`] — i.e. the swap already finished on a previous
/// run. Spec §6.1 documents this as intentional: "if a claim lands and the
/// client resubmits, the second attempt finds no balance and is rejected
/// outright." [`transition_tx1_accepted`] treats `None` the same way,
/// completing the operation as [`SwapState::Done`] instead of resubmitting.
///
/// Retries indefinitely on network/API errors, via its own loop rather than
/// `fedimint_core::util::retry` (which would need an `.expect()` to unwrap
/// its `Result` — avoided here per this crate's no-`expect`-in-non-test-code
/// rule, even though `api_networking_backoff()`'s `max_retries_or: None`
/// means that particular `.expect()` could never actually fire in practice).
/// There is no failure state to fall back to here, by design (spec §6.3): a
/// swap can only ever finish or keep waiting, never be abandoned.
async fn await_own_balance(
    module_api: DynModuleApi,
    recipient_pk: PublicKey,
    unit: AmountUnit,
) -> Option<Amount> {
    let mut backoff = api_networking_backoff();
    loop {
        match find_balance_recovery_entry(
            |req| module_api.amm_balance_recovery_page(req),
            |entry| entry.pubkey == recipient_pk && entry.unit == unit,
        )
        .await
        {
            Ok(found) => return found.map(|entry| entry.amount),
            Err(error) => {
                warn!(%error, "Failed to re-read swap balance before building Tx2; retrying");
                // `api_networking_backoff()` never actually exhausts
                // (`max_retries_or: None`, verified by reading
                // `backoff_util::custom_backoff`), but even if `next()`
                // somehow returned `None`, looping straight back around is
                // still correct here: there is no failure state to fall back
                // to.
                if let Some(delay) = backoff.next() {
                    fedimint_core::runtime::sleep(delay).await;
                }
            }
        }
    }
}

/// Builds and submits Tx2 (`ClaimBalanceV0`) once the current balance is
/// known, or completes the operation if the balance is already gone (see
/// [`await_own_balance`]'s doc comment).
async fn transition_tx1_accepted(
    dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
    balance: Option<Amount>,
    global_context: DynGlobalClientContext,
    old_state: SwapStateMachine,
) -> SwapStateMachine {
    let Some(balance) = balance else {
        return old_state.update(SwapState::Done);
    };

    let unit_out = old_state.common.unit_out;
    let keypair = old_state.common.recipient_keypair;

    let client_input = ClientInput {
        input: AmmInput::ClaimBalanceV0 {
            pubkey: keypair.public_key(),
            unit: unit_out,
        },
        keys: vec![keypair],
        amounts: Amounts::new_custom(unit_out, balance),
    };

    // `claim_inputs` fails only if the primary module registered for
    // `unit_out` cannot balance the transaction (e.g. it has no funding
    // strategy for this unit at all) — a locally broken client
    // configuration, not something retrying the same call differently would
    // fix, and not a consensus rejection (`SwapState::Tx2Rejected` is for
    // that case, once Tx2 actually reaches the federation). There is
    // deliberately no way to abandon a swap (spec's no-refund design, §6.3),
    // so on failure this logs and leaves the state unchanged: the executor
    // will invoke `transitions()` again and retry the whole step, which is
    // safe since `await_own_balance` above is idempotent and the balance is
    // still there (nothing was submitted).
    match global_context
        .claim_inputs(dbtx, ClientInputBundle::new_no_sm(vec![client_input]))
        .await
    {
        Ok(range) => old_state.update(SwapState::Tx2Submitted { txid: range.txid() }),
        Err(error) => {
            warn!(%error, "Failed to submit swap Tx2 (ClaimBalanceV0); will retry");
            old_state
        }
    }
}

#[cfg(test)]
mod tests {
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::module::AmountUnit;
    use fedimint_core::module::registry::ModuleDecoderRegistry;
    use fedimint_core::secp256k1::{Keypair, SECP256K1};
    use fedimint_core::BitcoinHash as _;

    use super::*;

    fn keypair(seed: u8) -> Keypair {
        Keypair::from_seckey_slice(SECP256K1, &[seed; 32])
            .expect("a repeated non-zero byte is a valid secret key")
    }

    fn sm(state: SwapState) -> SwapStateMachine {
        SwapStateMachine {
            common: SwapCommon {
                operation_id: OperationId::new_random(),
                unit_out: AmountUnit::new_custom(1),
                recipient_keypair: keypair(1),
            },
            state,
        }
    }

    /// `Tx1Rejected` and `Tx2Rejected` are terminal: no further work is ever
    /// scheduled for a rejected swap, matching spec §12 ("terminal").
    #[test]
    fn rejected_and_done_states_have_no_transitions() {
        for state in [
            SwapState::Tx1Rejected("boom".to_string()),
            SwapState::Tx2Rejected("boom".to_string()),
            SwapState::Done,
        ] {
            let machine = sm(state);
            let context = AmmClientContext;
            let global = DynGlobalClientContext::new_fake();
            assert!(machine.transitions(&context, &global).is_empty());
        }
    }

    /// `Tx1Submitted`, `Tx1Accepted` and `Tx2Submitted` must each schedule
    /// exactly one pending transition — this is what makes the swap actually
    /// progress rather than stalling silently.
    #[test]
    fn non_terminal_states_schedule_a_transition() {
        for state in [
            SwapState::Tx1Submitted {
                txid: TransactionId::from_byte_array([0u8; 32]),
            },
            SwapState::Tx1Accepted,
            SwapState::Tx2Submitted {
                txid: TransactionId::from_byte_array([0u8; 32]),
            },
        ] {
            let machine = sm(state);
            let context = AmmClientContext;
            let global = DynGlobalClientContext::new_fake();
            assert_eq!(machine.transitions(&context, &global).len(), 1);
        }
    }

    #[test]
    fn operation_id_is_read_from_common() {
        let op = OperationId::new_random();
        let machine = SwapStateMachine {
            common: SwapCommon {
                operation_id: op,
                unit_out: AmountUnit::new_custom(1),
                recipient_keypair: keypair(1),
            },
            state: SwapState::Done,
        };
        assert_eq!(machine.operation_id(), op);
    }

    /// The whole state machine must round-trip through consensus encoding —
    /// it is persisted by the executor between restarts, which is the
    /// mechanism the "no timeout, crash is resumed" design (spec §6.3, §12)
    /// actually relies on.
    #[test]
    fn state_machine_round_trips_through_encoding() {
        for state in [
            SwapState::Tx1Submitted {
                txid: TransactionId::from_byte_array([3u8; 32]),
            },
            SwapState::Tx1Rejected("rejected".to_string()),
            SwapState::Tx1Accepted,
            SwapState::Tx2Submitted {
                txid: TransactionId::from_byte_array([4u8; 32]),
            },
            SwapState::Tx2Rejected("rejected".to_string()),
            SwapState::Done,
        ] {
            let machine = sm(state);
            let bytes = machine.consensus_encode_to_vec();
            let decoded =
                SwapStateMachine::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default())
                    .expect("round trip");
            assert_eq!(machine, decoded);
        }
    }

    /// A balance that has fully vanished by the time Tx1Accepted re-reads it
    /// means Tx2 already succeeded on a previous run (spec §6.1's documented
    /// retry semantics) — the operation must complete as `Done`, not get
    /// stuck retrying a claim that will only ever find nothing.
    ///
    /// This calls the real `transition_tx1_accepted`, not a copy of it: the
    /// `None` branch returns before touching either argument, so a fake
    /// `DynGlobalClientContext` (whose every other method panics) and a
    /// `ClientSMDatabaseTransaction` over a throwaway in-memory database are
    /// both safe to pass here, unlike for the `Some(balance)` branch, which
    /// calls `claim_inputs` and would need a fully wired executor to exercise
    /// for real.
    #[tokio::test]
    async fn tx1_accepted_completes_when_balance_already_claimed() {
        let old_state = sm(SwapState::Tx1Accepted);

        let db = fedimint_core::db::Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            ModuleDecoderRegistry::default(),
        );
        let mut dbtx = db.begin_transaction_nc().await;
        let mut sm_dbtx = ClientSMDatabaseTransaction::new(&mut dbtx, 0);

        let next = transition_tx1_accepted(
            &mut sm_dbtx,
            None,
            DynGlobalClientContext::new_fake(),
            old_state,
        )
        .await;

        assert_eq!(next.state, SwapState::Done);
    }
}
