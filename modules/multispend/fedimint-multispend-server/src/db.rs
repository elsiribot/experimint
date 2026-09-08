//! Server-side database schema. Spec §3.2.

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::Amounts;
use fedimint_core::{impl_db_lookup, impl_db_record};
use fedimint_multispend_common::account::AccountId;
use serde::Serialize;
use strum_macros::EnumIter;

#[repr(u8)]
#[derive(Clone, Copy, Debug, EnumIter)]
pub enum DbKeyPrefix {
    Account = 0x01,
}

impl std::fmt::Display for DbKeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct AccountKey(pub AccountId);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Encodable, Decodable)]
pub struct AccountPrefix;

/// The module's entire consensus state, one row per account.
///
/// The key set is deliberately absent: it is not stored anywhere. An account
/// exists exactly while it has a record, and a record is created by its first
/// credit — so there is no registration step and no way to create an account
/// without funding it.
///
/// Note the absent `Serialize`: `Amounts` has no serde impl at this rev
/// (`fedimint-core/src/module/mod.rs` derives only `Debug, Clone, Eq,
/// PartialEq, Hash, Encodable, Decodable`, and there is no manual impl). That
/// is why `AccountBalance` (in the common crate's `endpoints` module) exists:
/// it is the serde-able view `dump_database` and the `account_balance`
/// endpoint return instead of this type.
#[derive(Debug, Clone, Eq, PartialEq, Encodable, Decodable)]
pub struct AccountState {
    pub balance: Amounts,
    /// Incremented by every input or output touching this account.
    ///
    /// Consensus never reads it. It exists so the non-consensus bulletin
    /// board (spec §4, a later phase) can notice "this account did something"
    /// without any consensus write of its own.
    pub op_counter: u64,
}

impl_db_record!(
    key = AccountKey,
    value = AccountState,
    db_prefix = DbKeyPrefix::Account
);
impl_db_lookup!(key = AccountKey, query_prefix = AccountPrefix);
