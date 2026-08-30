# experimint

Fedimint module experiments, developed outside the Fedimint tree and built
against a pinned platform branch.

Two module families live here:

| Family | Crates | What it is |
| --- | --- | --- |
| `modules/amm` | `fedimint-amm-{common,server,client,tests}` | A constant-product AMM (Uniswap V2 as reference implementation) trading between the federation's `AmountUnit`s. See [`modules/amm/fedimint-amm-spec.md`](modules/amm/fedimint-amm-spec.md). |
| `modules/usdt` | `fedimint-usdt-{common,server,client,tests}` | USDT-on-EVM peg-in/peg-out via threshold ECDSA and ERC-4337. Lifted from the fork at consensus version 0.12. |
| `bin/fedimintd-experimint` | — | A `fedimintd` carrying the v2 core modules, `meta`, and both local modules. |

## Running a federation

`bin/fedimintd-experimint` is a thin wrapper around the platform branch's
`fedimintd::run`, supplying this module set: `mintv2`, `walletv2`, `lnv2`,
`meta`, `amm`, `usdt`. Every flag, env var, setup UI and API endpoint is
inherited from upstream.

```bash
cargo run -p fedimintd-experimint -- --help
```

It deliberately omits the v1 `mint`/`wallet`/`ln` modules that a stock
`fedimintd` also attaches: this binary targets a multi-unit federation, which is
a v2-only story — the v1 modules predate `AmountUnit`.

### The intended topology

| Instance | Purpose |
| --- | --- |
| `walletv2` | on-chain BTC peg-in/peg-out |
| `mintv2` (`amount_unit: 0`) | BTC ecash |
| `mintv2` (`amount_unit: 1`) | USDT ecash |
| `usdt` | USDT-on-EVM peg (issues `AmountUnit::new_custom(1)`) |
| `amm` | constant-product market between units 0 and 1 |
| `lnv2` | Lightning |
| `meta` | guardian-published metadata |

Note the **two `mintv2` instances**, one per asset. Both setup paths express it.

**CLI / API.** `set-local-params` takes a repeatable
`--module <kind>[=<json>]` that builds the instance list directly. Instance ids
are assigned by flag position:

```bash
fedimint-cli admin setup set-local-params \
    --module walletv2 \
    --module 'mintv2={"amount_unit":0}' \
    --module 'mintv2={"amount_unit":1}' \
    --module lnv2 \
    --module 'usdt={"chain_id":1}' \
    --module amm \
    --module meta
```

The platform branch has a test (`parses_full_deployment_topology`) asserting
exactly this shape, two `mintv2` instances included.

**Setup UI — supported.** The form builds the instance list one row at a time:
pick a kind, and for kinds denominated in an asset pick that too. Add a second
`mintv2` row, choose "USDT (unit 1)", and the federation runs two mints. Rows
can be added and removed, so the UI expresses the same topologies as `--module`.

Multiple instances of one kind were always supported by the data model —
`ServerModuleConfigGenParamsRegistry` is keyed by `ModuleInstanceId`, and
`ConfigGenParams::module_params` is the single source of truth for which
instances run. Only the form couldn't express it. (The `"Can't insert module of
same kind twice"` assert is on `ModuleInitRegistry`, kind → *init*: one
implementation per kind, unrelated to instance count.)

### Assets

A mint holds no reserves of its own — its ecash is a claim on whatever backs the
unit it is denominated in. Both sides are declared:

| Module | Declares |
| --- | --- |
| `walletv2` | backs Bitcoin (unit 0) |
| `usdt` | backs USDT (unit 1) |
| `mintv2` | requires whichever unit its `amount_unit` names |

Config generation refuses a topology whose ecash is denominated in an asset
nothing backs, and it does so before DKG — the denomination is baked into the
module's consensus config and cannot be changed afterwards. The setup UI offers
only the declared assets, so the same mistake is unreachable through the form.

Bitcoin is always available and never needs a backing module: it is the
federation's native unit, and a lightning-only federation denominates in it with
no on-chain wallet enabled.

### Modules are not pre-ticked by default

The setup UI pre-selects only modules whose `is_enabled_by_default()` is true —
here just `lnv2`, `meta` and `amm`. The three carrying the interesting topology
are opt-in via `FM_ENABLE_MODULE_MINTV2`, `FM_ENABLE_MODULE_WALLETV2` and
`FM_ENABLE_MODULE_USDT`. They are still *available* without those variables; the
variables only decide what starts pre-selected.

## Nothing here is deployed

No federation runs these modules yet. That is load-bearing for how changes are
made: consensus behaviour, wire encoding and DB layout can all still change
freely, and dead consensus paths can be deleted rather than frozen for replay.

The extraction brief this repo started from assumed the opposite — that `usdt`
had live federations at consensus version 0.12 — and several rounds of work
were scoped around that constraint. Findings parked as "not actionable without
a version bump" during those rounds are actionable now. If a deployment does
happen, this section is the first thing that needs updating, because a good
deal of judgement downstream keys off it.

## The platform pin

Every `fedimint-*` dependency is git-pinned, by revision, to a single commit:

```toml
fedimint-core = { git = "https://github.com/elsiribot/fedimint", rev = "a2d0207702fa13f386b40d41da8133f14fd000ac" }
```

That revision is `experimint-v0.11` on the `elsiribot/fedimint` fork —
Fedimint plus the fork's core/platform changes, with the custom modules
stripped out. Workspace version `0.12.0-alpha`.

