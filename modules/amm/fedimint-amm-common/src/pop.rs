//! Proof of possession for outputs that name a destination key.
//!
//! # Why outputs need one
//!
//! Core validates a transaction's signatures against the pubkeys returned by
//! `process_input` only (`fedimint-server/src/consensus/transaction.rs`:
//! inputs push `meta.pub_key`, `validate_signatures` runs, *then* outputs are
//! processed). Outputs contribute no pubkey and are never signed for — which
//! is correct in general, because an output is how you pay someone, and the
//! recipient is not a party to your transaction.
//!
//! The consequence for this module is that `recipient_pk`/`owner_pk` are
//! attacker-writable destinations: anyone may spend their *own* funds to
//! create a `Balance` or `LpPosition` at *your* key. That is not theft (the
//! pubkey, not the record, authorises spending), but the record also carries
//! the `tweak` your seed-only recovery needs in order to recognise the record
//! as yours (§8.2). An attacker who creates the record first fixes a garbage
//! tweak in it, and your position becomes invisible to recovery forever.
//!
//! A proof of possession closes this structurally rather than defensively: a
//! signature by the named key, over the output that names it, cannot be
//! produced by anyone who does not hold that key. You cannot create a record
//! at a key you do not control, so the first writer is always the owner.
//!
//! # What it must cover
//!
//! The signature covers the output's own fields — **including the pubkey and
//! the tweak** — and nothing else.
//!
//! - Covering the **tweak** is what makes the PoP non-transplantable. A PoP
//!   over a bare "I own this key" constant would be liftable out of a pending
//!   transaction and re-paired with a garbage tweak, reinstating the attack
//!   with extra steps.
//! - Covering the **pubkey** defeats the x-only parity edge. Schnorr verifies
//!   against `pk.x_only_public_key()`, so `P` and its negation share a
//!   verification key while being *different* database keys. With the full
//!   pubkey in the signed message, a signature made for `P` does not verify
//!   for the negated `P`.
//!
//! Replaying the *whole* tuple verbatim is harmless and deliberately allowed:
//! it recreates the record the owner already asked for, with the owner's own
//! tweak. There is nothing to gain and a deposit to lose.
//!
//! The signature does not cover the transaction. It cannot — the output is
//! part of the transaction being hashed — and it does not need to: the PoP
//! answers "may this key be named here", while funding and authorisation are
//! already answered by core's input signature check.
//!
//! # Domain separation
//!
//! Swap and deposit payloads lead with distinct domain bytes, so a signature
//! solicited for one can never be replayed as the other. The two payload
//! shapes already differ, but relying on that would make the separation an
//! accident of field types rather than a stated property.

use fedimint_core::encoding::Encodable;
use fedimint_core::module::AmountUnit;
use fedimint_core::secp256k1::{Keypair, Message, PublicKey, SECP256K1, schnorr};
use fedimint_core::{Amount, BitcoinHash};

use crate::pool_id::PoolId;

/// Domain byte for [`swap_pop_message`]. Never reuse a value here.
const DOMAIN_SWAP: u8 = 0;
/// Domain byte for [`deposit_pop_message`]. Never reuse a value here.
const DOMAIN_DEPOSIT: u8 = 1;

/// Exactly the bytes a `SwapV0` proof of possession commits to.
#[derive(Encodable)]
struct SwapPopPayload {
    domain: u8,
    unit_in: AmountUnit,
    unit_out: AmountUnit,
    amount_in: Amount,
    min_out: Amount,
    recipient_pk: PublicKey,
    tweak: [u8; 16],
}

/// Exactly the bytes a `DepositV0` proof of possession commits to.
#[derive(Encodable)]
struct DepositPopPayload {
    domain: u8,
    pool: PoolId,
    amount_lo: Amount,
    amount_hi: Amount,
    min_shares: u64,
    owner_pk: PublicKey,
    tweak: [u8; 16],
}

/// The message a `SwapV0`'s `pop` must sign.
///
/// The single source of truth for both sides: the client signs this, the
/// server verifies against it. If these ever became two implementations they
/// could silently disagree and reject every honest swap — so they are one
/// function, called from both.
#[must_use]
pub fn swap_pop_message(
    unit_in: AmountUnit,
    unit_out: AmountUnit,
    amount_in: Amount,
    min_out: Amount,
    recipient_pk: PublicKey,
    tweak: [u8; 16],
) -> Message {
    let payload = SwapPopPayload {
        domain: DOMAIN_SWAP,
        unit_in,
        unit_out,
        amount_in,
        min_out,
        recipient_pk,
        tweak,
    };

    Message::from_digest(payload.consensus_hash_sha256().to_byte_array())
}

/// The message a `DepositV0`'s `pop` must sign. See [`swap_pop_message`].
#[must_use]
pub fn deposit_pop_message(
    pool: PoolId,
    amount_lo: Amount,
    amount_hi: Amount,
    min_shares: u64,
    owner_pk: PublicKey,
    tweak: [u8; 16],
) -> Message {
    let payload = DepositPopPayload {
        domain: DOMAIN_DEPOSIT,
        pool,
        amount_lo,
        amount_hi,
        min_shares,
        owner_pk,
        tweak,
    };

    Message::from_digest(payload.consensus_hash_sha256().to_byte_array())
}

