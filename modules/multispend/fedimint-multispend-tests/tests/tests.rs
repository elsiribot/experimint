//! End-to-end integration tests for the `multispend` module against a real,
//! consensus-driven federation running `mintv2` (primary, BITCOIN), `dummy`
//! (bootstrapping BITCOIN from nothing), `multispend` itself, and the
//! test-only `msfaucet` (bootstrapping a second `AmountUnit`).
//!
//! Per spec §9.3 the module has no client and does not use
//! `TransactionBuilder`: every transaction here is assembled by hand and
//! submitted directly, which is exactly what the real client will have to
//! do — the witness must sign the txid, and the txid is not knowable before
//! `build()` generates its own nonce and `finalize_transaction` adds change.
//!
//! This is also the first `SelfVerified` module ever run against a real
//! federation: core performs no authorization check of its own for a
//! `multispend` `Spend` input, so if the plumbing between the module and
//! core were wrong, it would show up here, not in the module's own unit
//! tests.

use std::collections::BTreeMap;

use fedimint_api_client::api::FederationApiExt as _;
use fedimint_core::Amount;
use fedimint_core::config::ClientConfig;
use fedimint_core::core::{DynInput, DynOutput, ModuleInstanceId, ModuleKind};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::module::{AmountUnit, Amounts, ApiRequestErased, CommonModuleInit};
use fedimint_core::secp256k1::rand::rngs::OsRng;
use fedimint_core::secp256k1::{Keypair, Secp256k1};
use fedimint_core::transaction::{
    Transaction, TransactionError, TransactionSignature, TransactionSubmissionOutcome,
};
use fedimint_core::{TransactionId, secp256k1};
use fedimint_dummy_common::{DummyCommonInit, DummyInput, DummyOutput};
use fedimint_mintv2_common::MintCommonInit;
use fedimint_multispend_common::MultispendCommonInit;
use fedimint_multispend_common::account::{Account, AccountId, AccountUnchecked};
use fedimint_multispend_common::endpoints::{ACCOUNT_BALANCE_ENDPOINT, AccountBalance};
use fedimint_multispend_common::types::{
    MultispendInput, MultispendInputError, MultispendOutput, SpendWitness,
};
use fedimint_multispend_tests::faucet::common::{
    FaucetCommonInit, FaucetInput, FaucetOutput, faucet_unit,
};
use fedimint_multispend_tests::fixtures::new_federation;
use fedimint_testing::federation::FederationTest;

// ---------------------------------------------------------------------------
// Shared helpers. Every test uses `submit` and `balance`; getting `submit`
// right is the whole difficulty of this task — see its own doc comment for
// how it differs from the brief's illustrative version and why.
// ---------------------------------------------------------------------------

