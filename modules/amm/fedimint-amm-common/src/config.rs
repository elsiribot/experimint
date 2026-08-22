//! Spec §11.

use std::collections::BTreeMap;

use fedimint_core::Amount;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::AmountUnit;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    /// Applied to any pool without an explicit override. Default 3 (= 0.30%,
    /// the Uniswap V2 reference value).
    pub default_fee_per_mille: u16,
    pub fee_overrides: BTreeMap<PoolId, u16>,
}

/// Contains all the configuration for the client.
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
}
