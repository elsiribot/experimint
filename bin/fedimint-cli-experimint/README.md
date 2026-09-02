# fedimint-cli-experimint

`fedimint-cli` built with this repo's client module set: the v2 core modules
(`mintv2`, `walletv2`, `lnv2`), `meta`, the two local modules `amm` and
`usdt`, and Fedi's stability pool v2 (`multi_sig_stability_pool`, "multispend").
Everything a stock `fedimint-cli` can do, it can do — same flags, same
subcommands, same JSON output — plus the `module amm` / `module usdt` /
`module multi_sig_stability_pool` verbs a stock build cannot link, plus its own
[`info`](#info).

Written for someone who has never used it. `src/lib.rs`'s rustdoc is the same
material for someone reading the code; this file is the invocations.

## Building

```bash
nix develop --accept-flake-config --command cargo build --release -p fedimint-cli-experimint
```

The binary lands at `target/release/fedimint-cli-experimint`. It is not part of
the `fedimintd-experimint` flake package, so `nix build` will not produce it.
Copy it somewhere outside `target/` if you want it to survive a `cargo clean`.

Everything below uses:

```bash
CLI=target/release/fedimint-cli-experimint
export FM_CLIENT_DIR=/path/to/wallet-dir
```

## Joining a federation

The data directory holds the client database and the seed. `--data-dir` and the
`FM_CLIENT_DIR` environment variable are the same flag; every command needs one
or the other, and there is no default.

```bash
$CLI --data-dir /path/to/wallet-dir join fed11qgqp...
```

`join` creates the directory, generates a BIP39 seed if there is none, and
downloads the federation's config. From then on any command against that
directory operates as that wallet.

The invite code names one guardian's API endpoint plus the federation id. Onion
endpoints need `--use-tor` (this binary is built with the `tor` feature on).

Back up the seed with `print-secret` before putting funds in — the ecash lives
in the client database, and the seed is what recovers it.

## Address modules by id, not by kind

**This is the one thing to get right.** The topology this repo targets runs
*two* `mintv2` instances, one per asset. `module <kind>` resolves through
`Client::get_first_instance`, which returns the **lowest** instance id of that
kind — so `module mintv2` always reaches the bitcoin mint and can never reach
the USDt mint, whatever you ask it for. It does not warn; it answers about the
wrong mint.

The selector rule is: **an all-digit argument is an instance id, anything else
is a kind.** So use ids.

```bash
$CLI module 2 ...     # the USDt mint — reachable only this way
$CLI module mintv2 .. # the BTC mint, always, silently
```

`module` with no argument lists the instances, and [`info`](#info) adds what
each one deals in.

```console
$ $CLI module
{
  "list": [
    { "id": 0, "kind": "walletv2", "status": "Active" },
    { "id": 1, "kind": "mintv2",   "status": "Active" },
    { "id": 2, "kind": "mintv2",   "status": "Active" },
    { "id": 3, "kind": "lnv2",     "status": "Active" },
    { "id": 4, "kind": "usdt",     "status": "Active" },
    { "id": 5, "kind": "amm",      "status": "Active" },
    { "id": 6, "kind": "meta",     "status": "Active" }
  ]
}
```

### Instance ids come from `--module` flag order

Ids are assigned by the position of the repeated `--module` flag at config
generation (or by row order in the setup UI), and are then frozen in the
federation's consensus config for its lifetime. The table below is what the
documented config-gen ordering produces:

| id | kind | what it is |
| --- | --- | --- |
| 0 | `walletv2` | on-chain BTC peg-in / peg-out |
| 1 | `mintv2` | **BTC** ecash (`amount_unit: 0`) |
| 2 | `mintv2` | **USDt** ecash (`amount_unit: 1`) |
| 3 | `lnv2` | Lightning |
| 4 | `usdt` | USDt-on-EVM peg |
| 5 | `amm` | constant-product market between units 0 and 1 |
| 6 | `meta` | guardian-published metadata |
| 7 | `multi_sig_stability_pool` | Fedi's stability pool v2 (multispend), if attached |

The `console` samples in this README are from the deployed federation, which
does not attach `multi_sig_stability_pool` — they stop at id 6. A federation
that does attach it gets id 7, same rule as every other row.

**They are not portable.** A federation generated with the flags in another
order has different ids, and nothing in an invite code or a client database
tells you which convention was used. Never copy an id from a runbook into a
different federation — run `info` against the federation you are actually
joined to.

## `info`

```bash
$CLI info
```

Reports every module instance, its id and kind, and the `AmountUnit`s it deals
in, followed by one entry per unit with the instance that is primary for it and
the balance held in it.

```console
$ $CLI --data-dir /path/to/wallet-dir info
{
  "federation_id": "91a43fc1a5e0622a0e06e709accbf4f62bb5d2622e969faa8922f976ebe81623",
  "federation_name": "experimint USDT/AMM",
  "modules": [
    { "id": 0, "kind": "walletv2", "status": "active", "units": [],     "units_source": "undeclared" },
    { "id": 1, "kind": "mintv2",   "status": "active", "units": [0],    "units_source": "primary_module_support" },
    { "id": 2, "kind": "mintv2",   "status": "active", "units": [1],    "units_source": "primary_module_support" },
    { "id": 3, "kind": "lnv2",     "status": "active", "units": [],     "units_source": "undeclared" },
    { "id": 4, "kind": "usdt",     "status": "active", "units": [1],    "units_source": "module_constant" },
    { "id": 5, "kind": "amm",      "status": "active", "units": [0, 1], "units_source": "module_config" },
    { "id": 6, "kind": "meta",     "status": "active", "units": [],     "units_source": "undeclared" }
  ],
  "units": [
    { "balance": 36994560,  "primary_module": 1, "unit": 0 },
    { "balance": 181264384, "primary_module": 2, "unit": 1 }
  ]
}
```

(Reformatted here to one instance per line; the real output is
`serde_json`-pretty.)

`units_source` names the module API that answered, because none of these units
comes from a kind → unit table:

| `units_source` | means |
| --- | --- |
| `primary_module_support` | the module's `supports_being_primary()`, which is also what the client routes funding by. A `mintv2` returns its configured `amount_unit`. |
| `module_config` | the module's own client config, as agreed at config generation. `amm`'s `units` allowlist. |
| `module_constant` | a constant the module's `-common` crate publishes as its unit of account. `usdt`'s `USDT_UNIT`. |
| `undeclared` | nothing this instance exposes says what it deals in. |

`walletv2` and `lnv2` transact in bitcoin by construction, but on the pinned
platform revision neither declares an `AmountUnit` through any client-side API,
so `info` reports `undeclared` rather than filling the answer in. A table
written into the CLI would go stale silently the day a module changed its unit;
`undeclared` is at least true.

`primary_module` is the instance that supplies funding inputs and takes change
in that unit — the reason a `usdt` deposit lands as a *balance in mint instance
2* rather than in the `usdt` module.

This `info` replaces `fedimint-cli`'s. Upstream's resolves the mint by kind, so
against this topology it prints the BTC mint's note tiers and never mentions the
USDt mint. Note that `--help` still shows upstream's one-line description
("Display wallet info (holdings, tiers)"), because the top-level help page is
upstream's; `info --help` is this command's.

## AMM

```bash
$CLI module 5 pools
$CLI module 5 quote  --unit-in 0 --unit-out 1 --amount-in 100000
$CLI module 5 swap   --unit-in 0 --unit-out 1 --amount-in 100000 --max-slippage-bps 50
$CLI module 5 deposit  --pool 0:1 --amount-lo <btc-msats> --amount-hi <usdt-micros> --max-slippage-bps 50
$CLI module 5 withdraw --pool 0:1 --owner-pk <hex-pk> --shares <n>
$CLI module 5 list-positions
$CLI module 5 recover
```

`--pool` is `lo:hi` with `lo < hi` — the canonical form that stops one unit pair
from producing two pool records — so the BTC/USDt pool is `0:1`. `--amount-lo`
is the low-numbered unit's leg, `--amount-hi` the high-numbered one.

**`--max-slippage-bps` is required on `swap` and `deposit`, and there is no
`--min-out`.** It bounds how much worse than the quote a trade may settle at,
and it has no default because a default would make that trade-off silently on
your behalf; `0` demands the quote exactly. There is no `--min-out` because the
library re-quotes immediately before submitting and derives `min_out` from that
fresh quote itself (spec §12) — a quote is a snapshot of pool state and goes
stale the moment anything else touches the pool, so a figure you passed in from
an earlier `quote` call would be the wrong thing to enforce.

`swap`, `deposit` and `withdraw` wait for the operation to finish by default;
`--no-wait` returns the operation id instead. `swap`'s second transaction
retries indefinitely by design rather than failing, so a wait can hang if the
federation is wedged.

`pools` returns `[]` until someone has deposited both legs, and until then every
`quote` and `swap` fails.

## USDt

```bash
$CLI module 4 status                             # must be `Ready` before a deposit is allocated
$CLI module 4 deposit-address
$CLI module 4 deposit-status <claim-pk>
$CLI module 4 submit-deposit-proof --index <n>
$CLI module 4 deposit-fee-quote
$CLI module 4 fee-quote <micros>                 # withdrawal fee for that amount
$CLI module 4 withdraw <0x-recipient> <micros>
$CLI module 4 withdrawal-status <txid> <out-idx>
$CLI module 4 recover
```

Note the positional arguments: `deposit-status`, `fee-quote`, `withdraw` and
`withdrawal-status` take theirs positionally, while `submit-deposit-proof` and
the fee ceilings (`--max-fee`, `--max-deposit-fee`) are flags. `module 4 --help`
is authoritative.

The `usdt` module handles the EVM peg only. The *balance* lives in the unit-1
mint — instance 2 — which is what `info` reports under unit 1. Deposit
addresses come from the module; the ecash comes from the mint.

## Units and denominations

Amounts are always integers in the unit's own base denomination, and no
argument or output carries the unit with it:

| unit | asset | base denomination |
| --- | --- | --- |
| 0 | Bitcoin | **msats** (1e-11 BTC) |
| 1 | USDt | **micros** (1e-6 USDt) |

So `info`'s `"balance": 181264384` under `"unit": 1` is 181.264384 USDt, and
`"balance": 36994560` under `"unit": 0` is 36 994.56 sats. `AmountUnit` has no
string form, which is why every `--unit-in` / `--unit-out` takes the raw id.

## Commands that fail, and why that is expected

`fedimint-cli`'s v1-era top-level verbs resolve their module by kind `mint`,
which this federation does not run:

```console
$ $CLI spend 1000
{
  "error": "No modules found of kind mint"
}
```

`spend`, `reissue` and friends all fail this way. **That is not a broken
wallet.** The v1 `mint`/`wallet`/`ln` client modules are deliberately not
registered, because a multi-unit federation is a v2-only story and those modules
predate `AmountUnit`; they would fail identically against this federation even
if they were registered, because it runs no v1 instances. The v2 equivalents are
the `module <id>` verbs above.

The one v1-era verb that does *not* fail is `info`, which is why this binary
replaces it — see above.
