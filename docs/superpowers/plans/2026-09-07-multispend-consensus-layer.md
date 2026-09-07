# Multispend Module, Phase 1: Consensus Layer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A fedimint module whose accounts are naive t-of-n multisigs holding balances in any `AmountUnit`, fundable by anyone and spendable by any t of n signers to fund arbitrary transaction outputs.

**Architecture:** An account is `{pub_keys, threshold}` and its id is that struct's hash, so the key set is never stored — it travels inline in every spend and is checked against the id. Consensus state is one record per account: a multi-unit balance and an operation counter. A `Credit` output adds to a balance (creating the record on first credit); a `Spend` input debits it, authorized by t schnorr signatures over the txid carried in the transaction witness, outside the txid itself. This will be **the first module in existence to return `InputAuth::SelfVerified`**.

**Tech Stack:** Rust 2024, `fedimint-core` / `fedimint-server-core` / `fedimint-testing` at the witness branch, `secp256k1` schnorr, `sha256`.

**Spec:** `/home/user/projects/experimint/docs/superpowers/specs/2026-09-05-multispend-module-design.md`, sections 3 and 9. Section 4 (bulletin board) and section 5 (client) are **out of scope** and get their own plans.

**Reference implementation:** `modules/amm/` in this repo is the closest analogue at the same pinned rev, is heavily documented, and should be mirrored for every piece of module boilerplate. When this plan says "mirror amm", read the named file and follow its structure — do not invent a different shape.

**Repo:** `/home/user/projects/experimint`, branch `es/multispend`.

## Global Constraints

- **The account key set is NEVER stored.** Consensus state is `AccountId -> AccountState` only. The full `Account` arrives inline in every `Spend` input and is checked by hash. There is no registration step.
- **`1 <= threshold <= pub_keys.len() <= 210`.** Enforced on construction via `TryFrom` and a hand-written `Decodable`, so an invalid account cannot be decoded off the wire. The upper bound is a DoS bound: a 210-of-210 input costs 210 schnorr verifications in `verify_input`.
- **The two-sided safety obligation, from spec §9.2.** Both halves are required:
  1. `verify_input` verifies the witness signatures against the key material carried **inside the input**, bound to `ctx.txid_message()`.
  2. `process_input` **must** then confirm `account.id()` names an existing funded account record.

  Doing only the first makes the input forgeable — an attacker supplies their own `Account`, signs with their own keys, and passes verification. **This is the single thing in this plan that must not be got wrong.**
- **`verify_input` must bind to `ctx.txid_message()`** and nothing else. Verifying against any other message lets an attacker detach the input and reattach it to a transaction with different outputs. Prefer `ctx.verify_schnorr(&pk, &sig)`, which cannot be pointed at the wrong message.
- **No nonce field on the input.** Signatures bind to the txid, and the txid covers inputs and outputs, so the same authorization cannot be replayed into a different transaction; an identical resubmission has the same txid and is deduplicated by consensus. Do not add one "for safety".
- **This repo builds inside its nix devshell.** Every command must be wrapped: `nix develop -c cargo ...`.
- **The verification gate**, green before every commit:

  ```bash
  nix develop -c cargo fmt --all -- --check
  nix develop -c cargo check --workspace --all-targets
  nix develop -c cargo clippy --workspace --all-targets -- -D warnings
  nix develop -c cargo test -p fedimint-multispend-common
  nix develop -c cargo test -p fedimint-multispend-server
  ```

  From Task 6 onward add `nix develop -c cargo test -p fedimint-multispend-tests`.
- Commit at the end of every task. Do not squash tasks together.

---

## File Structure

| Path | Responsibility |
| --- | --- |
| `modules/multispend/fedimint-multispend-common/src/account.rs` | `Account`, `AccountUnchecked`, `AccountId`, and the invariants. Nothing else. |
| `modules/multispend/fedimint-multispend-common/src/types.rs` | `MultispendInput`, `MultispendOutput`, `SpendWitness`, `MultispendOutputOutcome`, `MultispendConsensusItem`, and the two error enums. |
| `modules/multispend/fedimint-multispend-common/src/config.rs` | Config structs. No key material; mirrors `fedimint-amm-common/src/config.rs` minus the AMM's parameters. |
| `modules/multispend/fedimint-multispend-common/src/endpoints.rs` | Endpoint name constants. |
| `modules/multispend/fedimint-multispend-common/src/lib.rs` | `KIND`, `MODULE_CONSENSUS_VERSION`, `plugin_types_trait_impl_common!`, `CommonModuleInit`. |
| `modules/multispend/fedimint-multispend-server/src/db.rs` | `DbKeyPrefix`, `AccountKey`, `AccountState`, `impl_db_record!`/`impl_db_lookup!`. |
| `modules/multispend/fedimint-multispend-server/src/lib.rs` | `MultispendInit`, `Multispend`, the `ServerModule` impl, the API endpoint. |
| `modules/multispend/fedimint-multispend-tests/src/fixtures.rs` | The `fixtures()` federation helper. |
| `modules/multispend/fedimint-multispend-tests/tests/tests.rs` | Integration tests driving hand-assembled transactions. |

Splitting `account.rs` out of `types.rs` is deliberate: the account invariants are the security-critical part and should be readable and testable without the transaction plumbing around them.

---

### Task 1: Move the fedimint pin onto the witness branch

Nothing in this plan compiles until the pin moves. This task also pays the downstream migration debt the witness branch's final review predicted: experimint's own `usdt` and `amm` modules still use the pre-branch `InputMeta.pub_key` and `ClientInput.keys` APIs.

**Files:**
- Modify: `Cargo.toml` (36 `rev = ` occurrences and the explanatory comment above them)
- Modify: `modules/usdt/fedimint-usdt-server/src/lib.rs` (around :2532 and :4572)
- Modify: `modules/amm/fedimint-amm-client/src/lib.rs` (around :849 and :1222)
- Possibly modify: any other site the compiler names

**Interfaces:**
- Consumes: nothing.
- Produces: a workspace pinned at `b7a752db799a962c4a5e0d3c87bc2ac37e9799c2` on `elsiribot/fedimint` branch `experimint-v0.11-input-auth`, building green.

- [ ] **Step 1: Bump the rev**

```bash
cd /home/user/projects/experimint
sed -i 's/a50619eafc6fc2b6eb1e147200f444939a1e9785/b7a752db799a962c4a5e0d3c87bc2ac37e9799c2/g' Cargo.toml
grep -c 'b7a752db799a962c4a5e0d3c87bc2ac37e9799c2' Cargo.toml
```

Expected: `36`.

- [ ] **Step 2: Update the pin comment**

`Cargo.toml` carries a long comment above the dependency block explaining what the pinned rev is and why the fork exists. Read it, and replace the rev description with:

```
# rev b7a752db799a962c4a5e0d3c87bc2ac37e9799c2 == elsiribot/fedimint
# `experimint-v0.11-input-auth` ("polish: surface verify_input contract,
# harden filter_map, note upgrade caveat"), branched from a50619eafc6.
#
# This branch lets a module verify its own input authorization against an
# opaque per-input witness: `InputMeta.auth: InputAuth::{Key, SelfVerified}`,
# a `TransactionSignature::Witnessed` variant carrying one witness per input
# outside the txid, and an `InputAuthCtx` handed to `verify_input`. The
# multispend module is the first consumer; nothing else needs it.
```

Leave the rest of the comment (why the fork, how to bump) intact.

- [ ] **Step 3: Migrate the downstream call sites**

Run the build and let the compiler find them:

```bash
nix develop -c cargo check --workspace --all-targets 2>&1 | grep -E "^error" | head -30
```

Two mechanical changes, matching what the witness branch did to fedimint's own modules:

