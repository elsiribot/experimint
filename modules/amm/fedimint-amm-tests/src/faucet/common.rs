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
/// "an output that credits a pubkey-keyed balance"), unconditionally — this
/// IS the faucet: anyone may mint any amount to any key for free. Safe only
/// because this module is test-only and never runs in a real federation.
///
/// Declares **no** backing to core (`TransactionItemAmounts::amounts` is
/// always empty — see `server.rs`'s `process_output`), exactly mirroring how
/// `fedimint-amm-server`'s own `SwapV0` credits a `Balance` without
/// declaring `unit_out` (spec §7.4, §9.1): the credit is real (written to
/// this module's own database) but invisible to core's per-unit funding
/// check, which is what makes minting from nothing possible in the first
/// place — a real input can't be conjured for a mint transaction that has
/// nothing to spend. See `server.rs`'s module doc comment for why this stays
/// solvent under the global audit assert (`fedimint-server/src/consensus/
/// engine.rs:1058-1067`) despite creating value with no backing input.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct FaucetOutput {
    pub amount: Amount,
    pub pub_key: PublicKey,
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
        write!(f, "FaucetOutput {}", self.amount)
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
