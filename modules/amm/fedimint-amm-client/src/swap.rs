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
//!      |              ^    |          |
//!      v              |    v          v
//! Tx1Rejected         +-Tx2Failed  Tx2Rejected (terminal only after
//!                        (backoff,                MAX_TX2_ATTEMPTS)
//!                         retries)
//! ```
//!
//! `Tx2Failed` is a single retry state reached from two different failure
//! causes — `claim_inputs` failing locally (fix pass 3, Important 1) and Tx2
//! being rejected by consensus (fix pass 3, Important 3) — because both are
//! resolved the same way: back off, then re-enter `Tx1Accepted`, which
//! re-reads the balance and tries again. `await_own_balance` returning `None`
//! there resolves to `Done` (the balance is already gone, so a previous
//! attempt must have landed); `Some` re-attempts the claim. See
//! [`MAX_TX2_ATTEMPTS`] for why this cannot spin forever.

use std::time::Duration;

use fedimint_amm_common::endpoints::BalanceRequest;
use fedimint_amm_common::types::AmmInput;
use fedimint_api_client::api::DynModuleApi;
use fedimint_client_module::DynGlobalClientContext;
use fedimint_client_module::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_client_module::transaction::{ClientInput, ClientInputBundle};
use fedimint_core::core::OperationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{AmountUnit, Amounts};
use fedimint_core::secp256k1::{Keypair, PublicKey};
use fedimint_core::util::backoff_util::aggressive_backoff;
use fedimint_core::{Amount, TransactionId};
use tracing::warn;

use crate::AmmClientContext;
use crate::api::AmmFederationApi;

/// Bound on how many times a swap will retry a failed or rejected Tx2 before
/// giving up and settling into the terminal [`SwapState::Tx2Rejected`] (fix
/// pass 3, Important 1 and Important 3). Chosen comfortably below
/// `aggressive_backoff()`'s own `max_retries_or: Some(14)` (verified by
/// reading `fedimint_core::util::backoff_util::aggressive_backoff`), so
/// [`retry_delay`] never runs off the end of that iterator.
const MAX_TX2_ATTEMPTS: u32 = 8;