- Server modules returning `InputMeta`: `pub_key: <expr>` becomes `auth: InputAuth::Key(<expr>)`, importing `InputAuth` from `fedimint_core::module`.
- Clients constructing `ClientInput`: `keys: vec![<expr>]` becomes `auth: ClientInputAuth::Key(<expr>)`, importing `ClientInputAuth` from `fedimint_client_module::transaction`. **Note the singular `Key`** — the arm wraps one `Keypair`, not a `Vec`.

Also fix doc comments that name `InputMeta.pub_key`; the compiler will not catch those. Find them with:

```bash
grep -rn "InputMeta" --include=*.rs modules/ bin/ | grep -v "InputAuth"
```

- [ ] **Step 4: Run the gate**

```bash
nix develop -c cargo fmt --all -- --check
nix develop -c cargo check --workspace --all-targets
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
```

Expected: clean. If `Cargo.lock` changed, that is expected — commit it.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock modules/ bin/
git commit -m "build: follow fedimint onto the input-auth witness branch

Moves the pin to experimint-v0.11-input-auth, which lets a module verify its
own input authorization against a per-input witness carried outside the txid.
The multispend module needs it; nothing else does.

Migrates usdt and amm onto the new InputMeta.auth and ClientInputAuth shapes."
```

---

### Task 2: `Account`, `AccountId`, and their invariants

The security-critical core of the module, testable with no fedimint machinery around it.

**Files:**
- Create: `modules/multispend/fedimint-multispend-common/Cargo.toml`
- Create: `modules/multispend/fedimint-multispend-common/src/lib.rs`
- Create: `modules/multispend/fedimint-multispend-common/src/account.rs`
- Modify: `Cargo.toml` (workspace `members`)

**Interfaces:**
- Consumes: Task 1's pin.
- Produces:
  - `Account` with `try_from(AccountUnchecked) -> anyhow::Result<Account>`, `id() -> AccountId`, `threshold() -> u64`, `pub_keys() -> impl Iterator<Item = &PublicKey>`, `pub_key_at(idx: u16) -> Option<&PublicKey>`, `len() -> usize`
  - `AccountUnchecked { pub_keys: BTreeSet<PublicKey>, threshold: u64 }`
  - `AccountId(pub sha256::Hash)`
  - `pub const MAX_ACCOUNT_KEYS: usize = 210;`

- [ ] **Step 1: Create the crate**

`modules/multispend/fedimint-multispend-common/Cargo.toml`, mirroring `modules/amm/fedimint-amm-common/Cargo.toml`:

```toml
[package]
authors = { workspace = true }
description = "fedimint-multispend-common contains the types shared between the multispend module's client and server"
edition = { workspace = true }
license = { workspace = true }
name = "fedimint-multispend-common"
repository = { workspace = true }
version = { workspace = true }

[lib]
name = "fedimint_multispend_common"
path = "src/lib.rs"

[dependencies]
anyhow = { workspace = true }
fedimint-core = { workspace = true }
serde = { workspace = true }
thiserror = { workspace = true }

[lints]
workspace = true
```

Add `"modules/multispend/fedimint-multispend-common",` to the workspace `members` list in the root `Cargo.toml`, keeping it alphabetically ordered with the existing entries.

- [ ] **Step 2: Write the failing tests**

Create `modules/multispend/fedimint-multispend-common/src/account.rs` containing only:

```rust
//! Account identity and its invariants. Spec §3.1.

#[cfg(test)]
mod tests {
    use fedimint_core::secp256k1::rand::rngs::OsRng;
    use fedimint_core::secp256k1::{Keypair, PublicKey, Secp256k1};

    use super::*;

    fn keys(n: usize) -> BTreeSet<PublicKey> {
        let secp = Secp256k1::new();
        (0..n)
            .map(|_| Keypair::new(&secp, &mut OsRng).public_key())
            .collect()
    }

    fn unchecked(n: usize, threshold: u64) -> AccountUnchecked {
        AccountUnchecked { pub_keys: keys(n), threshold }
    }

    #[test]
    fn accepts_a_valid_account() {
        let account = Account::try_from(unchecked(3, 2)).expect("2-of-3 is valid");
        assert_eq!(account.threshold(), 2);
        assert_eq!(account.len(), 3);
    }

    #[test]
    fn accepts_one_of_one() {
        Account::try_from(unchecked(1, 1)).expect("1-of-1 is an ordinary personal account");
    }

    #[test]
    fn accepts_the_maximum() {
        Account::try_from(unchecked(MAX_ACCOUNT_KEYS, MAX_ACCOUNT_KEYS as u64))
            .expect("210-of-210 is the largest valid account");
    }

    #[test]
    fn rejects_zero_threshold() {
        assert!(Account::try_from(unchecked(3, 0)).is_err());
    }

    #[test]
    fn rejects_threshold_above_key_count() {
        assert!(Account::try_from(unchecked(3, 4)).is_err());
    }

    #[test]
    fn rejects_empty_key_set() {
        assert!(Account::try_from(unchecked(0, 1)).is_err());
    }

    #[test]
    fn rejects_too_many_keys() {
        assert!(Account::try_from(unchecked(MAX_ACCOUNT_KEYS + 1, 1)).is_err());
    }

    #[test]
    fn id_is_stable_and_distinguishes_accounts() {
        let a = Account::try_from(unchecked(3, 2)).unwrap();
        let same = a.clone();
        assert_eq!(a.id(), same.id());

        let b = Account::try_from(unchecked(3, 2)).unwrap();
        assert_ne!(a.id(), b.id(), "different key sets must have different ids");
    }

    #[test]
    fn threshold_is_part_of_the_id() {
        let pub_keys = keys(3);
        let two = Account::try_from(AccountUnchecked { pub_keys: pub_keys.clone(), threshold: 2 })
            .unwrap();
        let three =
            Account::try_from(AccountUnchecked { pub_keys, threshold: 3 }).unwrap();

        assert_ne!(
            two.id(),
            three.id(),
            "a 2-of-3 and a 3-of-3 over the same keys must be different accounts, \
             or funds could be spent under a lower threshold than the depositor agreed to"
        );
    }

    #[test]
    fn decoding_rejects_an_invalid_account() {
        // An attacker who can get a threshold-0 account past the decoder can
        // spend from it with no signatures at all.
        let raw = AccountUnchecked { pub_keys: keys(3), threshold: 0 };
        let bytes = raw.consensus_encode_to_vec();

        assert!(
            Account::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).is_err(),
            "invalid accounts must not survive decoding"
        );
    }

    #[test]
    fn pub_key_at_indexes_in_sorted_order() {
        let account = Account::try_from(unchecked(5, 3)).unwrap();
        let expected: Vec<_> = account.pub_keys().copied().collect();

        for (idx, key) in expected.iter().enumerate() {
            assert_eq!(account.pub_key_at(idx as u16), Some(key));
        }
        assert_eq!(account.pub_key_at(5), None);
    }
}
```

- [ ] **Step 3: Run them to verify they fail**

Run: `nix develop -c cargo test -p fedimint-multispend-common`

Expected: FAIL to compile — `Account`, `AccountUnchecked`, `MAX_ACCOUNT_KEYS` are undefined.

- [ ] **Step 4: Implement**

Above the test module in `account.rs`:

```rust
use std::collections::BTreeSet;
use std::io;

use anyhow::{bail, ensure};
use fedimint_core::encoding::{Decodable, DecodeError, Encodable};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::{BitcoinHash, bitcoin};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use bitcoin::hashes::sha256;

/// Upper bound on an account's key set.
///
/// This is a denial-of-service bound, not a usability one: `verify_input`
/// runs before any database access and a 210-of-210 spend costs 210 schnorr
/// verifications, so the cost of an attacker-supplied input has to be capped
/// somewhere. Core does not enforce it; we do.
pub const MAX_ACCOUNT_KEYS: usize = 210;

