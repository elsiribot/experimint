# Asset-agnostic multispend module

Status: design, approved in part. Section 2 (core patch) was reviewed and
accepted. Sections 3 through 6 are written from the same design conversation
but have not yet been walked through section by section.

## 1. Scope

A fedimint module whose accounts are naive t-of-n multisigs holding balances in
any `AmountUnit`, spendable by any t of n signers to fund arbitrary transaction
outputs, plus a federation-hosted bulletin board that lets the signers exchange
proposals and signatures without depending on an external messaging layer.

It replaces what fedi built inside its stability pool. Two things fedi has are
deliberately not here:

- **No fiat denomination and no stabilization cycle.** Accounts hold real
  `AmountUnit` balances. The module never consults a price oracle.
- **No internal transfer system.** Fedi needed one because a stabilized fiat
  position is not a fedimint asset and cannot be a transaction input or output
  amount, so it required its own settlement layer. Our balances are real units,
  so an account-to-account transfer is just `Spend(A)` and `Credit(B)` in one
  fedimint transaction, balanced by core's funding verifier.

### Non-goals

- Membership changes. `AccountId` is the hash of the account, so members and
  threshold are immutable. Changing them means creating a new account and
  moving the funds, which is one ordinary spend.
- Deciding whether a co-signer *should* approve. The module places no
  restriction on what a t-of-n input funds. Whether an output is legible to a
  co-signer is a client rendering concern: an `lnv2` contract is transparent
  (payment hash, amount, claim and refund keys), a `mintv2` output is blinded
  nonces and cannot be attributed to a recipient. The client renders what it
  can read and warns loudly on what it cannot.
- FROST, or any threshold signature scheme. The core patch is designed so a
  future module can add one without touching core again, but this module uses
  naive multisig.

## 2. Core patch (`elsiribot/fedimint`, branch `experimint-v0.11-input-auth`)

Branched from `a50619eafc6`, the commit experimint currently pins.

Core today hard-codes one authorization model: exactly one public key per
input, producing one schnorr signature over the txid, matched positionally.
`InputMeta.pub_key` has **one** consumer in the tree
(`fedimint-server/src/consensus/transaction.rs:79`) and seven module producers,
so the change is contained.

### 2.1 Inputs declare how they are authorized

```rust
// fedimint-core/src/module/mod.rs
pub struct InputMeta {
    pub amount: TransactionItemAmounts,
    pub auth: InputAuth,
}

pub enum InputAuth {
    /// Core verifies a schnorr signature over the txid against this key.
    /// Current behaviour; what all seven existing modules return.
    Key(secp256k1::PublicKey),
    /// The module already verified authorization in `verify_input`.
    SelfVerified,
}
```

### 2.2 A witness variant, outside the tx hash

```rust
// fedimint-core/src/transaction.rs
pub enum TransactionSignature {
    /// Legacy: one signature per input, all inputs `Key`.
    NaiveMultisig(Vec<schnorr::Signature>),
    /// One witness per input, positionally aligned with `inputs`.
    /// For an `InputAuth::Key` input the witness is its 64-byte signature.
    Witnessed(Vec<Vec<u8>>),
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}
```

One witness vector covering every input, rather than a witness channel beside
the signature channel, keeps mixed transactions simple: a transaction with a
`mintv2` input and a multispend input has one uniform place to look.

`tx_hash_from_parts` hashes inputs, outputs and nonce, and is untouched.
Witnesses stay outside the txid, exactly as signatures already are — which is
the property that makes this work at all.

### 2.3 `verify_input` gains a context

```rust
// fedimint-server-core/src/lib.rs
fn verify_input(
    &self,
    input: &<Self::Common as ModuleCommon>::Input,
    ctx: &InputAuthCtx,
) -> Result<(), <Self::Common as ModuleCommon>::InputError> { Ok(()) }
```

`InputAuthCtx` exposes no raw txid bytes, only a message already derived from
them, so the ergonomic thing to write is also the bound thing:

```rust
impl InputAuthCtx {
    pub fn txid_message(&self) -> secp256k1::Message;
    pub fn witness(&self) -> &[u8];
    pub fn in_idx(&self) -> u64;
}
```

`verify_input` is the right home. It is the existing hook for stateless
cryptographic verification parallelised across a transaction, it runs under
`into_par_iter()` before anything touches the database, and self-verification
is exactly stateless crypto. The consequence is that a self-verifying input
must carry everything needed to check it, rather than looking anything up —
which is why the multispend input carries its account inline.

### 2.4 Core threads it through

`txid` is currently computed after the rayon pass; hoist it above so the
context can be built. Then per input:

```rust
match meta.auth {
    InputAuth::Key(pk) => /* verify witness as a schnorr sig over txid */,
    InputAuth::SelfVerified => { /* verify_input already did it */ }
}
```

The seven existing modules each change one line: `pub_key: pk` becomes
`auth: InputAuth::Key(pk)`.

### 2.5 Client side

`ClientInput.keys: Vec<Keypair>` becomes an auth enum whose second arm produces
witness bytes from the txid. Today the builder can only sign with keypairs it
holds (`input_keys.iter().flatten().map(|kp| sign_schnorr(&msg, kp))`), so a
client can never assemble a transaction authorized by other people — which is
precisely the multi-party case.

