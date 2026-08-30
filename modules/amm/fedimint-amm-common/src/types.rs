//! Transaction item types, and the errors `process_input`/`process_output`
//! may return for them. Spec §6.
//!
//! Outputs CREATE records (`Balance`, `LpPosition`) and therefore consume
//! value; inputs DESTROY pre-existing authenticated records and therefore
//! provide value. Spec §3.1. `SwapV0`/`DepositV0` are outputs;
//! `ClaimBalanceV0`/`WithdrawV0` are inputs — do not move an item across
//! that line.

use std::fmt;

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::AmountUnit;
use fedimint_core::{Amount, secp256k1};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::pool_id::PoolId;

/// Outputs CREATE records, so they consume value. Spec §3.1.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub enum AmmOutput {
    /// Sell `amount_in` of `unit_in` into the pool; credit the proceeds as a
    /// `Balance` claimable by `recipient_pk`.
    SwapV0 {
        unit_in: AmountUnit,
        /// MUST differ from `unit_in`.
        unit_out: AmountUnit,
        amount_in: Amount,
        /// Router-equivalent `amountOutMin`, enforced server-side.
        min_out: Amount,
        recipient_pk: secp256k1::PublicKey,
        /// Ground per spec §8; stored on the Balance record for recovery.
        tweak: [u8; 16],
        /// Proof that the submitter holds `recipient_pk`, over every field
        /// above. See [`crate::pop`] for why an output needs one: core signs
        /// for inputs only, so without this anyone could create a record at
        /// anyone's key and fix a garbage `tweak` in it, destroying that
        /// key's seed-only recoverability.
        pop: secp256k1::schnorr::Signature,
    },
    /// Add liquidity; create the pool if absent.
    DepositV0 {
        pool: PoolId,
        amount_lo: Amount,
        amount_hi: Amount,
        /// Router-equivalent slippage guard.
        min_shares: u64,
        owner_pk: secp256k1::PublicKey,
        tweak: [u8; 16],
        /// Proof that the submitter holds `owner_pk`, over every field above.
        /// See [`crate::pop`].
        pop: secp256k1::schnorr::Signature,
    },
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}

impl AmmOutput {
    /// Build a `SwapV0` that proves possession of its own recipient key.
    ///
    /// Takes the keypair rather than the pubkey so that the named key, the
    /// tweak and the signature cannot disagree: the pubkey is *derived* from
    /// the keypair here, and the signature is made over the finished field
    /// set. Constructing the variant literally is still possible (tests that
    /// want a deliberately invalid proof need it), but every honest caller
    /// should come through here.
    #[must_use]
    pub fn new_swap_v0(
        keypair: &secp256k1::Keypair,
        unit_in: AmountUnit,
        unit_out: AmountUnit,
        amount_in: Amount,
        min_out: Amount,
        tweak: [u8; 16],
    ) -> Self {
        let recipient_pk = keypair.public_key();
        let pop = crate::pop::sign_pop(
            keypair,
            &crate::pop::swap_pop_message(
                unit_in,
                unit_out,
                amount_in,
                min_out,
                recipient_pk,
                tweak,
            ),
        );

        AmmOutput::SwapV0 {
            unit_in,
            unit_out,
            amount_in,
            min_out,
            recipient_pk,
            tweak,
            pop,
        }
    }

    /// Build a `DepositV0` that proves possession of its own owner key. See
    /// [`Self::new_swap_v0`].
    #[must_use]
    pub fn new_deposit_v0(
        keypair: &secp256k1::Keypair,
        pool: PoolId,
        amount_lo: Amount,
        amount_hi: Amount,
        min_shares: u64,
        tweak: [u8; 16],
    ) -> Self {
        let owner_pk = keypair.public_key();
        let pop = crate::pop::sign_pop(
            keypair,
            &crate::pop::deposit_pop_message(
                pool, amount_lo, amount_hi, min_shares, owner_pk, tweak,
            ),
        );

        AmmOutput::DepositV0 {
            pool,
            amount_lo,
            amount_hi,
            min_shares,
            owner_pk,
            tweak,
            pop,
        }
    }

