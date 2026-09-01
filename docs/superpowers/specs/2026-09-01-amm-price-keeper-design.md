# `amm-price-keeper` — Design

A bot that holds the `experimint` AMM's BTC/USDt pool near an external
reference price. Its mandate is price accuracy, not profit: at roughly $700 of
pool depth there is not enough to extract for arbitrage to be a business, and
its expected P&L over time is negative.

**Where the loss actually comes from — corrected 2026-09-01 against live
behaviour.** An earlier draft of this section said the bot "pays the pool's fee
in exchange for" price accuracy, implying each correction is itself lossy. That
is wrong, and the first live correction disproved it: closing a 201 bps gap
spent 3,171,840 micros and returned 4,122,112 msats, netting about **+$0.02**.
Closing a gap wider than the fee is ordinary arbitrage and pays at the instant
of the trade.

The bleed is **adverse selection over time**. The bot is structurally the
counterparty that buys whichever asset is falling and sells whichever is
rising, so as the oracle moves it accumulates the losing side — §4.3's floors
and the "expect to top it up" guidance exist for exactly that reason. Budget
for the loss, but do not expect to see it in any individual correction.

## 1. Goal and non-goals

**Goal.** Whenever the pool's spot price deviates from the oracle by more than a
configured band, place the swap that lands the post-trade spot price on the
oracle price, subject to inventory and size limits.

**Non-goals**, decided explicitly and recorded so they are not re-litigated:

- No profit motive, no inventory-skew pricing, no fee capture.
- No burn budget or spend-based kill switch.
- No dry-run mode.
- No auto-rebalancing. When a side runs low, a human tops it up.
- No LP management: the bot never deposits or withdraws liquidity.

`policy.rs` (§5) is a pure function returning a `Decision`, so burn accounting
and a dry-run are each a small later addition. The shape does not foreclose
them; this section only says they are not being built now.

## 2. Assumptions

Each of these is load-bearing. If one is false the bot tracks the wrong price
and nothing in the system will notice.

- **1 USDt = 1 USD.** `https://price-feed.dev.fedibtc.com/latest` publishes 67
  pairs, of which `BTC/USD` is the only crypto pair — there is no `USDT/USD`.
  A USDt depeg therefore moves the pool to the wrong price by exactly the depeg.
- **Unit 0 is Bitcoin denominated in msats** (1e-11 BTC) and **unit 1 is USDt
  denominated in micros** (1e-6 USDt). The bot asserts unit 0 is
  `AmountUnit::BITCOIN` at startup rather than trusting the pool id alone.
- **Pool `0:1` exists and is seeded.** An absent or empty pool is a hold with a
  warning, not a crash.
- **The bot is the sole writer of its client data dir.** It holds rocksdb open
  for its lifetime; a concurrent `fedimint-cli-experimint` against the same dir
  will fail to open.
- Funding is manual: the data dir is joined to the federation and holds
  balances in both units before the bot is armed.

## 3. Price and deviation, in exact integer arithmetic

With `R_lo` msats and `R_hi` micros, the pool's spot price in USD per BTC is

```
P_pool = (R_hi / R_lo) × 10^5          (10^5 = 10^(11-6), the decimal gap)
```

The oracle rate arrives as a JSON float. It is converted once, at the edge, to
`p_micro: u128` = `round(rate × 1e6)` micro-USD per BTC. **Every subsequent
comparison is integer**, by cross-multiplication:

```
P_pool ≥ P_oracle   ⟺   R_hi × 10^11  ≥  R_lo × p_micro
dev_bps = (R_hi × 10^11 − R_lo × p_micro) × 10_000 / (R_lo × p_micro)   [i128]
```

At current depth both sides of that comparison are ≈ 3.5 × 10^19, comfortably
inside `u128`. There is no float on the decision path — only in parsing the
feed and in log formatting.

## 4. Direction and sizing

### 4.1 Direction

| condition | the pool is… | bot swaps | spends | receives |
| --- | --- | --- | --- | --- |
| `dev_bps > +band` | overvaluing BTC | `unit_in 0 → unit_out 1` | msats | micros |
| `dev_bps < −band` | undervaluing BTC | `unit_in 1 → unit_out 0` | micros | msats |
| otherwise | in band | — | — | — |

Selling BTC into the pool raises `R_lo` and lowers `R_hi`, so `P_pool` falls
toward the oracle. Getting this sign wrong makes the bot diverge rather than
converge, at increasing speed; §7 requires a test that pins both directions.

### 4.2 Sizing by binary search over the module's own curve

A closed form exists — the post-trade-price constraint is a quadratic in `dx`,

```
f'·dx² + R_in(1 + f')·dx + R_in² − R_in·R_out/r = 0        f' = (1000 − fee)/1000
```

— and it is deliberately **not** used. It needs an integer square root to stay
off floats, and it is a second copy of the curve that can drift from the
settlement math without any test noticing.

Instead: `amount_out` is strictly monotone in `amount_in`, so the post-trade
price is monotone too. **Binary search `dx` over `[1, hi]`, calling
`fedimint_amm_common::math::amount_out` — the exact function consensus settles
with — and keep the largest `dx` that does not overshoot the target.**

```
post-trade:  R_in' = R_in + dx
             R_out' = R_out − amount_out(R_in, R_out, dx, fee_per_mille)
overshoot:   selling BTC → stop while P'(dx) ≥ P_oracle
             buying  BTC → stop while P'(dx) ≤ P_oracle
```

Roughly 64 iterations of `u64` arithmetic per tick. The fee is read from
`PoolSummary::fee_per_mille` on every tick — the effective, guardian-voted fee
(spec §12) — never from config, so a fee vote mid-flight sizes correctly.