/// The backoff delay for the `attempt`-th retry (1-indexed: `attempt == 1` is
/// the first retry, taken after the first failure). Built fresh each call —
/// `aggressive_backoff()`'s `FibonacciBackoff` is a plain, non-random-seeded
/// (beyond per-element jitter) sequence, so indexing into a fresh instance
/// with `.nth(attempt - 1)` deterministically reproduces the delay for that
/// position in the sequence.
fn retry_delay(attempt: u32) -> Duration {
    aggressive_backoff()
        .nth(usize::try_from(attempt.saturating_sub(1)).unwrap_or(usize::MAX))
        .unwrap_or(Duration::from_secs(5))
}

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
    Tx1Submitted {
        txid: TransactionId,
    },
    Tx1Rejected(String),
    /// Waiting to re-read the balance and build/submit Tx2. `attempts` is
    /// the number of prior Tx2 attempts that have already failed or been
    /// rejected (`0` the first time this state is entered, from
    /// `Tx1Submitted`).
    Tx1Accepted {
        attempts: u32,
    },
    /// Tx2 failed to build/submit locally (`claim_inputs` returned `Err`) or
    /// was rejected by consensus; will back off and retry from
    /// `Tx1Accepted`, unless `attempts` has reached [`MAX_TX2_ATTEMPTS`], in
    /// which case the previous transition already chose
    /// [`SwapState::Tx2Rejected`] instead of this state. `error` is
    /// retained for logging only.
    Tx2Failed {
        attempts: u32,
        error: String,
    },
    Tx2Submitted {
        txid: TransactionId,
        attempts: u32,
    },
    /// Terminal: only reached once [`MAX_TX2_ATTEMPTS`] retries have been
    /// exhausted (fix pass 3, Important 3) — not on the first rejection,
    /// unlike the original design.
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
                                Ok(()) => old_state.update(SwapState::Tx1Accepted { attempts: 0 }),
                                Err(error) => old_state.update(SwapState::Tx1Rejected(error)),
                            }
                        })
                    },
                )]
            }
            SwapState::Tx1Accepted { attempts } => {
                let attempts = *attempts;
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
                            attempts,
                            global_for_transition.clone(),
                            old_state,
                        ))
                    },
                )]
            }
            SwapState::Tx2Failed { attempts, .. } => {
                let attempts = *attempts;
                vec![StateTransition::new(
                    async move { fedimint_core::runtime::sleep(retry_delay(attempts)).await },
                    move |_dbtx, (), old_state: Self| {
                        Box::pin(
                            async move { old_state.update(SwapState::Tx1Accepted { attempts }) },
                        )
                    },
                )]
            }
            SwapState::Tx2Submitted { txid, attempts } => {
                let txid = *txid;
                let attempts = *attempts;
                let global = global_context.clone();
                vec![StateTransition::new(
                    async move { global.await_tx_accepted(txid).await },
                    move |_dbtx, result: Result<(), String>, old_state: Self| {
                        Box::pin(async move {
                            match result {
                                Ok(()) => old_state.update(SwapState::Done),
                                Err(error) => {
                                    old_state.update(next_after_tx2_failure(attempts, error))
                                }
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

/// Shared by both Tx2 failure causes (`claim_inputs` erroring locally, and
/// Tx2 being rejected by consensus): retry via [`SwapState::Tx2Failed`] while
/// under [`MAX_TX2_ATTEMPTS`], otherwise give up via the terminal
/// [`SwapState::Tx2Rejected`] (fix pass 3, Important 1 and Important 3).
fn next_after_tx2_failure(prior_attempts: u32, error: String) -> SwapState {
    let attempts = prior_attempts + 1;
    if attempts >= MAX_TX2_ATTEMPTS {
        SwapState::Tx2Rejected(format!(
            "giving up after {attempts} attempts to complete Tx2: {error}"
        ))
    } else {
        SwapState::Tx2Failed { attempts, error }
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
/// Uses [`crate::api::AmmFederationApi::amm_balance`], a point lookup (fix
/// pass 3, Important 5) — not a paginated scan of `BALANCE_RECOVERY_ENDPOINT`,
/// which would cost a `ThresholdConsensus` round *per page* against a table
/// that mutates on every swap in the federation, and which an attacker could
/// grow arbitrarily for one `min_swap_in` per row (see that endpoint's doc
/// comment).
///
/// Returns `None` if no balance is found at all. Given Tx1 already landed
/// (this only runs from [`SwapState::Tx1Accepted`]), the one legitimate way
/// for that to happen is a crash or retry that struck after Tx2 was already
/// accepted by consensus but before this state machine recorded reaching
/// [`SwapState::Tx2Submitted`] — i.e. the swap already finished on a previous
/// attempt. Spec §6.1 documents this as intentional: "if a claim lands and
/// the client resubmits, the second attempt finds no balance and is rejected
/// outright." [`transition_tx1_accepted`] treats `None` the same way,
/// completing the operation as [`SwapState::Done`] instead of resubmitting.
///
/// Retries indefinitely on network/API errors, via its own loop rather than
/// `fedimint_core::util::retry` (which would need an `.expect()` to unwrap
/// its `Result` — avoided here per this crate's no-`expect`-in-non-test-code
/// rule, even though `api_networking_backoff()`'s `max_retries_or: None`
/// means that particular `.expect()` could never actually fire in practice).
/// There is no failure state to fall back to here, by design (spec §6.3): a
/// swap can only ever finish or keep waiting, never be abandoned. This is
/// deliberately unbounded, unlike [`MAX_TX2_ATTEMPTS`]: a network/API error
/// here means the request never reached (or never came back from) the
/// federation at all, so nothing has been attempted yet to count against
/// that bound.
async fn await_own_balance(
    module_api: DynModuleApi,
    recipient_pk: PublicKey,
    unit: AmountUnit,
) -> Option<Amount> {
    let mut backoff = fedimint_core::util::backoff_util::api_networking_backoff();
    loop {
        match module_api
            .amm_balance(BalanceRequest {
                pubkey: recipient_pk,
                unit,
            })
            .await
        {
            Ok(balance) => return balance,
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

/// Builds the `ClientInput` for Tx2's `ClaimBalanceV0`, given the balance
/// [`await_own_balance`] just re-read. Pure and separated out from
/// [`transition_tx1_accepted`] so the one property that matters here — the
/// declared amount equals the balance just re-read, not some other value —
/// is directly unit-testable (fix pass 3, Important 7c) without a live
/// executor or federation.
fn build_claim_input(keypair: Keypair, unit: AmountUnit, balance: Amount) -> ClientInput<AmmInput> {
    ClientInput {
        input: AmmInput::ClaimBalanceV0 {
            pubkey: keypair.public_key(),
            unit,
        },
        keys: vec![keypair],
        amounts: Amounts::new_custom(unit, balance),
    }
}

/// Builds and submits Tx2 (`ClaimBalanceV0`) once the current balance is
/// known, or completes the operation if the balance is already gone (see
/// [`await_own_balance`]'s doc comment).
async fn transition_tx1_accepted(
    dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
    balance: Option<Amount>,
    attempts: u32,
    global_context: DynGlobalClientContext,
    old_state: SwapStateMachine,
) -> SwapStateMachine {
    let Some(balance) = balance else {
        return old_state.update(SwapState::Done);
    };

    let unit_out = old_state.common.unit_out;
    let keypair = old_state.common.recipient_keypair;

    let client_input = build_claim_input(keypair, unit_out, balance);

    // `claim_inputs` routes to `finalize_and_submit_transaction_inner`
    // (`fedimint-client/src/client/global_ctx.rs:48-63`, verified against the
    // pinned source), which can fail for more than one reason: the primary
    // module registered for `unit_out` being unable to fund/balance the
    // transaction, `MAX_TX_SIZE` being exceeded, or `AddStateMachinesError`
    // from the executor's own bookkeeping. None of these are consensus
    // rejections — `SwapState::Tx2Rejected` (via `Tx2Submitted`) is for that,
    // once Tx2 actually reaches the federation.
    //
    // On failure this moves to `SwapState::Tx2Failed` (or, once
    // `MAX_TX2_ATTEMPTS` is exhausted, the terminal `Tx2Rejected`) rather
    // than returning `old_state` unchanged (fix pass 3, Important 1):
    // returning the same state the executor is currently running would make
    // this a self-transition, which the pinned executor
    // (`fedimint-client/src/sm/executor.rs:717-800`, verified by reading it)
    // handles by inserting the identical key into both its active and
    // inactive state tables and racing a fresh "new state" notification
    // against its own "transition complete" event on `sm_update_tx` before
    // returning `Completed`. `tokio::select!` (`:621-633`) picks between
    // those two ready branches at random: on the branch order that processes
    // the "new state" notification before its own completion, `currently_running_sms`
    // still contains the (identical) state, so the notification is logged as
    // "already running" and silently dropped (`:773-777`) — stalling the
    // swap in `Tx1Accepted` until the client restarts. On the other branch
    // order it re-runs immediately with no backoff, hot-looping a paginated
    // re-read against the federation. `Tx2Failed` is a genuinely distinct
    // state, so this is a real state change every time, and its own
    // transition (see `transitions()` above) sleeps before returning to
    // `Tx1Accepted`.
    match global_context
        .claim_inputs(dbtx, ClientInputBundle::new_no_sm(vec![client_input]))
        .await
    {
        Ok(range) => old_state.update(SwapState::Tx2Submitted {
            txid: range.txid(),
            attempts,
        }),
        Err(error) => {
            let error = error.to_string();
            warn!(%error, attempts, "Failed to submit swap Tx2 (ClaimBalanceV0); will retry");
            old_state.update(next_after_tx2_failure(attempts, error))
        }
    }
}

#[cfg(test)]
mod tests {
    use fedimint_core::BitcoinHash as _;
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::module::AmountUnit;
    use fedimint_core::module::registry::ModuleDecoderRegistry;
    use fedimint_core::secp256k1::{Keypair, SECP256K1};

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

    /// `Tx1Rejected`, `Tx2Rejected` and `Done` are terminal: no further work
    /// is ever scheduled once a swap reaches one of them.
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

    /// Every non-terminal state, including the two retry states added in fix
    /// pass 3, must each schedule exactly one pending transition — this is
    /// what makes the swap actually progress rather than stalling silently.
    #[test]
    fn non_terminal_states_schedule_a_transition() {
        for state in [
            SwapState::Tx1Submitted {
                txid: TransactionId::from_byte_array([0u8; 32]),
            },
            SwapState::Tx1Accepted { attempts: 0 },
            SwapState::Tx1Accepted { attempts: 3 },
            SwapState::Tx2Failed {
                attempts: 1,
                error: "boom".to_string(),
            },
            SwapState::Tx2Submitted {
                txid: TransactionId::from_byte_array([0u8; 32]),
                attempts: 0,
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
            SwapState::Tx1Accepted { attempts: 0 },
            SwapState::Tx1Accepted { attempts: 5 },
            SwapState::Tx2Failed {
                attempts: 2,
                error: "boom".to_string(),
            },
            SwapState::Tx2Submitted {
                txid: TransactionId::from_byte_array([4u8; 32]),
                attempts: 1,
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
    /// means Tx2 already succeeded on a previous attempt (spec §6.1's
    /// documented retry semantics) — the operation must complete as `Done`,
    /// not get stuck retrying a claim that will only ever find nothing.
    ///
    /// This calls the real `transition_tx1_accepted`, not a copy of it: the
    /// `None` branch returns before touching either fallible argument, so a
    /// fake `DynGlobalClientContext` (whose every other method panics) and a
    /// `ClientSMDatabaseTransaction` over a throwaway in-memory database are
    /// both safe to pass here, unlike for the `Some(balance)` branch, which
    /// calls `claim_inputs` and would need a fully wired executor to exercise
    /// for real.
    #[tokio::test]
    async fn tx1_accepted_completes_when_balance_already_claimed() {
        let old_state = sm(SwapState::Tx1Accepted { attempts: 0 });

        let db = fedimint_core::db::Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            ModuleDecoderRegistry::default(),
        );
        let mut dbtx = db.begin_transaction_nc().await;
        let mut sm_dbtx = ClientSMDatabaseTransaction::new(&mut dbtx, 0);

        let next = transition_tx1_accepted(
            &mut sm_dbtx,
            None,
            0,
            DynGlobalClientContext::new_fake(),
            old_state,
        )
        .await;

        assert_eq!(next.state, SwapState::Done);
    }

    /// Fix pass 3, Important 1 and Important 3: a failed or rejected Tx2
    /// must produce a genuinely different state, not `old_state` unchanged —
    /// otherwise the executor treats it as a self-transition, which (verified
    /// against the pinned `fedimint-client/src/sm/executor.rs:717-800`)
    /// either stalls the swap or hot-loops it. `next_after_tx2_failure` is
    /// the one place both failure causes (`claim_inputs` erroring locally,
    /// and Tx2 being rejected by consensus) go through, so this test is
    /// sufficient to cover both call sites.
    #[test]
    fn tx2_failure_moves_to_a_distinct_retry_state_under_the_attempt_bound() {
        let next = next_after_tx2_failure(0, "boom".to_string());
        match next {
            SwapState::Tx2Failed { attempts, .. } => assert_eq!(attempts, 1),
            other => panic!("expected Tx2Failed, got {other:?}"),
        }
    }

    /// Fix pass 3, Important 3: rejection is not immediately terminal — a
    /// claimable balance must not be stranded on the first failure. This
    /// asserts a swap survives several rejections in a row while under the
    /// bound, each one incrementing `attempts` rather than resetting or
    /// ignoring it.
    #[test]
    fn tx2_failure_retries_repeatedly_while_under_the_attempt_bound() {
        let mut attempts = 0;
        for _ in 0..MAX_TX2_ATTEMPTS - 1 {
            match next_after_tx2_failure(attempts, "boom".to_string()) {
                SwapState::Tx2Failed { attempts: next, .. } => attempts = next,
                other => panic!("expected Tx2Failed while under the bound, got {other:?}"),
            }
        }
        assert_eq!(attempts, MAX_TX2_ATTEMPTS - 1);
    }

    /// Fix pass 3, Important 3: retries are bounded, not infinite — once
    /// `MAX_TX2_ATTEMPTS` is reached the swap gives up via the terminal
    /// `Tx2Rejected` rather than retrying forever.
    #[test]
    fn tx2_failure_becomes_terminal_once_the_attempt_bound_is_reached() {
        let next = next_after_tx2_failure(MAX_TX2_ATTEMPTS - 1, "boom".to_string());
        match next {
            SwapState::Tx2Rejected(error) => assert!(error.contains("giving up")),
            other => panic!("expected terminal Tx2Rejected, got {other:?}"),
        }
    }

    /// Fix pass 3, Important 3: a swap that has just retried from
    /// `Tx2Failed` must land back in `Tx1Accepted` — the state that
    /// self-heals by re-reading the balance (`await_own_balance` returning
    /// `None` resolves to `Done`, `Some` re-claims) — carrying the same
    /// `attempts` count forward rather than resetting it.
    ///
    /// This invokes the real `transition` function `Tx2Failed`'s
    /// `StateTransition` is built with (not a copy of it), skipping only the
    /// `trigger` future itself (which sleeps for `retry_delay`) by feeding
    /// its transition function the `()` value that trigger eventually
    /// produces — exactly what the executor would give it once the sleep
    /// elapses.
    #[tokio::test]
    async fn tx2_failed_transition_returns_to_tx1_accepted_with_attempts_preserved() {
        let context = AmmClientContext;
        let global = DynGlobalClientContext::new_fake();
        let old_state = sm(SwapState::Tx2Failed {
            attempts: 3,
            error: "boom".to_string(),
        });
        let transitions = old_state.transitions(&context, &global);
        assert_eq!(transitions.len(), 1);

        let db = fedimint_core::db::Database::new(
            fedimint_core::db::mem_impl::MemDatabase::new(),
            ModuleDecoderRegistry::default(),
        );
        let mut dbtx = db.begin_transaction_nc().await;
        let mut sm_dbtx = ClientSMDatabaseTransaction::new(&mut dbtx, 0);

        let unit_value = serde_json::to_value(()).expect("() serializes");
        let next = (transitions[0].transition)(&mut sm_dbtx, unit_value, old_state).await;

        assert_eq!(next.state, SwapState::Tx1Accepted { attempts: 3 });
    }

    /// [`build_claim_input`] is the one place Tx2's declared amount is set
    /// (fix pass 3, Important 7c): it must equal the balance
    /// `await_own_balance` just re-read, not some other value (a cached
    /// quote, the swap's original `amount_in`, etc.), and the input must
    /// authorize with the same keypair that owns `unit`.
    #[test]
    fn build_claim_input_declares_exactly_the_balance_just_read() {
        let keypair = keypair(7);
        let unit = AmountUnit::new_custom(2);
        let balance = Amount::from_msats(123_456);

        let input = build_claim_input(keypair, unit, balance);

        assert_eq!(input.keys, vec![keypair]);
        assert_eq!(input.amounts, Amounts::new_custom(unit, balance));
        match input.input {
            AmmInput::ClaimBalanceV0 {
                pubkey,
                unit: got_unit,
            } => {
                assert_eq!(pubkey, keypair.public_key());
                assert_eq!(got_unit, unit);
            }
            other => panic!("expected ClaimBalanceV0, got {other:?}"),
        }
    }

    /// A different re-read balance must produce a different declared amount
    /// — guards against a hardcoded or stale value sneaking into
    /// [`build_claim_input`].
    #[test]
    fn build_claim_input_tracks_the_balance_argument_not_a_fixed_value() {
        let keypair = keypair(8);
        let unit = AmountUnit::new_custom(3);

        let small = build_claim_input(keypair, unit, Amount::from_msats(10));
        let large = build_claim_input(keypair, unit, Amount::from_msats(10_000));

        assert_ne!(small.amounts, large.amounts);
    }

    #[test]
    fn retry_delay_stays_within_aggressive_backoffs_bounds() {
        for attempt in 1..=MAX_TX2_ATTEMPTS {
            let delay = retry_delay(attempt);
            assert!(
                delay <= Duration::from_secs(5),
                "attempt {attempt}: {delay:?}"
            );
        }
    }
}
