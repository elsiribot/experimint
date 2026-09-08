# Multispend module — handover

Written at the end of phase 1. Everything here is either a fact a successor
needs to get building, or a decision that is deliberately open. It exists
because the working ledger lived under `.superpowers/`, which is gitignored
(`.gitignore:20`), so none of the reasoning below would otherwise survive the
machine it was produced on.

## What this is

A fedimint module whose accounts are naive t-of-n multisigs holding balances in
any `AmountUnit`, spendable by any t of n signers to fund arbitrary transaction
outputs. It replaces what fedi built inside its stability pool, minus the fiat
denomination, the stabilization cycle, and the internal transfer system — our
balances are real units, so an account-to-account transfer is just `Spend(A)`
plus `Credit(B)` in one transaction, balanced by core's funding verifier.

Design: [`specs/2026-09-05-multispend-module-design.md`](specs/2026-09-05-multispend-module-design.md).
Sections 3 and 9 are phase 1; section 4 is the bulletin board and section 5 the
client, both unbuilt.

Phase 1 plan and its post-hoc deviations:
[`plans/2026-09-07-multispend-consensus-layer.md`](plans/2026-09-07-multispend-consensus-layer.md).

## State across three repositories

| Repo | Branch | Rev | Pushed | Merged |
| --- | --- | --- | --- | --- |
| `elsiribot/fedimint` | `experimint-v0.11-input-auth` | `b7a752db799` | yes | **no** |
| `elsiribot/fedi` | `experimint-input-auth` | `2a2503ca125` | yes | **no** |
| `elsirion/experimint` | `es/multispend` | `4921a9e` | yes | **no** |

**Nothing is merged and no pull request is open anywhere.** experimint's
`Cargo.toml` pins the first two by rev, so all three move together or none do.

### Why fedi is in the picture at all

experimint pins `stability-pool-{client,common,server}` from `elsiribot/fedi`,
which pinned the *same* fedimint git URL at the old rev. Cargo resolves two
revs of one URL as two distinct sources, so the trait impls stop lining up —
exactly what the comment in experimint's `Cargo.toml` warns about. Bumping
experimint's pin therefore forced fedi to move in lockstep. The fedi commit is
API-shape-only: eight `InputMeta.pub_key` → `InputAuth::Key` and
`ClientInput.keys` → `ClientInputAuth::Key` sites, no behaviour change.

Note `stability-pool-server-old` does not compile on that branch, on
`ServerModuleInit::{trusted_dealer_gen, distributed_gen}` arity and a missing
`type Params`. That predates the branch — the old server never declared
`Params`, which the fedimint trait already required at fedi's *previous* pin.
experimint pins only the non-`old` crates, which are clean.

## The core patch, and why it was needed

The module could not be built correctly without changing fedimint. `process_input`
cannot see a transaction's outputs, so the only thing that can bind a multisig
spend to what it pays is core's signature over the txid — but the txid covers
the inputs, so if the input had to name which key indices signed, the signer set
would be frozen before anyone signed.

The patch moves signatures into a per-input **witness** carried in
`TransactionSignature`, which `tx_hash_from_parts` already excludes. So the
signatures sit outside the txid, any t of n can sign, and each member signs
once. `InputMeta.auth` is now `InputAuth::{Key(PublicKey), SelfVerified}`, and
`verify_input` receives an `InputAuthCtx` exposing a `Message` derived from the
txid — deliberately never the raw bytes.

**This module is the first in existence to return `InputAuth::SelfVerified`.**
Core performs no authorization check of its own for such an input.

A draft upstream issue proposing the core change is at
`/tmp/.../scratchpad/fedimint-issue-input-auth.md` — **unfiled, and it will not
survive**. Its symbol names were verified against this fork, not upstream
master, which is a different API generation.

## The safety obligation

Two-sided. **Neither half is safe alone**, and this is the one thing in the
module that must not be got wrong:

1. `verify_input` verifies the witness signatures against the key material
   carried **inside the input**, bound to `ctx.txid_message()`. It is stateless
   — it runs in a rayon pass before any database access — so it cannot look a
   key set up.
2. `process_input` must then confirm `account.id()` names an **existing funded
   account record**.

Do only the first and the input is forgeable: an attacker supplies their own
`Account`, signs with their own keys, passes verification, and spends. There is
an end-to-end test for exactly that (`spending_a_forged_account_is_rejected`).

`AccountId` is the hash of `{pub_keys, threshold}`, so it commits to the
threshold as well as the keys — a 2-of-3 and a 3-of-3 over the same keys are
different accounts, which closes the threshold-downgrade-by-id variant.

## Things review caught that are worth not re-learning

- **X-only key collapse.** `Account` deduped members on the 33-byte compressed
  `PublicKey`, but schnorr verification compares x-only keys. A point and its
  negation are distinct `PublicKey`s sharing an x-coordinate, and one private
  key signs for both — so `{A, M, -M}` with threshold 2 looked like a 2-of-3
  but was spendable by M's holder alone. Since the member proposing a group
  account picks the key set, this was a live downgrade the others could not
  see. Now rejected in `Account::try_from`, the single construction site both
  decode paths route through.
- **`Amounts` derives `Decodable` with no invariant enforcement.** A crafted
  transaction can carry explicit zero-amount entries that no safe constructor
  produces, and `checked_add_mut` asserts on them **per entry** — so a debug
  guardian panics and a release one persists a spurious zero. Guarded on both
  the credit and the spend path via `amounts_are_well_formed`. Two earlier
  attempts at this guard were wrong: `is_empty()` misses `{unit: 0}`, and
  `all(== ZERO)` misses `{BTC: 5, custom: 0}`.