/// A naive t-of-n multisig account.
///
/// Invariants, enforced on construction and on decode:
/// `1 <= threshold <= pub_keys.len() <= MAX_ACCOUNT_KEYS`.
///
/// The key set is never stored by the module. It travels inline in every
/// spend and is checked against [`AccountId`], which is this struct's hash.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Encodable, Serialize)]
pub struct Account {
    pub_keys: BTreeSet<PublicKey>,
    threshold: u64,
}

/// [`Account`] without its invariants, for decoding. Convert with `try_from`.
#[derive(Clone, Debug, Decodable, Deserialize, Encodable)]
pub struct AccountUnchecked {
    pub pub_keys: BTreeSet<PublicKey>,
    pub threshold: u64,
}

impl TryFrom<AccountUnchecked> for Account {
    type Error = anyhow::Error;

    fn try_from(raw: AccountUnchecked) -> anyhow::Result<Account> {
        ensure!(!raw.pub_keys.is_empty(), "account must have at least one key");
        ensure!(
            raw.pub_keys.len() <= MAX_ACCOUNT_KEYS,
            "account has {} keys, the maximum is {MAX_ACCOUNT_KEYS}",
            raw.pub_keys.len()
        );
        ensure!(raw.threshold != 0, "account threshold must not be zero");
        if raw.threshold > raw.pub_keys.len() as u64 {
            bail!(
                "account threshold {} exceeds its {} keys",
                raw.threshold,
                raw.pub_keys.len()
            );
        }

        Ok(Account { pub_keys: raw.pub_keys, threshold: raw.threshold })
    }
}

impl Decodable for Account {
    fn consensus_decode_partial<R: io::Read>(
        r: &mut R,
        modules: &ModuleDecoderRegistry,
    ) -> Result<Self, DecodeError> {
        let raw = AccountUnchecked::consensus_decode_partial(r, modules)?;
        Self::try_from(raw).map_err(DecodeError::new_custom)
    }
}

impl<'de> Deserialize<'de> for Account {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = AccountUnchecked::deserialize(deserializer)?;
        Self::try_from(raw).map_err(D::Error::custom)
    }
}

impl Account {
    /// The account's identity: the hash of this struct.
    ///
    /// Note this commits to the threshold as well as the keys, so a 2-of-3
    /// and a 3-of-3 over the same keys are different accounts.
    pub fn id(&self) -> AccountId {
        AccountId(self.consensus_hash())
    }

    pub fn threshold(&self) -> u64 {
        self.threshold
    }

    pub fn len(&self) -> usize {
        self.pub_keys.len()
    }

    pub fn is_empty(&self) -> bool {
        false // the invariant forbids it; present because clippy asks for it
    }

    /// Keys in ascending order. Witness signature indices refer to this order.
    pub fn pub_keys(&self) -> impl Iterator<Item = &PublicKey> {
        self.pub_keys.iter()
    }

    /// The key at `idx` in ascending order, or `None` if out of range.
    pub fn pub_key_at(&self, idx: u16) -> Option<&PublicKey> {
        self.pub_keys.iter().nth(usize::from(idx))
    }

    /// A 1-of-1 account, which is how an ordinary personal wallet is expressed.
    pub fn single(key: PublicKey) -> Self {
        Self { pub_keys: BTreeSet::from([key]), threshold: 1 }
    }
}

#[derive(
    Clone, Copy, Debug, Hash, Eq, PartialEq, Ord, PartialOrd,
    Encodable, Decodable, Serialize, Deserialize,
)]
pub struct AccountId(pub sha256::Hash);

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
```

Create `modules/multispend/fedimint-multispend-common/src/lib.rs` with just `pub mod account;` for now; Task 3 fills it in.

If `DecodeError::new_custom` does not exist at this rev, check how `fedimint-core` constructs a custom decode error and follow that — `modules/amm/fedimint-amm-common/src/pool_id.rs` has a hand-written `Decodable` to copy from.

- [ ] **Step 5: Run them to verify they pass**

Run: `nix develop -c cargo test -p fedimint-multispend-common`

Expected: PASS, 11 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml modules/multispend/
git commit -m "feat(multispend): account identity and its invariants

An account is {pub_keys, threshold} and its id is that struct's hash, so the
threshold is part of the identity: a 2-of-3 and a 3-of-3 over the same keys
are different accounts, and a depositor's funds cannot be spent under a lower
threshold than they agreed to.

Invalid accounts cannot be decoded off the wire, which is what stops a
threshold-0 account from being spendable with no signatures."
```

---

### Task 3: Transaction types and module wiring

**Files:**
- Create: `modules/multispend/fedimint-multispend-common/src/types.rs`
- Create: `modules/multispend/fedimint-multispend-common/src/config.rs`
- Create: `modules/multispend/fedimint-multispend-common/src/endpoints.rs`
- Modify: `modules/multispend/fedimint-multispend-common/src/lib.rs`

**Interfaces:**
- Consumes: `Account`, `AccountId` (Task 2).
- Produces:
  - `MultispendInput::Spend { account: Account, amounts: Amounts }`
  - `MultispendOutput::Credit { account: AccountId, amounts: Amounts }`
  - `SpendWitness { signatures: BTreeMap<u16, schnorr::Signature> }` with `SpendWitness::decode(&[u8]) -> Result<Self, MultispendInputError>` and `encode_to_vec(&self) -> Vec<u8>`
  - `MultispendInputError`, `MultispendOutputError`, `MultispendOutputOutcome`, `MultispendConsensusItem`
  - `MultispendClientConfig`, `MultispendConfig`, `MultispendConfigPrivate`, `MultispendConfigConsensus`
  - `KIND`, `MODULE_CONSENSUS_VERSION`, `MultispendModuleTypes`, `MultispendCommonInit`
  - `ACCOUNT_BALANCE_ENDPOINT: &str = "account_balance"`

- [ ] **Step 1: Write the failing tests**

Append to `types.rs` (create the file with this test module first):

```rust
#[cfg(test)]
mod tests {
    use fedimint_core::secp256k1::rand::rngs::OsRng;
    use fedimint_core::secp256k1::{Keypair, Message, Secp256k1};

    use super::*;

    #[test]
    fn witness_roundtrips() {
        let secp = Secp256k1::new();
        let kp = Keypair::new(&secp, &mut OsRng);
        let sig = secp.sign_schnorr(&Message::from_digest([1u8; 32]), &kp);

        let witness = SpendWitness {
            signatures: BTreeMap::from([(0u16, sig), (7u16, sig)]),
        };

        let decoded = SpendWitness::decode(&witness.encode_to_vec())
            .expect("a witness we just encoded must decode");
        assert_eq!(decoded.signatures, witness.signatures);
    }

    #[test]
    fn witness_rejects_trailing_garbage() {
        let secp = Secp256k1::new();
        let kp = Keypair::new(&secp, &mut OsRng);
        let sig = secp.sign_schnorr(&Message::from_digest([1u8; 32]), &kp);

        let mut bytes = SpendWitness { signatures: BTreeMap::from([(0u16, sig)]) }
            .encode_to_vec();
        bytes.push(0xff);

        assert!(
            SpendWitness::decode(&bytes).is_err(),
            "a witness is attacker-supplied; trailing bytes must not be ignored"
        );
    }

    #[test]
    fn witness_rejects_empty_input() {
        assert!(SpendWitness::decode(&[]).is_err());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `nix develop -c cargo test -p fedimint-multispend-common`

Expected: FAIL to compile — `SpendWitness` is undefined.

- [ ] **Step 3: Implement the types**

Above the tests in `types.rs`:

```rust
//! Transaction types. Spec §3.3 and §3.4.

use std::collections::BTreeMap;

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::module::{Amounts, plugin_types_trait_impl_common};
use fedimint_core::secp256k1::schnorr;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::account::{Account, AccountId};

