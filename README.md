# experimint

Fedimint module experiments, developed outside the Fedimint tree and built
against a pinned platform branch.

Two module families live here:

| Family | Crates | What it is |
| --- | --- | --- |
| `modules/amm` | `fedimint-amm-{common,server,client,tests}` | A constant-product AMM (Uniswap V2 as reference implementation) trading between the federation's `AmountUnit`s. See [`modules/amm/fedimint-amm-spec.md`](modules/amm/fedimint-amm-spec.md). |
| `modules/usdt` | `fedimint-usdt-{common,server,client,tests}` | USDT-on-EVM peg-in/peg-out via threshold ECDSA and ERC-4337. Lifted from the fork; consensus version 0.12, with deployed federations. |

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

- **Runnable binaries.** There is no `fedimintd-experimint` or
  `fedimint-cli-experimint` yet — the modules build and test, but nothing in
  this repo starts a federation with them attached. Adding them means a thin
  `fedimintd` that attaches `UsdtInit` (and `AmmInit`), mirroring the fork's
  `default_modules()`, plus the matching client-side registrations.
- **The `swap` module**, the other custom module on the fork's
  `2026-07-usdt-wallet` branch.
- **CI.** There is no `.github/` in this repo, so none of the checks above run
  automatically.