### 2.6 Compatibility

- No txid changes, for any transaction.
- `NaiveMultisig` retained; an all-`Key` transaction encodes as it does today.
- `Witnessed` is additive under `#[encodable_default]`.
- Consensus-rule change: it changes which transactions are valid even though it
  changes no encoding, so it wants a `CoreConsensusVersion` bump.

### 2.7 Risks

**The txid-binding footgun.** Core's invariant — every input is authorized by
someone signing *this exact transaction* — becomes a per-module obligation. A
module verifying against anything other than `ctx.txid_message()` lets an
attacker detach a valid input and reattach it to a transaction with different
outputs. Mitigated by never exposing raw txid bytes, and by documenting it as
the one rule that matters.

**Verification cost.** A module can make `verify_input` arbitrarily expensive;
a 210-of-210 input is 210 schnorr checks. It lands in the best available place
(parallel, pre-database, rejected at submission), but `n <= 210` is a real DoS
bound here, not tidiness.

**Witness malleability.** Two submissions of one txid with different valid
witnesses become possible. Consensus dedups on txid and the effect is identical
either way, so this looks benign — but it is the one risk in this document that
has not been confirmed against how submission dedup actually behaves.

**Error reporting.** Self-verification failures surface through the existing
`.map_err(|_| TransactionError::InvalidWitnessLength)` funnel, whose own
comment apologises for not being extensible. This change makes that path much
hotter.

## 3. Module consensus layer

### 3.1 Account

```rust
pub struct Account {
    pub_keys: BTreeSet<PublicKey>,   // 1 <= len <= 210
    threshold: u64,                  // 1 <= threshold <= len
}

pub struct AccountId(sha256::Hash);  // = account.consensus_hash()
```

Invariants are enforced on construction via a `TryFrom<AccountUnchecked>` and a
hand-written `Decodable`, the pattern fedi already uses, so an invalid account
cannot be decoded off the wire. `n <= 210` bounds `verify_input` cost; `t` may
be anything from 1 to `n`.

### 3.2 Database

```
AccountId -> AccountState { balance: Amounts, op_counter: u64 }
```

That is the entire consensus state. **The key set is never stored.** A record
is created by the first credit, and the account exists exactly while it has
one; the full `Account` travels inline in every spend and is checked against
the id by hash. So there is no registration step and nothing to keep in sync.

`op_counter` increments on every input or output touching the account. It
exists solely so the non-consensus bulletin board can notice "this account did
something" without any consensus write. See 4.3.

### 3.3 Output

```rust
pub enum MultispendOutput {
    Credit { account: AccountId },
}
```

Amount comes from the transaction item. Crediting an account that has no record
creates one, so there is no separate open operation and no way to create an
account without funding it — which matters, because a funded account is what
earns bulletin board allowance.

### 3.4 Input

```rust
pub enum MultispendInput {
    Spend {
        account: Account,   // full account inline, id checked by hash
        amounts: Amounts,   // what to debit, per unit
    },
}

// witness, decoded from ctx.witness()
pub struct SpendWitness {
    signatures: BTreeMap<u16, schnorr::Signature>,
}
```

`amounts` lives in the input, and so inside the txid, which is what makes a
co-signer's signature a commitment to the amount being spent.

`verify_input` (stateless, parallel, pre-database):

1. Decode the witness.
2. Every index is within `account.pub_keys.len()`.
3. `signatures.len() >= account.threshold`.
4. Every signature verifies against its indexed key over `ctx.txid_message()`.

`process_input` (has the dbtx):

1. Look up `account.id()`; error if absent.
2. Balance covers `amounts` in every unit named; debit.
3. Bump `op_counter`.
4. Return `InputMeta { amount, auth: InputAuth::SelfVerified }`.

**No nonce field is needed.** The signatures bind to the txid, and the txid
covers the inputs and outputs, so the same authorization cannot be replayed
into a different transaction; an identical resubmission has the same txid and
is deduplicated by consensus.

Because the witness sits outside the txid, **any t of n can sign, each member
signs exactly once, and the signer set is not fixed in advance** — the property
that the whole core patch exists to buy. Under the smaller
`pub_keys: Vec<PublicKey>` patch the signer list would have to live in the
input, hence inside the txid, forcing a proposer to guess its signers before
anyone had signed.

### 3.5 API

`account_balance(AccountId) -> Option<AccountState>`. Read-only.

## 4. Bulletin board (non-consensus)

### 4.1 Shape

Entirely outside consensus. Each guardian keeps its own local store under a
dedicated key prefix that **no consensus code path ever reads** — not
`process_input`, `process_output`, `process_consensus_item` or `audit` — so
divergence between guardians is harmless by construction. Writes happen from
API handlers via `ApiEndpointContext::db()`, which hands back a module-isolated
`Database` on which a handler can open and commit its own transaction.

Clients post to all guardians and read from all, unioning results. A censoring,
crashed or lagging guardian therefore degrades nothing.

The board may read consensus state (does this account have a balance, what is
its `op_counter`) but never writes to it.