/// Debits a t-of-n account.
///
/// The full [`Account`] travels inline because `verify_input` is stateless
/// and cannot look the key set up; `process_input` is what checks that this
/// account is an authorized one. See spec §9.2 — both halves are required.
///
/// `amounts` lives here, and so inside the txid, which is what makes a
/// co-signer's signature a commitment to the amount being spent.
///
/// There is deliberately no nonce: the signatures bind to the txid, which
/// covers inputs and outputs, so this authorization cannot be replayed into
/// a different transaction.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable, Serialize, Deserialize)]
pub enum MultispendInput {
    Spend { account: Account, amounts: Amounts },
}

/// Credits an account, creating its record if this is the first credit.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable, Serialize, Deserialize)]
pub enum MultispendOutput {
    Credit { account: AccountId, amounts: Amounts },
}

/// The witness for a [`MultispendInput::Spend`], carried in the transaction's
/// `TransactionSignature::Witnessed` vector rather than in the input, so it
/// sits outside the txid and the signer set need not be fixed in advance.
///
/// Keys are indices into the account's keys in ascending order. `BTreeMap`
/// makes duplicate indices unrepresentable.
#[derive(Debug, Clone, Eq, PartialEq, Encodable, Decodable)]
pub struct SpendWitness {
    pub signatures: BTreeMap<u16, schnorr::Signature>,
}

impl SpendWitness {
    /// Decode attacker-supplied witness bytes.
    ///
    /// Uses whole-input decoding: trailing bytes are an error, not ignored.
    pub fn decode(bytes: &[u8]) -> Result<Self, MultispendInputError> {
        Self::consensus_decode_whole(bytes, &ModuleDecoderRegistry::default())
            .map_err(|_| MultispendInputError::MalformedWitness)
    }

    pub fn encode_to_vec(&self) -> Vec<u8> {
        self.consensus_encode_to_vec()
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Error, Encodable, Decodable)]
pub enum MultispendInputError {
    #[error("The witness could not be decoded")]
    MalformedWitness,
    #[error("Signature index {0} is out of range for this account")]
    SignatureIndexOutOfRange(u16),
    #[error("Signature at index {0} is invalid")]
    InvalidSignature(u16),
    #[error("Only {provided} of the required {threshold} signatures were supplied")]
    ThresholdNotMet { provided: u64, threshold: u64 },
    #[error("No such account")]
    UnknownAccount,
    #[error("Account balance is insufficient")]
    InsufficientBalance,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Error, Encodable, Decodable)]
pub enum MultispendOutputError {
    #[error("A credit must be for a non-zero amount")]
    ZeroCredit,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable, Serialize, Deserialize)]
pub struct MultispendOutputOutcome;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable, Serialize, Deserialize)]
pub enum MultispendConsensusItem {}
```

Note `MultispendConsensusItem` is an empty enum: this module contributes nothing to consensus beyond its transaction items. `consensus_proposal` returns an empty vec and `process_consensus_item` is unreachable.

- [ ] **Step 4: Config and endpoints**

`config.rs` mirrors `modules/amm/fedimint-amm-common/src/config.rs` structurally but has no parameters — read that file and follow its use of `plugin_types_trait_impl_config!`. The four structs (`MultispendConfig`, `MultispendConfigPrivate`, `MultispendConfigConsensus`, `MultispendClientConfig`) are all unit structs or empty, because this module has no key material and no tunable consensus parameters.

`endpoints.rs`:

```rust
//! API endpoint names. Spec §3.5.

pub const ACCOUNT_BALANCE_ENDPOINT: &str = "account_balance";
```

- [ ] **Step 5: Wire the module types**

`lib.rs`, mirroring `modules/amm/fedimint-amm-common/src/lib.rs`:

```rust
//! Types shared between the multispend module's client and server.

pub mod account;
pub mod config;
pub mod endpoints;
pub mod types;

use fedimint_core::core::{Decoder, ModuleKind};
use fedimint_core::module::{CommonModuleInit, ModuleCommon, ModuleConsensusVersion};
use fedimint_core::plugin_types_trait_impl_common;

use crate::config::MultispendClientConfig;
use crate::types::{
    MultispendConsensusItem, MultispendInput, MultispendInputError, MultispendOutput,
    MultispendOutputError, MultispendOutputOutcome,
};

pub const KIND: ModuleKind = ModuleKind::from_static_str("multispend");

pub const MODULE_CONSENSUS_VERSION: ModuleConsensusVersion = ModuleConsensusVersion::new(0, 0);

pub struct MultispendModuleTypes;

plugin_types_trait_impl_common!(
    KIND,
    MultispendModuleTypes,
    MultispendClientConfig,
    MultispendInput,
    MultispendOutput,
    MultispendOutputOutcome,
    MultispendConsensusItem,
    MultispendInputError,
    MultispendOutputError
);

#[derive(Debug)]
pub struct MultispendCommonInit;

impl CommonModuleInit for MultispendCommonInit {
    const CONSENSUS_VERSION: ModuleConsensusVersion = MODULE_CONSENSUS_VERSION;
    const KIND: ModuleKind = KIND;

    type ClientConfig = MultispendClientConfig;

    fn decoder() -> Decoder {
        MultispendModuleTypes::decoder_builder().build()
    }
}
```

`plugin_types_trait_impl_common!` requires `Display` on the input, output and outcome types. Add `impl Display` for each — for `MultispendInput::Spend` print the account id and amounts, for `MultispendOutput::Credit` print the account id. Check what the macro actually demands at this rev and satisfy it; `modules/amm/fedimint-amm-common/src/types.rs` shows the full set of impls a module needs.

- [ ] **Step 6: Run the gate**

```bash
nix develop -c cargo fmt --all -- --check
nix develop -c cargo check --workspace --all-targets
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo test -p fedimint-multispend-common
```

Expected: clean; 14 tests pass.

- [ ] **Step 7: Commit**

```bash
git add modules/multispend/
git commit -m "feat(multispend): transaction types and module wiring

Spend debits a t-of-n account and carries the full Account inline, because
verify_input is stateless and cannot look the key set up. The witness lives
in the transaction's Witnessed vector, outside the txid, so any t of n can
sign and the signer set need not be fixed in advance.

Witness decoding is whole-input: trailing bytes are an error, since the
witness is attacker-supplied."
```

---

### Task 4: Server crate, database schema, and the `Credit` output

Everything except the input, so the security-critical path lands alone in Task 5 with a reviewer's full attention.

**Files:**
- Create: `modules/multispend/fedimint-multispend-server/Cargo.toml`
- Create: `modules/multispend/fedimint-multispend-server/src/db.rs`
- Create: `modules/multispend/fedimint-multispend-server/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)

**Interfaces:**
- Consumes: everything from Tasks 2 and 3.
- Produces:
  - `AccountKey(pub AccountId)`, `AccountPrefix`
  - `AccountState { balance: Amounts, op_counter: u64 }`
  - `MultispendInit`, `Multispend`
  - `Multispend::process_output` crediting an account
  - `DbKeyPrefix::Account = 0x01`
  - the `account_balance` API endpoint, returning `Option<AccountState>`

- [ ] **Step 1: Create the crate**

`modules/multispend/fedimint-multispend-server/Cargo.toml`, mirroring `modules/amm/fedimint-amm-server/Cargo.toml`'s dependency set (read it — it names the exact workspace deps a server module needs, including `async-trait`, `erased-serde`, `strum`, `strum_macros`). Add `fedimint-multispend-common = { path = "../fedimint-multispend-common" }`.

Add the crate to the workspace `members`.

- [ ] **Step 2: Database schema**

`modules/multispend/fedimint-multispend-server/src/db.rs`, mirroring the structure of `modules/amm/fedimint-amm-server/src/db.rs`:

