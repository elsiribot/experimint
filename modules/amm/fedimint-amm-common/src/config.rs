//! Spec §11.

use std::collections::BTreeMap;

use fedimint_core::Amount;
use fedimint_core::core::ModuleKind;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{AmountUnit, serde_json};
use fedimint_core::plugin_types_trait_impl_config;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::AmmCommonInit;
use crate::pool_id::PoolId;

/// Per-unit configuration.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct UnitParams {
    /// Minimum accepted swap input in this unit. Anti-dust and anti-spam; a
    /// DoS control, NOT a privacy control — see spec §13.1. Do not document
    /// it as one.
    pub min_swap_in: Amount,
}

/// Will be the same for every federation member.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct AmmConfigConsensus {
    /// Units this federation permits trading, with per-unit dust thresholds.
    /// A unit is only reachable if some mintv2 instance issues it — a setup
    /// requirement this module cannot verify (P13).
    pub units: BTreeMap<AmountUnit, UnitParams>,
    /// Applied to any pool without an explicit override, and the fallback the
    /// server uses for a pool that does not yet have a threshold of guardian
    /// fee votes. Default 3 (= 0.30%, the Uniswap V2 reference value).
    pub default_fee_per_mille: u16,
    pub fee_overrides: BTreeMap<PoolId, u16>,
    /// Lower bound of the band a guardian-voted fee is confined to.
    ///
    /// The band is fixed at DKG and is the only thing standing between the
    /// voting mechanism and a confiscatory fee: the threshold-index
    /// aggregation the server applies keeps the outcome inside the range of
    /// *honest* votes, but if the honest guardians themselves are persuaded
    /// to vote 999 that is a 99.9% fee on every swap. A band chosen at DKG
    /// is a commitment the guardians cannot later revise by vote alone.
    pub min_fee_per_mille: u16,
    /// Upper bound of the band; see [`Self::min_fee_per_mille`]. Must be
    /// `>= min_fee_per_mille` and `< 1000`.
    pub max_fee_per_mille: u16,
}

/// Contains all the configuration for the client.
///
/// Deliberately does NOT mirror [`AmmConfigConsensus`]'s fee band: the client
/// never reads a fee out of its config at all (it takes the effective,
/// already-aggregated fee from `PoolSummary::fee_per_mille`), so a mirrored
/// band would be config surface with no reader. Mirror it only alongside a
/// client that actually checks a server-reported fee against it.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct AmmClientConfig {
    pub units: BTreeMap<AmountUnit, UnitParams>,
    pub default_fee_per_mille: u16,
    pub fee_overrides: BTreeMap<PoolId, u16>,
}

impl std::fmt::Display for AmmClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AmmClientConfig {}",
            serde_json::to_string(self).map_err(|_e| std::fmt::Error)?
        )
    }
}

/// Empty: this module holds no key material, which is why one instance can
/// host many pools while a mintv2 instance hosts exactly one unit (P13).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct AmmConfigPrivate;

/// Contains all the configuration for the server: the private and consensus
/// parts combined.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmmConfig {
    pub private: AmmConfigPrivate,
    pub consensus: AmmConfigConsensus,
}

// Wire together the configs for this module.
plugin_types_trait_impl_config!(
    AmmCommonInit,
    AmmConfig,
    AmmConfigPrivate,
    AmmConfigConsensus,
    AmmClientConfig
);

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    #[error("units must not be empty")]
    NoUnits,
    #[error("fee_per_mille must be < 1000")]
    InvalidFee,
    #[error("min_swap_in must be non-zero")]
    ZeroMinSwapIn,
    #[error("fee override names a unit not in `units`")]
    UnknownUnitInOverride,
    #[error("min_fee_per_mille must be <= max_fee_per_mille")]
    InvertedFeeBand,
}

/// Shared by [`AmmConfigConsensus::fee_for`] and [`AmmClientConfig::fee_for`]
/// so the fee rule can't drift between the two mirrored types.
fn fee_for(fee_overrides: &BTreeMap<PoolId, u16>, default_fee_per_mille: u16, pool: PoolId) -> u16 {
    fee_overrides
        .get(&pool)
        .copied()
        .unwrap_or(default_fee_per_mille)
}

