# experimint

Fedimint module experiments, developed outside the Fedimint tree and built
against a pinned platform branch.

Three module families live here:

| Family | Crates | What it is |
| --- | --- | --- |
| `modules/amm` | `fedimint-amm-{common,server,client,tests}` | A constant-product AMM (Uniswap V2 as reference implementation) trading between the federation's `AmountUnit`s. See [`modules/amm/fedimint-amm-spec.md`](modules/amm/fedimint-amm-spec.md). |
| `modules/usdt` | `fedimint-usdt-{common,server,client,tests}` | USDT-on-EVM peg-in/peg-out via threshold ECDSA and ERC-4337. Lifted from the fork at consensus version 0.12. |
| `multi_sig_stability_pool` | `stability-pool-{common,server,client}` | Fedi's stability pool v2 ("multispend"): multisig accounts with threshold transfers, and seeker/provider BTC↔fiat stabilization. Sourced from [`elsiribot/fedi`](https://github.com/elsiribot/fedi), branch `experimint`, not this repo's own `modules/`. |
| `bin/fedimintd-experimint` | — | A `fedimintd` carrying the v2 core modules, `meta`, both local modules, and `multi_sig_stability_pool`. |
| `bin/fedimint-cli-experimint` | — | The matching `fedimint-cli`, the only client that can drive `amm`, `usdt` and `multi_sig_stability_pool`. See [its README](bin/fedimint-cli-experimint/README.md). |

## Running a federation

`bin/fedimintd-experimint` is a thin wrapper around the platform branch's
`fedimintd::run`, supplying this module set: `mintv2`, `walletv2`, `lnv2`,
`meta`, `amm`, `usdt`, `multi_sig_stability_pool`. Every flag, env var, setup
UI and API endpoint is inherited from upstream.

```bash
cargo run -p fedimintd-experimint -- --help
```

It deliberately omits the v1 `mint`/`wallet`/`ln` modules that a stock
`fedimintd` also attaches: this binary targets a multi-unit federation, which is
a v2-only story — the v1 modules predate `AmountUnit`.

[`docs/mainnet-deployment.md`](docs/mainnet-deployment.md) is the runbook for
standing seven of these up on Bitcoin and Ethereum mainnet: config-gen
commands, the full env var table, secret provisioning, the DKG ceremony, and
how to tell whether the usdt module actually reached `BootstrapState::Ready`
afterwards.

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
| `multi_sig_stability_pool` | multisig accounts + threshold transfers (Fedi multispend); seeker/provider BTC↔fiat stabilization |

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
    --module meta \
    --module multi_sig_stability_pool
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
`FM_ENABLE_MODULE_USDT`. `multi_sig_stability_pool` is opt-in the same way, via
`FM_ENABLE_MODULE_SPV2`, independent of the asset topology above. They are
still *available* without those variables; the variables only decide what
starts pre-selected.

## Talking to a federation

`bin/fedimint-cli-experimint` is the client counterpart: a `fedimint-cli` with
this module set linked in, which is what makes `module amm` and `module usdt`
resolve at all.

```bash
cargo run -p fedimint-cli-experimint -- --data-dir /path/to/wallet join fed11qgqp...
cargo run -p fedimint-cli-experimint -- --data-dir /path/to/wallet info
```

[`bin/fedimint-cli-experimint/README.md`](bin/fedimint-cli-experimint/README.md)
is the usage guide: joining, the instance-id table and why ids are not portable
between federations, the AMM and USDt verbs with their real argument shapes,
denominations per unit, and which upstream subcommands are expected to fail
here.

The one thing to know before reading anything else: **`module <kind>` resolves
to the lowest instance id of that kind**, so with two `mintv2` instances it
always reaches the BTC mint. Address instances by id.

## This is deployed now

**As of 2026-08-31 a seven-guardian federation runs these modules on Bitcoin
mainnet and Ethereum mainnet, and it holds real funds.** `usdt` is at consensus
version 0.13, `amm` at 0.0. See `docs/handover-2026-08-30.md` for the invite
code, topology and operational notes.

