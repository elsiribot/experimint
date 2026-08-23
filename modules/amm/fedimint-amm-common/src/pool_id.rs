//! Canonical, unordered identifier for a pool. Spec §5.1.

use std::cmp::Ordering;
use std::io::{Error, Read, Write};

use fedimint_core::encoding::{Decodable, DecodeError, Encodable};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::module::{AmountUnit, serde_json};
use serde::de::Error as SerdeDeError;
use serde::ser::Error as SerdeSerError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A pool over an unordered pair of units, stored sorted so that `(A, B)` and
/// `(B, A)` resolve to the same record.
///
/// Fields are private and the `Decodable` impl is hand-written: a
/// non-canonical encoding (`lo >= hi`) MUST be rejected, or one unit pair
/// yields two distinct `Pool` records.
///
/// `Serialize`/`Deserialize` are also hand-written, independently of
/// `Encodable`/`Decodable`: `PoolId` is used as a `BTreeMap` key in configs
/// that are distributed to clients as JSON, and `serde_json` rejects any map
/// key that isn't a string. So `PoolId` serialises to (and parses from) the
/// string `"<lo>:<hi>"`, e.g. `"0:7"`. `Deserialize` re-validates
/// canonicality (`lo < hi`) exactly as `Decodable` does, for the same reason
/// (spec §5.1) — a deserialiser that accepted `"7:0"` would reintroduce the
/// split-pool bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PoolId {
    lo: AmountUnit,
    hi: AmountUnit,
}

impl PoolId {
    pub fn new(x: AmountUnit, y: AmountUnit) -> Option<Self> {
        match x.cmp(&y) {
            Ordering::Equal => None,
            Ordering::Less => Some(Self { lo: x, hi: y }),
            Ordering::Greater => Some(Self { lo: y, hi: x }),
        }
    }

    pub fn lo(&self) -> AmountUnit {
        self.lo
    }

    pub fn hi(&self) -> AmountUnit {
        self.hi
    }

    pub fn contains(&self, unit: AmountUnit) -> bool {
        self.lo == unit || self.hi == unit
    }

    /// Given one side, return the other. `None` if `unit` is not in this pair.
    pub fn other(&self, unit: AmountUnit) -> Option<AmountUnit> {
        if unit == self.lo {
            Some(self.hi)
        } else if unit == self.hi {
            Some(self.lo)
        } else {
            None
        }
    }
}

impl Serialize for PoolId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let lo = amount_unit_to_u64(self.lo).ok_or_else(|| {
            SerdeSerError::custom(format!(
                "AmountUnit did not serialise to a plain u64: {:?}",
                self.lo
            ))
        })?;
        let hi = amount_unit_to_u64(self.hi).ok_or_else(|| {
            SerdeSerError::custom(format!(
                "AmountUnit did not serialise to a plain u64: {:?}",
                self.hi
            ))
        })?;
        serializer.collect_str(&format_args!("{lo}:{hi}"))
    }
}

impl<'de> Deserialize<'de> for PoolId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;

        let (lo_str, hi_str) = s.split_once(':').ok_or_else(|| {
            SerdeDeError::custom(format!("invalid PoolId {s:?}: expected \"lo:hi\""))
        })?;
        // `split_once` only splits on the first `:`, so `"1:2:3"` would
        // otherwise silently parse `hi_str` as `"2:3"`.
        if hi_str.contains(':') {
            return Err(SerdeDeError::custom(format!(
                "invalid PoolId {s:?}: too many ':' separators"
            )));
        }
        let lo: u64 = lo_str.parse().map_err(|_| {
            SerdeDeError::custom(format!("invalid PoolId {s:?}: lo is not a valid u64"))
        })?;
        let hi: u64 = hi_str.parse().map_err(|_| {
            SerdeDeError::custom(format!("invalid PoolId {s:?}: hi is not a valid u64"))
        })?;

        // Same canonicality rule as `Decodable`: reject `lo >= hi` so a
        // non-canonical string can't reintroduce the split-pool bug (spec
        // §5.1).
        if lo >= hi {
            return Err(SerdeDeError::custom(format!(
                "PoolId must be canonical: lo < hi, got {lo}:{hi}"
            )));
        }

        Ok(PoolId {
            lo: AmountUnit::new_custom(lo),
            hi: AmountUnit::new_custom(hi),
        })
    }
}