**This is not upstream master, and it is not behind it.** The two diverged
from `be854220f`; `experimint-v0.11` is a different API generation, not an
older one. Do not assume an upstream `fedimint/fedimint` commit is a drop-in
replacement.

Three things force the pin:

- `fedimint-usdt-server` depends on `fedimint-threshold-ecdsa`
  (`crypto/threshold-ecdsa`), which exists only on the fork.
- The typed per-module `ServerModuleInit::Params` config-gen hook and the
  leader `--module` CLI are fork-only.
- Both module families must resolve to **one and the same** `fedimint-core`.
  Two copies would compile, but the modules could never be registered into
  the same federation — which is the point of an AMM that trades USDT.

### Bumping the pin

1. Change `rev` on every `fedimint-*` line in the root `Cargo.toml`. They must
   all move together — a mismatched rev makes cargo resolve them as distinct
   source packages and the trait impls silently stop lining up.
2. Re-seed the lock (see below) and run
   `cargo check --workspace --all-targets`.

`branch = "experimint-v0.11"` also resolves, but moves under you; prefer the
rev.

### Cargo.lock is seeded from the platform branch

**Cargo does not inherit a git dependency's lockfile.** It re-resolves the
whole tree from scratch, which can land on version combinations the platform
branch itself never builds. Two have bitten this repo already:

- a fresh resolve picks `bdk_electrum 0.23.2` → `electrum-client 0.24.1`,
  while `fedimint-ldk-node` also wants `electrum-client 0.23.1` directly; two
  semver-incompatible copies land in the graph and it fails to compile.
- `fedimint-connectors` wants `iroh-next = "=1.0.0"` *and*
  `iroh-mainline-address-lookup ^0.4.0`; a fresh resolve picks `iroh 1.0.3`
  for the latter and fails to unify.

`Cargo.lock` is therefore seeded from the platform branch's own lock and
**committed**. When bumping the pin, seed it again rather than deleting it:

```bash
git -C /path/to/fedimint show experimint-v0.11:Cargo.lock > Cargo.lock
cargo check --workspace --all-targets
```

Keeping third-party versions in `[workspace.dependencies]` byte-identical to
the platform branch's is part of the same defence — it is what keeps
`alloy*`, `secp256k1`, `bls12_381` and `cggmp21` from unifying into two
copies across the git boundary. Do not upgrade one in isolation.

## Building

Everything runs in the Nix dev shell:

```bash
nix develop --accept-flake-config
cargo check --workspace --all-targets
```

Or without entering it:

```bash
nix develop --accept-flake-config --command cargo check --workspace --all-targets
```

The shell exists because several dependencies build C from source and need
specific host tooling: `m4` and `file` for `gmp-mpfr-sys` (via `rug` ←
`cggmp21`), `protobuf`/`cmake`/`perl` for the gateway and `aws-lc-sys`, and a
`fortify`-free hardening set for `tikv-jemalloc-sys`' autoconf probes.

## Tests

```bash
cargo test --workspace          # everything that runs hermetically
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

### What needs external binaries

The `fedimint-usdt-tests` end-to-end suites drive a real EVM chain. They
**skip rather than fail** when `anvil` is not on `PATH`, so a plain
`cargo test --workspace` is green without it but is *not* exercising them.
Affected files, all under `modules/usdt/fedimint-usdt-tests/tests/`:
`deploy_and_sweep_e2e.rs`, `withdraw_e2e.rs`, `recovery_e2e.rs`,
`nonstandard_usdt_e2e.rs`, `anvil_reorg_drill.rs`, `erc4337_harness.rs`,
`evm_adapter.rs`, `user_op_hash.rs`, `user_op_isolation.rs`,
`withdrawal_batch_isolation.rs`, `adversary.rs`.

To run them, put `anvil` (from Foundry) on `PATH`, or point
`FM_ANVIL_BASE_EXECUTABLE` at it. Some also want `bitcoind` via `devimint`.

Three `fedimint-usdt-tests` binaries are not part of any test lane:

- `usdt-e2e-test` — devimint-driven end-to-end run.
- `usdt-adversary` — joins a **live** federation from an invite code and runs
  the no-funds deposit-by-proof attacks against it.
- `capture-deposit-proof-fixtures` — one-shot regenerator for the committed
  mainnet `eth_getProof` fixtures.

### WASM

`*-common` and `*-client` must stay WASM-safe:

```bash
cargo check --target wasm32-unknown-unknown \
  -p fedimint-usdt-common -p fedimint-usdt-client \
  -p fedimint-amm-common -p fedimint-amm-client
```

The dev shell carries the target and the cross-compilation environment this
needs (unwrapped clang for the vendored `secp256k1-sys` C, plus `getrandom`'s
`wasm_js` backend flag). Do not add non-WASM-safe dependencies to these four
crates.

## Not here yet

- **A matching client binary.** There is no `fedimint-cli-experimint`
  registering the `amm` and `usdt` *client* modules, so a stock `fedimint-cli`
  can drive setup and the generic admin API but cannot exercise those two
  modules' client-side flows.
- **The `swap` module**, the other custom module on the fork's
  `2026-07-usdt-wallet` branch.
- **An end-to-end run of the full topology.** `fedimintd-experimint` builds and
  its module set is unit-tested, but no test in this repo stands up a
  federation with all seven instances and moves value across them.
