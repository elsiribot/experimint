//! Wire types shared between the faucet's client and server halves.
//!
//! Copied from `fedimint-amm-tests::faucet::common` — see
//! `crate::faucet`'s module doc comment for why this is a copy, not a
//! dependency, and why the `ModuleKind` was renamed from `"ammfaucet"` to
//! `"msfaucet"`.
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

/// Unique module kind for the test faucet, distinct from `mintv2`,
/// `multispend`, and `dummy` so `ModuleInitRegistry`'s one-instance-per-
/// `ModuleKind` rule lets a federation run all four side by side.
pub const KIND: ModuleKind = ModuleKind::from_static_str("msfaucet");

/// Unreleased test scaffolding, never versioned for real deployment.
pub const MODULE_CONSENSUS_VERSION: ModuleConsensusVersion = ModuleConsensusVersion::new(0, 0);

/// The one `AmountUnit` this module issues and burns, hardcoded rather than
/// configurable: this module has no config-gen params channel either, so
/// there is nowhere to thread a chosen unit through even if we wanted one.
pub fn faucet_unit() -> AmountUnit {
    AmountUnit::new_custom(1)
}

/// No consensus items (mirrors `fedimint-dummy-common` and
/// `fedimint-multispend-common`: neither needs one).
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct FaucetConsensusItem;

/// Debits `amount` from `pub_key`'s server-side balance. Consensus-checked:
/// rejected with [`FaucetInputError::InsufficientBalance`] if the stored
/// balance is less than `amount`. This is the only real "spend" protection
/// this module has — deliberately no cryptography beyond the ordinary
/// input-signature check every module gets for free from `pub_key` (via
/// `InputMeta.auth`'s `InputAuth::Key`), no blind signatures, no DKG key
/// material.
///
/// Declares real backing (`Amounts::new_custom(faucet_unit(), amount)`) to
/// core, exactly like a `mintv2` note spend — this is what funds a
/// same-transaction `MultispendOutput::Credit` in the second-unit test.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct FaucetInput {
    pub amount: Amount,
    pub pub_key: PublicKey,
}

/// Credits `amount` to `pub_key`'s server-side balance. Split into two
/// variants so this module tells core the truth about how much value each
/// output really represents, rather than always declaring nothing:
///
/// - [`FaucetOutput::MintV0`] is the deliberate free-minting bootstrap path:
///   it credits `amount` to `pub_key` unconditionally, for free — this IS
///   the faucet, safe only because the module is test-only and never runs in
///   a real federation. Declares **no** backing to core
///   (`TransactionItemAmounts::amounts` is always empty — see `server.rs`'s
///   `process_output`).
/// - [`FaucetOutput::ReceiveV0`] also credits `amount` to `pub_key`, but
///   declares real backing to core (`Amounts::new_custom(faucet_unit(),
///   amount)`), exactly like a `mintv2` receive output: by construction it
///   only ever appears in a transaction whose matching input or other-module
///   output funds it.
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

/// No information beyond acceptance is needed — mirrors
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

/// Empty: this module holds no key material and has no per-federation setup
/// parameters (`faucet_unit()` is a hardcoded constant, not config). There is
/// no client for this module (see `crate::faucet`'s module doc comment), but
/// `CommonModuleInit::ClientConfig` still needs a concrete type, mirroring
/// how `fedimint-multispend-common::MultispendClientConfig` is empty for the
/// same reason.
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
