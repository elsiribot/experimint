//! Canonical, unordered identifier for a pool. Spec §5.1.

use std::cmp::Ordering;
use std::io::{Error, Read, Write};

use fedimint_core::encoding::{Decodable, DecodeError, Encodable};
use fedimint_core::module::AmountUnit;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use serde::{Deserialize, Serialize};

/// A pool over an unordered pair of units, stored sorted so that `(A, B)` and
/// `(B, A)` resolve to the same record.
///
/// Fields are private and the `Decodable` impl is hand-written: a
/// non-canonical encoding (`lo >= hi`) MUST be rejected, or one unit pair
/// yields two distinct `Pool` records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
}