/// `AmountUnit` is a newtype over `u64` with a private field and no public
/// accessor, no `Display`, and no `From<AmountUnit> for u64` on the pinned
/// rev (fedimint-core/src/module/mod.rs:77-98). Its derived `Serialize` is
/// transparent, so this is the only available route to the inner value.
/// Remove this helper if upstream ever exposes one.
fn amount_unit_to_u64(unit: AmountUnit) -> Option<u64> {
    serde_json::to_value(unit).ok()?.as_u64()
}

impl Encodable for PoolId {
    fn consensus_encode<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.lo.consensus_encode(writer)?;
        self.hi.consensus_encode(writer)
    }
}

impl Decodable for PoolId {
    fn consensus_decode_partial<R: Read>(
        reader: &mut R,
        modules: &ModuleDecoderRegistry,
    ) -> Result<Self, DecodeError> {
        let lo = AmountUnit::consensus_decode_partial(reader, modules)?;
        let hi = AmountUnit::consensus_decode_partial(reader, modules)?;
        if lo >= hi {
            return Err(DecodeError::from_str("PoolId must be canonical: lo < hi"));
        }
        Ok(Self { lo, hi })
    }
}

#[cfg(test)]
mod tests {
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::module::AmountUnit;
    use fedimint_core::module::registry::ModuleDecoderRegistry;

    use super::*;

    #[test]
    fn new_sorts_the_pair() {
        let a = AmountUnit::new_custom(1);
        let b = AmountUnit::new_custom(7);
        assert_eq!(PoolId::new(a, b), PoolId::new(b, a));
        let id = PoolId::new(b, a).unwrap();
        assert_eq!(id.lo(), a);
        assert_eq!(id.hi(), b);
    }

    #[test]
    fn new_rejects_identical_units() {
        let a = AmountUnit::new_custom(3);
        assert_eq!(PoolId::new(a, a), None);
    }

    #[test]
    fn round_trips_through_encoding() {
        let id = PoolId::new(AmountUnit::new_custom(2), AmountUnit::new_custom(9)).unwrap();
        let bytes = id.consensus_encode_to_vec();
        let decoded =
            PoolId::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).unwrap();
        assert_eq!(id, decoded);
    }

    /// Spec §5.1 — the whole point of a hand-written Decodable.
    #[test]
    fn rejects_non_canonical_encoding() {
        // Hand-build the wire form with lo > hi.
        let mut bytes = Vec::new();
        AmountUnit::new_custom(9).consensus_encode(&mut bytes).unwrap();
        AmountUnit::new_custom(2).consensus_encode(&mut bytes).unwrap();
        assert!(
            PoolId::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).is_err()
        );
    }

    #[test]
    fn rejects_equal_units_in_encoding() {
        let mut bytes = Vec::new();
        AmountUnit::new_custom(4).consensus_encode(&mut bytes).unwrap();
        AmountUnit::new_custom(4).consensus_encode(&mut bytes).unwrap();
        assert!(
            PoolId::consensus_decode_whole(&bytes, &ModuleDecoderRegistry::default()).is_err()
        );
    }

    #[test]
    fn serde_round_trips_through_json_as_lo_hi_string() {
        let id = PoolId::new(AmountUnit::new_custom(0), AmountUnit::new_custom(7)).unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"0:7\"");
        let decoded: PoolId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, decoded);
    }

    /// Mirrors `rejects_non_canonical_encoding` for the serde representation
    /// — see spec §5.1.
    #[test]
    fn deserialize_rejects_non_canonical_string() {
        assert!(serde_json::from_str::<PoolId>("\"7:0\"").is_err());
    }

    #[test]
    fn deserialize_rejects_equal_units_in_string() {
        assert!(serde_json::from_str::<PoolId>("\"4:4\"").is_err());
    }

    #[test]
    fn deserialize_rejects_malformed_strings() {
        for bad in ["abc", "1", "1:2:3", ""] {
            let json = format!("{bad:?}");
            assert!(
                serde_json::from_str::<PoolId>(&json).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }
}