`amount_out` returns `Err` for dust and for draining the pool; treat any error
as "this `dx` is not viable" and search below it.

### 4.3 Clamping, in order

1. **Overshoot** — the binary search result above.
2. **Max trade size** — `--max-trade-usd`, converted to the input unit at the
   oracle price.
3. **Inventory floor** — never spend below `--btc-floor-msat` /
   `--usdt-floor-micros` of the input unit.
4. **Dust** — if the survivor is under that unit's `min_swap_in` (from the AMM
   client config), **hold and log at WARN**. This is the "side is exhausted"
   signal, and it is expected to fire eventually.

Clamping is partial correction, not refusal: a clamped trade still moves the
price the right way, just not all the way.

### 4.4 The band must exceed the fee

At `fee_per_mille = 3` a round trip burns ~60 bps. A band under 30 bps means
oracle noise alone pays fees to move the price nowhere. **The bot refuses to
start if `band_bps < fee_per_mille × 10`**, reading the fee from the live pool.
Default band: 50 bps.

## 5. Structure

```
bin/amm-price-keeper/
  src/oracle.rs   fetch, parse, staleness + jump validation → Result<OraclePrice>
  src/policy.rs   pure: (reserves, fee, min_swap_in, oracle, balances, cfg)
                    → Decision::{Hold(reason), Trade{unit_in, amount_in}}
  src/exec.rs     amm.swap(..) → await_swap(..), strictly one at a time
  src/main.rs     client via experimint_modules(), tick loop, signal handling
```

`policy.rs` performs no I/O and knows nothing about fedimint's client: it takes
plain integers and returns a `Decision`. It is where every rule in §4 lives and
where the tests in §7 point.

The client is built the way `bin/fedimint-cli-experimint/src/info.rs`'s
`open_client` builds it — same rocksdb path, same `Bip39RootSecretStrategy`,
same `experimint_modules()` registry. Resolve the `amm` instance by kind and
**fail if the federation reports more than one**, rather than silently taking
the lowest id the way `get_first_instance` would.

Workspace deps already carry everything needed: `reqwest`, `serde_json`,
`clap` (with `env`), `tokio`, `tracing`, `anyhow`.

## 6. Guardrails and operational behaviour

- **Oracle staleness** — refuse to trade if `prices["BTC/USD"].timestamp` is
  older than `--max-oracle-age` (default 300s), or on any HTTP/parse error.
- **Oracle jump** — refuse if the rate moved more than `--max-rate-jump-pct`
  (default 10) versus the last accepted tick. First tick has no predecessor and
  is accepted on staleness alone.
- Both failures are **fail-closed**: hold, log WARN, try again next tick. The
  bot never trades on a price it could not validate.
- **One swap in flight, ever.** A swap is two federation transactions and Tx2
  retries indefinitely by design (spec §12). The bot awaits `Done` before the
  next tick may trade, so corrections are never stacked on stale reserves. Log
  at WARN if a swap has been outstanding for more than 5 minutes; never
  abandon it.
- **Startup recovery** — call `amm.recover()` once before the first tick, which
  sweeps any balance stranded by a crash between Tx1 and Tx2.
- **Slippage** — pass `--max-slippage-bps` (default 50) to `amm.swap`, which
  re-quotes immediately before submitting and derives `min_out` itself. `0`
  would reject on any concurrent pool activity, including a fee vote landing
  mid-rollout (handover doc, "Operating the guardian-voted fee").
- **Shutdown** — SIGINT/SIGTERM finishes an in-flight swap's await, then exits.

## 7. Testing

Required:

- **`policy.rs` table-driven unit tests**, one case per branch: in band; above
  band; below band; both directions pinned by sign (§4.1); dust below
  `min_swap_in`; floor clamp; max-size clamp; empty pool; zero reserve; fee at
  both ends of the `[1, 50]` band.
- **Property test**: applying the returned trade to the reserves via
  `amount_out` must reduce `|dev_bps|` and must not overshoot past the target
  (the sign of `dev_bps` never flips).
- **`oracle.rs` parse tests** against a captured fixture of the live response,
  plus stale-timestamp, absent-`BTC/USD`, and jump cases.

Acceptance against the live federation, in order: run with the band set very
wide so every tick holds, and confirm the read path reports sane pool price,
deviation and sizing in the logs; then narrow the band to arm it.

A `devimint` end-to-end is a stretch goal, not a gate.

## 8. Configuration

All flags take an env var (`clap`'s `env` feature).

| flag | default | meaning |
| --- | --- | --- |
| `--data-dir` / `FM_CLIENT_DIR` | — | client data dir, required |
| `--oracle-url` | `https://price-feed.dev.fedibtc.com/latest` | feed endpoint |
| `--pool` | `0:1` | canonical `lo:hi` pool id |
| `--tick-interval` | `60s` | the feed updates ≈ once a minute |
| `--band-bps` | `50` | deadband half-width; must exceed the fee (§4.4) |
| `--max-trade-usd` | — | per-tick size cap, required |
| `--btc-floor-msat` | `0` | reserve floor, unit 0 |
| `--usdt-floor-micros` | `0` | reserve floor, unit 1 |
| `--max-oracle-age` | `300s` | staleness limit |
| `--max-rate-jump-pct` | `10` | tick-over-tick sanity limit |
| `--max-slippage-bps` | `50` | passed to `amm.swap` |

## 9. Deployment

Out of scope for this spec beyond the binary itself. It is a normal workspace
member, so it builds with `nix develop --command cargo build --release -p
amm-price-keeper`. Wiring a systemd unit in the infra repo is separate work.