    /// Verify this output's proof of possession against the key it names.
    ///
    /// Lives here, next to the variants, so the field list fed to the message
    /// builder cannot drift from the field list the variant actually has —
    /// adding a field to a variant without adding it here is the mistake this
    /// placement is meant to make obvious.
    ///
    /// `Default` (an unknown future variant) has no key to prove and returns
    /// `false`: the server rejects it on the variant match anyway, and a
    /// permissive answer here would be a trap for any later caller.
    #[must_use]
    pub fn verify_pop(&self) -> bool {
        match self {
            AmmOutput::SwapV0 {
                unit_in,
                unit_out,
                amount_in,
                min_out,
                recipient_pk,
                tweak,
                pop,
            } => crate::pop::verify_pop(
                pop,
                &crate::pop::swap_pop_message(
                    *unit_in,
                    *unit_out,
                    *amount_in,
                    *min_out,
                    *recipient_pk,
                    *tweak,
                ),
                recipient_pk,
            ),
            AmmOutput::DepositV0 {
                pool,
                amount_lo,
                amount_hi,
                min_shares,
                owner_pk,
                tweak,
                pop,
            } => crate::pop::verify_pop(
                pop,
                &crate::pop::deposit_pop_message(
                    *pool,
                    *amount_lo,
                    *amount_hi,
                    *min_shares,
                    *owner_pk,
                    *tweak,
                ),
                owner_pk,
            ),
            AmmOutput::Default { .. } => false,
        }
    }
}

impl fmt::Display for AmmOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AmmOutput::SwapV0 {
                unit_in,
                unit_out,
                amount_in,
                ..
            } => write!(
                f,
                "AmmOutput::SwapV0 {amount_in} {unit_in:?} -> {unit_out:?}"
            ),
            AmmOutput::DepositV0 {
                pool,
                amount_lo,
                amount_hi,
                ..
            } => write!(f, "AmmOutput::DepositV0 {pool:?} {amount_lo}/{amount_hi}"),
            AmmOutput::Default { variant, bytes } => write!(
                f,
                "AmmOutput::Default variant={variant} bytes_len={}",
                bytes.len()
            ),
        }
    }
}

/// Inputs DESTROY pre-existing authenticated records, so they provide value.
/// Spec §3.1.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub enum AmmInput {
    /// Claims the ENTIRE balance. No amount field: spec §6.1 explains why a
    /// declared amount would be a second source of truth, and why partial
    /// claims would leave permanent residue in an ungarbage-collected table.
    ClaimBalanceV0 {
        pubkey: secp256k1::PublicKey,
        unit: AmountUnit,
    },
    /// Burn shares, withdraw both sides pro rata.
    WithdrawV0 {
        pool: PoolId,
        owner_pk: secp256k1::PublicKey,
        shares: u64,
        /// Router-equivalent `amountAMin`.
        min_lo: Amount,
        /// Router-equivalent `amountBMin`.
        min_hi: Amount,
        /// Upper bound on the `lo`-side payout (fix pass 4, Important 3).
        /// Has no Uniswap V2 router equivalent — a router's caller declares
        /// exact output amounts up front and a mismatch simply fails to
        /// balance, where here the client instead declares a preview and the
        /// server settles at current reserves, so an upper bound is needed
        /// to catch settlement *above* the preview: without it, the surplus
        /// passes core's checks fine and is silently forfeited under the
        /// overpay rule (P5/P6), since the client's declared transaction
        /// outputs are fixed at the preview regardless. See
        /// `fedimint-amm-client`'s `AmmClientModule::withdraw` doc comment.
        max_lo: Amount,
        /// Upper bound on the `hi`-side payout. See `max_lo`.
        max_hi: Amount,
    },
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}

impl fmt::Display for AmmInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AmmInput::ClaimBalanceV0 { unit, .. } => {
                write!(f, "AmmInput::ClaimBalanceV0 {unit:?}")
            }
            AmmInput::WithdrawV0 { pool, shares, .. } => {
                write!(f, "AmmInput::WithdrawV0 {pool:?} shares={shares}")
            }
            AmmInput::Default { variant, bytes } => write!(
                f,
                "AmmInput::Default variant={variant} bytes_len={}",
                bytes.len()
            ),
        }
    }
}

