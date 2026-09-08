//! Transaction types. Spec §3.3 and §3.4.

use std::collections::BTreeMap;
use std::fmt;

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::Amounts;
use fedimint_core::module::registry::ModuleDecoderRegistry;
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
    /// A zero credit would create an account record with no funds, breaking
    /// the invariant that an account exists exactly while it has a balance.
    /// Enforced in `process_output` (Task 4); this only names the error.
    #[error("A credit must be for a non-zero amount")]
    ZeroCredit,
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
