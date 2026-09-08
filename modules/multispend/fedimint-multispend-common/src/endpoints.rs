//! API endpoint names and their response types. Spec §3.5.

use std::collections::BTreeMap;

use fedimint_core::Amount;
use fedimint_core::module::AmountUnit;
use serde::{Deserialize, Serialize};

pub const ACCOUNT_BALANCE_ENDPOINT: &str = "account_balance";

/// What `account_balance` returns: `None` if the account has never been
/// credited, `Some` otherwise.
///
/// A serde-able view of the server's `AccountState`, which cannot itself be
/// serialized because `Amounts` has no serde impl at this rev — see
/// `fedimint-multispend-server::db::AccountState`'s doc comment.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccountBalance {
    pub balance: BTreeMap<AmountUnit, Amount>,
    pub op_counter: u64,
}