/// Placeholder `OutputOutcome`: this module carries no information a client
/// needs beyond "the output was accepted", mirroring `DummyOutputOutcome` /
/// `MintOutputOutcome` on the pinned rev.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct AmmOutputOutcome;

impl fmt::Display for AmmOutputOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AmmOutputOutcome")
    }
}

/// This module has no consensus items (spec §10): a `Balance` is always
/// claimable and no reserves are ever earmarked, so there is nothing that
/// needs deadline- or vote-style consensus. `ModuleCommon::ConsensusItem` is
/// nonetheless a mandatory associated type, so — mirroring
/// `MintConsensusItem` on the pinned rev, which documents exactly this
/// situation — this is an enum with only the `#[encodable_default]` variant,
/// so a future peer that *does* gain a real consensus item still decodes
/// against an older binary without breaking it.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub enum AmmConsensusItem {
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}

impl fmt::Display for AmmConsensusItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AmmConsensusItem")
    }
}

#[derive(
    Debug, Error, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable, Hash,
)]
pub enum AmmInputError {
    #[error("unknown input variant")]
    UnknownVariant,
    #[error("no balance for this key and unit")]
    NoSuchBalance,
    #[error("no such pool")]
    NoSuchPool,
    #[error("no LP position for this key")]
    NoSuchPosition,
    #[error("not enough shares")]
    InsufficientShares,
    #[error("payout below min_lo/min_hi")]
    SlippageExceeded,
    #[error("arithmetic error: {0}")]
    Curve(String),
}

#[derive(
    Debug, Error, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable, Hash,
)]
pub enum AmmOutputError {
    #[error("unknown output variant")]
    UnknownVariant,
    /// The output's `pop` is not a valid signature by the key it names.
    ///
    /// Checked before anything else, and before any database read, so an
    /// output that does not prove control of its destination key never
    /// influences module state at all.
    #[error("invalid proof of possession for the named key")]
    InvalidProofOfPossession,
    #[error("unit_in and unit_out must differ")]
    IdenticalUnits,
    #[error("unit not in the federation's allowlist")]
    UnknownUnit,
    #[error("no such pool")]
    NoSuchPool,
    #[error("amount_in below the unit's min_swap_in")]
    BelowMinSwapIn,
    #[error("output below min_out, or shares below min_shares")]
    SlippageExceeded,
    #[error("would exceed MAX_RESERVE")]
    ReserveCapExceeded,
    /// `process_output`'s `SwapV0` arm returns this when
    /// `math::k_non_decreasing` reports a decrease (spec §7.1's backstop).
    ///
    /// Verified (final review, finding T2) not to be reachable through any
    /// legitimate swap given a correct `math::amount_out`: for any
    /// `reserve_in, reserve_out, amount_in >= 1` and `fee_per_mille` in
    /// `[0, 999]`, `(reserve_in + amount_in) * (reserve_out -
    /// amount_out(..)) >= reserve_in * reserve_out` always — algebraically
    /// (the continuous, non-floored formula gives `k_new_exact / k_old =
    /// (reserve_in*1000 + amount_in*1000) / (reserve_in*1000 +
    /// amount_in*(1000-fee)) >= 1`, and `amount_out` only floors `out`
    /// downward, which can only raise `reserve_out_new` and thus `k_new`
    /// further) and confirmed by exhaustive brute force over `reserve_in,
    /// reserve_out, amount_in` in `1..=60` and every `fee_per_mille` in
    /// `0..1000` (186_271_143 combinations, zero violations). This makes
    /// `KInvariantViolated` defense-in-depth against a hypothetical future
    /// bug in `amount_out`, not a path any test can drive through real
    /// settlement — mirroring `CurveError::InsufficientLiquidity` in
    /// `math.rs`, which is unreachable for the same reason (both guards
    /// exist to fail closed rather than trust the arithmetic above them).
    /// `math::k_non_decreasing` itself IS tested directly (with a
    /// hand-constructed, not-otherwise-reachable decrease) in
    /// `math.rs`'s `k_non_decreasing_rejects_a_manufactured_decrease`.
    #[error("k invariant violated")]
    KInvariantViolated,
    #[error("arithmetic error: {0}")]
    Curve(String),
}