```rust
//! Server-side database schema. Spec §3.2.

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::Amounts;
use fedimint_core::{impl_db_lookup, impl_db_record};
use fedimint_multispend_common::account::AccountId;
use serde::Serialize;
use strum_macros::EnumIter;

#[repr(u8)]
#[derive(Clone, Copy, Debug, EnumIter)]
pub enum DbKeyPrefix {
    Account = 0x01,
}

impl std::fmt::Display for DbKeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct AccountKey(pub AccountId);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct AccountPrefix;

/// The module's entire consensus state, one row per account.
///
/// The key set is deliberately absent: it is not stored anywhere. An account
/// exists exactly while it has a record, and a record is created by its first
/// credit — so there is no registration step and no way to create an account
/// without funding it.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Encodable, Decodable)]
pub struct AccountState {
    pub balance: Amounts,
    /// Incremented by every input or output touching this account.
    ///
    /// Consensus never reads it. It exists so the non-consensus bulletin
    /// board (spec §4, a later phase) can notice "this account did something"
    /// without any consensus write of its own.
    pub op_counter: u64,
}

impl_db_record!(key = AccountKey, value = AccountState, db_prefix = DbKeyPrefix::Account);
impl_db_lookup!(key = AccountKey, query_prefix = AccountPrefix);
```

- [ ] **Step 3: Write the failing test**

Create `modules/multispend/fedimint-multispend-server/src/lib.rs` with a test module at the end:

```rust
#[cfg(test)]
mod tests {
    use fedimint_core::Amount;
    use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
    use fedimint_core::module::{AmountUnit, Amounts};
    use fedimint_core::secp256k1::rand::rngs::OsRng;
    use fedimint_core::secp256k1::{Keypair, Secp256k1};
    use fedimint_multispend_common::account::{Account, AccountUnchecked};

    use crate::db::{AccountKey, AccountState};

    fn account(n: usize, threshold: u64) -> Account {
        let secp = Secp256k1::new();
        let pub_keys = (0..n)
            .map(|_| Keypair::new(&secp, &mut OsRng).public_key())
            .collect();
        Account::try_from(AccountUnchecked { pub_keys, threshold }).unwrap()
    }

    #[tokio::test]
    async fn credit_creates_then_accumulates() {
        let db = Database::new(fedimint_core::db::mem_impl::MemDatabase::new(), Default::default());
        let account = account(3, 2);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(1000),
        )
        .await;
        dbtx.commit_tx().await;

        let mut dbtx = db.begin_transaction().await;
        let state = dbtx.get_value(&AccountKey(account.id())).await.expect("created");
        assert_eq!(state.balance.get(&AmountUnit::BITCOIN), Some(&Amount::from_msats(1000)));
        assert_eq!(state.op_counter, 1, "the creating credit counts as an operation");

        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(500),
        )
        .await;
        let state = dbtx.get_value(&AccountKey(account.id())).await.expect("still there");
        assert_eq!(state.balance.get(&AmountUnit::BITCOIN), Some(&Amount::from_msats(1500)));
        assert_eq!(state.op_counter, 2);
    }

    #[tokio::test]
    async fn credit_keeps_units_separate() {
        let db = Database::new(fedimint_core::db::mem_impl::MemDatabase::new(), Default::default());
        let account = account(2, 1);
        let other = AmountUnit::new_custom(1);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(1000),
        )
        .await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_custom(other, Amount::from_msats(7)),
        )
        .await;

        let state = dbtx.get_value(&AccountKey(account.id())).await.unwrap();
        assert_eq!(state.balance.get(&AmountUnit::BITCOIN), Some(&Amount::from_msats(1000)));
        assert_eq!(state.balance.get(&other), Some(&Amount::from_msats(7)));
    }
}
```

- [ ] **Step 4: Run to verify it fails**

Run: `nix develop -c cargo test -p fedimint-multispend-server`

Expected: FAIL to compile — `credit_account` is undefined.

- [ ] **Step 5: Implement the module skeleton and `credit_account`**

Write `MultispendInit` and `Multispend` mirroring `modules/amm/fedimint-amm-server/src/lib.rs:180-290` (its `ServerModuleInit` impl) — same `versions`, `supported_api_versions`, `init`, `trusted_dealer_gen`, `distributed_gen`, `get_client_config`, `validate_config`, `get_database_migrations`, and `dump_database` shapes, with the AMM's config parameters removed. `type Params = ();`.

Then the shared credit helper, which both `process_output` and Task 5's tests use:

```rust
/// Add to an account's balance, creating its record if this is the first
/// credit, and count the operation.
pub(crate) async fn credit_account(
    dbtx: &mut DatabaseTransaction<'_>,
    account: AccountId,
    amounts: &Amounts,
) {
    let mut state = dbtx
        .get_value(&AccountKey(account))
        .await
        .unwrap_or(AccountState { balance: Amounts::ZERO, op_counter: 0 });

    state.balance = state
        .balance
        .checked_add(amounts)
        .expect("an account balance cannot overflow: total issuance is bounded");
    state.op_counter += 1;

    dbtx.insert_entry(&AccountKey(account), &state).await;
}
```

Check `Amounts`' actual addition API at this rev — `fedimint-server/src/consensus/transaction.rs`'s `FundingVerifier` uses `checked_add_mut`, so an owned `checked_add` may not exist; use whichever it provides and keep the overflow behaviour explicit rather than silently saturating.

And `process_output`:

```rust
async fn process_output<'a, 'b>(
    &'a self,
    dbtx: &mut DatabaseTransaction<'b>,
    output: &'a MultispendOutput,
    _out_point: OutPoint,
) -> Result<TransactionItemAmounts, MultispendOutputError> {
    let MultispendOutput::Credit { account, amounts } = output;

    // A zero credit would create an account record with no funds, which the
    // "an account exists exactly while it has a balance" invariant relies on
    // being impossible.
    if amounts.is_zero() {
        return Err(MultispendOutputError::ZeroCredit);
    }

    credit_account(dbtx, *account, amounts).await;

    Ok(TransactionItemAmounts { amounts: amounts.clone(), fees: Amounts::ZERO })
}
```

The output carries its own `amounts`, which is how every module in the tree does it — `AmmOutput::SwapV0` has an `amount_in` field and `process_output` reads it (`modules/amm/fedimint-amm-server/src/lib.rs:964-995`). There is no mechanism by which an output's amount arrives from outside the output; core takes the amount from what `process_output` returns.

If `Amounts` has no `is_zero()` at this rev, the equivalent is "every unit's amount is zero, or the map is empty" — write it explicitly rather than comparing against a constructed zero value, since `Amounts` is a map and two zero-valued maps may not compare equal.

`audit` must report the module's liability: every account balance, per unit, negated (the module owes those funds to account holders). Mirror `modules/amm/fedimint-amm-server/src/lib.rs`'s `audit`, which walks its balance rows.

`consensus_proposal` returns `vec![]`; `process_consensus_item` returns an error, since `MultispendConsensusItem` is uninhabited and the arm is unreachable.

- [ ] **Step 6: The `account_balance` API endpoint**

Spec §3.5. The module's only endpoint, read-only, so that a client (and Task 6's integration tests) can see an account's balance:

```rust
fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
    vec![api_endpoint! {
        ACCOUNT_BALANCE_ENDPOINT,
        ApiVersion::new(0, 0),
        async |_module: &Multispend, context, account: AccountId| -> Option<AccountState> {
            Ok(context.dbtx().get_value(&AccountKey(account)).await)
        }
    }]
}
```

Reads are deliberately unauthenticated: an `AccountId` is a hash, and the balance of an account whose id you already know is visible in consensus transactions anyway.

Mirror `modules/amm/fedimint-amm-server/src/lib.rs`'s `api_endpoints` for the exact macro shape and how it reaches a dbtx — the snippet above is the intent, not necessarily the literal signature at this rev.

Add a test asserting the endpoint returns `None` for an account that has never been credited, and the right balance after a credit. An account that does not exist must not read as a zero balance — the two are different, and the bulletin board in a later phase distinguishes them to decide whether an account is funded.

- [ ] **Step 7: Run the gate**

```bash
nix develop -c cargo fmt --all -- --check
nix develop -c cargo check --workspace --all-targets
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo test -p fedimint-multispend-common
nix develop -c cargo test -p fedimint-multispend-server
```

