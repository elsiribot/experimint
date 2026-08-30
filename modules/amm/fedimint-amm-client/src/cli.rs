//! The `fedimint-cli module amm <verb>` surface, behind the `cli` feature.
//!
//! Mirrors `fedimint-usdt-client`'s `cli.rs`: an `Opts` enum parsed with
//! [`Parser::parse_from`], one JSON [`Value`] per verb, and nothing that is not
//! already an [`AmmClientModule`] method. Every verb is a thin adapter, so a
//! guard the library enforces (the fresh-quote rule, `withdraw`'s
//! exact-or-reject bounds, the unit allowlist) cannot be bypassed by driving
//! the module from here rather than from Rust.

use std::future::Future;
use std::{ffi, iter};

use clap::Parser;
use fedimint_amm_common::pool_id::PoolId;
use fedimint_core::Amount;
use fedimint_core::core::OperationId;
use fedimint_core::module::AmountUnit;
use fedimint_core::secp256k1::PublicKey;
use serde::Serialize;
use serde_json::Value;

use crate::AmmClientModule;

#[derive(Debug, Clone, Parser, Serialize)]
enum Opts {
    /// List every pool with its reserves, `total_shares` and effective fee.
    Pools,
    /// Quote `amount_in` of `unit_in` sold for `unit_out`, computed by the
    /// same `math::amount_out` the server settles with (spec §12).
    ///
    /// A quote is a snapshot of pool state, not a commitment: it goes stale
    /// the moment anything else touches the pool. `swap` therefore takes its
    /// own fresh quote rather than accepting a figure produced here.
    Quote {
        /// Unit sold, by raw unit id. `0` is bitcoin.
        #[arg(long)]
        unit_in: u64,
        /// Unit bought, by raw unit id.
        #[arg(long)]
        unit_out: u64,
        /// Input amount, in msats of `unit_in`.
        #[arg(long)]
        amount_in: u64,
    },
    /// Sell `amount_in` of `unit_in` for `unit_out`.
    ///
    /// `--max-slippage-bps` has no default on purpose. It is the only thing
    /// bounding how much worse than the pre-submission quote this swap may
    /// settle at, and a default would make that trade-off silently on the
    /// caller's behalf; `0` demands the quote exactly.
    Swap {
        /// Unit sold, by raw unit id. `0` is bitcoin.
        #[arg(long)]
        unit_in: u64,
        /// Unit bought, by raw unit id.
        #[arg(long)]
        unit_out: u64,
        /// Input amount, in msats of `unit_in`.
        #[arg(long)]
        amount_in: u64,
        /// Tolerated shortfall against the fresh quote, out of 10_000.
        #[arg(long)]
        max_slippage_bps: u64,
        /// Print the operation id and return rather than waiting for the swap
        /// to complete.
        ///
        /// A swap's second transaction retries forever instead of ever
        /// abandoning a permanently claimable balance (spec §6.3), so waiting
        /// has no failure deadline of its own to fall back on — see
        /// [`AmmClientModule::await_swap`]. Use this when the caller wants to
        /// impose its own deadline.
        #[arg(long)]
        no_wait: bool,
    },
    /// Add liquidity to `--pool`, creating the pool if this is its first
    /// deposit.
    Deposit {
        /// The pool, in the canonical `<lo>:<hi>` form this module's own wire
        /// encoding uses. `lo < hi` is required (a non-canonical spelling is
        /// rejected, spec §5.1), which is also what makes `--amount-lo` and
        /// `--amount-hi` unambiguous.
        #[arg(long, value_parser = parse_pool_id)]
        pool: PoolId,
        /// Amount in msats of the pool's `lo` unit.
        #[arg(long)]
        amount_lo: u64,
        /// Amount in msats of the pool's `hi` unit.
        #[arg(long)]
        amount_hi: u64,
        /// Tolerated shortfall against the share preview, out of 10_000. No
        /// default, for the same reason `swap`'s has none.
        #[arg(long)]
        max_slippage_bps: u64,
        /// Print the operation id and return rather than waiting for the
        /// deposit to be accepted or rejected.
        #[arg(long)]
        no_wait: bool,
    },
    /// Burn `--shares` of the LP position `(pool, owner_pk)`.
    ///
    /// Deliberately has no slippage argument: a withdrawal is exact-or-reject
    /// in both directions (see [`AmmClientModule::withdraw`]), so there is no
    /// tolerance left to express. One that loses the race against a concurrent
    /// swap is rejected outright and should be retried against a fresh
    /// preview.
    Withdraw {
        /// The pool, in the canonical `<lo>:<hi>` form, as printed by `pools`.
        #[arg(long, value_parser = parse_pool_id)]
        pool: PoolId,
        /// The position's owner key, as printed by `list-positions`.
        #[arg(long)]
        owner_pk: PublicKey,
        /// Shares to burn. The position's real share count is whatever the
        /// federation holds at settlement time, not what `list-positions`
        /// cached, so a burn of more than that is rejected rather than
        /// clamped.
        #[arg(long)]
        shares: u64,
        /// Print the operation id and return rather than waiting for the
        /// withdrawal to be accepted or rejected.
        #[arg(long)]
        no_wait: bool,
    },
    /// List the LP positions this client has created or recovered, from its
    /// local cache.
    ListPositions,
    /// Rescan both recovery endpoints for the positions and balances this
    /// seed owns, restore the positions into the local cache, and claim the
    /// balances (spec §8.2).
    Recover,
}

