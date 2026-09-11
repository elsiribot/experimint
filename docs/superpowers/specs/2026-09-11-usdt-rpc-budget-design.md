# USDT RPC Budget: Block-Driven Scanning, Retiring Housekeeping, Per-Loop Cadence

**Date:** 2026-09-11
**Status:** approved (from 2026-09-11 design discussion)

## Problem

The live btc-usdt federation's seven guardians exhaust their shared Infura
allowance partway through the day and start taking `429`s (observed
2026-09-08 ~13:37 UTC, recovered overnight). A previous patch bounded the
runaway transaction watchers (`863128b`), but that addressed burst
behaviour, not the standing daily budget.

Counting the guardian-local background loops against Infura's published
per-method credit costs shows the federation spends more than its whole
daily allowance doing *nothing*: no deposits, no withdrawals, no user
activity. Two of the loops also grow without bound — one with deposit
history, one with restart frequency — so the overrun widens on its own.

## Infura's credit model

Infura bills credits, not requests, and the methods this module leans on
are not equally priced:

| Method | Credits |
| --- | --- |
| `eth_getLogs` | 255 |
| `eth_getUserOperationReceipt` | 100 |
| `eth_call`, `eth_blockNumber`, `eth_getBlockByNumber`, `eth_getBalance`, `eth_getStorageAt`, `eth_gasPrice`, `eth_getTransactionCount`, `eth_getTransactionReceipt`, `eth_sendRawTransaction`, `eth_getCode` | 80 |
| `eth_chainId` | 5 |

| Plan | Price | Daily credits | Rate limit |
| --- | --- | --- | --- |
| Free | $0 | 3,000,000 | 500 credits/sec |
| Developer | $50/mo | 15,000,000 | 4,000 credits/sec |
| Team | $225/mo | 75,000,000 | 40,000 credits/sec |

`eth_getLogs` at 255 credits is the single most expensive method in this
module's vocabulary — 3.2× a plain call. Any design that counts *requests*
rather than *credits* will systematically under-budget the scanner.

## Measured baseline

Per guardian, per day, at the deployed defaults (`poll_interval_secs` = 15,
`slow_poll_interval_secs` = 60, `confirmation_depth` = 12,
`scan_batch_blocks` = 50):

| Loop | Method(s) per tick | Calls/day | Credits/day |
| --- | --- | --- | --- |
| `spawn_block_count_poller` (lib.rs:3272) | `eth_blockNumber` | 5,760 | 460,800 |
| `spawn_block_hash_observer` (lib.rs:3745) | `eth_getBlockByNumber` | 5,760 | 460,800 |
| `spawn_transfer_scanner` (lib.rs:3817) | `eth_getLogs` | ~5,400 | **1,377,000** |
| `spawn_fee_estimate_poller` (lib.rs:3307) | `eth_gasPrice` + 2×`eth_call` + `eth_getBlockByNumber` | 5,760 | 460,800 |
| `spawn_bootstrap_observer` (lib.rs:3451) | `eth_getBalance` | 1,440 | 115,200 |
| **Idle total** | | **~24,000** | **~2,874,600** |

That is **96% of a Free tier's daily allowance at complete idle**, and
**~20.1M credits/day across seven guardians — 134% of a Developer plan**,
which is what the demo federation currently shares.

Two loops are excluded above because they scale with state rather than
time, and both are unbounded:

- **Residual-gas recovery** (`spawn_residual_recovery_observer`,
  lib.rs:4093) issues one `EntryPoint.balanceOf` `eth_call` per
  fully-swept deposit account per slow tick, forever: **115,200
  credits/day per account per guardian**, or **806,400 credits/day per
  account** federation-wide. Completing a recovery does not remove the
  account from the eligibility condition, so the cost grows with deposit
  history even with zero new deposits.
- **Cold-start backfill** re-scans the whole retention window on every
  process start: `CANDIDATE_RETENTION_BLOCKS` (50,400, scan.rs:27) ÷
  `scan_batch_blocks` (50) = **1,008 `eth_getLogs` = 257,040 credits per
  restart**, 8.6% of a Free tier's daily allowance.

The observed burn rate corroborates this. Exhausting 15M credits by 13.6
hours into the day implies ~26M credits/day, about 6M above the ~20.1M
idle figure — roughly seven or eight swept accounts' worth of residual
recovery.

### Why the original estimate was wrong

The deposit-discovery design
(`2026-09-06-usdt-deposit-discovery-design.md:110`) budgeted
"~7,200 blocks/day ÷ 50-block batches ≈ 144 `eth_getLogs`/day per
guardian". That arithmetic assumes the scanner is **block-driven**: it
waits for a 50-block batch to accumulate, then reads it.