/// Assemble a transaction, sign each input's witness, and submit it.
///
/// `signers[i]` are the keypairs authorizing `inputs[i]`. Two shapes exist,
/// distinguished by which module the input belongs to (`inputs[i]`'s own
/// `module_instance_id`, not by the length of `signers[i]` — see below for
/// why the brief's original "empty means not multispend" rule does not
/// survive contact with a real federation):
///
/// - A `multispend` `Spend` input (`module_instance_id == multispend`) is
///   `InputAuth::SelfVerified`: `signers[i]` is its full t-of-n set, each
///   `(index, keypair)` pair producing one entry of a [`SpendWitness`].
/// - Every other input in this crate's tests (`dummy`, `msfaucet`) is
///   ordinary `InputAuth::Key`: core itself verifies a single raw 64-byte
///   schnorr signature over the txid against the key the module returned in
///   `InputMeta::auth` (see `fedimint-server`'s
///   `process_transaction_with_dbtx`). `signers[i]` must hold exactly one
///   keypair for these — the matching one for the input's `pub_key` field —
///   and its `u16` index is ignored.
///
/// Deviation from the brief: the brief's illustrative `submit` treats *any*
/// empty `signers[i]` as "not a multispend input, contributes an empty
/// witness" and otherwise always builds a `SpendWitness`. That does not
/// compile into a working transaction: `dummy`'s `DummyInput` (this crate's
/// only funding source for BITCOIN) declares `InputAuth::Key`, which core
/// verifies with a mandatory 64-byte signature regardless of which module
/// issued the input — an empty witness gets `TransactionError::
/// InvalidWitnessLength` from `Transaction::verify_key_witness`, not
/// acceptance. There was no way to keep the brief's "empty means no
/// witness" rule and also fund a deposit, so this version distinguishes by
/// the input's actual module instead, which is information `submit` already
/// has.
///
/// Also deviates by needing `multispend_id` as an explicit parameter (the
/// brief's version had no way to know which module id is `multispend`
/// without it) and by resolving the decoder registry and the transaction
/// submission API itself, both of which the brief left unspecified — see the
/// module doc comment and `decoders` below.
async fn submit(
    fed: &FederationTest,
    multispend_id: ModuleInstanceId,
    inputs: Vec<DynInput>,
    outputs: Vec<DynOutput>,
    signers: &[&[(u16, Keypair)]],
) -> Result<TransactionId, TransactionError> {
    assert_eq!(
        inputs.len(),
        signers.len(),
        "one signer slice per input, positionally aligned"
    );

    let nonce: [u8; 8] = [0x77; 8];
    let txid = Transaction::tx_hash_from_parts(&inputs, &outputs, nonce);
    let msg = secp256k1::Message::from_digest(*txid.as_ref());
    let secp = Secp256k1::new();

    let witnesses: Vec<Vec<u8>> = inputs
        .iter()
        .zip(signers.iter())
        .map(|(input, kps)| {
            if input.module_instance_id() == multispend_id {
                let signatures = kps
                    .iter()
                    .map(|(idx, kp)| (*idx, secp.sign_schnorr(&msg, kp)))
                    .collect::<BTreeMap<_, _>>();
                SpendWitness { signatures }.encode_to_vec()
            } else {
                assert_eq!(
                    kps.len(),
                    1,
                    "an ordinary Key-auth input needs exactly one signing keypair"
                );
                secp.sign_schnorr(&msg, &kps[0].1).as_ref().to_vec()
            }
        })
        .collect();

    let tx = Transaction {
        inputs,
        outputs,
        nonce,
        signatures: TransactionSignature::Witnessed(witnesses),
    };

    // Deliberately not TransactionBuilder: the witness must sign the txid,
    // and the txid is not knowable before build() picks its own nonce and
    // finalize_transaction adds change. Spec §9.3.
    let client = fed.new_client().await;
    let decoders = decoders(&client.config().await);

    let outcome: TransactionSubmissionOutcome = client
        .api()
        .submit_transaction(tx)
        .await
        .try_into_inner(&decoders)
        .expect("the federation must speak a decodable transaction-submission encoding");

    let txid = outcome.0?;
    // `submit_transaction` only validates and enqueues; wait for consensus
    // to actually order and apply it before a test goes on to read state
    // that depends on it (e.g. via `balance`). Bounded: confirmed by
    // deliberately reintroducing the "reported amount != debited amount"
    // bug `deposit_then_spend_two_of_three`'s overdraw check guards against
    // while writing these tests — a submission that gets wrongly accepted
    // but makes the federation insolvent trips `fedimint-server`'s own
    // balance-sheet audit assert, which panics every guardian and leaves
    // this txid permanently unaccepted. Without the timeout, `await_transaction`
    // retries forever and the test hangs instead of failing.
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.api().await_transaction(txid),
    )
    .await
    .expect(
        "the federation did not reach consensus on this transaction within 30s -- likely a \
             guardian panic (e.g. the balance-sheet audit going negative), check the test output \
             above for a panic message",
    );
    Ok(txid)
}

/// Read an account's balance in one unit through the module's API.
///
/// Deviation from the brief: `FederationTest` has no `api_request` method.
/// Reads go through an ordinary client's `DynGlobalApi`, scoped to the
/// module with `with_module`, exactly as `fedimint-amm-tests`'s own
/// `tests.rs` reads its `FEE_VOTES_ENDPOINT` (`request_current_consensus`
/// there too — this endpoint needs no guardian auth, per its own doc
/// comment: "an `AccountId` is a hash, and the balance of an account whose
/// id you already know is visible in consensus transactions anyway").
async fn balance(fed: &FederationTest, account: &Account, unit: AmountUnit) -> Amount {
    let client = fed.new_client().await;
    let multispend = module_id(&client.config().await, &fedimint_multispend_common::KIND);

    let state: Option<AccountBalance> = client
        .api()
        .with_module(multispend)
        .request_current_consensus(
            ACCOUNT_BALANCE_ENDPOINT.to_string(),
            ApiRequestErased::new(account.id()),
        )
        .await
        .expect("the endpoint must answer");

    state
        .map(|s| s.balance.get(&unit).copied().unwrap_or(Amount::ZERO))
        .unwrap_or(Amount::ZERO)
}

