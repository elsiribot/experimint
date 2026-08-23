//! Wire types shared between the faucet's client and server halves.
//!
//! **Test-only. Never deploy this module to a real federation** — see the
//! module-level doc comment on [`crate::faucet`] for why.

use std::fmt;

use fedimint_core::core::{Decoder, ModuleInstanceId, ModuleKind};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{AmountUnit, CommonModuleInit, ModuleCommon, ModuleConsensusVersion};
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::{Amount, plugin_types_trait_impl_common};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Unique module kind for the test faucet. Distinct from `"mintv2"` and
/// `"amm"` so `ModuleInitRegistry`'s one-instance-per-`ModuleKind` rule
/// (`fedimint-core/src/config.rs:540,616-625`, spec §3.2) lets a federation
/// run mintv2 (BITCOIN), this module (a second unit), and `amm` side by side.
pub const KIND: ModuleKind = ModuleKind::from_static_str("ammfaucet");

/// Unreleased test scaffolding, never versioned for real deployment.
pub const MODULE_CONSENSUS_VERSION: ModuleConsensusVersion = ModuleConsensusVersion::new(0, 0);

/// The one `AmountUnit` this module issues and burns, hardcoded rather than
/// configurable: this module has no config-gen params channel either (the
/// same platform gap `mintv2` hits, spec P16/§3.2), so there is nowhere to
/// thread a chosen unit through even if we wanted one. `new_custom(1)` is not
/// arbitrary — it is the exact second entry `fedimint-amm-server`'s own
/// `default_consensus_config()` (`fedimint-amm-server/src/lib.rs:77`)
/// allowlists alongside `AmountUnit::BITCOIN`, so an AMM instance built with
/// its default config trades against this unit with no AMM-side changes at
/// all.
pub fn faucet_unit() -> AmountUnit {
    AmountUnit::new_custom(1)
}

/// No consensus items (mirrors `fedimint-dummy-common` and `fedimint-amm`:
/// neither needs one, and this module's balances need no session replay or
/// deadline logic either).
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct FaucetConsensusItem;

/// Debits `amount` from `pub_key`'s server-side balance (spec-mandated shape:
/// "an input that debits it"). Consensus-checked: rejected with
/// [`FaucetInputError::InsufficientBalance`] if the stored balance is less
/// than `amount`. This is the only real "spend" protection this module has —
/// deliberately no cryptography beyond the ordinary input-signature check
/// every module gets for free from `pub_key` (`InputMeta::pub_key`), no
/// blind signatures, no DKG key material.
///
/// Declares real backing (`Amounts::new_custom(faucet_unit(), amount)`) to
/// core, exactly like a `mintv2` note spend — this is what a same-transaction
/// `AmmOutput` (e.g. `DepositV0`'s hi leg) actually gets funded from.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct FaucetInput {
    pub amount: Amount,
    pub pub_key: PublicKey,
}

/// Credits `amount` to `pub_key`'s server-side balance (spec-mandated shape:
/// "an output that credits a pubkey-keyed balance"). Split into two variants
/// (fix pass: review Important 1) so this module tells core the truth about
/// how much value each output really represents, rather than always
/// declaring nothing:
///
/// - [`FaucetOutput::MintV0`] is the deliberate free-minting bootstrap path —
///   only ever built by [`crate::faucet::client::FaucetClientModule::mint`],
///   this module's one "give me money" entry point. It credits `amount` to
///   `pub_key` unconditionally, for free: this IS the faucet, safe only
///   because the module is test-only and never runs in a real federation.
///   Declares **no** backing to core (`TransactionItemAmounts::amounts` is
///   always empty — see `server.rs`'s `process_output`): there genuinely is
///   nothing real behind it, since it is the one place this module invents
///   value from nothing rather than moving value already accounted for
///   elsewhere in the same transaction. See `server.rs`'s module doc comment
///   for why that stays solvent under the global audit assert
///   (`fedimint-server/src/consensus/engine.rs:1058-1067`) despite creating
///   value with no backing input.
/// - [`FaucetOutput::ReceiveV0`] is what
///   [`crate::faucet::client::FaucetClientModule::create_final_inputs_and_outputs`]
///   builds for the ordinary receive/change branch — the credit a swap's
///   `unit_out` leg lands in, or the change a spend leaves behind. It also
///   credits `amount` to `pub_key`, but declares real backing to core
///   (`Amounts::new_custom(faucet_unit(), amount)`), exactly like a `mintv2`
///   receive output: by construction it only ever appears in a transaction
///   whose matching input or other-module output funds it, so declaring the
///   real amount is both truthful and required — see `server.rs`'s module doc
///   comment for why the funding check needs this to be true for the AMM
///   integration tests to actually exercise a real `mintv2`-equivalent
///   funding check on this unit.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub enum FaucetOutput {
    MintV0 {
        amount: Amount,
        pub_key: PublicKey,
    },
    ReceiveV0 {
        amount: Amount,
        pub_key: PublicKey,
    },
    #[encodable_default]
    Default {
        variant: u64,
        bytes: Vec<u8>,
    },
}

/// No information beyond acceptance is needed by the client — mirrors
/// `fedimint-dummy-common::DummyOutputOutcome`.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct FaucetOutputOutcome;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Error, Encodable, Decodable)]
pub enum FaucetInputError {
    #[error("balance is insufficient to debit this amount")]
    InsufficientBalance,
    #[error("unknown input variant")]
    UnknownVariant,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Error, Encodable, Decodable)]
pub enum FaucetOutputError {
    #[error("crediting this amount would overflow the stored balance")]
    BalanceOverflow,
    #[error("unknown output variant")]
    UnknownVariant,
}

pub struct FaucetModuleTypes;

plugin_types_trait_impl_common!(
    KIND,
    FaucetModuleTypes,
    FaucetClientConfig,
    FaucetInput,
    FaucetOutput,
    FaucetOutputOutcome,
    FaucetConsensusItem,
    FaucetInputError,
    FaucetOutputError
);

#[derive(Debug)]
pub struct FaucetCommonInit;

impl CommonModuleInit for FaucetCommonInit {
    const CONSENSUS_VERSION: ModuleConsensusVersion = MODULE_CONSENSUS_VERSION;
    const KIND: ModuleKind = KIND;

    type ClientConfig = FaucetClientConfig;

    fn decoder() -> Decoder {
        FaucetModuleTypes::decoder_builder().build()
    }
}

/// Empty: this module holds no key material (spec-mandated: "no DKG key
/// material") and has no per-federation setup parameters (`faucet_unit()` is
/// a hardcoded constant, not config).
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct FaucetClientConfig;

impl fmt::Display for FaucetClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FaucetClientConfig")
    }
}

impl fmt::Display for FaucetInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FaucetInput {}", self.amount)
    }
}

impl fmt::Display for FaucetOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FaucetOutput::MintV0 { amount, .. } => write!(f, "FaucetOutput::MintV0 {amount}"),
            FaucetOutput::ReceiveV0 { amount, .. } => write!(f, "FaucetOutput::ReceiveV0 {amount}"),
            FaucetOutput::Default { variant, bytes } => write!(
                f,
                "FaucetOutput::Default variant={variant} bytes_len={}",
                bytes.len()
            ),
        }
    }
}

impl fmt::Display for FaucetOutputOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FaucetOutputOutcome")
    }
}

impl fmt::Display for FaucetConsensusItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FaucetConsensusItem")
    }
}