The implemented `scan_step` (lib.rs:3868) is **clock-driven**. It fires on
every poll tick and reads whatever range happens to exist between its
cursor and `consensus_block_count − confirmation_depth`, which at steady
state on a 12s chain is ~1.3 blocks. `scan_batch_blocks` is applied as
`from + (scan_batch_blocks − 1)` (lib.rs:3890) — an upper *cap* on the
range, never a *fill* threshold.

So the batch size bounds the worst case and does nothing in the common
case: ~5,400 requests/day instead of 144, a **~37× overshoot**, on the
most expensive method Infura sells. The design was right; the
implementation silently substituted a different trigger.

## Target

**Free tier per guardian: 3M credits/day, 500 credits/sec.** Not the
demo's shared Developer key.

This is deliberate. `fedimint-usdt-common/SECURITY.md:32` requires that
operators *"SHOULD configure distinct, independent RPC providers per
guardian (not all guardians on the same Alchemy/Infura account, and
ideally not the same vendor)"* — because a single upstream provider
feeding identical false answers to a threshold of guardians can poison the
block-hash ring or fake a UserOp outcome without any guardian
misbehaving. The shared key is a demo convenience that the security model
explicitly tells real deployments not to copy, so budgeting against it
would optimise for a topology we do not want.

Budgeting for one free key per guardian makes the shared-key demo fit as a
side effect, with room to spare.

Aim: **~1M credits/day idle**, leaving ~3× headroom for real deposit and
withdrawal activity.

## Changes

### 1. Scanner: fire on blocks, not on the clock

Replace `scan_step`'s unconditional per-tick read with a trigger that
fires when **either** condition holds:

- at least `scan_batch_blocks` confirmed blocks are pending, or
- `FM_USDT_SCAN_MAX_LAG_SECS` has elapsed since the last successful scan.

`FM_USDT_SCAN_MAX_LAG_SECS` defaults to **60**, floored at 15 (mirroring
how `MIN_POLL_INTERVAL_SECS`, lib.rs:516, floors the existing interval
knob). It is the deliberate latency/cost dial: raising it to 180 cuts the
scanner's cost to ~122,000 credits/day at the price of up to three
minutes' extra discovery latency, on top of the ~2.4 minutes
`confirmation_depth` already imposes.

At the default, the time trigger dominates: 50 blocks take ~10 minutes to
accumulate on a 12s chain, so the 60s flush always fires first, yielding
**1,440 requests/day covering ~5 blocks each**. The block-count trigger
governs catch-up and backfill, where ranges are large.

**Grid snapping.** Align each request's `to` to a multiple of
`SCAN_GRID_BLOCKS` (default **5**), so a request covers
`[last_grid + 1, next_grid]`. Guardians run independent cursors and
restart at different times, so their ranges would otherwise never
coincide; snapping to a shared grid makes every caught-up guardian issue
*byte-identical* ranges for the same span of chain. That is what makes any
upstream cache — a shared reverse proxy, or the provider's own — hit
instead of miss.

The grid size is 5 rather than 50 because grid alignment and flush cadence
must agree: a 50-block grid forces waiting for a 50-block boundary
(~10 min), which contradicts the 60s flush this design chose. Five blocks
is ~60s of chain at 12s blocks, and divides evenly into
`scan_batch_blocks` = 50, so backfill batches stay grid-aligned too.
Raising `FM_USDT_SCAN_MAX_LAG_SECS` without raising `SCAN_GRID_BLOCKS` is
safe but wasteful: the flush then spans several grid cells, and the request
is issued at the next grid boundary at or past the deadline, so alignment
holds and only the latency bound loosens. Keeping `SCAN_GRID_BLOCKS ≈
FM_USDT_SCAN_MAX_LAG_SECS ÷ 12` keeps one flush to one grid cell.

**Cost:** 1,377,000 → **367,200 credits/day/guardian**.

### 2. Residual recovery: exponential dust backoff

The current eligibility condition (lib.rs:4155) selects every fully-swept
deposit account whose recovery is not already in flight, and re-reads its
`EntryPoint` balance every slow tick indefinitely — including accounts
that have already been recovered and now hold nothing.

Add **in-memory** per-account backoff state (`next_check_at`,
`consecutive_dust_reads`):

- A read **below** `RESIDUAL_RECOVERY_MIN_WEI` (lib.rs:620) schedules the
  next check at `interval × 2^k`, capped by
  `FM_USDT_RESIDUAL_MAX_BACKOFF_SECS` (default **86,400** — 24h).
