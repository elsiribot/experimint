//! Account identity and its invariants. Spec §3.1.

use std::collections::BTreeSet;
use std::io;

use anyhow::{bail, ensure};
use fedimint_core::bitcoin::hashes::sha256;
use fedimint_core::encoding::{Decodable, DecodeError, Encodable};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::secp256k1::PublicKey;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

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
        ensure!(
            !raw.pub_keys.is_empty(),
            "account must have at least one key"
        );
        ensure!(
            raw.pub_keys.len() <= MAX_ACCOUNT_KEYS,
            "account has {} keys, the maximum is {MAX_ACCOUNT_KEYS}",
            raw.pub_keys.len()
        );

        // `pub_keys` dedupes on the full 33-byte compressed key, but schnorr
        // verification (`InputAuthCtx::verify_schnorr`) compares only the
        // x-only key. A point `M` and its negation `-M` are distinct
        // compressed keys sharing an x-coordinate, and one private key
        // produces valid signatures for both -- so admitting both would let
        // that single key satisfy two "independent" members, collapsing the
        // threshold below what it appears to require.
        let distinct_x_only = raw
            .pub_keys
            .iter()
            .map(|pub_key| pub_key.x_only_public_key().0)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        ensure!(
            distinct_x_only == raw.pub_keys.len(),
            "two keys in this account share an x-coordinate: one private key would produce \
             valid signatures for both, letting it satisfy two members at once and collapse \
             the threshold"
        );

        ensure!(raw.threshold != 0, "account threshold must not be zero");
        if raw.threshold > raw.pub_keys.len() as u64 {
            bail!(
                "account threshold {} exceeds its {} keys",
                raw.threshold,
                raw.pub_keys.len()
            );
        }

        Ok(Account {
            pub_keys: raw.pub_keys,
            threshold: raw.threshold,
        })
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
        AccountId(self.consensus_hash_sha256())
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
    ///
    /// Routed through `try_from` rather than constructing `Self` directly, so
    /// this stays a second construction path in name only: any invariant
    /// `TryFrom<AccountUnchecked>` gains in the future (the x-only check
    /// above is exactly such a case — it postdates this function) applies
    /// here too, instead of silently being skippable by calling `single`.
    /// A single well-formed key can never fail `TryFrom`, so the `expect` is
    /// unreachable, not a real fallibility surface.
    pub fn single(key: PublicKey) -> Self {
        Self::try_from(AccountUnchecked {
            pub_keys: BTreeSet::from([key]),
            threshold: 1,
        })
        .expect("a single key with threshold 1 always satisfies Account's invariants")
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Hash,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Encodable,
    Decodable,
    Serialize,
    Deserialize,
)]
pub struct AccountId(pub sha256::Hash);

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

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
        AccountUnchecked {
            pub_keys: keys(n),
            threshold,
        }
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
        let two = Account::try_from(AccountUnchecked {
            pub_keys: pub_keys.clone(),
            threshold: 2,
        })
        .unwrap();
        let three = Account::try_from(AccountUnchecked {
            pub_keys,
            threshold: 3,
        })
        .unwrap();

        assert_ne!(
            two.id(),
            three.id(),
            "a 2-of-3 and a 3-of-3 over the same keys must be different accounts, \
             or funds could be spent under a lower threshold than the depositor agreed to"
        );
    }

    /// A public key and its negation are distinct 33-byte compressed keys
    /// (so they dedupe as two separate entries in `pub_keys`) but share an
    /// x-coordinate, and schnorr verification (`InputAuthCtx::verify_schnorr`)
    /// compares only the x-only key. One private key produces valid
    /// signatures for both, so an account admitting both members has fewer
    /// independent signers than its `pub_keys.len()` suggests -- a
    /// `{M, -M}` account with `threshold: 2` looks like 2-of-3 (with a third
    /// honest key) but is satisfiable by `M`'s holder alone.
    #[test]
    fn rejects_an_x_only_key_collision() {
        let secp = Secp256k1::new();
        let m = Keypair::new(&secp, &mut OsRng).public_key();
        let neg_m = m.negate(&secp);
        assert_ne!(
            m, neg_m,
            "a key and its negation must be distinct compressed public keys, or this test \
             isn't reproducing the collision it claims to"
        );
        assert_eq!(
            m.x_only_public_key().0,
            neg_m.x_only_public_key().0,
            "a key and its negation must share an x-coordinate, or this test isn't \
             reproducing the collision it claims to"
        );

        let raw = AccountUnchecked {
            pub_keys: BTreeSet::from([m, neg_m]),
            threshold: 2,
        };

        let err = Account::try_from(raw).expect_err(
            "two keys sharing an x-coordinate must be rejected: one private key would \
             satisfy both, collapsing the threshold",
        );
        assert!(
            err.to_string().contains("x-coordinate"),
            "the error should say plainly that two keys share an x-coordinate: {err}"
        );
    }

    /// Same collision as [`rejects_an_x_only_key_collision`], but proving the
    /// check is reached through the decoder, not just `TryFrom` -- mirrors
    /// `decoding_rejects_an_invalid_account` below.
    #[test]
    fn decoding_rejects_an_x_only_key_collision() {
        let secp = Secp256k1::new();
        let m = Keypair::new(&secp, &mut OsRng).public_key();
        let neg_m = m.negate(&secp);

        let raw = AccountUnchecked {
            pub_keys: BTreeSet::from([m, neg_m]),
            threshold: 2,
        };
        let bytes = raw.consensus_encode_to_vec();

        assert!(
            Account::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).is_err(),
            "an x-only key collision must not survive decoding either"
        );
    }

    #[test]
    fn decoding_rejects_an_invalid_account() {
        // An attacker who can get a threshold-0 account past the decoder can
        // spend from it with no signatures at all.
        let raw = AccountUnchecked {
            pub_keys: keys(3),
            threshold: 0,
        };
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