- **`Amounts` has no serde impl**, which is why `AccountState` cannot derive
  `Serialize` and the API endpoint returns an `AccountBalance` view instead.
- **Consensus-path panics halt every guardian deterministically.** The overflow
  `expect` in `credit_account` and the `i64::try_from` in `audit` were both
  turned into an error and saturation respectively. `audit` runs after every
  consensus item and feeds an `assert!(>= 0)` in the engine.
- **Witness malleability is benign** — the spec previously flagged this as
  unconfirmed. Consensus dedups on `AcceptedTransactionKey`, written only after
  processing succeeds, so a mangled-witness twin of a valid txid cannot poison
  it.

## Open decisions — not settled, do not assume

1. **Fees and unbounded state growth.** `process_output` creates a permanent
   consensus row for any account id on any well-formed credit, with no minimum,
   no fee and no reaping. A single 50 KB transaction holds roughly 1,100
   `Credit` outputs at ~42 encoded bytes each — ~1,100 permanent rows for about
   1.1 sat. Three options are recorded in spec §3.6 and **none is picked**:
   delete the record when the balance reaches zero, require a minimum credit,
   or charge per record.
2. **The bulletin board's allowance reset is free.** Spec §4.3 resets a group's
   daily allowance when `op_counter` changes, arguing a deposit "costs a real
   fedimint transaction and so is not itself a DoS vector". That does not
   survive phase 1 as built: anyone can credit 1 msat to any account, every
   credit bumps `op_counter`, and there are no fees. So an attacker can post
   the full allowance, credit themselves a msat, reset, and repeat. Recorded in
   §4.3 as **unresolved**, with two candidates: reset only on a *spend*, which
   requires t signatures; or rate-limit the reset itself.
3. **Whether to file the upstream fedimint issue**, and against which repo. The
   draft targets upstream `fedimint/fedimint`, whose API generation differs
   from the fork the patch was built on.

## Traps phase 2 inherits

- **A drained account keeps its record.** `checked_sub` removes a unit that
  reaches zero, so spending an account to exactly zero leaves
  `AccountState { balance: ZERO, op_counter }` behind. The bulletin board's
  admission policy must therefore key on **balance, never on record
  existence**, and must not read the `account_balance` endpoint's `Some` as
  "funded" — an attacker can manufacture such a record for 1 msat plus one
  1-of-1 spend. A test documents the behaviour
  (`draining_an_account_to_zero_keeps_its_record`).
- **`op_counter` is the board's hook and costs consensus nothing**, but see
  open decision 2 — it bumps on credits from anyone.
- **The client cannot use `TransactionBuilder`.** The witness must sign the
  txid, and the txid is not knowable before `build()` generates its own nonce
  and `finalize_transaction` adds change. So a real client must assemble a
  `Transaction` itself and call `submit_transaction` directly, choosing its own
  nonce, balancing exactly (no primary-module change handling), and tracking
  its own submissions. The `submit` helper in
  `modules/multispend/fedimint-multispend-tests/tests/tests.rs` is the
  prototype. Spec §9.3 has the detail. `ClientInputAuth::Witness` exists but
  takes fixed bytes, so it does not reach this flow.
- **Deployment ordering:** a `Witnessed` transaction is undecodable to an
  unpatched guardian. All guardians must upgrade before any client does, and
  there is no negotiable signal — the `CORE_CONSENSUS_VERSION` bump to 2.2 is
  inert, because compatibility is decided on major only and API discovery
  ignores minor. A client that needs to know must compare
  `config.global.consensus_version` against 2.2 itself.

## Residual known issues

None block phase 2.

- `audit` sums every unit into one `i64` before negating, so a non-bitcoin
  unit's magnitude lands in the federation-wide sats figure as if it were
  msats. This mirrors amm and is a limit of `Audit`'s single-currency API at
  this rev, not fixable in-module. The sign is correct and no unit is skipped.
- `audit`'s saturation biases toward *apparent solvency* — it reports a
  liability smaller in magnitude than reality — and core's own
  `calculate_net_assets` can still panic across two saturated accounts.
- Nothing asserts `meta.amount.amounts` at the unit level; the end-to-end tests
  cover it in both directions instead.
- `MultispendInit` is opt-in via `FM_ENABLE_MODULE_MULTISPEND`. A module set is
  fixed at DKG, so this matters: without the env var no federation gets the
  module.

## Verifying the branch

Everything runs inside the nix devshell; a bare `cargo` will not resolve.

```bash
nix develop -c cargo fmt --all -- --check
nix develop -c cargo check --workspace --all-targets
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo test -p fedimint-multispend-common   # 16
nix develop -c cargo test -p fedimint-multispend-server   # 19
nix develop -c cargo test -p fedimint-multispend-tests    #  7
nix develop -c cargo test -p fedimintd-experimint         #  5
nix build .#fedimintd-experimint
```

All green at `4921a9e`. If `nix build` reports a `cargoHash` mismatch, the lock
file moved: set `cargoHash = lib.fakeHash`, rebuild, and paste the hash nix
reports.

`fedimint-mint-tests` in the *fedimint* repo is worth knowing about: nothing
else in that workspace calls `process_transaction_with_dbtx`, so it is the only
cheap suite that exercises the core patch against a real federation.
