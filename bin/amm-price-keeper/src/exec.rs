//! Submitting a swap, and waiting for it (design §6).
//!
//! Plumbing only — every rule lives in [`crate::policy`]. What this module
//! contributes is the "one swap in flight, ever" discipline: a swap is two
//! federation transactions and Tx2 retries indefinitely by design (AMM spec
//! §12), so the caller awaits `Done` here before the next tick may trade, and
//! corrections are never stacked on stale reserves.

use std::time::Duration;

use fedimint_amm_client::AmmClientModule;
use fedimint_core::Amount;
use fedimint_core::module::AmountUnit;
use tracing::{info, warn};

/// How long a swap may be outstanding before each further wait is logged at
/// WARN (design §6). Purely a report: the wait itself never gives up, because
/// the balance Tx1 created is permanently claimable and abandoning it would
/// strand funds.
pub const SWAP_WARN_AFTER: Duration = Duration::from_secs(300);

/// Submits one swap and waits for it to reach `Done`.
///
/// `max_slippage_bps` goes to `AmmClientModule::swap`, which re-quotes
/// immediately before submitting and derives `min_out` itself; there is
/// deliberately no `min_out` computed here, since a quote taken at decision
/// time would already be stale (AMM spec §12).
pub async fn execute_swap(
    amm: &AmmClientModule,
    unit_in: AmountUnit,
    unit_out: AmountUnit,
    amount_in: Amount,
    max_slippage_bps: u64,
) -> anyhow::Result<()> {
    let operation_id = amm
        .swap(unit_in, unit_out, amount_in, max_slippage_bps)
        .await?;

    info!(
        operation = %operation_id.fmt_short(),
        unit_in = unit_in.id(),
        unit_out = unit_out.id(),
        amount_in = amount_in.msats,
        "swap submitted"
    );

    let awaiting = amm.await_swap(operation_id);
    tokio::pin!(awaiting);

    let mut outstanding = Duration::ZERO;
    loop {
        tokio::select! {
            result = &mut awaiting => {
                result?;
                info!(operation = %operation_id.fmt_short(), "swap done");
                return Ok(());
            }
            () = tokio::time::sleep(SWAP_WARN_AFTER) => {
                outstanding += SWAP_WARN_AFTER;
                warn!(
                    operation = %operation_id.fmt_short(),
                    outstanding_secs = outstanding.as_secs(),
                    "swap still outstanding; Tx2 retries forever by design, not abandoning it"
                );
            }
        }
    }
}