impl AmmConfigConsensus {
    /// DKG-time validation. Spec §11.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.units.is_empty() {
            return Err(ConfigError::NoUnits);
        }
        if self.default_fee_per_mille >= 1000 {
            return Err(ConfigError::InvalidFee);
        }
        // `max < 1000` is the same ceiling every other fee in this config is
        // held to; `min <= max` is what makes the band a band at all, and
        // what makes the server's clamp into it well-defined (an inverted
        // band would make `u16::clamp` panic).
        if self.max_fee_per_mille >= 1000 {
            return Err(ConfigError::InvalidFee);
        }
        if self.min_fee_per_mille > self.max_fee_per_mille {
            return Err(ConfigError::InvertedFeeBand);
        }
        if self.units.values().any(|p| p.min_swap_in == Amount::ZERO) {
            return Err(ConfigError::ZeroMinSwapIn);
        }
        for (pool, fee) in &self.fee_overrides {
            if *fee >= 1000 {
                return Err(ConfigError::InvalidFee);
            }
            if !self.units.contains_key(&pool.lo()) || !self.units.contains_key(&pool.hi()) {
                return Err(ConfigError::UnknownUnitInOverride);
            }
        }
        Ok(())
    }

    /// The fee, in per-mille, that applies to `pool`: its override if one
    /// exists, else [`Self::default_fee_per_mille`].
    pub fn fee_for(&self, pool: PoolId) -> u16 {
        fee_for(&self.fee_overrides, self.default_fee_per_mille, pool)
    }

    /// Whether `fee` lies inside the DKG-fixed band. The admission rule for
    /// every guardian fee vote, applied both where a guardian's own intent is
    /// recorded and again where another peer's ordered vote is processed —
    /// the second is the one that matters, since a peer's proposal is not
    /// bound by our copy of the first check.
    pub fn fee_in_band(&self, fee: u16) -> bool {
        (self.min_fee_per_mille..=self.max_fee_per_mille).contains(&fee)
    }

    /// Forces `fee` into the band. Applied to the *aggregate* fee actually
    /// charged, so the band holds even for a value that never passed
    /// [`Self::fee_in_band`] — most importantly the
    /// [`Self::default_fee_per_mille`] fallback, which is a DKG-time
    /// constant that nothing requires to sit inside a band added later.
    ///
    /// [`Self::validate`] guarantees `min <= max`, so this never panics.
    pub fn clamp_fee_to_band(&self, fee: u16) -> u16 {
        fee.clamp(self.min_fee_per_mille, self.max_fee_per_mille)
    }
}

impl AmmClientConfig {
    /// The fee, in per-mille, that applies to `pool`: its override if one
    /// exists, else [`Self::default_fee_per_mille`].
    pub fn fee_for(&self, pool: PoolId) -> u16 {
        fee_for(&self.fee_overrides, self.default_fee_per_mille, pool)
    }
}

#[cfg(test)]
mod tests {
    use fedimint_core::Amount;
    use fedimint_core::module::AmountUnit;

    use super::*;
    use crate::pool_id::PoolId;

    fn units() -> BTreeMap<AmountUnit, UnitParams> {
        BTreeMap::from([
            (
                AmountUnit::new_custom(0),
                UnitParams {
                    min_swap_in: Amount::from_msats(1_000),
                },
            ),
            (
                AmountUnit::new_custom(1),
                UnitParams {
                    min_swap_in: Amount::from_msats(10),
                },
            ),
        ])
    }

    fn cfg() -> AmmConfigConsensus {
        AmmConfigConsensus {
            units: units(),
            default_fee_per_mille: 3,
            fee_overrides: BTreeMap::new(),
            min_fee_per_mille: 1,
            max_fee_per_mille: 50,
        }
    }

    #[test]
    fn accepts_a_well_formed_config() {
        assert_eq!(cfg().validate(), Ok(()));
    }

    #[test]
    fn rejects_empty_units() {
        let mut c = cfg();
        c.units.clear();
        assert_eq!(c.validate(), Err(ConfigError::NoUnits));
    }

    #[test]
    fn rejects_fee_at_or_above_one_thousand() {
        let mut c = cfg();
        c.default_fee_per_mille = 1_000;
        assert_eq!(c.validate(), Err(ConfigError::InvalidFee));
    }

    #[test]
    fn rejects_zero_min_swap_in() {
        let mut c = cfg();
        c.units.insert(
            AmountUnit::new_custom(0),
            UnitParams {
                min_swap_in: Amount::ZERO,
            },
        );
        assert_eq!(c.validate(), Err(ConfigError::ZeroMinSwapIn));
    }

