//! Database schema for the test faucet's server half. See `server.rs`'s
//! module doc comment for why there are two tables (a live, mutable balance
//! and a permanent, append-only minted total) rather than one.

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::{Amount, OutPoint, impl_db_lookup, impl_db_record};
use strum_macros::EnumIter;

#[repr(u8)]
#[derive(Clone, Debug, EnumIter)]
pub enum DbKeyPrefix {
    Balance = 0x01,
    MintedAudit = 0x02,
}

impl std::fmt::Display for DbKeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// The live, spendable balance credited by [`super::FaucetOutput`] and
/// debited by [`super::FaucetInput`]. Removed once it reaches zero, so the
/// table only ever holds pubkeys with a genuinely positive balance.
#[derive(Debug, Clone, Copy, Encodable, Decodable, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct BalanceKey(pub PublicKey);

#[derive(Debug, Encodable, Decodable)]
pub struct BalancePrefix;

impl_db_record!(key = BalanceKey, value = Amount, db_prefix = DbKeyPrefix::Balance,);
impl_db_lookup!(key = BalanceKey, query_prefix = BalancePrefix);

/// A permanent, per-`OutPoint` record of every amount this module has ever
/// minted via [`super::FaucetOutput`]. Never removed or decremented — see
/// `server.rs`'s module doc comment for why this table, reported as a
/// positive audit item, is what keeps the global balance-sheet audit
/// non-negative despite this module creating value from nothing.
#[derive(Debug, Clone, Copy, Encodable, Decodable, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct MintedAuditKey(pub OutPoint);

#[derive(Debug, Encodable, Decodable)]
pub struct MintedAuditPrefix;

impl_db_record!(
    key = MintedAuditKey,
    value = Amount,
    db_prefix = DbKeyPrefix::MintedAudit,
);
impl_db_lookup!(key = MintedAuditKey, query_prefix = MintedAuditPrefix);