### 4.2 Vocabulary

```rust
// costs allowance: the guardian cannot check that the ciphertext
// really decrypts to a transaction with this txid
Proposal {
    account: Account,       // full, so the guardian can authenticate the poster
    txid: TransactionId,
    ciphertext: Vec<u8>,    // the unsigned tx, encrypted to the members
    poster: u16,
    sig: schnorr::Signature,   // over (txid, H(ciphertext)) by pub_keys[poster]
}

// free, because it is fully verifiable
Signature { account: AccountId, txid: TransactionId, key: u16, sig: Signature }
Rejection { account: AccountId, txid: TransactionId, key: u16, sig: Signature }
```

A signature posting is a schnorr signature over the txid by a member key, and
the guardian knows both — so it verifies it outright. Forged signature postings
are rejected rather than rate-limited, and the only cap needed is one posting
per member per proposal, hence at most `n`. This is why the proposal id is the
txid rather than an opaque identifier: it makes the high-volume posting type
self-authenticating.

Only the encrypted proposal body needs an allowance, because it is the only
thing the guardian cannot check.

`Rejection` is advisory. Because any t of n can sign, a rejection blocks
nothing; it exists so a client can show "Bob declined" rather than leaving the
group waiting on someone who never intends to sign.

### 4.3 Admission policy (per guardian, local)

A proposal is accepted when:

- `account.id()` has a non-zero balance in consensus state,
- `poster` indexes a member key and `sig` verifies,
- `ciphertext` is within the size cap,
- the account has allowance remaining.

The guardian keeps, per account, a local record of `{ used, day, seen_op }`.
Allowance is reset when **either** its own wall clock has passed UTC midnight
since `day`, **or** the account's current `op_counter` differs from `seen_op`. The second condition
is how "a successful operation restores the allowance" is expressed with zero
consensus writes: any deposit or withdrawal bumps `op_counter`, the guardian
notices on the next post, and resets. A group that exhausts its allowance can
always unstick itself with a deposit, which costs a real fedimint transaction
and so is not itself a DoS vector.

`MAX`, the ciphertext cap and the retention window are **guardian-local
configuration**, not consensus parameters — guardians are free to disagree.
Proposed defaults: `MAX = 32`/day, 64 KiB ciphertext, at most `MAX` live
proposals per account with the oldest evicted, giving a hard 2 MiB per funded
account.

Garbage collection is local too: drop a proposal once a transaction with its
txid has been processed, and otherwise after the retention window.

### 4.4 Reads

Open, unauthenticated. Bodies are end-to-end encrypted, and account ids are
already visible to anyone watching consensus transactions, so requiring a
membership proof to read would add a challenge round trip and replay handling
to protect metadata that consensus already leaks. What it does leak is *how
much* a group is negotiating, to someone who already knows the account id.
Noted, accepted.

Posting a proposal reveals the full member list to the guardian, since the
account travels with the post. Spending reveals it anyway.

## 5. Client

- **Encryption.** Per-member envelopes, ECDH against the account's existing
  signing keys, Nostr-style. Reusing a signing key for ECDH is mildly
  disfavoured but avoids putting a second key set into `Account`, which is
  carried inline in every spend input.
- **Flow.** Propose (build the unsigned tx, encrypt, post to all guardians) ->
  members fetch, decrypt, render, sign the txid, post -> anyone with t
  signatures assembles the witness and submits.
- **Rendering.** Show the decoded outputs. Transparent output types are
  displayed; opaque ones are shown as unattributable with a warning.
- **State machine.** Track submitted spends to completion, as the amm client
  does.

## 6. Testing

- Core patch: existing suites must pass untouched (all-`Key` transactions), plus
  mixed `Key`/`SelfVerified` transactions, a witness rejected for a different
  txid, and wrong witness count.
- Module unit: account invariants (`t = 0`, `t > n`, `n > 210`, empty), witness
  with too few signatures, out-of-range index, duplicate index, wrong-txid
  signature, insufficient balance, per-unit balance isolation.
- Module integration (`fedimint-multispend-tests`, mirroring the amm and usdt
  crates): fund from `mintv2`, spend t-of-n to a `mintv2` output, account-to-
  account transfer in one transaction, multi-unit account, 1-of-1 as an
  ordinary wallet.
- Board: allowance exhaustion and both reset paths, forged signature posting
  rejected, one-posting-per-member, oversized ciphertext, eviction, GC on
  txid seen, and that a client tolerates one guardian returning nothing.

## 7. Open questions

- Should `InputAuthCtx` expose `txid_message()`, or only a
  `verify_schnorr(&pk)` helper making the binding impossible to get wrong?
- Should core cap witness size per input, and where?
- Is the witness malleability analysis in 2.7 correct?
- `MAX = 32`/day and 64 KiB are guesses that want a sanity check.

## 8. Future directions

Raised during design, deliberately not built:

- A Lightning contract funded by a multispend account with a pre-signed FROST
  refund path back into the account, so a group can pay out without the refund
  branch needing a fresh round of signatures.
- Threshold signature schemes generally. The core patch is shaped so these need
  no further core change.
