//! Local (client-side) ledger for the test faucet. Mirrors
//! `fedimint-dummy-client::db` except there is exactly one unit to track
//! (`faucet_unit()`), so the key carries no `AmountUnit` field.

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{Amount, impl_db_lookup, impl_db_record};
use strum_macros::EnumIter;

#[repr(u8)]
#[derive(Clone, Debug, EnumIter)]
pub enum DbKeyPrefix {
    ClientFunds = 0x01,
    /// Prefixes between 0xb0..=0xcf shall all be considered allocated for
    /// historical and future external use (mirrors every other module's
    /// reserved range in this codebase).
    ExternalReservedStart = 0xb0,
    CoreInternalReservedStart = 0xd0,
    CoreInternalReservedEnd = 0xff,
}

impl std::fmt::Display for DbKeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// This wallet's believed balance in `faucet_unit()`. Optimistic: debited
/// immediately when a spend is built (refunded if the transaction is
/// rejected), credited only once a receive/mint transaction is accepted —
/// same timing discipline as `fedimint-dummy-client`.
#[derive(Debug, Clone, Copy, Encodable, Decodable, Eq, PartialEq, Hash)]
pub struct FaucetClientFundsKey;

#[derive(Debug, Clone, Encodable, Decodable)]
pub struct FaucetClientFundsPrefix;

impl_db_record!(
    key = FaucetClientFundsKey,
    value = Amount,
    db_prefix = DbKeyPrefix::ClientFunds,
);
impl_db_lookup!(
    key = FaucetClientFundsKey,
    query_prefix = FaucetClientFundsPrefix,
);