Expected: clean; 2 server tests pass.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml modules/multispend/
git commit -m "feat(multispend): server crate, account records, and the credit output

Consensus state is one row per account: a multi-unit balance and an operation
counter that consensus never reads, present so a later non-consensus bulletin
board can notice account activity without a consensus write.

An account record is created by its first credit, so there is no way to make
one without funding it."
```

---

### Task 5: `verify_input` and `process_input`

The security-critical task. Both halves of the obligation in the Global Constraints land here, and neither is safe alone.

**Files:**
- Modify: `modules/multispend/fedimint-multispend-server/src/lib.rs`

**Interfaces:**
- Consumes: `Account::{pub_key_at, threshold, id}`, `SpendWitness::decode`, `AccountKey`, `AccountState`, `credit_account`.
- Produces: `Multispend::verify_input`, `Multispend::process_input` returning `InputAuth::SelfVerified`.

- [ ] **Step 1: Write the failing tests**

Append to the server crate's test module. `InputAuthCtx::new(txid, in_idx, witness)` is public, so `verify_input` is directly testable without a federation:

```rust
    use fedimint_core::TransactionId;
    use fedimint_core::module::InputAuthCtx;
    use fedimint_core::secp256k1::Keypair;
    use fedimint_multispend_common::types::{MultispendInput, SpendWitness};
    use std::collections::BTreeMap;

    /// An account plus the keypairs behind it, in the same ascending order
    /// the account's indices use.
    fn account_with_keys(n: usize, threshold: u64) -> (Account, Vec<Keypair>) {
        let secp = Secp256k1::new();
        let mut kps: Vec<Keypair> = (0..n).map(|_| Keypair::new(&secp, &mut OsRng)).collect();
        kps.sort_by_key(|kp| kp.public_key());

        let pub_keys = kps.iter().map(|kp| kp.public_key()).collect();
        let account = Account::try_from(AccountUnchecked { pub_keys, threshold }).unwrap();
        (account, kps)
    }

    fn sign_at(ctx: &InputAuthCtx<'_>, kps: &[Keypair], idxs: &[u16]) -> Vec<u8> {
        let secp = Secp256k1::new();
        let signatures = idxs
            .iter()
            .map(|&i| (i, secp.sign_schnorr(&ctx.txid_message(), &kps[usize::from(i)])))
            .collect::<BTreeMap<_, _>>();
        SpendWitness { signatures }.encode_to_vec()
    }

    const TXID: [u8; 32] = [0x33; 32];

    fn ctx_for(witness: &[u8]) -> InputAuthCtx<'_> {
        InputAuthCtx::new(TransactionId::from_byte_array(TXID), 0, witness)
    }

    fn spend(account: &Account) -> MultispendInput {
        MultispendInput::Spend {
            account: account.clone(),
            amounts: Amounts::new_bitcoin_msats(500),
        }
    }

    #[test]
    fn accepts_exactly_threshold_signatures() {
        let (account, kps) = account_with_keys(3, 2);
        let witness = sign_at(&ctx_for(&[]), &kps, &[0, 2]);

        Multispend::test_verify_input(&spend(&account), &ctx_for(&witness))
            .expect("2 of 3 is enough");
    }

    #[test]
    fn accepts_more_than_threshold_signatures() {
        let (account, kps) = account_with_keys(3, 2);
        let witness = sign_at(&ctx_for(&[]), &kps, &[0, 1, 2]);

        Multispend::test_verify_input(&spend(&account), &ctx_for(&witness))
            .expect("any t of n, and more than t is still fine");
    }

    #[test]
    fn rejects_below_threshold() {
        let (account, kps) = account_with_keys(3, 2);
        let witness = sign_at(&ctx_for(&[]), &kps, &[1]);

        assert_eq!(
            Multispend::test_verify_input(&spend(&account), &ctx_for(&witness)),
            Err(MultispendInputError::ThresholdNotMet { provided: 1, threshold: 2 })
        );
    }

    #[test]
    fn rejects_a_signature_over_a_different_txid() {
        let (account, kps) = account_with_keys(3, 2);

        // Sign a different transaction entirely.
        let other = InputAuthCtx::new(TransactionId::from_byte_array([0x44; 32]), 0, &[]);
        let witness = sign_at(&other, &kps, &[0, 1]);

        assert_eq!(
            Multispend::test_verify_input(&spend(&account), &ctx_for(&witness)),
            Err(MultispendInputError::InvalidSignature(0)),
            "this is the detach-and-reattach attack; it must not verify"
        );
    }

    #[test]
    fn rejects_a_signature_by_a_non_member() {
        let (account, _) = account_with_keys(3, 2);
        let (_, outsiders) = account_with_keys(3, 2);
        let witness = sign_at(&ctx_for(&[]), &outsiders, &[0, 1]);

        assert!(Multispend::test_verify_input(&spend(&account), &ctx_for(&witness)).is_err());
    }

    #[test]
    fn rejects_an_out_of_range_index() {
        let (account, kps) = account_with_keys(3, 2);
        let secp = Secp256k1::new();
        let sig = secp.sign_schnorr(&ctx_for(&[]).txid_message(), &kps[0]);

        let witness = SpendWitness {
            signatures: BTreeMap::from([(0u16, sig), (9u16, sig)]),
        }
        .encode_to_vec();

        assert_eq!(
            Multispend::test_verify_input(&spend(&account), &ctx_for(&witness)),
            Err(MultispendInputError::SignatureIndexOutOfRange(9))
        );
    }

    #[test]
    fn rejects_a_malformed_witness() {
        let (account, _) = account_with_keys(3, 2);
        assert_eq!(
            Multispend::test_verify_input(&spend(&account), &ctx_for(&[0xde, 0xad])),
            Err(MultispendInputError::MalformedWitness)
        );
    }

    #[tokio::test]
    async fn process_input_rejects_a_forged_account() {
        // The attack the two-sided obligation exists to stop: an attacker
        // builds their OWN account, signs with their OWN keys -- so
        // verify_input passes -- and tries to spend. process_input must
        // refuse, because that account id has no funded record.
        let db = Database::new(fedimint_core::db::mem_impl::MemDatabase::new(), Default::default());
        let (funded, _) = account_with_keys(3, 2);
        let (forged, forged_kps) = account_with_keys(1, 1);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            funded.id(),
            &Amounts::new_bitcoin_msats(10_000),
        )
        .await;

        let witness = sign_at(&ctx_for(&[]), &forged_kps, &[0]);
        Multispend::test_verify_input(&spend(&forged), &ctx_for(&witness))
            .expect("the attacker's own signatures verify against their own account");

        let err = Multispend::test_process_input(&mut dbtx.to_ref_nc(), &spend(&forged))
            .await
            .expect_err("a self-declared account must not be spendable");
        assert_eq!(err, MultispendInputError::UnknownAccount);
    }

    #[tokio::test]
    async fn process_input_debits_and_counts() {
        let db = Database::new(fedimint_core::db::mem_impl::MemDatabase::new(), Default::default());
        let (account, _) = account_with_keys(3, 2);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(10_000),
        )
        .await;

        Multispend::test_process_input(&mut dbtx.to_ref_nc(), &spend(&account))
            .await
            .expect("a funded account can be spent");

        let state = dbtx.get_value(&AccountKey(account.id())).await.unwrap();
        assert_eq!(state.balance.get(&AmountUnit::BITCOIN), Some(&Amount::from_msats(9_500)));
        assert_eq!(state.op_counter, 2, "credit then spend");
    }

    #[tokio::test]
    async fn process_input_rejects_an_overdraft() {
        let db = Database::new(fedimint_core::db::mem_impl::MemDatabase::new(), Default::default());
        let (account, _) = account_with_keys(3, 2);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(100),
        )
        .await;

        let err = Multispend::test_process_input(&mut dbtx.to_ref_nc(), &spend(&account))
            .await
            .expect_err("500 > 100");
        assert_eq!(err, MultispendInputError::InsufficientBalance);
    }

    #[tokio::test]
    async fn process_input_rejects_spending_a_unit_the_account_lacks() {
        let db = Database::new(fedimint_core::db::mem_impl::MemDatabase::new(), Default::default());
        let (account, _) = account_with_keys(2, 1);

        let mut dbtx = db.begin_transaction().await;
        crate::credit_account(
            &mut dbtx.to_ref_nc(),
            account.id(),
            &Amounts::new_bitcoin_msats(10_000),
        )
        .await;

        let input = MultispendInput::Spend {
            account: account.clone(),
            amounts: Amounts::new_custom(AmountUnit::new_custom(1), Amount::from_msats(1)),
        };

        let err = Multispend::test_process_input(&mut dbtx.to_ref_nc(), &input)
            .await
            .expect_err("a BTC balance must not fund a USDT spend");
        assert_eq!(err, MultispendInputError::InsufficientBalance);
    }
