//! Spec §11.
//!
//! Mirrors `modules/amm/fedimint-amm-common/src/config.rs` structurally, but
//! every type here is empty: this module holds no key material (the account
//! key set is never stored, per the design) and has no tunable consensus
//! parameters.

use fedimint_core::core::ModuleKind;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::serde_json;
use fedimint_core::plugin_types_trait_impl_config;
use serde::{Deserialize, Serialize};

use crate::MultispendCommonInit;

/// Will be the same for every federation member. Empty: this module has no
/// tunable consensus parameters.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct MultispendConfigConsensus;

/// Contains all the configuration for the client. Empty: the client never
/// reads a parameter out of it, and the account key set is never stored by
/// the module — it travels inline in every spend instead.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct MultispendClientConfig;

impl std::fmt::Display for MultispendClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MultispendClientConfig {}",
            serde_json::to_string(self).map_err(|_e| std::fmt::Error)?
        )
    }
}

/// Empty: this module holds no key material.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct MultispendConfigPrivate;

/// Contains all the configuration for the server: the private and consensus
/// parts combined.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultispendConfig {
    pub private: MultispendConfigPrivate,
    pub consensus: MultispendConfigConsensus,
}

// Wire together the configs for this module.
plugin_types_trait_impl_config!(
    MultispendCommonInit,
    MultispendConfig,
    MultispendConfigPrivate,
    MultispendConfigConsensus,
    MultispendClientConfig
);