- A read **at or above** the threshold keeps the account on the fast path;
  it is actionable, and a recovery is either in flight or about to be.
- An **RPC error** gets its own jittered backoff and never increments the
  dust counter. "Cannot check" must not be confused with "nothing there",
  which is the same fail-safe discipline `spawn_balances_slot_checker`
  already applies (lib.rs:3437).

This needs **no schema change, no consensus change, and no sec-13
exception**, because the balance read is itself the retirement signal. A
confirmed `RecoverResidual` op withdraws the account's deposit, so the
next read sees dust and the account retires itself. A *reverted* recovery
leaves the balance intact, so the account stays on the fast path and is
retried — which is the correct behaviour, and falls out of the same rule
rather than needing a second one.

State is in-memory by choice. A restart costs one sweep of N accounts
(N × 80 credits; 4,000 credits at N = 50) before they re-retire —
negligible against the 5.76M credits/day that N = 50 costs today. Persisting
retirement would buy back 4,000 credits per restart in exchange for a
`DepositRecord` field and a migration; not worth it.

Note that `apply_user_op_confirmed`'s `RecoverResidual` arm (lib.rs:5962)
bumps `record.nonce` and clears the account's vote rows but writes no
explicit "recovered" marker. Inferring recovery from nonce arithmetic
would be fragile — re-deposits trigger further sweeps that also bump it —
which is a second reason to key off the balance instead.

**Cost:** 115,200 × N → **~80 × N credits/day/guardian** (4,000 at
N = 50 retired accounts — one check per account per day at the 24h cap).

### 3. Per-loop cadence decoupling

Today two knobs (`poll_interval_secs`, lib.rs:594, and
`slow_poll_interval_secs`, lib.rs:605, a fixed `SLOW_POLL_MULTIPLIER` = 4
multiple) drive every loop. That couples latency-critical work to work
that is cheap to slow down, so the only way to cut cost is to also slow
deposit settlement. Give each loop its own cadence:

| Loop | Now | Proposed | Credits/day |
| --- | --- | --- | --- |
| `spawn_user_op_submitter` (lib.rs:4222) | 15s | **15s — unchanged** | activity-only |
| `spawn_block_count_poller` | 15s | 30s | 460,800 → 230,400 |
| `spawn_block_hash_observer` | 15s | 30s + dedupe by height | 460,800 → 230,400 |
| `spawn_fee_estimate_poller` | 60s | 180s, `decimals()` cached | 460,800 → 115,200 |
| `spawn_bootstrap_observer` | 60s | 300s | 115,200 → 23,040 |

Three notes on safety:

- **The UserOp submitter does not move.** It is the loop that settles
  sweeps and withdrawals; slowing it would slow every active deposit. Its
  cost is proportional to live ops, not to time, so it is not part of the
  idle budget anyway.
- **Fee cadence.** `FEE_VOTE_REFRESH_BLOCKS` = 20 (lib.rs:3023) — about
  240s at 12s blocks — is the deadline by which a guardian must refresh
  its vote's `recorded_block` or age out of the median quorum. Polling at
  180s stays inside that with a 60s margin. The Chainlink ETH/USD feed's
  own heartbeat is far longer, so 60s polling was already ~4× more often
  than the data changes.
- **Block cadence.** Deposits already wait `confirmation_depth` = 12
  blocks (~2.4 min). Moving the two block observers to 30s adds at most
  one poll interval (30s worst case, ~15s on average) to that wait.

Two further reductions are pure wins with no latency cost, and are folded
into the loops above:

- The fee poller re-reads the Chainlink aggregator's `decimals()` every
  tick (rpc.rs:781). It is immutable; read it once and cache it. That is
  one of four calls per tick eliminated outright.
- The block-hash observer re-reads the same height whenever
  `consensus_block_count` has not advanced (lib.rs:3761). Skip the RPC
  when the target height is unchanged and a proposal for it is already
  queued. This matters most exactly when the anchor is stalled, which is
  when the federation can least afford wasted calls.

### 4. Backfill batching, and the restart feedback loop

Cold start costs 1,008 `eth_getLogs` (257,040 credits). Worse, it is
*reflexive*: `spawn_anchor_watchdog` (lib.rs:3949) deliberately exits the
process when the deposit anchor stalls past `ANCHOR_STALL_RESTART_SECS`
(lib.rs:547), so the supervisor restarts the guardian with fresh pollers.
A rate-limited guardian stalls its anchor; the watchdog restarts it; the
restart fires a 257k-credit burst; the burst deepens the rate limiting.
Credit exhaustion and the self-heal mechanism feed each other.