#[cfg(test)]
mod tests {
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::module::AmountUnit;
    use fedimint_core::module::registry::ModuleDecoderRegistry;
    use fedimint_core::{Amount, secp256k1};

    use super::*;
    use crate::pool_id::PoolId;

    fn kp() -> secp256k1::Keypair {
        secp256k1::Keypair::from_seckey_slice(secp256k1::SECP256K1, &[1u8; 32])
            .expect("valid secret key")
    }

    fn pk() -> secp256k1::PublicKey {
        kp().public_key()
    }

    #[test]
    fn swap_output_round_trips() {
        let out = AmmOutput::new_swap_v0(
            &kp(),
            AmountUnit::new_custom(0),
            AmountUnit::new_custom(1),
            Amount::from_msats(10_000),
            Amount::from_msats(9_000),
            [7u8; 16],
        );
        let bytes = out.consensus_encode_to_vec();
        let back =
            AmmOutput::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(out, back);
    }

    #[test]
    fn deposit_output_round_trips() {
        let out = AmmOutput::new_deposit_v0(
            &kp(),
            PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap(),
            Amount::from_msats(1_000),
            Amount::from_msats(2_000),
            1,
            [9u8; 16],
        );
        let bytes = out.consensus_encode_to_vec();
        let back =
            AmmOutput::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(out, back);
    }

    #[test]
    fn claim_balance_input_round_trips() {
        let inp = AmmInput::ClaimBalanceV0 {
            pubkey: pk(),
            unit: AmountUnit::new_custom(1),
        };
        let bytes = inp.consensus_encode_to_vec();
        let back =
            AmmInput::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(inp, back);
    }

    #[test]
    fn withdraw_input_round_trips() {
        let inp = AmmInput::WithdrawV0 {
            pool: PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap(),
            owner_pk: pk(),
            shares: 500,
            min_lo: Amount::from_msats(1),
            min_hi: Amount::from_msats(1),
            max_lo: Amount::from_msats(2),
            max_hi: Amount::from_msats(2),
        };
        let bytes = inp.consensus_encode_to_vec();
        let back =
            AmmInput::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(inp, back);
    }

    /// Unknown variants must survive a round trip via #[encodable_default],
    /// so a newer peer's item does not break an older decoder.
    #[test]
    fn unknown_input_variant_round_trips_through_default() {
        let unknown = AmmInput::Default {
            variant: 42,
            bytes: vec![1, 2, 3],
        };
        let bytes = unknown.consensus_encode_to_vec();
        let back =
            AmmInput::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(unknown, back);
    }

    #[test]
    fn unknown_output_variant_round_trips_through_default() {
        let unknown = AmmOutput::Default {
            variant: 7,
            bytes: vec![4, 5, 6],
        };
        let bytes = unknown.consensus_encode_to_vec();
        let back =
            AmmOutput::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(unknown, back);
    }

    /// The consensus-item type has no real variants (spec §10), but the
    /// `#[encodable_default]` catch-all must still round-trip so a future
    /// peer that gains a real item doesn't break an older decoder.
    #[test]
    fn consensus_item_default_round_trips() {
        let unknown = AmmConsensusItem::Default {
            variant: 1,
            bytes: vec![0xAB],
        };
        let bytes = unknown.consensus_encode_to_vec();
        let back =
            AmmConsensusItem::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default())
                .unwrap();
        assert_eq!(unknown, back);
    }

    #[test]
    fn output_outcome_round_trips() {
        let bytes = AmmOutputOutcome.consensus_encode_to_vec();
        let back =
            AmmOutputOutcome::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default())
                .unwrap();
        assert_eq!(AmmOutputOutcome, back);
    }
}
