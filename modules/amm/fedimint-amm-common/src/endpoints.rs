//! Public API endpoint names and their wire types. Spec §12.
//!
//! Kept in `common` (not `server`) so a client crate can depend on the
//! request/response shapes without depending on the server crate.

use fedimint_core::module::AmountUnit;
use fedimint_core::{Amount, secp256k1};
use serde::{Deserialize, Serialize};

use crate::pool_id::PoolId;

/// Every [`PoolId`] with its reserves, `total_shares`, and effective fee.
pub const POOLS_ENDPOINT: &str = "amm_pools";

/// `(unit_in, unit_out, amount_in) -> (amount_out, price_impact_per_mille)`,
/// computed with the same `math::amount_out` the server settles with, so a
/// quote can never disagree with settlement.
pub const QUOTE_ENDPOINT: &str = "amm_quote";

/// Streams `(tweak, pubkey, unit, amount)` for every stored `Balance`, so
/// client recovery is a table scan rather than a session-history replay
/// (spec §8.2).
pub const BALANCE_RECOVERY_ENDPOINT: &str = "amm_balance_recovery";

/// Streams `(tweak, pool, pubkey, shares)` for every stored `LpPosition`,
/// mirroring [`BALANCE_RECOVERY_ENDPOINT`] for LP positions.
pub const LP_RECOVERY_ENDPOINT: &str = "amm_lp_recovery";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSummary {
    pub pool: PoolId,
    pub reserve_lo: Amount,
    pub reserve_hi: Amount,
    pub total_shares: u64,
    pub fee_per_mille: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteRequest {
    pub unit_in: AmountUnit,
    pub unit_out: AmountUnit,
    pub amount_in: Amount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteResponse {
    pub amount_out: Amount,
    /// How much worse the effective price is than the current spot price,
    /// in per-mille. `0` means no slippage; `1000` would mean the swap
    /// received nothing (never actually reachable — `amount_out` always
    /// yields a strictly positive, non-draining result, spec §7.1).
    pub price_impact_per_mille: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalanceRecoveryEntry {
    pub tweak: [u8; 16],
    pub pubkey: secp256k1::PublicKey,
    pub unit: AmountUnit,
    pub amount: Amount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LpRecoveryEntry {
    pub tweak: [u8; 16],
    pub pool: PoolId,
    pub pubkey: secp256k1::PublicKey,
    pub shares: u64,
}
