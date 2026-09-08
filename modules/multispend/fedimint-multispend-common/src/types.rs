//! Transaction types. Spec §3.3 and §3.4.

use std::collections::BTreeMap;
use std::fmt;

use fedimint_core::Amount;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::Amounts;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::secp256k1::schnorr;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::account::{Account, AccountId};

/// True if `amounts` is safe to hand to `Amounts`' arithmetic.
///
/// `Amounts` derives `Decodable` with no invariant enforcement, so a crafted
/// transaction can carry explicit zero-amount entries that no safe
/// constructor (`new_bitcoin`, `new_custom`, `checked_add_mut`, ...) would
/// ever produce — those all filter or assert against a zero entry ever being
/// inserted. `checked_add_mut` asserts per entry, not on the total, so such a
/// value panics a debug node's `debug_assert!` and, on a release node,
/// silently inserts a spurious zero-valued entry that persists in an
/// `AccountState.balance` forever. Every wire-supplied `Amounts` — whether it
/// arrives via a `MultispendOutput::Credit` or (Task 5) a
/// `MultispendInput::Spend` — must be checked with this before it is added to
/// any balance or returned to core inside `TransactionItemAmounts`.
///
/// Also rejects the empty map: an all-zero (or entirely empty) `amounts`
/// carries no value, and `process_output` relies on that to keep the
/// invariant that an account record exists only while it holds a balance.
pub fn amounts_are_well_formed(amounts: &Amounts) -> bool {
    !amounts.is_empty() && amounts.values().all(|amount| *amount != Amount::ZERO)
}

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
///
/// Deliberately does NOT derive `Serialize`/`Deserialize`: [`Amounts`] has no
/// serde impl, so a type containing one can't derive it either. Nothing in
/// `plugin_types_trait_impl_common!` requires serde of an `Input` type —
/// only `ClientConfig` needs it.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub enum MultispendInput {
    Spend { account: Account, amounts: Amounts },
}

impl fmt::Display for MultispendInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MultispendInput::Spend { account, amounts } => write!(
                f,
                "MultispendInput::Spend account={} amounts={amounts:?}",
                account.id()
            ),
        }
    }
}

/// Credits an account, creating its record if this is the first credit.
///
/// See [`MultispendInput`]'s doc comment for why this does not derive serde
/// either: it also carries an [`Amounts`].
#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub enum MultispendOutput {
    Credit {
        account: AccountId,
        amounts: Amounts,
    },
}

impl fmt::Display for MultispendOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MultispendOutput::Credit { account, amounts } => write!(
                f,
                "MultispendOutput::Credit account={account} amounts={amounts:?}"
            ),
        }
    }
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
    /// Uses [`Decodable::consensus_decode_whole`], which is whole-input
    /// decoding: trailing bytes are an error, not ignored.
    pub fn decode(bytes: &[u8]) -> Result<Self, MultispendInputError> {
        Self::consensus_decode_whole(bytes, &ModuleDecoderRegistry::default())
            .map_err(|_| MultispendInputError::MalformedWitness)
    }

    pub fn encode_to_vec(&self) -> Vec<u8> {
        self.consensus_encode_to_vec()
    }
}

#[derive(
    Debug, Error, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable, Hash,
)]
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
    #[error("The input's amounts are not well-formed")]
    MalformedAmounts,
}

#[derive(
    Debug, Error, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable, Hash,
)]
pub enum MultispendOutputError {
    /// Either the whole amount is zero — which would create an account
    /// record with no funds — or an individual unit within it is, which
    /// `Amounts`' arithmetic cannot tolerate (see [`amounts_are_well_formed`]).
    /// Enforced in `process_output` (Task 4); this only names the error.
    ///
    /// Rejecting an all-zero credit does NOT establish "an account exists
    /// exactly while it has a balance": a drained account (spent down to
    /// exactly zero via `process_spend`'s `checked_sub`) keeps its record
    /// with `balance: Amounts::ZERO` by design. Record existence and
    /// "currently funded" are different questions — see `db::AccountState`'s
    /// doc comment.
    #[error("A credit must be non-empty and have a non-zero amount for every unit")]
    ZeroCredit,
    /// `credit_account` hit `Amounts::checked_add`'s overflow ceiling. Only
    /// reachable for a non-BITCOIN unit: `AmountUnit::BITCOIN`'s total supply
    /// is bounded well under `u64::MAX` msats by the wallet backing it, but
    /// this module accepts arbitrary units (spec — scope), and nothing
    /// bounds an attacker-chosen unit's issuance the same way. Returned
    /// instead of panicking: every guardian evaluates `process_output`
    /// identically inside consensus, so a panic here would be a
    /// deterministic, simultaneous halt of the whole federation rather than
    /// a single rejected transaction.
    #[error("Crediting this account would overflow its balance for this unit")]
    BalanceOverflow,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable, Serialize, Deserialize)]
pub struct MultispendOutputOutcome;

impl fmt::Display for MultispendOutputOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MultispendOutputOutcome")
    }
}

/// This module contributes nothing to consensus beyond its transaction
/// items: `consensus_proposal` returns an empty vec and
/// `process_consensus_item` is unreachable.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable, Serialize, Deserialize)]
pub enum MultispendConsensusItem {}

impl fmt::Display for MultispendConsensusItem {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {}
    }
}

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

        let mut bytes = SpendWitness {
            signatures: BTreeMap::from([(0u16, sig)]),
        }
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
