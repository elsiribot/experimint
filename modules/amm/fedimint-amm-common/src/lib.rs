//! Types shared between the AMM module's client and server.

pub mod config;
pub mod endpoints;
pub mod math;
pub mod pool_id;
pub mod types;

use fedimint_core::core::{Decoder, ModuleInstanceId, ModuleKind};
use fedimint_core::module::{CommonModuleInit, ModuleCommon, ModuleConsensusVersion};
use fedimint_core::plugin_types_trait_impl_common;

use crate::config::AmmClientConfig;
use crate::types::{
    AmmConsensusItem, AmmInput, AmmInputError, AmmOutput, AmmOutputError, AmmOutputOutcome,
};

/// Unique name for this module.
pub const KIND: ModuleKind = ModuleKind::from_static_str("amm");

/// Spec §11: any consensus-relevant change bumps this.
pub const MODULE_CONSENSUS_VERSION: ModuleConsensusVersion = ModuleConsensusVersion::new(0, 0);

/// Contains the types defined in [`types`] and [`config`].
pub struct AmmModuleTypes;

// Wire together the types for this module.
plugin_types_trait_impl_common!(
    KIND,
    AmmModuleTypes,
    AmmClientConfig,
    AmmInput,
    AmmOutput,
    AmmOutputOutcome,
    AmmConsensusItem,
    AmmInputError,
    AmmOutputError
);

#[derive(Debug)]
pub struct AmmCommonInit;

impl CommonModuleInit for AmmCommonInit {
    const CONSENSUS_VERSION: ModuleConsensusVersion = MODULE_CONSENSUS_VERSION;
    const KIND: ModuleKind = KIND;

    type ClientConfig = AmmClientConfig;

    fn decoder() -> Decoder {
        AmmModuleTypes::decoder_builder().build()
    }
}