/// This federation's decoder registry, built from the module kinds actually
/// registered in `config`.
///
/// `Client::decoders()` cannot be reused for this: it is built by
/// `fedimint-client`'s own `client_decoders`, which silently *skips* any
/// module kind with no registered `ClientModuleInit`
/// (`fedimint-client/src/client.rs`'s own `client_decoders`, "Detected
/// configuration for unsupported module id... skipping") — exactly what
/// `multispend` and `msfaucet` are in this fixture, per spec §9.3 and
/// `crate::faucet`'s module doc comment. Without a decoder for `multispend`,
/// decoding a `TransactionSubmissionOutcome` carrying a `TransactionError::
/// Input(DynInputError)` tagged with `multispend`'s module instance id would
/// fail outright — which is exactly the error path every rejection test in
/// this file exercises. This crate controls the exact four module kinds a
/// fixture built from `fixtures()` can ever contain, so a plain match
/// covering all four (rather than a generic, config-driven registry lookup)
/// is enough.
fn decoders(config: &ClientConfig) -> ModuleDecoderRegistry {
    let modules = config
        .modules
        .iter()
        .map(|(id, cfg)| {
            let decoder = if cfg.kind == MintCommonInit::KIND {
                MintCommonInit::decoder()
            } else if cfg.kind == DummyCommonInit::KIND {
                DummyCommonInit::decoder()
            } else if cfg.kind == MultispendCommonInit::KIND {
                MultispendCommonInit::decoder()
            } else if cfg.kind == FaucetCommonInit::KIND {
                FaucetCommonInit::decoder()
            } else {
                panic!("unexpected module kind {} in the test fixture", cfg.kind);
            };
            (*id, (cfg.kind.clone(), decoder))
        })
        .collect::<BTreeMap<_, _>>();
    ModuleDecoderRegistry::from(modules)
}

/// The instance id this federation assigned to a module kind.
fn module_id(config: &ClientConfig, kind: &ModuleKind) -> ModuleInstanceId {
    config
        .modules
        .iter()
        .find_map(|(id, cfg)| (&cfg.kind == kind).then_some(*id))
        .unwrap_or_else(|| panic!("module kind {kind} is not registered in this federation"))
}

async fn multispend_id(fed: &FederationTest) -> ModuleInstanceId {
    let config = fed.new_client().await.config().await;
    module_id(&config, &fedimint_multispend_common::KIND)
}

async fn dummy_id(fed: &FederationTest) -> ModuleInstanceId {
    let config = fed.new_client().await.config().await;
    module_id(&config, &fedimint_dummy_common::KIND)
}

async fn faucet_id(fed: &FederationTest) -> ModuleInstanceId {
    let config = fed.new_client().await.config().await;
    module_id(&config, &fedimint_multispend_tests::faucet::common::KIND)
}

/// A fresh, uniformly random keypair.
fn fresh_keypair() -> Keypair {
    Keypair::new(&Secp256k1::new(), &mut OsRng)
}

/// A t-of-n multispend account over `n` fresh keys, and the keys themselves
/// in the same ascending order `Account::pub_key_at` (and hence a witness's
/// `u16` index) uses, so `kps[i]` is always the keypair for index `i`.
fn account_with_keys(n: usize, threshold: u64) -> (Account, Vec<Keypair>) {
    let secp = Secp256k1::new();
    let mut kps: Vec<Keypair> = (0..n).map(|_| Keypair::new(&secp, &mut OsRng)).collect();
    kps.sort_by_key(|kp| kp.public_key());

    let pub_keys = kps.iter().map(Keypair::public_key).collect();
    let account = Account::try_from(AccountUnchecked {
        pub_keys,
        threshold,
    })
    .expect("account_with_keys always builds a valid t-of-n account");
    (account, kps)
}

