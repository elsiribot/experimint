//! Recovery-safe client key derivation.
//!
//! See spec §8 for the full rationale. In short: every key the client derives
//! (swap keys, LP keys, ...) is `derive_keypair(root, child, tweak)` for a
//! `tweak` that is **random**, never a counter. A counter would make
//! derivation carry shared mutable state across wallet instances restored
//! from the same seed — two clients would both derive "the next" key and
//! collide, and an old client left running would keep issuing into slots a
//! new one is using. A random tweak has no state to desync.
//!
//! The cost of a random tweak is that recovery (scanning history for outputs
//! that belong to this seed) can no longer just recompute "the next" tweak —
//! it must recognize *any* tweak as ours. We solve that with a grinding
//! scheme ported from `fedimint-mintv2-client`'s `issuance.rs`:
//!
//! - [`tweak_filter`] derives a 32-byte filter from the root secret. Only the
//!   seed holder can compute it.
//! - [`grind_tweak`] repeatedly samples a random 16-byte tweak until
//!   `sha256(tweak, filter)` has two leading zero bytes (checked by
//!   [`check_tweak`]), which happens after roughly 65536 attempts on average.
//! - Recovery then does one cheap hash per historical row to test
//!   [`check_tweak`], and only pays for a real key derivation (comparatively
//!   expensive) on the roughly 1-in-65536 rows that pass the filter. An
//!   outside observer without the root secret cannot compute the filter, so
//!   the tag leaks no ownership information.
//!
//! `grind_tweak` runs client-side only, during transaction construction. It
//! must never be called from consensus-critical code (server-side input/
//! output processing), since it uses randomness and loops for a
//! non-deterministic number of iterations.

use bitcoin_hashes::{Hash, sha256};
use fedimint_core::encoding::Encodable;
use fedimint_core::secp256k1::{Keypair, SECP256K1};
use fedimint_derive_secret::{ChildId, DerivableSecret};
use rand::Rng;

/// Child id for keys used in swap outputs/inputs.
///
/// Only needs to be unique within this module: `module_root_secret` is
/// already namespaced per module instance by `fedimint-client-module`, so
/// this id is uncorrelated with child ids used by other modules (e.g.
/// mintv2's denomination-keyed child ids).
pub const CHILD_SWAP: u64 = 0;

/// Child id for keys used in LP (liquidity provider) outputs/inputs.
pub const CHILD_LP: u64 = 1;

/// Derive the seed-private filter used to recognize this seed's tweaks.
///
/// Only someone holding `root` can compute this value, so publishing
/// `check_tweak` results (e.g. as part of a recovery scan) does not reveal
/// which on-chain outputs belong to this seed to anyone else.
pub fn tweak_filter(root: &DerivableSecret) -> [u8; 32] {
    root.to_random_bytes()
}

/// Check whether `tweak` passes the grinding filter derived from some root
/// secret's [`tweak_filter`].
///
/// A tweak passes when the consensus hash of `(tweak, filter)` has two
/// leading zero bytes, which happens for about 1 in 65536 tweaks. This is
/// the same check `grind_tweak` grinds for and that recovery scanning uses
/// to cheaply reject tweaks that don't belong to a given seed.
pub fn check_tweak(tweak: [u8; 16], filter: [u8; 32]) -> bool {
    (tweak, filter)
        .consensus_hash::<sha256::Hash>()
        .to_byte_array()
        .iter()
        .take(2)
        .all(|b| *b == 0)
}

/// Sample a random tweak that passes `root`'s grinding filter.
///
/// The tweak is drawn fresh from the thread RNG on every call, never from a
/// counter: two client instances (or two calls in the same instance) grinding
/// concurrently on the same root secret produce independent, non-colliding
/// tweaks with overwhelming probability, since the 16-byte tweak space is
/// astronomically larger than the number of tweaks any client will ever
/// grind. Runs client-side only; must not be called from consensus code.
pub fn grind_tweak(root: &DerivableSecret) -> [u8; 16] {
    let filter = tweak_filter(root);

    loop {
        let tweak = rand::thread_rng().r#gen();

        if check_tweak(tweak, filter) {
            return tweak;
        }
    }
}

/// Derive the keypair for a given child id and tweak.
///
/// Deterministic in `(root, child, tweak)`: the same inputs always yield the
/// same keypair, which is what lets recovery reconstruct a key once it has
/// found the tweak that was used to create it.
pub fn derive_keypair(root: &DerivableSecret, child: u64, tweak: [u8; 16]) -> Keypair {
    root.child_key(ChildId(child))
        .tweak(&tweak)
        .to_secp_key(SECP256K1)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use fedimint_derive_secret::DerivableSecret;

    use super::{CHILD_LP, CHILD_SWAP, check_tweak, derive_keypair, grind_tweak, tweak_filter};

    /// A fixed root secret for tests that don't care which seed they use.
    fn test_root_secret() -> DerivableSecret {
        test_root_secret_from(&[0x42; 32])
    }

    /// A root secret derived from an explicit 32-byte seed, for tests that
    /// need two distinct, reproducible seeds to compare.
    fn test_root_secret_from(seed: &[u8; 32]) -> DerivableSecret {
        DerivableSecret::new_root(seed, b"fedimint-amm-client tests")
    }

    #[test]
    fn ground_tweaks_pass_their_own_filter() {
        let root = test_root_secret();
        let filter = tweak_filter(&root);
        for _ in 0..8 {
            assert!(check_tweak(grind_tweak(&root), filter));
        }
    }

    #[test]
    fn a_different_seed_rejects_the_tweak_almost_always() {
        let a = test_root_secret_from(&[1u8; 32]);
        let b = test_root_secret_from(&[2u8; 32]);
        let filter_b = tweak_filter(&b);
        let hits = (0..200)
            .filter(|_| check_tweak(grind_tweak(&a), filter_b))
            .count();
        // ~1/65536 chance each; 200 trials should essentially never hit.
        assert!(hits <= 1, "filter is not seed-specific");
    }

    #[test]
    fn derivation_is_deterministic_in_the_tweak() {
        let root = test_root_secret();
        let t = [9u8; 16];
        assert_eq!(
            derive_keypair(&root, CHILD_SWAP, t).public_key(),
            derive_keypair(&root, CHILD_SWAP, t).public_key()
        );
    }

    #[test]
    fn child_ids_are_namespaced() {
        let root = test_root_secret();
        let t = [9u8; 16];
        assert_ne!(
            derive_keypair(&root, CHILD_SWAP, t).public_key(),
            derive_keypair(&root, CHILD_LP, t).public_key()
        );
    }

    /// The property that motivates random tweaks over a counter (spec §8):
    /// two clients on the SAME seed must never collide.
    #[test]
    fn concurrent_clients_on_one_seed_do_not_collide() {
        let root = test_root_secret();
        let keys: BTreeSet<_> = (0..100)
            .map(|_| derive_keypair(&root, CHILD_SWAP, grind_tweak(&root)).public_key())
            .collect();
        assert_eq!(keys.len(), 100);
    }
}