```

`test_verify_input` and `test_process_input` are thin `pub(crate)` wrappers you add so the tests can call the logic without a `Multispend` instance and its config. Implement them as free functions in the crate that both the trait methods and the tests call — do not duplicate the logic into a test-only copy.

- [ ] **Step 2: Run to verify they fail**

Run: `nix develop -c cargo test -p fedimint-multispend-server`

Expected: FAIL to compile — the two helpers are undefined.

- [ ] **Step 3: Implement**

```rust
/// Half one of the obligation in spec §9.2: verify the witness against the
/// key material carried inside the input, bound to the txid.
///
/// Stateless by necessity — `verify_input` has no database access — which is
/// exactly why this alone is NOT sufficient. An attacker can supply their own
/// `Account` and their own signatures and pass this check. `process_input`
/// closes it by requiring the account id to name a funded record.
pub(crate) fn verify_spend(
    input: &MultispendInput,
    ctx: &InputAuthCtx<'_>,
) -> Result<(), MultispendInputError> {
    let MultispendInput::Spend { account, .. } = input;

    let witness = SpendWitness::decode(ctx.witness())?;

    let provided = witness.signatures.len() as u64;
    if provided < account.threshold() {
        return Err(MultispendInputError::ThresholdNotMet {
            provided,
            threshold: account.threshold(),
        });
    }

    for (&idx, sig) in &witness.signatures {
        let pub_key = account
            .pub_key_at(idx)
            .ok_or(MultispendInputError::SignatureIndexOutOfRange(idx))?;

        // `verify_schnorr` binds to the txid and cannot be pointed at another
        // message. Do not replace it with a hand-rolled check.
        if !ctx.verify_schnorr(pub_key, sig) {
            return Err(MultispendInputError::InvalidSignature(idx));
        }
    }

    Ok(())
}

/// Half two: the account must be one the module actually holds funds for.
pub(crate) async fn process_spend(
    dbtx: &mut DatabaseTransaction<'_>,
    input: &MultispendInput,
) -> Result<Amounts, MultispendInputError> {
    let MultispendInput::Spend { account, amounts } = input;

    let mut state = dbtx
        .get_value(&AccountKey(account.id()))
        .await
        .ok_or(MultispendInputError::UnknownAccount)?;

    state.balance = state
        .balance
        .checked_sub(amounts)
        .ok_or(MultispendInputError::InsufficientBalance)?;
    state.op_counter += 1;

    dbtx.insert_entry(&AccountKey(account.id()), &state).await;

    Ok(amounts.clone())
}
```

Check `Amounts`' subtraction API at this rev — it must fail rather than saturate when any unit is short, including a unit the balance does not contain at all. If no per-unit checked subtraction exists, write one and unit-test it; a saturating subtraction here mints money.

Then the trait methods:

```rust
fn verify_input(
    &self,
    input: &MultispendInput,
    ctx: &InputAuthCtx<'_>,
) -> Result<(), MultispendInputError> {
    verify_spend(input, ctx)
}

