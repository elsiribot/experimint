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
//!      |              ^    |
//!      v              |    v
//! Tx1Rejected         +-Tx2Failed
//!                        (backoff,
//!                         retries forever)
//! ```
//!
//! `Tx2Failed` is a single retry state reached from two different failure
//! causes — `claim_inputs` failing locally (fix pass 3, Important 1) and Tx2
//! being rejected by consensus (fix pass 3, Important 3) — because both are
//! resolved the same way: back off, then re-enter `Tx1Accepted`, which
//! re-reads the balance and tries again. `await_own_balance` returning `None`
//! there resolves to `Done` (the balance is already gone, so a previous
//! attempt must have landed); `Some` re-attempts the claim.
//!
//! **There is deliberately no attempt bound and no terminal Tx2 failure
//! state** (fix pass 4, Critical 2 — reverting a bound introduced in fix pass
//! 3 and caught in re-review). A `Balance` created by Tx1 is permanently
//! claimable with "no deadline, no expiry sweep, no refund path" (spec §6.3,
//! word for word), so a client that gives up pursuing one strands it forever
//! — a terminal state is inactive, and nothing else in this module will ever
//! resume the claim. The retry loop instead runs on
//! [`fedimint_core::util::backoff_util::background_backoff`]
//! (`max_retries_or: None`, verified by reading that function's own
//! definition — its own doc comment says "starts at 1s and increases to
//! 60s", but see [`retry_delay`]'s doc comment for why the actual steady-state
//! delay this crate observed by testing is closer to 90s): every retry
//! passes through a real, growing sleep in [`SwapState::Tx2Failed`]'s own
//! transition before landing back in `Tx1Accepted`, so this cannot hot-loop
//! the federation, and it never runs out of attempts to give up on.

use std::time::Duration;

use fedimint_amm_common::endpoints::BalanceRequest;
use fedimint_amm_common::types::AmmInput;
use fedimint_api_client::api::DynModuleApi;
use fedimint_client_module::DynGlobalClientContext;
use fedimint_client_module::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_client_module::transaction::{ClientInput, ClientInputBundle};
use fedimint_core::core::OperationId;
use fedimint_core::db::IDatabaseTransactionOpsCoreTyped as _;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{AmountUnit, Amounts};
use fedimint_core::secp256k1::{Keypair, PublicKey};
use fedimint_core::util::backoff_util::background_backoff;
use fedimint_core::{Amount, TransactionId};
use tracing::warn;

use crate::AmmClientContext;
use crate::api::AmmFederationApi;
use crate::db::{SwapOutcome, SwapOutcomeKey};

