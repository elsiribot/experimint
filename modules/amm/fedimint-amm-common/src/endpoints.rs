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

/// Streams `(tweak, pubkey, unit, amount)` for every stored `Balance`, one
/// page at a time (finding I2: `Balance` rows are attacker-creatable for one
/// `min_swap_in` each and are never garbage-collected, so an unpaginated dump
/// lets a single tiny request amplify into an O(live rows) scan, allocation
/// and response — spec §8.2, §9.2, §12). Paginated with a keyset cursor (fix
/// pass 2, Important 2): resuming from the last row's key, not a row offset,
/// so a `Balance` deleted below the cursor between pages — which happens
/// routinely in a live federation, since every completed swap deletes one —
/// can never shift a later row into a skipped position.
///
/// [`BALANCE_ENDPOINT`] is the point lookup to use when the exact
/// `(pubkey, unit)` is already known (fix pass 3, Important 5) — this
/// endpoint remains for the case that needs a scan: finding balances by
/// tweak-match during [`crate::endpoints`]-consumer recovery.
pub const BALANCE_RECOVERY_ENDPOINT: &str = "amm_balance_recovery";

/// `(pubkey, unit) -> Option<Amount>`, a point lookup for a single stored
/// `Balance` (fix pass 3, Important 5). Added because
/// [`BALANCE_RECOVERY_ENDPOINT`] was the only way to re-read a balance the
/// caller already knows the key of (the swap Tx1→Tx2 re-read, spec §6.1),
/// and that is worse than an O(rows) scan: `request_current_consensus` uses
/// `ThresholdConsensus` (`fedimint-api-client/src/query.rs:120-135`), which
/// requires threshold-many byte-identical responses **per page**, against a
/// table that mutates on every swap in the federation. Since `Balance` rows
/// are attacker-creatable for one `min_swap_in` each and are never
/// garbage-collected (see [`BALANCE_RECOVERY_ENDPOINT`]'s doc comment), an
/// attacker could otherwise permanently tax every swap's Tx2 re-read. This
/// point lookup leaks nothing [`BALANCE_RECOVERY_ENDPOINT`] does not already
/// expose publicly — both are `public_api_endpoint!` over the same table.
pub const BALANCE_ENDPOINT: &str = "amm_balance";

/// Request for [`BALANCE_ENDPOINT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalanceRequest {
    pub pubkey: secp256k1::PublicKey,
    pub unit: AmountUnit,
}

/// Streams `(tweak, pool, pubkey, shares)` for every stored `LpPosition`, one
/// page at a time, mirroring [`BALANCE_RECOVERY_ENDPOINT`] for LP positions,
/// including its keyset cursor (fix pass 2, Important 2).
pub const LP_RECOVERY_ENDPOINT: &str = "amm_lp_recovery";

/// Server-enforced ceiling on a recovery page's size (finding I2). The
/// client-supplied `limit` in [`RecoveryPageRequest`] is clamped to this —
/// never trusted directly, since an unbounded client-chosen limit would
/// reintroduce the exact full-table-dump amplification pagination exists to
/// remove.
pub const MAX_RECOVERY_PAGE_SIZE: u32 = 500;

/// Request shared by both recovery endpoints (finding I2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RecoveryPageRequest {
    /// Keyset resume point (fix pass 2, Important 2): the exact bytes a
    /// previous page returned as `next_cursor`, or `None` to start from the
    /// beginning. Opaque to the client — it is the server's own encoding of
    /// the last row returned, not a row count, so a client must pass back
    /// exactly what it was given rather than construct or mutate one. This
    /// replaces an earlier `u64` row-offset cursor, which a deletion below
    /// the cursor between pages could make skip a row (Important 2): an
    /// offset counts positions, and a row removed below it shifts every
    /// later row left by one, so the next page's `.skip(offset)` lands one
    /// row too far and silently drops whatever was there.
    pub cursor: Option<Vec<u8>>,
    /// Requested page size. `None` requests the maximum. Always clamped
    /// server-side to `[1, MAX_RECOVERY_PAGE_SIZE]` — a client cannot ask
    /// for an unbounded page.
    pub limit: Option<u32>,
}

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

/// One page of [`BalanceRecoveryEntry`] rows (finding I2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalanceRecoveryResponse {
    pub entries: Vec<BalanceRecoveryEntry>,
    /// `Some(cursor)` to pass as the next [`RecoveryPageRequest::cursor`]
    /// iff more rows remain; `None` means this was the last page. Opaque
    /// keyset bytes, not a row count (fix pass 2, Important 2).
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LpRecoveryEntry {
    pub tweak: [u8; 16],
    pub pool: PoolId,
    pub pubkey: secp256k1::PublicKey,
    pub shares: u64,
}

/// One page of [`LpRecoveryEntry`] rows (finding I2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LpRecoveryResponse {
    pub entries: Vec<LpRecoveryEntry>,
    /// `Some(cursor)` to pass as the next [`RecoveryPageRequest::cursor`]
    /// iff more rows remain; `None` means this was the last page. Opaque
    /// keyset bytes, not a row count (fix pass 2, Important 2).
    pub next_cursor: Option<Vec<u8>>,
}
