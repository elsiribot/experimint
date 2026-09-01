# amm-price-keeper

A bot that holds this federation's AMM BTC/USDt pool near an external
reference price.

Its mandate is price accuracy, not profit. At roughly $700 of pool depth there
is not enough to extract for arbitrage to be a business, and **its expected P&L
over time is negative**. Budget for that.

Be precise about *why*, though, because the obvious explanation is wrong. An
individual correction is usually profitable: closing a gap wider than the pool's
fee is ordinary arbitrage, and the first live correction netted about +$0.02
closing a 201 bps gap. The loss comes from **adverse selection over time** — the
bot is structurally the counterparty that buys the falling asset and sells the
rising one, so it accumulates the losing side as the oracle moves. That is what
eventually empties a side and needs a top-up.

Every tick it reads the oracle, reads the pool, reads its own balances, and —
if the pool's spot price is outside a configured band around the oracle —
places the one swap that lands the post-trade price back on the oracle,
subject to a size cap and inventory floors.

`docs/superpowers/specs/2026-09-01-amm-price-keeper-design.md` is the design
this implements, including the reasoning this file only states.

## Building

```bash
nix develop --accept-flake-config --command cargo build --release -p amm-price-keeper
```

The binary lands at `target/release/amm-price-keeper`.

`nix build .#amm-price-keeper` produces it too, which is what the NixOS unit in
the infra repo (`hosts/modules/amm-price-keeper.nix`) consumes. That unit also
installs `nix build .#fedimint-cli-experimint`, since joining and funding the
wallet below needs a client on the host.

## Flags

Every flag also takes an environment variable, so it can run from a systemd
unit with nothing on the command line.

| flag | env | default | meaning |
| --- | --- | --- | --- |
| `--data-dir` | `FM_CLIENT_DIR` | — | client data dir; **required** |
| `--oracle-url` | `AMM_KEEPER_ORACLE_URL` | `https://price-feed.dev.fedibtc.com/latest` | price feed; only its `BTC/USD` pair is read |
| `--pool` | `AMM_KEEPER_POOL` | `0:1` | canonical `lo:hi` pool id; `lo` must be unit 0 |
| `--tick-interval` | `AMM_KEEPER_TICK_INTERVAL` | `60s` | the feed updates about once a minute |
| `--band-bps` | `AMM_KEEPER_BAND_BPS` | `50` | deadband half-width; must exceed the fee |
| `--max-trade-usd` | `AMM_KEEPER_MAX_TRADE_USD` | — | per-tick size cap in dollars (`12.50`); **required** |
| `--btc-floor-msat` | `AMM_KEEPER_BTC_FLOOR_MSAT` | `0` | never spend the BTC balance below this |
| `--usdt-floor-micros` | `AMM_KEEPER_USDT_FLOOR_MICROS` | `0` | never spend the USDt balance below this |
| `--max-oracle-age` | `AMM_KEEPER_MAX_ORACLE_AGE` | `300s` | staleness limit on the feed's own timestamp |
| `--max-rate-jump-pct` | `AMM_KEEPER_MAX_RATE_JUMP_PCT` | `10` | tick-over-tick sanity limit |
| `--max-slippage-bps` | `AMM_KEEPER_MAX_SLIPPAGE_BPS` | `50` | passed to `amm.swap`, which re-quotes and derives `min_out` itself |

Durations take a `s`/`m`/`h` suffix; a bare number is seconds. Amounts are
integers in the unit's own base denomination — **msats** (1e-11 BTC) for unit
0, **micros** (1e-6 USDt) for unit 1 — exactly as in
`bin/fedimint-cli-experimint/README.md`. `--max-trade-usd` is the one decimal
figure, and it is parsed digit by digit rather than through a float.

## Funding it

The bot never deposits, withdraws, or rebalances. A human funds it, and a
human tops it up when a side runs low.

1. **Give it its own data dir.** It holds rocksdb open for its whole lifetime
   and is the sole writer: a `fedimint-cli-experimint` run against the same
   directory while the bot is up will fail to open the database. Do not point
   it at your personal wallet.

   ```bash
   CLI=target/release/fedimint-cli-experimint
   $CLI --data-dir /var/lib/amm-price-keeper join fed11qgqp...
   $CLI --data-dir /var/lib/amm-price-keeper print-secret   # back this up
   ```

2. **Fund both units.** It needs a BTC (unit 0) balance to sell BTC and a
   USDt (unit 1) balance to buy it; a side with nothing in it simply stops
   correcting in that direction, and says so at WARN every tick. Send ecash
   to the wallet, or peg in, and confirm with:

   ```bash
   $CLI --data-dir /var/lib/amm-price-keeper info
   ```

   `info`'s `units` section is what the bot reads as its balances.