This section previously said the opposite, and said that if a deployment ever
happened it would be the first thing needing an update — because a good deal of
judgement downstream keys off it. So, explicitly:

**What has changed.** Wire encoding, DB layout and consensus behaviour are no
longer free to change. A breaking change now needs a `MODULE_CONSENSUS_VERSION`
bump and, for stored records, a migration — not just an edit. The cost of
getting that wrong is already demonstrated: `usdt` shipped two breaking wire
changes under one version, and the mismatch surfaced as a deposit that hung
forever rather than a clean rejection (see the `0.13` note in
`fedimint-usdt-common`). `fedimint-derive` encodes enum variants by **positional
index**, so deleting or reordering a variant is breaking even when nothing
references it.

**What has not been decided.** How much of the old freedom to give up is a
judgement call, not a fact, and it is not made here. `amm` is still at `0.0` and
its pool holds real liquidity; whether that warrants freezing its wire format or
whether this deployment is disposable enough to keep breaking it is the
maintainer's call. Findings previously parked as "not actionable without a
version bump" are still actionable — a bump is now the price, rather than being
free.

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

The daemon is also a flake package, which is what a NixOS host consumes:

```bash
nix build .#fedimintd-experimint
./result/bin/fedimintd-experimint --version
```

It builds only `bin/fedimintd-experimint`, not the `*-tests` crates, and
vendors from the committed `Cargo.lock` rather than re-resolving — see the trap
described under [Cargo.lock is seeded from the platform
branch](#cargolock-is-seeded-from-the-platform-branch).

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

### The EVM end-to-end suites

The `fedimint-usdt-tests` end-to-end suites drive a real EVM chain via `anvil`,
which the dev shell provides (Foundry). Nothing extra to install.

**They skip silently if `anvil` is missing.** `spawn_anvil` returns `Ok(None)`
on `ErrorKind::NotFound`, so an absent binary and a passing test produce
identical output. That is why Foundry is a dev-shell dependency rather than a
CI step — the shell is what CI and developers share, so neither can quietly
lose the coverage. Every other failure mode (bad `FM_ANVIL_BASE_EXECUTABLE`,
wrong permissions, anvil spawning but never serving RPC) is a hard failure, by
design.

`tests/full_topology_e2e.rs` deliberately does not take that affordance: it
treats a missing `anvil` as a hard failure, because its central claim is that
the `amm` trades *real* USDt and a silently-skipped USDt leg would make that
claim unfalsifiable.

These suites are the only coverage of the real ERC-4337 UserOp path,
withdrawal batching against a live chain, reorg handling, residual recovery and
non-standard token behaviour. `FM_ANVIL_BASE_EXECUTABLE` overrides the binary
if you need a specific build. Some devimint tests separately want `bitcoind`.

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

- **The `swap` module**, the other custom module on the fork's
  `2026-07-usdt-wallet` branch.
- **A non-CLI client.** `bin/fedimint-cli-experimint` is the only thing that
  drives `amm` and `usdt` client-side; there is no library-shaped wallet SDK,
  and the `*-client` crates are WASM-safe but nothing consumes them from a
  browser or a mobile host.
- **A deployment-shaped run of the full topology.** The topology itself is now
  covered: `fedimint-usdt-tests`' `tests/full_topology_e2e.rs` stands up one
  in-process federation with all seven instances (both `mintv2`s included),
  pegs BTC in over `walletv2`, deposits USDt by proof against a real `anvil`,
  seeds an `amm` pool with both legs, swaps each way at the quoted price,
  withdraws the position and checks every guardian's balance sheet. What is
  still untested is the deployment shape around it: `fedimint-testing`
  trusted-dealer-generates configs rather than driving the `--module` CLI/UI
  path, and no Lightning gateway is attached, so `lnv2` boots but never
  routes.