/// A `dummy` input, this crate's source of BITCOIN from nothing. Its
/// `InputAuth::Key` witness (a single raw signature by `kp`) is built by
/// `submit`, keyed off `kp` being the one entry in the matching
/// `signers[i]` slice.
fn dummy_input(instance: ModuleInstanceId, amount: Amount, kp: &Keypair) -> DynInput {
    DynInput::from_typed(
        instance,
        DummyInput {
            amount,
            unit: AmountUnit::BITCOIN,
            pub_key: kp.public_key(),
        },
    )
}

/// A `dummy` output, this crate's withdrawal sink for BITCOIN.
fn dummy_output(instance: ModuleInstanceId, amount: Amount) -> DynOutput {
    DynOutput::from_typed(
        instance,
        DummyOutput {
            amount,
            unit: AmountUnit::BITCOIN,
        },
    )
}

fn credit_output(multispend: ModuleInstanceId, account: AccountId, amounts: Amounts) -> DynOutput {
    DynOutput::from_typed(multispend, MultispendOutput::Credit { account, amounts })
}

fn spend_input(multispend: ModuleInstanceId, account: Account, amounts: Amounts) -> DynInput {
    DynInput::from_typed(multispend, MultispendInput::Spend { account, amounts })
}

/// Unwraps `err` as `TransactionError::Input`, downcasts the module error to
/// `MultispendInputError`, and returns it — so every rejection test asserts
/// on the module's own specific error variant rather than merely "submission
/// failed" (which would also pass for a malformed transaction).
fn multispend_error(err: &TransactionError) -> &MultispendInputError {
    let TransactionError::Input(dyn_err) = err else {
        panic!("expected TransactionError::Input, got {err:?}");
    };
    dyn_err
        .as_any()
        .downcast_ref::<MultispendInputError>()
        .unwrap_or_else(|| panic!("expected a MultispendInputError, got: {dyn_err}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Deposit BITCOIN into a 2-of-3 account, then spend part of it with 2 of
/// the 3 keys. Would fail if: the deposit/credit path were wired wrong, or
/// the spend path required all 3 keys instead of the threshold.
///
/// Also closes the coverage gap the task called out: nothing before this
/// asserted on the *amount* `process_input` reports back to core
/// (`process_spend`'s `Ok(amounts.clone())`). A same-value spend and
/// withdrawal (as above) can't expose a mutant that reports something other
/// than what it actually debited — e.g. doubling it — because the account's
/// stored balance is debited by the input's own `amounts` field directly,
/// never by whatever gets reported to core, and `fedimint-server`'s funding
/// verifier tolerates the input side being *larger* than the output side
/// (an implicit fee); confirmed by actually introducing that mutant against
/// this test while writing it, which passed unless the check below was
/// present. The check that does catch it: a spend that reports a small,
/// honest `amounts` but is paired with a *larger* withdrawal output must be
/// rejected as unbalanced — a doubling mutant would instead report enough
/// backing to cover it, wrongly accepting a withdrawal for more than the
/// account was actually debited.
#[tokio::test(flavor = "multi_thread")]
async fn deposit_then_spend_two_of_three() -> anyhow::Result<()> {
    let fed = new_federation().await;
    let multispend = multispend_id(&fed).await;
    let dummy = dummy_id(&fed).await;
    let (account, kps) = account_with_keys(3, 2);
    let depositor = fresh_keypair();

    // Deposit: a dummy input funds a Credit output.
    submit(
        &fed,
        multispend,
        vec![dummy_input(dummy, Amount::from_msats(10_000), &depositor)],
        vec![credit_output(
            multispend,
            account.id(),
            Amounts::new_bitcoin_msats(10_000),
        )],
        &[&[(0, depositor)]],
    )
    .await
    .expect("a deposit needs no multispend witness, only the dummy input's own signature");

    assert_eq!(
        balance(&fed, &account, AmountUnit::BITCOIN).await,
        Amount::from_msats(10_000)
    );

    // Spend: 2 of the 3 keys authorize a withdrawal to a dummy output.
    submit(
        &fed,
        multispend,
        vec![spend_input(
            multispend,
            account.clone(),
            Amounts::new_bitcoin_msats(4_000),
        )],
        vec![dummy_output(dummy, Amount::from_msats(4_000))],
        &[&[(0, kps[0]), (2, kps[2])]],
    )
    .await
    .expect("2 of 3 authorizes the spend");

    assert_eq!(
        balance(&fed, &account, AmountUnit::BITCOIN).await,
        Amount::from_msats(6_000)
    );

    // A spend that declares (and would debit) only 1,000 msats cannot fund
    // a 2,000 msat withdrawal. If `process_input` ever reported more than
    // the amount it actually debited, this would wrongly succeed.
    let overdraw = submit(
        &fed,
        multispend,
        vec![spend_input(
            multispend,
            account.clone(),
            Amounts::new_bitcoin_msats(1_000),
        )],
        vec![dummy_output(dummy, Amount::from_msats(2_000))],
        &[&[(0, kps[0]), (2, kps[2])]],
    )
    .await
    .expect_err("a 1,000 msat spend cannot fund a 2,000 msat withdrawal");

    assert!(
        matches!(overdraw, TransactionError::UnbalancedTransaction { .. }),
        "expected UnbalancedTransaction, got {overdraw:?}"
    );
    // The rejected overdraw must not have moved any funds: the balance is
    // still exactly what the earlier, legitimate 4,000 msat spend left.
    assert_eq!(
        balance(&fed, &account, AmountUnit::BITCOIN).await,
        Amount::from_msats(6_000)
    );
    Ok(())
}

/// Two keys that are not members of the account cannot authorize a spend
/// from it, even though they meet the account's threshold count. Would fail
/// (pass when it shouldn't) if `verify_input` checked only the *count* of
/// signatures and not that each one verifies against the account's actual
/// key at that index.
#[tokio::test(flavor = "multi_thread")]
async fn a_third_party_cannot_spend() -> anyhow::Result<()> {
    let fed = new_federation().await;
    let multispend = multispend_id(&fed).await;
    let dummy = dummy_id(&fed).await;
    let (account, _kps) = account_with_keys(3, 2);
    let depositor = fresh_keypair();

    submit(
        &fed,
        multispend,
        vec![dummy_input(dummy, Amount::from_msats(5_000), &depositor)],
        vec![credit_output(
            multispend,
            account.id(),
            Amounts::new_bitcoin_msats(5_000),
        )],
        &[&[(0, depositor)]],
    )
    .await
    .expect("deposit must succeed to set up the test");

    let outsider_a = fresh_keypair();
    let outsider_b = fresh_keypair();

    let err = submit(
        &fed,
        multispend,
        vec![spend_input(
            multispend,
            account.clone(),
            Amounts::new_bitcoin_msats(1_000),
        )],
        vec![dummy_output(dummy, Amount::from_msats(1_000))],
        &[&[(0, outsider_a), (1, outsider_b)]],
    )
    .await
    .expect_err("signatures from non-member keys must not authorize a spend");

    assert!(
        matches!(
            multispend_error(&err),
            MultispendInputError::InvalidSignature(_)
        ),
        "expected MultispendInputError::InvalidSignature, got {err:?}"
    );

    // The rejected spend must not have moved any funds.
    assert_eq!(
        balance(&fed, &account, AmountUnit::BITCOIN).await,
        Amount::from_msats(5_000)
    );
    Ok(())
}

/// A single signature on a 2-of-3 account is rejected before any signature
/// is even checked for validity — `ThresholdNotMet` names the shortfall
/// exactly. Would fail if the threshold check were off-by-one, or absent
/// entirely (letting a 1-of-N-in-appearance spend through).
#[tokio::test(flavor = "multi_thread")]
async fn below_threshold_is_rejected() -> anyhow::Result<()> {
    let fed = new_federation().await;
    let multispend = multispend_id(&fed).await;
    let dummy = dummy_id(&fed).await;
    let (account, kps) = account_with_keys(3, 2);
    let depositor = fresh_keypair();

    submit(
        &fed,
        multispend,
        vec![dummy_input(dummy, Amount::from_msats(5_000), &depositor)],
        vec![credit_output(
            multispend,
            account.id(),
            Amounts::new_bitcoin_msats(5_000),
        )],
        &[&[(0, depositor)]],
    )
    .await
    .expect("deposit must succeed to set up the test");

    let err = submit(
        &fed,
        multispend,
        vec![spend_input(
            multispend,
            account.clone(),
            Amounts::new_bitcoin_msats(1_000),
        )],
        vec![dummy_output(dummy, Amount::from_msats(1_000))],
        &[&[(0, kps[0])]],
    )
    .await
    .expect_err("one signature on a 2-of-3 account must not authorize a spend");

    assert_eq!(
        multispend_error(&err),
        &MultispendInputError::ThresholdNotMet {
            provided: 1,
            threshold: 2,
        },
        "expected ThresholdNotMet {{ provided: 1, threshold: 2 }}, got {err:?}"
    );

    assert_eq!(
        balance(&fed, &account, AmountUnit::BITCOIN).await,
        Amount::from_msats(5_000)
    );
    Ok(())
}

/// A `Spend(A)` and a `Credit(B)` of equal amounts in one transaction —
/// balanced by core's own funding verifier, with no other input or output
/// — moves value from A to B atomically. This is what replaces fedi's
/// separate transfer system, so it is the most important test here after
/// the first: would fail if a spend from one account could not fund a
/// credit to a *different* account in the same transaction, or if the two
/// account balances were not updated atomically.
#[tokio::test(flavor = "multi_thread")]
async fn account_to_account_transfer_in_one_transaction() -> anyhow::Result<()> {
    let fed = new_federation().await;
    let multispend = multispend_id(&fed).await;
    let dummy = dummy_id(&fed).await;
    let (account_a, kps_a) = account_with_keys(3, 2);
    let (account_b, _kps_b) = account_with_keys(2, 1);
    let depositor = fresh_keypair();

    submit(
        &fed,
        multispend,
        vec![dummy_input(dummy, Amount::from_msats(10_000), &depositor)],
        vec![credit_output(
            multispend,
            account_a.id(),
            Amounts::new_bitcoin_msats(10_000),
        )],
        &[&[(0, depositor)]],
    )
    .await
    .expect("deposit into A must succeed to set up the test");

    submit(
        &fed,
        multispend,
        vec![spend_input(
            multispend,
            account_a.clone(),
            Amounts::new_bitcoin_msats(3_000),
        )],
        vec![credit_output(
            multispend,
            account_b.id(),
            Amounts::new_bitcoin_msats(3_000),
        )],
        &[&[(0, kps_a[0]), (1, kps_a[1])]],
    )
    .await
    .expect("2 of 3 authorizes the transfer, and equal amounts balance with no other funding");

    assert_eq!(
        balance(&fed, &account_a, AmountUnit::BITCOIN).await,
        Amount::from_msats(7_000),
        "A must have fallen by exactly the transferred amount"
    );
    assert_eq!(
        balance(&fed, &account_b, AmountUnit::BITCOIN).await,
        Amount::from_msats(3_000),
        "B must have risen by exactly the transferred amount"
    );
    Ok(())
}

/// `Account::single` — a 1-of-1 — behaves as an ordinary personal wallet:
/// fund it, then spend all of it with the one key. Would fail if the t-of-n
/// machinery mishandled the degenerate `n = 1` case (e.g. an off-by-one in
/// the threshold check that only shows up at the boundary).
#[tokio::test(flavor = "multi_thread")]
async fn one_of_one_behaves_as_an_ordinary_wallet() -> anyhow::Result<()> {
    let fed = new_federation().await;
    let multispend = multispend_id(&fed).await;
    let dummy = dummy_id(&fed).await;
    let key = fresh_keypair();
    let account = Account::single(key.public_key());
    let depositor = fresh_keypair();

    submit(
        &fed,
        multispend,
        vec![dummy_input(dummy, Amount::from_msats(2_000), &depositor)],
        vec![credit_output(
            multispend,
            account.id(),
            Amounts::new_bitcoin_msats(2_000),
        )],
        &[&[(0, depositor)]],
    )
    .await
    .expect("deposit must succeed to set up the test");

    submit(
        &fed,
        multispend,
        vec![spend_input(
            multispend,
            account.clone(),
            Amounts::new_bitcoin_msats(2_000),
        )],
        vec![dummy_output(dummy, Amount::from_msats(2_000))],
        &[&[(0, key)]],
    )
    .await
    .expect("the single key authorizes the whole spend");

    assert_eq!(
        balance(&fed, &account, AmountUnit::BITCOIN).await,
        Amount::ZERO
    );
    Ok(())
}

/// An account holds BITCOIN and a second, independent `AmountUnit`
/// (`msfaucet`'s) at once, and spending one leaves the other untouched.
/// Would fail if the module kept a single scalar balance per account
/// instead of one per unit, or if debiting one unit's entry corrupted or
/// zeroed another's.
///
/// The second unit is funded through `msfaucet`, per the Task 6 brief's
/// resolution to reuse `fedimint-amm-tests`'s faucet approach: first an
/// unconditional `FaucetOutput::MintV0` (no input needed — it declares no
/// backing to core) credits `minter`'s faucet balance, then a
/// `FaucetInput` debits it to fund a `multispend` `Credit`. See
/// `crate::faucet`'s module doc comment for why this crate copies the
/// faucet's server half rather than depending on `fedimint-amm-tests` or
/// building a client for it.
#[tokio::test(flavor = "multi_thread")]
async fn an_account_holds_two_units_independently() -> anyhow::Result<()> {
    let fed = new_federation().await;
    let multispend = multispend_id(&fed).await;
    let dummy = dummy_id(&fed).await;
    let msfaucet = faucet_id(&fed).await;
    let (account, kps) = account_with_keys(3, 2);
    let depositor = fresh_keypair();
    let minter = fresh_keypair();

    // Credit BITCOIN via dummy.
    submit(
        &fed,
        multispend,
        vec![dummy_input(dummy, Amount::from_msats(5_000), &depositor)],
        vec![credit_output(
            multispend,
            account.id(),
            Amounts::new_bitcoin_msats(5_000),
        )],
        &[&[(0, depositor)]],
    )
    .await
    .expect("BITCOIN deposit must succeed to set up the test");

    // Mint into the faucet's own per-pubkey balance for `minter` — free,
    // and needs no input at all.
    submit(
        &fed,
        multispend,
        vec![],
        vec![DynOutput::from_typed(
            msfaucet,
            FaucetOutput::MintV0 {
                amount: Amount::from_msats(7_000),
                pub_key: minter.public_key(),
            },
        )],
        &[],
    )
    .await
    .expect("minting from nothing is unconditional");

    // Credit the second unit into the account via a faucet input.
    submit(
        &fed,
        multispend,
        vec![DynInput::from_typed(
            msfaucet,
            FaucetInput {
                amount: Amount::from_msats(7_000),
                pub_key: minter.public_key(),
            },
        )],
        vec![credit_output(
            multispend,
            account.id(),
            Amounts::new_custom(faucet_unit(), Amount::from_msats(7_000)),
        )],
        &[&[(0, minter)]],
    )
    .await
    .expect("the faucet input funds the second-unit credit");

    assert_eq!(
        balance(&fed, &account, AmountUnit::BITCOIN).await,
        Amount::from_msats(5_000)
    );
    assert_eq!(
        balance(&fed, &account, faucet_unit()).await,
        Amount::from_msats(7_000)
    );

    // Spend only the second unit.
    submit(
        &fed,
        multispend,
        vec![spend_input(
            multispend,
            account.clone(),
            Amounts::new_custom(faucet_unit(), Amount::from_msats(7_000)),
        )],
        vec![DynOutput::from_typed(
            msfaucet,
            FaucetOutput::ReceiveV0 {
                amount: Amount::from_msats(7_000),
                pub_key: minter.public_key(),
            },
        )],
        &[&[(0, kps[0]), (2, kps[2])]],
    )
    .await
    .expect("2 of 3 authorizes spending the second unit");

    assert_eq!(
        balance(&fed, &account, AmountUnit::BITCOIN).await,
        Amount::from_msats(5_000),
        "BITCOIN balance must be untouched by a second-unit spend"
    );
    assert_eq!(balance(&fed, &account, faucet_unit()).await, Amount::ZERO);
    Ok(())
}