3. **Stop the CLI before starting the bot.** One process, one data dir.

## Arming it safely

Do this in two steps. The first step cannot trade at all, which is the point.

**Step 1 — wide band, read-only in practice.** Set the band far wider than any
real deviation, so every tick holds, and read the log lines:

```bash
target/release/amm-price-keeper \
  --data-dir /var/lib/amm-price-keeper \
  --band-bps 5000 \
  --max-trade-usd 1
```

Every tick logs the pool price, the oracle price, the deviation in basis
points, the effective fee, both reserves and both balances:

```
INFO holding pool_usd=77014.084507 oracle_usd=77785.250000 dev_bps=-99 fee_per_mille=3 ... reason=InBand { dev_bps: -99 }
```

Confirm all of that before going further: the pool price should match what
`$CLI module <amm-id> pools` reports, the oracle price should match the feed,
and the deviation's **sign** should read the way you expect (positive means
the pool is expensive relative to the oracle, and the correction is to sell
BTC into it).

**Step 2 — narrow the band to arm it.** Drop `--band-bps` to something real
and raise the size cap to what you are willing to lose per tick:

```bash
target/release/amm-price-keeper \
  --data-dir /var/lib/amm-price-keeper \
  --band-bps 50 \
  --max-trade-usd 25 \
  --btc-floor-msat 1000000 \
  --usdt-floor-micros 1000000
```

The first tick that finds the pool outside the band will log `trading` with
the size and the price the trade is expected to land at, then place it.

### The band must exceed the fee

At `fee_per_mille = 3` a round trip burns about 60 bps, so a band under 30 bps
means oracle noise alone pays fees to move the price nowhere. **The bot
refuses to start if `--band-bps` is under `fee_per_mille * 10`**, reading the
fee from the live pool rather than from any config of its own. If the
guardians vote the fee up past your band while it is running, it holds every
tick with `BandBelowFee` instead of trading, rather than exiting.

## What it will not do

- No dry-run mode. Step 1 above — a band nothing can escape — is the dry run.
- No auto-rebalancing, no LP management: it never deposits or withdraws
  liquidity, and never converts one side into the other except through the
  price-correcting swaps themselves.
- No burn budget or spend-based kill switch. `--max-trade-usd` bounds one
  tick, not the day.
- **One swap in flight, ever.** A swap is two federation transactions and the
  second retries indefinitely by design, so the bot waits for `Done` before
  the next tick may trade. A swap outstanding for more than five minutes is
  logged at WARN and is still not abandoned — abandoning it would strand the
  balance the first transaction created.

## Failure behaviour

Every failure is fail-closed: hold, log, try again next tick. The bot never
trades on a price it could not validate.

| situation | what happens |
| --- | --- |
| feed unreachable, malformed, or missing `BTC/USD` | hold, WARN |
| feed timestamp older than `--max-oracle-age` (or implausibly far in the future) | hold, WARN |
| rate moved more than `--max-rate-jump-pct` since the last accepted tick | hold, WARN |
| pool absent, empty, or one-sided | hold, WARN |
| a side is exhausted (clamps leave less than the AMM's `min_swap_in`) | hold, WARN — this is the "top me up" signal |
| deviation inside the band | hold, INFO |
| swap rejected or unsubmittable | ERROR; the next tick re-reads everything and decides again |

On startup it calls the AMM client's `recover()` once, which sweeps any
balance a crash between the two swap transactions stranded.

`SIGINT`/`SIGTERM` finish an in-flight swap's await and then exit.

## Assumptions that are load-bearing

If one of these is false the bot tracks the wrong price and nothing in the
system will notice.

- **1 USDt = 1 USD.** The feed publishes no `USDT/USD` pair, so a USDt depeg
  moves the pool off by exactly the depeg.
- **Unit 0 is Bitcoin in msats and unit 1 is USDt in micros.** The bot
  refuses to start unless `--pool`'s low side is unit 0, but nothing can check
  the denominations themselves.
- **One `amm` instance.** The bot resolves it by kind and fails if the
  federation reports more than one, rather than silently picking the
  lowest-numbered one.

## Tests

```bash
nix develop --accept-flake-config --command cargo test -p amm-price-keeper
```

`policy.rs` and `oracle.rs` are pure and carry all the logic; both are
table-tested, and the policy additionally has a property test asserting that
whatever trade it returns moves `|dev_bps|` toward zero and never flips its
sign — the failure a direction error in the sizing rule would produce.
