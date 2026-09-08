//! Types shared between the multispend module's client and server.

pub mod account;
pub mod config;
pub mod endpoints;
pub mod types;

use fedimint_core::core::{Decoder, ModuleInstanceId, ModuleKind};
use fedimint_core::module::{CommonModuleInit, ModuleCommon, ModuleConsensusVersion};
use fedimint_core::plugin_types_trait_impl_common;

use crate::config::MultispendClientConfig;
use crate::types::{
    MultispendConsensusItem, MultispendInput, MultispendInputError, MultispendOutput,
    MultispendOutputError, MultispendOutputOutcome,
};

/// Unique name for this module.
pub const KIND: ModuleKind = ModuleKind::from_static_str("multispend");

/// Any consensus-relevant change bumps this.
pub const MODULE_CONSENSUS_VERSION: ModuleConsensusVersion = ModuleConsensusVersion::new(0, 0);

/// Contains the types defined in [`types`] and [`config`].
pub struct MultispendModuleTypes;

// Wire together the types for this module.
plugin_types_trait_impl_common!(
    KIND,
    MultispendModuleTypes,
    MultispendClientConfig,
    MultispendInput,
    MultispendOutput,
    MultispendOutputOutcome,
    MultispendConsensusItem,
    MultispendInputError,
    MultispendOutputError
);

#[derive(Debug)]
pub struct MultispendCommonInit;

impl CommonModuleInit for MultispendCommonInit {
    const CONSENSUS_VERSION: ModuleConsensusVersion = MODULE_CONSENSUS_VERSION;
    const KIND: ModuleKind = KIND;

    type ClientConfig = MultispendClientConfig;

    fn decoder() -> Decoder {
        MultispendModuleTypes::decoder_builder().build()
    }
}