/// The backoff delay for the `attempt`-th retry (1-indexed: `attempt == 1` is
/// the first retry, taken after the first failure). Built fresh each call —
/// `background_backoff()`'s `FibonacciBackoff` never exhausts
/// (`max_retries_or: None`), but each element is still independently
/// jittered (`.with_jitter()`, verified by reading
/// `backoff_util::custom_backoff`), so unlike a fixed schedule, indexing a
/// fresh instance with `.nth(attempt - 1)` draws an independently jittered
/// value from that position in the sequence each time it is called — not a
/// deterministic replay of whatever delay was used the first time this
/// `attempt` was reached. That is fine here: nothing depends on two calls at
/// the same `attempt` agreeing, only on the delay growing roughly with
/// `attempt` and settling at a bounded plateau.
///
/// **That plateau is roughly 90s, not the 60s `background_backoff()`'s own
/// doc comment describes** — checked by running this exact function up to a
/// large `attempt` (see the test below) rather than trusting the "increases
/// to 60s" phrasing at face value. The vendored `backon` crate's
/// `FibonacciBackoff::next` (`backon-1.6.0/src/backoff/fibonacci.rs`) checks
/// its stored `current_delay` against `max_delay` *before* adding the next
/// Fibonacci term, not after, so the term that first pushes the running
/// value past `max_delay` is still returned and then kept forever — for
/// `background_backoff()`'s `min_delay = 1s`, the Fibonacci sequence
/// 1,1,2,3,5,8,13,21,34,55,89,... first exceeds the nominal 60s cap at 89s,
/// so that is where it actually plateaus (plus up to `min_delay` = 1s of
/// jitter on top). The exact number does not matter to this module — only
/// that it is a bounded, non-hot-looping sleep — but a comment claiming
/// "60s" here would be exactly the kind of unverified claim this crate is
/// trying to stop shipping.
fn retry_delay(attempt: u32) -> Duration {
    background_backoff()
        .nth(usize::try_from(attempt.saturating_sub(1)).unwrap_or(usize::MAX))
        .unwrap_or(Duration::from_secs(90))
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
    /// was rejected by consensus; always backs off and retries from
    /// `Tx1Accepted` (fix pass 4, Critical 2 — there is no attempt bound and
    /// no way out of this cycle other than the claim eventually succeeding,
    /// per this module's doc comment). `error` is retained for logging only.
    Tx2Failed {
        attempts: u32,
        error: String,
    },
    Tx2Submitted {
        txid: TransactionId,
        attempts: u32,
    },
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
                    |dbtx, result: Result<(), String>, old_state: Self| {
                        Box::pin(async move {
                            match result {
                                Ok(()) => old_state.update(SwapState::Tx1Accepted { attempts: 0 }),
                                Err(error) => {
                                    record_outcome(
                                        dbtx,
                                        old_state.common.operation_id,
                                        SwapOutcome::Tx1Rejected(error.clone()),
                                    )
                                    .await;
                                    old_state.update(SwapState::Tx1Rejected(error))
                                }
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
            SwapState::Tx1Rejected(_) | SwapState::Done => vec![],
        }
    }

    fn operation_id(&self) -> OperationId {
        self.common.operation_id
    }
}

/// Shared by both Tx2 failure causes (`claim_inputs` erroring locally, and
/// Tx2 being rejected by consensus): always retries via
/// [`SwapState::Tx2Failed`] (fix pass 4, Critical 2 — no attempt bound, no
/// terminal failure state). An earlier version of this function gave up
/// after a fixed number of attempts and moved to a terminal
/// `Tx2Rejected` state; that stranded the balance permanently the moment a
/// client hit the bound, contradicting spec §6.3's "no deadline, no expiry
/// sweep, no refund path" — a terminal state schedules no further
/// transitions, so nothing would ever have claimed that balance again. See
/// this module's doc comment for why unconditional retry cannot hot-loop.
fn next_after_tx2_failure(prior_attempts: u32, error: String) -> SwapState {
    SwapState::Tx2Failed {
        attempts: prior_attempts + 1,
        error,
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
/// swap can only ever finish or keep waiting, never be abandoned — the same
/// reason [`next_after_tx2_failure`] has no attempt bound either (fix pass 4,
/// Critical 2).
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

/// Writes `outcome` for `operation_id`, in the same database transaction as
/// the state change that established it.
///
/// Overwrites rather than insisting on a first write: [`SwapState::Tx1Accepted`]
/// is re-entered on every Tx2 retry and re-reads the balance each time, which
/// can legitimately have grown since the last attempt (see
/// [`await_own_balance`] on gifts credited to the recipient key after Tx1
/// landed). The freshest read is the one Tx2 actually claims, so it is the one
/// worth keeping.
async fn record_outcome(
    dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
    operation_id: OperationId,
    outcome: SwapOutcome,
) {
    dbtx.module_tx()
        .insert_entry(&SwapOutcomeKey(operation_id), &outcome)
        .await;
}

/// Builds the `ClientInput` for Tx2's `ClaimBalanceV0`, given the balance
/// [`await_own_balance`] just re-read. Pure and separated out from
/// [`transition_tx1_accepted`] so the one property that matters here — the
/// declared amount equals the balance just re-read, not some other value —
/// is directly unit-testable (fix pass 3, Important 7c) without a live
/// executor or federation.
///
/// This is deliberately the full extent of in-crate coverage for
/// `transition_tx1_accepted`'s `Some(balance)` branch (fix pass 3/4, Minor):
/// the branch itself calls `global_context.claim_inputs(..)`, and the fake
/// `DynGlobalClientContext` this crate's tests use (backed by `()`) has
/// `claim_inputs_dyn` as `unimplemented!("fake implementation, only for
/// tests")` (verified by reading the pinned
/// `fedimint-client-module/src/lib.rs`), so nothing short of a live executor
/// and primary-module wiring can actually drive that branch end to end. This
/// is not something the attempt-bound removal in Critical 2 changes — the
/// two are unrelated (one is about `next_after_tx2_failure`'s retry count,
/// this is about `claim_inputs` itself) — so it is stated here plainly rather
/// than implied by proximity to the tests that do exist.
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
    // rejections — the `Err` branch of `Tx2Submitted`'s own transition (see
    // `transitions()` above) is for that, once Tx2 actually reaches the
    // federation; both causes funnel through `next_after_tx2_failure` either
    // way.
    //
    // On failure this moves to `SwapState::Tx2Failed` (fix pass 4, Critical
    // 2 removed the attempt bound this comment used to describe here; see
    // `next_after_tx2_failure` and this module's doc comment) rather than
    // returning `old_state` unchanged (fix pass 3, Important 1): returning
    // the same state the executor is currently running would make this a
    // self-transition, which the pinned executor
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
    // order it re-runs immediately with no backoff, hot-looping a re-read
    // (the point-lookup `amm_balance` call, not a paginated scan — see
    // `await_own_balance`'s doc comment) against the federation. `Tx2Failed`
    // is a genuinely distinct state, so this is a real state change every
    // time, and its own transition (see `transitions()` above) sleeps before
    // returning to `Tx1Accepted`.
    match global_context
        .claim_inputs(dbtx, ClientInputBundle::new_no_sm(vec![client_input]))
        .await
    {
        Ok(range) => {
            // Same transaction as the state change that establishes it, so
            // there is no window where Tx2 is on its way but the amount it
            // claims went unrecorded.
            record_outcome(
                dbtx,
                old_state.common.operation_id,
                SwapOutcome::Settled {
                    amount_out: balance,
                },
            )
            .await;
            old_state.update(SwapState::Tx2Submitted {
                txid: range.txid(),
                attempts,
            })
        }
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

    /// `Tx1Rejected` and `Done` are terminal: no further work is ever
    /// scheduled once a swap reaches one of them. Tx2 deliberately has no
    /// terminal failure state at all (fix pass 4, Critical 2) — every Tx2
    /// failure keeps scheduling work forever, which
    /// `non_terminal_states_schedule_a_transition` below covers.
    #[test]
    fn rejected_and_done_states_have_no_transitions() {
        for state in [SwapState::Tx1Rejected("boom".to_string()), SwapState::Done] {
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
    fn tx2_failure_moves_to_a_distinct_retry_state() {
        let next = next_after_tx2_failure(0, "boom".to_string());
        match next {
            SwapState::Tx2Failed { attempts, .. } => assert_eq!(attempts, 1),
            other => panic!("expected Tx2Failed, got {other:?}"),
        }
    }

    /// Fix pass 4, Critical 2: there is no attempt bound — `Tx2Failed` must
    /// keep retrying no matter how many times it has already failed, each
    /// time incrementing `attempts` rather than resetting, ignoring it, or
    /// (the bug this test guards against re-introducing) ever producing
    /// something other than `Tx2Failed`. A hundred iterations stands in for
    /// "arbitrarily many" — the old bound was 8, so this comfortably shows
    /// retrying continues well past where the reverted design gave up.
    #[test]
    fn tx2_failure_retries_forever_without_ever_becoming_terminal() {
        let mut attempts = 0;
        for _ in 0..100 {
            match next_after_tx2_failure(attempts, "boom".to_string()) {
                SwapState::Tx2Failed { attempts: next, .. } => attempts = next,
                other => panic!("expected Tx2Failed, never anything else, got {other:?}"),
            }
        }
        assert_eq!(attempts, 100);
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

    /// [`retry_delay`] must plateau at a bounded value no matter how large
    /// `attempt` gets — this is what makes it safe to call with an
    /// ever-growing, never-reset `attempts` counter (fix pass 4, Critical 2:
    /// there is no bound on how high `attempts` can climb). `100s` is not
    /// `background_backoff()`'s own nominal `60s` max_delay: see
    /// [`retry_delay`]'s doc comment for why the actual plateau this crate
    /// measured is closer to 90s (89s of Fibonacci growth plus up to 1s of
    /// jitter), and this bound is set a comfortable margin above that
    /// measured value rather than the unverified 60s figure.
    #[test]
    fn retry_delay_plateaus_at_a_bounded_value() {
        for attempt in [1, 2, 3, 8, 100, 10_000] {
            let delay = retry_delay(attempt);
            assert!(
                delay <= Duration::from_secs(100),
                "attempt {attempt}: {delay:?}"
            );
        }
    }
}