    #[test]
    fn rejects_fee_override_for_an_unknown_unit() {
        let mut c = cfg();
        let unknown = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(99)).unwrap();
        c.fee_overrides.insert(unknown, 5);
        assert_eq!(c.validate(), Err(ConfigError::UnknownUnitInOverride));
    }

    #[test]
    fn rejects_fee_override_at_or_above_one_thousand() {
        let mut c = cfg();
        let pool = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap();
        c.fee_overrides.insert(pool, 1_000);
        assert_eq!(c.validate(), Err(ConfigError::InvalidFee));
    }

    #[test]
    fn rejects_an_inverted_fee_band() {
        let mut c = cfg();
        c.min_fee_per_mille = 51;
        c.max_fee_per_mille = 50;
        assert_eq!(c.validate(), Err(ConfigError::InvertedFeeBand));
    }

    #[test]
    fn accepts_a_degenerate_single_value_fee_band() {
        let mut c = cfg();
        c.min_fee_per_mille = 3;
        c.max_fee_per_mille = 3;
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn rejects_a_band_ceiling_at_or_above_one_thousand() {
        let mut c = cfg();
        c.max_fee_per_mille = 1_000;
        assert_eq!(c.validate(), Err(ConfigError::InvalidFee));
    }

    #[test]
    fn fee_band_admits_its_own_endpoints_and_nothing_outside() {
        let c = cfg();
        assert!(!c.fee_in_band(0));
        assert!(c.fee_in_band(1));
        assert!(c.fee_in_band(50));
        assert!(!c.fee_in_band(51));
    }

    #[test]
    fn clamping_pulls_out_of_band_fees_to_the_nearest_endpoint() {
        let c = cfg();
        assert_eq!(c.clamp_fee_to_band(0), 1);
        assert_eq!(c.clamp_fee_to_band(3), 3);
        assert_eq!(c.clamp_fee_to_band(999), 50);
    }

    #[test]
    fn fee_for_prefers_the_override() {
        let mut c = cfg();
        let pool = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap();
        assert_eq!(c.fee_for(pool), 3);
        c.fee_overrides.insert(pool, 1);
        assert_eq!(c.fee_for(pool), 1);
    }

    #[test]
    fn client_config_fee_for_prefers_the_override() {
        let mut c = AmmClientConfig {
            units: units(),
            default_fee_per_mille: 3,
            fee_overrides: BTreeMap::new(),
        };
        let pool = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap();
        assert_eq!(c.fee_for(pool), 3);
        c.fee_overrides.insert(pool, 1);
        assert_eq!(c.fee_for(pool), 1);
    }

    /// Regression test for the AMM config JSON-serialisation bug: `PoolId`
    /// used to derive struct-shaped serde, which `serde_json` rejects as a
    /// map key ("key must be a string") once `fee_overrides` is non-empty.
    /// This is how fedimint distributes client config, so this must succeed.
    #[test]
    fn client_config_serialises_to_json_with_fee_overrides() {
        let pool = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap();
        let mut c = AmmClientConfig {
            units: units(),
            default_fee_per_mille: 3,
            fee_overrides: BTreeMap::new(),
        };
        c.fee_overrides.insert(pool, 1);

        let json = fedimint_core::module::serde_json::to_string(&c)
            .expect("client config with fee_overrides must serialise to JSON");
        let round_tripped: AmmClientConfig =
            fedimint_core::module::serde_json::from_str(&json).expect("must deserialise back");
        assert_eq!(c, round_tripped);
    }

    /// `AmmConfigConsensus` mirrors `AmmClientConfig` and must serialise the
    /// same way.
    #[test]
    fn consensus_config_serialises_to_json_with_fee_overrides() {
        let pool = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap();
        let mut c = cfg();
        c.fee_overrides.insert(pool, 1);

        let json = fedimint_core::module::serde_json::to_string(&c)
            .expect("consensus config with fee_overrides must serialise to JSON");
        let round_tripped: AmmConfigConsensus =
            fedimint_core::module::serde_json::from_str(&json).expect("must deserialise back");
        assert_eq!(c, round_tripped);
    }

    /// The `Display` impl for `AmmClientConfig` goes through JSON
    /// serialisation internally; it must not panic (or return an `Err` that
    /// callers commonly `.expect()` on) for a non-empty `fee_overrides`.
    #[test]
    fn client_config_display_does_not_panic_with_fee_overrides() {
        let pool = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(1)).unwrap();
        let mut c = AmmClientConfig {
            units: units(),
            default_fee_per_mille: 3,
            fee_overrides: BTreeMap::new(),
        };
        c.fee_overrides.insert(pool, 1);

        let _ = c.to_string();
    }
}