/// `AmountUnit` has no `FromStr`, and its raw id is what survives a round trip
/// through a CLI argument (see `AmountUnit::id`'s doc comment), so units are
/// taken as plain `u64` here. `new_custom(0)` is exactly `AmountUnit::BITCOIN`,
/// so `--unit-in 0` names bitcoin without a special case.
fn unit(id: u64) -> AmountUnit {
    AmountUnit::new_custom(id)
}

/// Parses a [`PoolId`] by handing the string to `PoolId`'s own `Deserialize`
/// rather than re-splitting it here.
///
/// The canonicality rule (`lo < hi`) exists so that one unit pair cannot yield
/// two distinct pool records (spec §5.1); reimplementing the parse would give
/// this CLI a second place for that rule to drift out of.
fn parse_pool_id(s: &str) -> anyhow::Result<PoolId> {
    Ok(serde_json::from_value(Value::String(s.to_owned()))?)
}

/// Awaits `wait` unless the caller opted out, and renders the operation id
/// either way.
///
/// Shared by the three verbs that start an operation so they report an
/// identical shape; `completed` says whether the operation actually reached a
/// terminal state, or whether only its submission was observed.
async fn finish_operation(
    operation_id: OperationId,
    no_wait: bool,
    wait: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<Value> {
    if !no_wait {
        wait.await?;
    }

    Ok(json(serde_json::json!({
        "operation_id": operation_id.fmt_full().to_string(),
        "completed": !no_wait,
    })))
}

/// Handles `Opts::Swap`, factored out of [`handle_cli_command`] purely to keep
/// that function short enough to stay readable as a dispatch table.
async fn handle_swap(
    amm: &AmmClientModule,
    unit_in: u64,
    unit_out: u64,
    amount_in: u64,
    max_slippage_bps: u64,
    no_wait: bool,
) -> anyhow::Result<Value> {
    let operation_id = amm
        .swap(
            unit(unit_in),
            unit(unit_out),
            Amount::from_msats(amount_in),
            max_slippage_bps,
        )
        .await?;

    finish_operation(operation_id, no_wait, amm.await_swap(operation_id)).await
}

/// Handles `Opts::Deposit`, factored out for the same reason as
/// [`handle_swap`].
async fn handle_deposit(
    amm: &AmmClientModule,
    pool: PoolId,
    amount_lo: u64,
    amount_hi: u64,
    max_slippage_bps: u64,
    no_wait: bool,
) -> anyhow::Result<Value> {
    let operation_id = amm
        .deposit(
            pool,
            Amount::from_msats(amount_lo),
            Amount::from_msats(amount_hi),
            max_slippage_bps,
        )
        .await?;

    finish_operation(operation_id, no_wait, amm.await_deposit(operation_id)).await
}

/// Handles `Opts::Withdraw`, factored out for the same reason as
/// [`handle_swap`].
async fn handle_withdraw(
    amm: &AmmClientModule,
    pool: PoolId,
    owner_pk: PublicKey,
    shares: u64,
    no_wait: bool,
) -> anyhow::Result<Value> {
    let operation_id = amm.withdraw(pool, owner_pk, shares).await?;

    finish_operation(operation_id, no_wait, amm.await_withdraw(operation_id)).await
}

pub(crate) async fn handle_cli_command(
    amm: &AmmClientModule,
    args: &[ffi::OsString],
) -> anyhow::Result<Value> {
    let opts = Opts::parse_from(iter::once(&ffi::OsString::from("amm")).chain(args.iter()));

    let value = match opts {
        Opts::Pools => json(amm.pools().await?),
        Opts::Quote {
            unit_in,
            unit_out,
            amount_in,
        } => json(
            amm.quote(unit(unit_in), unit(unit_out), Amount::from_msats(amount_in))
                .await?,
        ),
        Opts::Swap {
            unit_in,
            unit_out,
            amount_in,
            max_slippage_bps,
            no_wait,
        } => handle_swap(amm, unit_in, unit_out, amount_in, max_slippage_bps, no_wait).await?,
        Opts::Deposit {
            pool,
            amount_lo,
            amount_hi,
            max_slippage_bps,
            no_wait,
        } => handle_deposit(amm, pool, amount_lo, amount_hi, max_slippage_bps, no_wait).await?,
        Opts::Withdraw {
            pool,
            owner_pk,
            shares,
            no_wait,
        } => handle_withdraw(amm, pool, owner_pk, shares, no_wait).await?,
        Opts::ListPositions => json(
            amm.list_lp_positions()
                .await
                .into_iter()
                // `LpPositionRecord::tweak` is deliberately not printed: it is
                // the derivation input for the position's owner key and is
                // "never published anywhere client-side" (see `db.rs`), and no
                // flow here needs it — `withdraw` re-reads it from the same
                // local cache itself.
                .map(|(key, record)| {
                    serde_json::json!({
                        "pool": key.pool,
                        "owner_pk": key.owner_pk,
                        "shares": record.shares,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        Opts::Recover => {
            let summary = amm.recover().await?;
            json(serde_json::json!({
                "balances_found": summary.balances_found,
                // Under-reports on a benign race with another claimant; see
                // `AmmRecoverySummary::balances_claimed`. Do not read it as
                // "how many balances were claimable".
                "balances_claimed": summary.balances_claimed,
                "positions_restored": summary.positions_restored,
                "claim_errors": summary.claim_errors,
            }))
        }
    };

    Ok(value)
}

fn json<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("JSON serialization failed")
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{Opts, parse_pool_id};

    /// A valid compressed secp256k1 public key (the SECP256K1 generator
    /// point), used only to exercise CLI arg parsing — no real position is
    /// involved.
    const TEST_PUBKEY: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    #[test]
    fn parses_pools() {
        assert!(matches!(
            Opts::try_parse_from(["amm", "pools"]).expect("parses"),
            Opts::Pools
        ));
    }

    #[test]
    fn parses_quote() {
        assert!(matches!(
            Opts::try_parse_from([
                "amm",
                "quote",
                "--unit-in",
                "0",
                "--unit-out",
                "1",
                "--amount-in",
                "1000",
            ])
            .expect("parses"),
            Opts::Quote {
                unit_in: 0,
                unit_out: 1,
                amount_in: 1_000,
            }
        ));
    }

    /// `--max-slippage-bps` is required, not defaulted: a swap submitted
    /// without an explicit tolerance would be one whose worst acceptable price
    /// nobody chose.
    #[test]
    fn swap_requires_an_explicit_slippage_tolerance() {
        let err = Opts::try_parse_from([
            "amm",
            "swap",
            "--unit-in",
            "0",
            "--unit-out",
            "1",
            "--amount-in",
            "1000",
        ])
        .expect_err("a swap without --max-slippage-bps must not parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_swap() {
        assert!(matches!(
            Opts::try_parse_from([
                "amm",
                "swap",
                "--unit-in",
                "0",
                "--unit-out",
                "1",
                "--amount-in",
                "1000",
                "--max-slippage-bps",
                "50",
            ])
            .expect("parses"),
            Opts::Swap {
                unit_in: 0,
                unit_out: 1,
                amount_in: 1_000,
                max_slippage_bps: 50,
                no_wait: false,
            }
        ));
        assert!(matches!(
            Opts::try_parse_from([
                "amm",
                "swap",
                "--unit-in",
                "0",
                "--unit-out",
                "1",
                "--amount-in",
                "1000",
                "--max-slippage-bps",
                "0",
                "--no-wait",
            ])
            .expect("parses"),
            Opts::Swap { no_wait: true, .. }
        ));
    }

    #[test]
    fn parses_deposit() {
        assert!(matches!(
            Opts::try_parse_from([
                "amm",
                "deposit",
                "--pool",
                "0:1",
                "--amount-lo",
                "100",
                "--amount-hi",
                "200",
                "--max-slippage-bps",
                "10",
            ])
            .expect("parses"),
            Opts::Deposit {
                amount_lo: 100,
                amount_hi: 200,
                max_slippage_bps: 10,
                no_wait: false,
                ..
            }
        ));
    }

    #[test]
    fn parses_withdraw() {
        assert!(matches!(
            Opts::try_parse_from([
                "amm",
                "withdraw",
                "--pool",
                "0:1",
                "--owner-pk",
                TEST_PUBKEY,
                "--shares",
                "42",
            ])
            .expect("parses"),
            Opts::Withdraw {
                shares: 42,
                no_wait: false,
                ..
            }
        ));
    }

    #[test]
    fn parses_list_positions_and_recover() {
        assert!(matches!(
            Opts::try_parse_from(["amm", "list-positions"]).expect("parses"),
            Opts::ListPositions
        ));
        assert!(matches!(
            Opts::try_parse_from(["amm", "recover"]).expect("parses"),
            Opts::Recover
        ));
    }

    /// The canonicality rule `PoolId`'s `Deserialize` enforces must reach the
    /// CLI unchanged — a `--pool 1:0` that parsed would give one unit pair two
    /// spellings, and with it two readings of `--amount-lo`/`--amount-hi`.
    #[test]
    fn pool_id_must_be_canonical() {
        assert!(parse_pool_id("0:1").is_ok());
        assert!(parse_pool_id("1:0").is_err());
        assert!(parse_pool_id("1:1").is_err());
        assert!(parse_pool_id("0").is_err());
    }

    /// Proves the `amm` module subcommand ("`fedimint-cli module amm --help`")
    /// renders a help page listing every verb, without a live federation.
    ///
    /// This is the only place the verb list is observable off-federation:
    /// `ClientCmd::Module`'s dispatch resolves the module instance against an
    /// opened client before it ever reaches this parser, so a real
    /// `module amm --help` needs a joined federation. `Opts::parse_from` is
    /// pure clap, so the rendered page can be checked here instead.
    #[test]
    fn help_lists_every_verb() {
        let err = Opts::try_parse_from(["amm", "--help"]).expect_err("--help short-circuits");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);

        let help = err.to_string();
        for verb in [
            "pools",
            "quote",
            "swap",
            "deposit",
            "withdraw",
            "list-positions",
            "recover",
        ] {
            assert!(
                help.contains(verb),
                "help page is missing `{verb}`:\n{help}"
            );
        }
    }
}