Introduce `scan_backfill_batch_blocks` (guardian-local, alongside
`scan_batch_blocks` at config.rs:92), default **500**, used only while
catching up. On a provider range-limit or response-size error, halve it
and retry, down to `scan_batch_blocks`.

At 500 blocks: 50,400 ÷ 500 = **101 requests, 25,755 credits** per
restart. Keeping the existing 1s pacing between catch-up batches holds the
burst at ~255 credits/sec, comfortably under the Free tier's 500/sec
ceiling. 500 blocks of all-USDT `Transfer` logs is ~3,500 events, well
inside typical 10,000-log response caps.

This shrinks the feedback loop's amplitude by 10×; it does not remove the
loop, which is correct — the watchdog's restart behaviour is a deliberate
self-heal for a real wedging bug and is out of scope here.

### 5. Per-method credit counters

Every figure in this document is derived from reading the loops and
multiplying by Infura's published table. None of it is measured.

Add a per-method request counter in `AlloyEvmRpc`, incremented at each
call site in `rpc.rs`, weighted by the credit table above and logged
periodically. Reporting in *credits* rather than *requests* is the point:
requests are what the previous estimate counted, and counting them is what
made a 255-credit method look like a 80-credit one.

This is what converts the rest of this design from estimate to
measurement, and what will tell us whether the assumptions below hold.

## Result

| | Now | After |
| --- | --- | --- |
| Per guardian, idle | ~2,874,600/day (96% of Free) | **~970,240/day (32% of Free)** |
| Per swept account | +806,400/day federation-wide | ~560/day federation-wide |
| Per restart (backfill) | 257,040 | 25,755 |
| Seven guardians, one key | ~20.1M/day (134% of Developer) | **~6.8M/day (45% of Developer)** |

The after-column idle figure breaks down as: scanner 367,200 + block-count
230,400 + block-hash 230,400 + fee 115,200 + bootstrap 23,040 + residual
4,000 (at N = 50 retired accounts).

So the demo fits its existing $50/mo Developer key with better than 2×
headroom, and real deployments fit free per-guardian keys with ~3× — which
is the configuration `SECURITY.md:32` asks for, and which no amount of
caching can deliver on its own.

## Considered and not adopted: a caching reverse proxy

A shared caching proxy in front of Infura would collapse the seven
guardians' identical read-only queries ~7×, and historical heights and
finalized log ranges are immutable, so they cache indefinitely. On the
shared-key demo that alone would take ~20.1M credits/day to roughly 3M
without touching a line of module code.

It is not a substitute, for one reason: it optimises the wrong topology.
With one free key per guardian — the configuration `SECURITY.md:32` calls
for, and the one this design targets — there is no cross-guardian traffic
to deduplicate. Each guardian has its own key and its own cache, and what
remains cacheable is a guardian re-asking for a height or account it
already asked for, which §1–§3 eliminate in-process for free.

The two are complementary, and the grid snapping in §1 is precisely what
makes a proxy effective: without it, independently-drifting cursors mean
seven guardians issue seven *different* ranges covering the same blocks,
and the cache hit rate is approximately zero regardless of how the proxy
is configured. With it, the hit rate approaches 100%.

A proxy is therefore worth deploying for the demo if convenient, but it is
tracked separately and neither blocks nor replaces this work.

## Assumptions

Both are flagged rather than silently relied upon; §5 is what would settle
either.

- **`FM_USDT_RESIDUAL_RECOVERY_RECIPIENT` is set in production.** The
  runbook's env table (`docs/mainnet-deployment.md:228`) lists its value
  as *unset* while sourcing it from the agenix `EnvironmentFile`, which is
  ambiguous. An all-zero recipient disables the residual observer's
  proposals entirely (lib.rs:4109), which would make §2 a
  future-proofing change rather than a present saving. The ~6M/day gap
  between observed burn and computed idle load strongly suggests it *is*
  set.
- **All credit figures are computed, not billed.** They come from reading
  the loops and applying Infura's published per-method costs, not from
  billing telemetry or a provider method breakdown. Actual usage will also
  include client-driven API traffic and failed/retried calls, which this
  document does not model.

## Out of scope

- `CANDIDATE_RETENTION_BLOCKS` (7 days) is a product decision about how
  long a client may be offline before falling back to the `recover`
  gap-limit walk, not a cost knob. Unchanged.
- The anchor watchdog's restart-on-stall behaviour. §4 reduces what a
  restart costs; whether the watchdog should restart at all is a separate
  question.
- Response *bandwidth* (~100–200 MB/day/guardian of `Transfer` logs, per
  the discovery spec). Not credit-billed, and unchanged by this design.