/// Sign a proof-of-possession message.
///
/// Deterministic (no auxiliary randomness): the same key and message always
/// produce the same signature. That keeps this WASM-safe — no RNG — and makes
/// a resubmitted output byte-identical to its first submission rather than a
/// second, differently-signed object.
#[must_use]
pub fn sign_pop(keypair: &Keypair, message: &Message) -> schnorr::Signature {
    SECP256K1.sign_schnorr_no_aux_rand(message, keypair)
}

/// Verify a proof of possession against the key it claims to prove.
///
/// Returns `bool` rather than `Result` so callers cannot accidentally
/// propagate a verification failure as some unrelated error; the caller
/// decides which rejection this maps to.
#[must_use]
pub fn verify_pop(signature: &schnorr::Signature, message: &Message, pubkey: &PublicKey) -> bool {
    SECP256K1
        .verify_schnorr(signature, message, &pubkey.x_only_public_key().0)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use fedimint_core::secp256k1::rand::rngs::OsRng;

    use super::*;

    fn keypair() -> Keypair {
        Keypair::new(SECP256K1, &mut OsRng)
    }

    fn swap_msg(pk: PublicKey, tweak: [u8; 16]) -> Message {
        swap_pop_message(
            AmountUnit::BITCOIN,
            AmountUnit::new_custom(1),
            Amount::from_msats(100),
            Amount::from_msats(1),
            pk,
            tweak,
        )
    }

    #[test]
    fn a_valid_pop_verifies() {
        let kp = keypair();
        let msg = swap_msg(kp.public_key(), [1u8; 16]);

        assert!(verify_pop(&sign_pop(&kp, &msg), &msg, &kp.public_key()));
    }

    /// The core property: you cannot produce a PoP for a key you do not hold.
    #[test]
    fn another_key_cannot_forge_a_pop() {
        let victim = keypair();
        let attacker = keypair();
        let msg = swap_msg(victim.public_key(), [1u8; 16]);

        assert!(!verify_pop(
            &sign_pop(&attacker, &msg),
            &msg,
            &victim.public_key()
        ));
    }

    /// A PoP lifted from a pending transaction must not survive being
    /// re-paired with a different tweak — that is the squatting attack this
    /// whole mechanism exists to stop.
    #[test]
    fn a_pop_does_not_transplant_to_a_different_tweak() {
        let kp = keypair();
        let honest = swap_msg(kp.public_key(), [1u8; 16]);
        let garbage = swap_msg(kp.public_key(), [0xffu8; 16]);
        let signature = sign_pop(&kp, &honest);

        assert!(verify_pop(&signature, &honest, &kp.public_key()));
        assert!(!verify_pop(&signature, &garbage, &kp.public_key()));
    }

    /// Every signed field must actually be covered; a PoP must not survive
    /// any of them changing.
    #[test]
    fn every_covered_field_changes_the_message() {
        let kp = keypair();
        let pk = kp.public_key();
        let base = swap_msg(pk, [1u8; 16]);

        let variants = [
            swap_pop_message(
                AmountUnit::new_custom(7),
                AmountUnit::new_custom(1),
                Amount::from_msats(100),
                Amount::from_msats(1),
                pk,
                [1u8; 16],
            ),
            swap_pop_message(
                AmountUnit::BITCOIN,
                AmountUnit::new_custom(7),
                Amount::from_msats(100),
                Amount::from_msats(1),
                pk,
                [1u8; 16],
            ),
            swap_pop_message(
                AmountUnit::BITCOIN,
                AmountUnit::new_custom(1),
                Amount::from_msats(101),
                Amount::from_msats(1),
                pk,
                [1u8; 16],
            ),
            swap_pop_message(
                AmountUnit::BITCOIN,
                AmountUnit::new_custom(1),
                Amount::from_msats(100),
                Amount::from_msats(2),
                pk,
                [1u8; 16],
            ),
            swap_pop_message(
                AmountUnit::BITCOIN,
                AmountUnit::new_custom(1),
                Amount::from_msats(100),
                Amount::from_msats(1),
                keypair().public_key(),
                [1u8; 16],
            ),
        ];

        for (i, variant) in variants.iter().enumerate() {
            assert_ne!(
                base.as_ref(),
                variant.as_ref(),
                "field {i} is not covered by the signed message"
            );
        }
    }

    /// A swap PoP must not be replayable as a deposit PoP. The domain byte,
    /// not the payload shape, is what guarantees this.
    #[test]
    fn swap_and_deposit_domains_are_separated() {
        let kp = keypair();
        let pk = kp.public_key();
        let tweak = [1u8; 16];

        let swap = swap_pop_message(
            AmountUnit::BITCOIN,
            AmountUnit::new_custom(1),
            Amount::from_msats(100),
            Amount::from_msats(1),
            pk,
            tweak,
        );
        let deposit = deposit_pop_message(
            PoolId::new(AmountUnit::BITCOIN, AmountUnit::new_custom(1)).expect("distinct units"),
            Amount::from_msats(100),
            Amount::from_msats(1),
            0,
            pk,
            tweak,
        );

        assert_ne!(swap.as_ref(), deposit.as_ref());
        assert!(!verify_pop(&sign_pop(&kp, &swap), &deposit, &pk));
    }
}