async fn process_input<'a, 'b, 'c>(
    &'a self,
    dbtx: &mut DatabaseTransaction<'c>,
    input: &'b MultispendInput,
    _in_point: InPoint,
) -> Result<InputMeta, MultispendInputError> {
    let amounts = process_spend(dbtx, input).await?;

    Ok(InputMeta {
        amount: TransactionItemAmounts { amounts, fees: Amounts::ZERO },
        // Authorization was checked in `verify_input`, bound to the txid.
        auth: InputAuth::SelfVerified,
    })
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `nix develop -c cargo test -p fedimint-multispend-server`

Expected: PASS, 13 tests.

- [ ] **Step 5: Prove the forgery test is not vacuous**

Temporarily change `process_spend`'s account lookup to `unwrap_or_default()`-style behaviour that creates a record instead of erroring, and confirm `process_input_rejects_a_forged_account` fails. Then revert. Report what you observed — a test that passes for the wrong reason leaves the module forgeable.

- [ ] **Step 6: Run the gate and commit**

```bash
nix develop -c cargo fmt --all -- --check
nix develop -c cargo check --workspace --all-targets
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo test -p fedimint-multispend-common
nix develop -c cargo test -p fedimint-multispend-server

git add modules/multispend/
git commit -m "feat(multispend): t-of-n spend authorization

verify_input checks the witness against the key material carried inside the
input, bound to the txid via InputAuthCtx::verify_schnorr. That alone is not
sufficient -- it is stateless, so an attacker can supply their own account and
their own signatures and pass it -- so process_input requires the account id
to name a funded record. Both halves are needed; a test asserts the forgery
is refused.

Signatures live in the transaction witness, outside the txid, so any t of n
can sign and the signer set need not be fixed before signing."
```

---

### Task 6: Integration tests through a real federation

**Files:**
- Create: `modules/multispend/fedimint-multispend-tests/Cargo.toml`
- Create: `modules/multispend/fedimint-multispend-tests/src/lib.rs`
- Create: `modules/multispend/fedimint-multispend-tests/src/fixtures.rs`
- Create: `modules/multispend/fedimint-multispend-tests/tests/tests.rs`
- Modify: `Cargo.toml` (workspace `members`)

**Interfaces:**
- Consumes: everything above.
- Produces: `fixtures() -> Fixtures`, `new_federation() -> FederationTest`.

Per spec §9.3 the module has no client and does not use `TransactionBuilder`: these tests assemble a `Transaction` by hand and submit it, which is exactly what the real client will have to do.

- [ ] **Step 1: Create the crate and fixtures**

Mirror `modules/amm/fedimint-amm-tests/Cargo.toml` and `src/fixtures.rs`. The federation needs `mintv2` as primary (for BITCOIN), `dummy` (to bootstrap value from nothing, since `fedimint-testing`'s in-process federation has no Lightning or peg-in), and `multispend`:

```rust
pub fn fixtures() -> Fixtures {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit);
    let fixtures = fixtures.with_module(MultispendClientInit, MultispendInit);
    fixtures.with_module(DummyClientInit, DummyInit)
}
```

There is no multispend client, and none is needed: `Fixtures::with_server_only_module(server)` exists for exactly this case (`fedimint-testing/src/fixtures.rs:170`). Use it. Do NOT write a client init stub, and do not build the real client — that is a later phase.

- [ ] **Step 2: Write the failing tests**

`tests/tests.rs`. Each test builds a transaction by hand: a `dummy` input funding a `multispend` `Credit` output to deposit, then a `multispend` `Spend` input funding a `dummy`-or-`mintv2` output to withdraw, with the witness assembled from the signers' keypairs after computing the txid.

Build the two shared helpers first. Every test uses them, and getting `submit` right is the whole difficulty of this task:

```rust
/// Assemble a transaction, sign each multispend input's witness with the
/// given keypairs, and submit it.
///
/// `signers[i]` are the keypairs signing `inputs[i]`; an empty slice means
/// that input is not a multispend input and contributes an empty witness.
/// Returns the federation's response so a test can assert on rejection.
async fn submit(
    fed: &FederationTest,
    inputs: Vec<DynInput>,
    outputs: Vec<DynOutput>,
    signers: &[&[(u16, Keypair)]],
) -> Result<TransactionId, TransactionError> {
    let nonce: [u8; 8] = [0x77; 8];
    let txid = Transaction::tx_hash_from_parts(&inputs, &outputs, nonce);
    let msg = secp256k1::Message::from_digest(*txid.as_ref());
    let secp = Secp256k1::new();

    let witnesses: Vec<Vec<u8>> = signers
        .iter()
        .map(|kps| {
            if kps.is_empty() {
                return Vec::new();
            }
            let signatures = kps
                .iter()
                .map(|(idx, kp)| (*idx, secp.sign_schnorr(&msg, kp)))
                .collect::<BTreeMap<_, _>>();
            SpendWitness { signatures }.encode_to_vec()
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
    fed.submit_transaction(tx).await
}

/// Read an account's balance in one unit through the module's API.
async fn balance(fed: &FederationTest, account: &Account, unit: AmountUnit) -> Amount {
    let state: Option<AccountState> = fed
        .api_request(ACCOUNT_BALANCE_ENDPOINT, account.id())
        .await
        .expect("the endpoint must answer");

    state
        .map(|s| s.balance.get(&unit).copied().unwrap_or(Amount::ZERO))
        .unwrap_or(Amount::ZERO)
}
```

`FederationTest`'s exact submission and API-request methods are what you must discover — read `modules/amm/fedimint-amm-tests/tests/tests.rs` for how it submits transactions and calls module endpoints, and adapt these two helpers to the real API. The *shape* above is the requirement: one place that computes the txid, signs, and submits.

Then the tests. Here is the first one written out; the rest follow its structure exactly:

```rust
#[tokio::test]
async fn deposit_then_spend_two_of_three() -> anyhow::Result<()> {
    let fed = new_federation().await;
    let (account, kps) = account_with_keys(3, 2);

    // Deposit: a dummy input funds a Credit output.
    submit(
        &fed,
        vec![dummy_input(Amount::from_msats(10_000))],
        vec![DynOutput::from_typed(
            multispend_id(&fed),
            MultispendOutput::Credit {
                account: account.id(),
                amounts: Amounts::new_bitcoin_msats(10_000),
            },
        )],
        &[&[]],
    )
    .await
    .expect("a deposit needs no multispend witness");

    assert_eq!(balance(&fed, &account, AmountUnit::BITCOIN).await, Amount::from_msats(10_000));

    // Spend: 2 of the 3 keys authorize a withdrawal to a dummy output.
    submit(
        &fed,
        vec![DynOutput::from_typed(
            multispend_id(&fed),
            MultispendInput::Spend {
                account: account.clone(),
                amounts: Amounts::new_bitcoin_msats(4_000),
            },
        )],
        vec![dummy_output(Amount::from_msats(4_000))],
        &[&[(0, kps[0]), (2, kps[2])]],
    )
    .await
    .expect("2 of 3 authorizes the spend");

    assert_eq!(balance(&fed, &account, AmountUnit::BITCOIN).await, Amount::from_msats(6_000));
    Ok(())
}
```

The remaining five, each written out in the same shape:

- `a_third_party_cannot_spend` — fund a 2-of-3, then submit a spend signed by two keypairs that are not members. Assert the error is `TransactionError::Input(..)` carrying `MultispendInputError::InvalidSignature(_)`, not merely that submission failed.
- `below_threshold_is_rejected` — one signature on a 2-of-3. Assert `MultispendInputError::ThresholdNotMet { provided: 1, threshold: 2 }`.
- `account_to_account_transfer_in_one_transaction` — `Spend(A)` and `Credit(B)` in one transaction with equal amounts, balanced by the core funding verifier. Assert A fell and B rose by the same amount. This is what replaces fedi's separate transfer system, so it is the most important test here after the first.
- `one_of_one_behaves_as_an_ordinary_wallet` — `Account::single(key)`: fund it, spend it with that one key.
- `an_account_holds_two_units_independently` — credit BITCOIN and a second unit, spend only the second, assert the BITCOIN balance is untouched.

Assert on specific error variants throughout. A test that only checks "submission failed" would pass if the transaction were rejected for being malformed, which is not what it claims to test.

For the second unit, the AMM tests use a test-only `faucet` module (`modules/amm/fedimint-amm-tests/src/faucet/`). Reuse the same approach rather than inventing another: read that module and either depend on it or copy it, and say which you did and why.

- [ ] **Step 3: Run to verify they fail, then implement until they pass**

Run: `nix develop -c cargo test -p fedimint-multispend-tests`

Expected: FAIL first, then PASS once assembly is right. Expect iteration here — this is the first time the module meets real consensus, and it is the first `SelfVerified` module ever run.

Note that a rejection in these tests arrives as `TransactionError::Input(..)` carrying the module's own error, because the witness branch made module rejections diagnosable rather than collapsing them into `InvalidWitnessLength`. Assert on the specific error, not merely that submission failed.

- [ ] **Step 4: Run the gate and commit**

```bash
nix develop -c cargo fmt --all -- --check
nix develop -c cargo check --workspace --all-targets
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo test -p fedimint-multispend-common
nix develop -c cargo test -p fedimint-multispend-server
nix develop -c cargo test -p fedimint-multispend-tests

git add Cargo.toml modules/multispend/
git commit -m "test(multispend): drive t-of-n spends through a real federation

The first SelfVerified module to run against consensus. Transactions are
assembled by hand rather than through TransactionBuilder, which is what the
real client will have to do: the witness must sign the txid, and the txid is
not knowable before build() generates its nonce."
```

---

### Task 7: Register the module in `fedimintd-experimint`

**Files:**
- Modify: `bin/fedimintd-experimint/Cargo.toml`
- Modify: `bin/fedimintd-experimint/src/main.rs`
- Modify: `README.md` (the module table and the intended-topology table)

**Interfaces:**
- Consumes: `MultispendInit`.
- Produces: a `fedimintd` that can run the module.

- [ ] **Step 1: Register it**

Read `bin/fedimintd-experimint/src/main.rs` and add `multispend` alongside `amm` and `usdt` in the module list, following exactly how those two are attached. Add the dependency to that crate's `Cargo.toml`.

- [ ] **Step 2: Update the README**

The root `README.md` has a table of module families and a table of the intended federation topology. Add a `modules/multispend` row to the first (`fedimint-multispend-{common,server,tests}` — note there is deliberately no client yet) and a `multispend` row to the second, describing it as t-of-n accounts holding any unit.

- [ ] **Step 3: Run the gate and commit**

```bash
nix develop -c cargo fmt --all -- --check
nix develop -c cargo check --workspace --all-targets
nix develop -c cargo clippy --workspace --all-targets -- -D warnings

git add bin/ README.md
git commit -m "feat(fedimintd): attach the multispend module"
```

---

## What this plan does not do

- **No bulletin board.** Spec §4 — the per-guardian, non-consensus store for encrypted proposals and verifiable signature postings — is a separate plan. Nothing here reads or writes it. `AccountState::op_counter` exists for it and is otherwise unused, which is deliberate.
- **No client.** Spec §5, and per §9.3 it cannot use `TransactionBuilder` at all, so it is a larger piece of work than it looks. Task 6's hand-assembled transactions are the prototype for it.
- **No membership changes.** `AccountId` is the hash of the account, so members and threshold are immutable by construction. Changing them means creating a new account and moving the funds, which is one ordinary spend.
- **No fee.** Every `TransactionItemAmounts` here has `fees: Amounts::ZERO`. Whether guardians should charge for multispend operations is a real question, deferred rather than answered.
