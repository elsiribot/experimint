# SPv2 (multispend) Adoption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Attach Fedi's stability pool v2 module (`multi_sig_stability_pool`) — the server side of "multispend" — to `fedimintd-experimint`, sourced from an `experimint` branch of `elsiribot/fedi` that builds against our fedimint fork.

**Architecture:** Two-repo change. Repo A: a new `experimint` branch of `elsiribot/fedi` (worktree off `~/projects/fedi` at `cb458841e`) switches the fedi workspace's `fedimint-*` git deps to `elsiribot/fedimint rev=51d011a…` and ports `stability-pool-{common,server,client}` to the 0.12-alpha `ServerModuleInit` API — consensus logic, `KIND`, encodings and DB layout untouched. Repo B: experimint adds those three crates as rev-pinned git deps, attaches the server init with Fedi's deployed parameters, registers the client in the CLI, updates the pinned-module-set tests and docs.

**Tech Stack:** Rust (edition 2024), cargo git deps, fedimint 0.12.0-alpha module API.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-09-02-spv2-multispend-design.md`.
- Fedimint pin everywhere: `git = "https://github.com/elsiribot/fedimint", rev = "51d011a47769c91aabe2ed6f1f62e91e53c50283"` — URL and rev byte-identical in both repos or cargo splits `fedimint-core` into two source packages.
- Wire/DB compatibility with Fedi's deployment is a hard requirement: `KIND = "multi_sig_stability_pool"`, `CONSENSUS_VERSION = (2, 2)`, all `Encodable`/`Decodable` types and DB key prefixes stay byte-identical. Any port change that touches an encoded type is a bug.
- Fedi branch point: `cb458841e` (fedi master, spv2 as deployed today).
- `ln-gateway` does not exist at `51d011a` (renamed upstream); its workspace entry stays pinned to `fedibtc/fedimint tag=v0.11.0-fedi10`. Every other `fedimint-*` entry switches.
- Fedi's deployed module parameters (verbatim from fedi `crates/fedimint/fedimintd/src/main.rs`): `OracleConfig::Aggregate`, `cycle_duration = 600s`, `CollateralRatio { provider: 1, seeker: 1 }`, `min_allowed_seek = 100_000 msat`, `min_allowed_provide = 100_000 msat`, `max_allowed_provide_fee_rate_ppb = 2000`, `min_allowed_cancellation_bps = 100`.
- New env vars (constants live in `stability-pool-server`): `FM_ENABLE_MODULE_SPV2` (pre-tick in setup UI), `FM_SPV2_TEST_PARAMS` (Mock oracle + 15s cycle), `FM_SPV2_CYCLE_DURATION_SECS` (override, default 600).
- Out of scope: `stability-pool-old`, fedi's spv2 `tests` crate (devimint-based), the Matrix multispend layer.
- Other fedi workspace members (bridge, ffi, multispend, …) are allowed to not compile on the branch. The check gate is `cargo check -p stability-pool-common -p stability-pool-server -p stability-pool-client` plus their unit tests, not the whole fedi workspace.
- Commit trailers: every commit ends with the Co-Authored-By/Claude-Session trailer used elsewhere in this session.

---

## Phase A — `elsiribot/fedi` branch `experimint`

### Task 1: Branch + fedimint dep switch

**Files:**
- Create: git worktree `~/projects/fedi-experimint` on new branch `experimint` at `cb458841e`
- Modify: `~/projects/fedi-experimint/Cargo.toml` (workspace `[workspace.dependencies]`, the `fedimint-*`/`fedimintd`/`fedimint-cli`/`devimint` git entries)

**Interfaces:**
- Produces: a fedi workspace whose `stability-pool-*` crates resolve against `elsiribot/fedimint @ 51d011a`. Later tasks run `cargo check` inside this worktree.

- [ ] **Step 1: Create the worktree and branch**

```bash
git -C ~/projects/fedi worktree add ~/projects/fedi-experimint -b experimint cb458841e484577556b249ccadd9024d2eaa26a3
```

Expected: worktree created, branch `experimint` at `cb458841e`.

- [ ] **Step 2: Switch the fedimint deps**

In `~/projects/fedi-experimint/Cargo.toml`, replace every
`git = "https://github.com/fedibtc/fedimint", tag = "v0.11.0-fedi10"`
with
`git = "https://github.com/elsiribot/fedimint", rev = "51d011a47769c91aabe2ed6f1f62e91e53c50283"`
**except** the `ln-gateway` line, which keeps the fedibtc tag:

```bash
cd ~/projects/fedi-experimint
sed -i '/^ln-gateway = /!s|git = "https://github.com/fedibtc/fedimint", tag = "v0.11.0-fedi10"|git = "https://github.com/elsiribot/fedimint", rev = "51d011a47769c91aabe2ed6f1f62e91e53c50283"|' Cargo.toml
grep -c 'elsiribot/fedimint' Cargo.toml   # expect ~28
grep 'fedibtc/fedimint' Cargo.toml        # expect exactly the ln-gateway line
```

- [ ] **Step 3: Verify resolution**

```bash
cd ~/projects/fedi-experimint && cargo metadata --format-version 1 > /dev/null
```

Expected: succeeds (may take minutes fetching). If it fails naming a missing package at `51d011a`, pin that entry back to the fedibtc tag like `ln-gateway` and note it in the commit message.

- [ ] **Step 4: Commit**

```bash
cd ~/projects/fedi-experimint && git add Cargo.toml Cargo.lock && git commit -m "build: point fedimint deps at elsiribot/fedimint 51d011a

ln-gateway stays on the fedibtc tag: the package does not exist on the
experimint platform branch (renamed upstream), and no crate this branch
needs to build depends on it."
```

(Add the session trailer. If `Cargo.lock` didn't change because resolution is deferred, commit `Cargo.toml` alone.)

### Task 2: Port `stability-pool-common`

**Files:**
- Modify (only if compile errors demand): `~/projects/fedi-experimint/crates/modules/stability-pool/common/src/*.rs`

**Interfaces:**
- Produces: `stability-pool-common` compiling against `fedimint-core @ 51d011a`, all its types byte-identical on the wire. Tasks 3–4 depend on it.

- [ ] **Step 1: Check**

```bash
cd ~/projects/fedi-experimint && cargo check -p stability-pool-common 2>&1 | tail -20
```

Expected: likely clean — `fedimint-core`'s changes between the forks are mostly additive and `AmountUnit` exists on both sides. If errors appear they will be import/name-level (e.g. moved items in `fedimint_core::module`); fix imports only. **Do not touch any `Encodable`/`Decodable` type, `KIND`, or a consensus version constant.**

- [ ] **Step 2: Run its unit tests (includes proptests)**

```bash
cargo test -p stability-pool-common 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 3: Commit (only if changes were needed)**

```bash
git add -u && git commit -m "fix(stability-pool-common): adapt to the experimint platform branch"
```

### Task 3: Port `stability-pool-server` to the 0.12-alpha `ServerModuleInit`

**Files:**
- Create: `~/projects/fedi-experimint/crates/modules/stability-pool/server/src/envs.rs`
- Modify: `~/projects/fedi-experimint/crates/modules/stability-pool/server/src/lib.rs`

**Interfaces:**
- Consumes: `stability-pool-common` (Task 2).
- Produces: `StabilityPoolInit` implementing our fork's `ServerModuleInit` with `type Params = ()`; env-var constants `FM_ENABLE_MODULE_SPV2_ENV`, `FM_SPV2_TEST_PARAMS_ENV`, `FM_SPV2_CYCLE_DURATION_SECS_ENV` exported from `stability_pool_server::envs`. Phase B imports all four names.

- [ ] **Step 1: See the breakage**

```bash
cd ~/projects/fedi-experimint && cargo check -p stability-pool-server 2>&1 | head -40
```

Expected: errors on `ServerModuleInit` — missing `type Params`, wrong arity on `trusted_dealer_gen`/`distributed_gen`.

- [ ] **Step 2: Add `envs.rs`**

```rust
//! Environment variables read by `fedimintd`-family binaries to configure the
//! stability pool v2 module. The constants live here so the module's
//! `get_documented_env_vars` and the binary constructing [`StabilityPoolInit`]
//! can never disagree about the names.

/// Pre-ticks the module in the setup UI (`is_enabled_by_default`).
pub const FM_ENABLE_MODULE_SPV2_ENV: &str = "FM_ENABLE_MODULE_SPV2";

/// Selects test parameters: `OracleConfig::Mock` and a 15s cycle. Read by the
/// binary when constructing [`StabilityPoolInit`], not by the module itself.
pub const FM_SPV2_TEST_PARAMS_ENV: &str = "FM_SPV2_TEST_PARAMS";

/// Overrides the cycle duration in seconds (default 600). Read by the binary
/// when constructing [`StabilityPoolInit`]. Ignored under test params.
pub const FM_SPV2_CYCLE_DURATION_SECS_ENV: &str = "FM_SPV2_CYCLE_DURATION_SECS";
```

Add `pub mod envs;` to `lib.rs`.

- [ ] **Step 3: Port the trait impl in `lib.rs`**

Add to the `impl ServerModuleInit for StabilityPoolInit` block (fedi's impl currently has no `Params` — its knobs are fields on the init struct, which stays exactly so):

```rust
    type Params = ();
```

Thread the new argument through both config-gen fns (bodies unchanged):

```rust
    fn trusted_dealer_gen(
        &self,
        peers: &[PeerId],
        _args: &ConfigGenModuleArgs,
        _params: &Self::Params,
    ) -> BTreeMap<PeerId, ServerModuleConfig> {
```

```rust
    async fn distributed_gen(
        &self,
        peers: &(dyn PeerHandleOps + Send + Sync),
        _args: &ConfigGenModuleArgs,
        _params: &Self::Params,
    ) -> anyhow::Result<ServerModuleConfig> {
```

Add the opt-in + documentation hooks (pattern copied from `fedimint-usdt-server`):

```rust
    /// Opt-in, matching the experimint house style for modules that carry
    /// consensus-relevant topology: available in the setup UI regardless, but
    /// only pre-ticked when the operator sets the env var.
    fn is_enabled_by_default(&self) -> bool {
        std::env::var(envs::FM_ENABLE_MODULE_SPV2_ENV).is_ok()
    }

    fn get_documented_env_vars(&self) -> Vec<EnvVarDoc> {
        vec![
            EnvVarDoc {
                name: envs::FM_ENABLE_MODULE_SPV2_ENV,
                description: "pre-tick the stability pool v2 module in the setup UI",
            },
            EnvVarDoc {
                name: envs::FM_SPV2_TEST_PARAMS_ENV,
                description: "use test parameters: mock price oracle and a 15s cycle",
            },
            EnvVarDoc {
                name: envs::FM_SPV2_CYCLE_DURATION_SECS_ENV,
                description: "stability pool cycle duration in seconds (default 600)",
            },
        ]
    }
```

Import `EnvVarDoc` from `fedimint_server_core` alongside the existing imports. If our fork's `fedimint_core::envs` exposes `is_env_var_set`, prefer it over `std::env::var(..).is_ok()` for the enable check — match whatever `fedimint-usdt-server` uses on the experimint side (`is_env_var_set_opt(...).unwrap_or(false)` style is fine to mirror if the helper exists in this fork's `fedimint-core`; otherwise `std::env::var(...).is_ok()`).

- [ ] **Step 4: Check, fix residual errors**

```bash
cargo check -p stability-pool-server 2>&1 | head -40
```

Fix remaining errors the compiler names (expected class: import paths, possibly `Amounts`/audit-signature drift in `fedimint-core/src/module/audit.rs`). Consensus logic stays untouched.

- [ ] **Step 5: Test and commit**

```bash
cargo test -p stability-pool-server 2>&1 | tail -5
git add -A crates/modules/stability-pool/server && git commit -m "feat(stability-pool-server): port to the experimint platform branch

type Params = (), the params-threaded config-gen signatures, and the
FM_SPV2_* env var surface. Consensus logic, encodings and DB layout are
untouched; wire compatibility with the deployed module is the invariant."
```

### Task 4: Port `stability-pool-client`

**Files:**
- Modify (as compile errors demand): `~/projects/fedi-experimint/crates/modules/stability-pool/client/src/{lib.rs,db.rs,history_service.rs,sync_service.rs}`

**Interfaces:**
- Consumes: `stability-pool-common` (Task 2).
- Produces: `StabilityPoolClientInit` implementing our fork's `ClientModuleInit`. Phase B's CLI task imports `stability_pool_client::StabilityPoolClientInit`.

- [ ] **Step 1: See the breakage**

```bash
cd ~/projects/fedi-experimint && cargo check -p stability-pool-client 2>&1 | head -60
```

Expected: small. The `fedimint-client-module` delta between the forks is almost entirely additive (`State::fmt_visualization` has a default impl; new builder/meta helpers). Likely error classes: `ClientModuleInit`/`ClientModule` associated items added on our fork, changed `TransactionBuilder` method signatures, moved imports.

- [ ] **Step 2: Fix exactly what the compiler names**

Rules: no behavioral rewrites; encoded types and DB prefixes untouched; if a required trait item is new on our fork, implement it the way `modules/amm/fedimint-amm-client` in experimint does (that crate is the reference implementation for this fork's client API).

- [ ] **Step 3: Test and commit**

```bash
cargo test -p stability-pool-client 2>&1 | tail -5
git add -A crates/modules/stability-pool/client && git commit -m "fix(stability-pool-client): adapt to the experimint platform branch"
```

### Task 5: Push the branch, capture the rev

**Interfaces:**
- Produces: `SPV2_REV` — the pushed tip sha. Every git dep added in Phase B uses it.

- [ ] **Step 1: Final check of all three crates from a clean slate**

```bash
cd ~/projects/fedi-experimint && cargo check -p stability-pool-common -p stability-pool-server -p stability-pool-client 2>&1 | tail -3
```

Expected: `Finished` with no errors.

- [ ] **Step 2: Push**

```bash
git push fork experimint   # remote 'fork' = git@github.com:elsiribot/fedi.git; from the worktree use: git push git@github.com:elsiribot/fedi.git experimint
git rev-parse HEAD         # this sha is SPV2_REV, used verbatim in Phase B
```

---

## Phase B — experimint integration

### Task 6: Workspace deps + fedimintd attach

**Files:**
- Modify: `Cargo.toml` (workspace deps), `bin/fedimintd-experimint/Cargo.toml`, `bin/fedimintd-experimint/src/lib.rs`

**Interfaces:**
- Consumes: `SPV2_REV` (Task 5); `stability_pool_server::{StabilityPoolInit, envs}`, `stability_pool_server::common::config::{OracleConfig, CollateralRatio}` (via `stability-pool-server`'s re-export of common — if there is no `common` re-export, depend on `stability-pool-common` directly and import `stability_pool_common::config::{OracleConfig, CollateralRatio}`).
- Produces: `experimint_modules()` carrying seven kinds; a `spv2_init()` helper in `fedimintd_experimint` that Task 8's docs reference.

- [ ] **Step 1: Add workspace deps**

In experimint `Cargo.toml`, after the in-workspace section, add a fedi-pin section mirroring the fedimint one's comment style:

```toml
# --- fedi (pinned to the experimint branch of elsiribot/fedi) ----------------
#
# rev <SPV2_REV> == elsiribot/fedi `experimint`: fedi master (cb458841e, spv2
# as deployed) with the fedimint deps switched to the platform branch above
# and stability-pool-{common,server,client} ported to its module API. The
# fedimint URL+rev on that branch are byte-identical to this workspace's —
# that is what keeps cargo resolving one fedimint-core, not two.
stability-pool-client = { git = "https://github.com/elsiribot/fedi", rev = "<SPV2_REV>" }
stability-pool-common = { git = "https://github.com/elsiribot/fedi", rev = "<SPV2_REV>" }
stability-pool-server = { git = "https://github.com/elsiribot/fedi", rev = "<SPV2_REV>" }
```

(`<SPV2_REV>` is the sha from Task 5 Step 2 — substitute the real value.)

In `bin/fedimintd-experimint/Cargo.toml` `[dependencies]`, add:

```toml
stability-pool-common = { workspace = true }
stability-pool-server = { workspace = true }
```

- [ ] **Step 2: Write the failing test change**

In `bin/fedimintd-experimint/src/lib.rs`, update `registry_carries_the_experimint_module_set` to expect seven kinds, sorted:

```rust
        assert_eq!(
            kinds,
            vec![
                "amm".to_string(),
                "lnv2".to_string(),
                "meta".to_string(),
                "mintv2".to_string(),
                "multi_sig_stability_pool".to_string(),
                "usdt".to_string(),
                "walletv2".to_string(),
            ],
            "unexpected module set (kinds() is sorted)"
        );
```

- [ ] **Step 3: Run it, verify it fails**

```bash
cargo test -p fedimintd-experimint registry_carries 2>&1 | tail -10
```

Expected: FAIL — left has six kinds.

- [ ] **Step 4: Implement `spv2_init()` and attach**

In `bin/fedimintd-experimint/src/lib.rs`:

```rust
use std::time::Duration;

use fedimint_core::Amount;
use stability_pool_common::config::{CollateralRatio, OracleConfig};
use stability_pool_server::StabilityPoolInit;
use stability_pool_server::envs::{FM_SPV2_CYCLE_DURATION_SECS_ENV, FM_SPV2_TEST_PARAMS_ENV};

/// The stability pool v2 init, carrying Fedi's deployed parameters verbatim
/// (fedi `crates/fedimint/fedimintd/src/main.rs`). `FM_SPV2_TEST_PARAMS`
/// switches to the mock oracle and a 15s cycle for devimint-style runs;
/// `FM_SPV2_CYCLE_DURATION_SECS` overrides the production cycle length.
#[must_use]
pub fn spv2_init() -> StabilityPoolInit {
    let test_params = std::env::var(FM_SPV2_TEST_PARAMS_ENV).is_ok();
    let cycle_duration_secs: u64 = std::env::var(FM_SPV2_CYCLE_DURATION_SECS_ENV)
        .ok()
        .map(|v| v.parse().expect("FM_SPV2_CYCLE_DURATION_SECS must be a u64"))
        .unwrap_or(600);

    StabilityPoolInit {
        oracle_config: if test_params {
            OracleConfig::Mock
        } else {
            OracleConfig::Aggregate
        },
        cycle_duration: Duration::from_secs(if test_params { 15 } else { cycle_duration_secs }),
        collateral_ratio: CollateralRatio {
            provider: 1,
            seeker: 1,
        },
        min_allowed_seek: Amount::from_msats(100_000),
        min_allowed_provide: Amount::from_msats(100_000),
        max_allowed_provide_fee_rate_ppb: 2000,
        min_allowed_cancellation_bps: 100,
    }
}
```

(If `stability-pool-server` re-exports `common`, import the config types through it instead and drop the direct `stability-pool-common` dep — match what compiles with the fewest deps.)

In `experimint_modules()`, after the local modules:

```rust
    // Fedi's stability pool v2 — the server side of multispend. Sourced from
    // the experimint branch of elsiribot/fedi; parameters in `spv2_init`.
    modules.attach(spv2_init());
```

Update the module-set docs at the top of the file (topology table gains a `multi_sig_stability_pool` row; the opt-in env var list gains `FM_ENABLE_MODULE_SPV2`).

- [ ] **Step 5: Run the tests**

```bash
cargo test -p fedimintd-experimint 2>&1 | tail -10
```

Expected: PASS, including the asset-validation tests (spv2 declares no assets, so they are unaffected).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock bin/fedimintd-experimint && git commit -m "feat(fedimintd): attach fedi's stability pool v2 (multispend)"
```

### Task 7: CLI registration

**Files:**
- Modify: `bin/fedimint-cli-experimint/Cargo.toml`, `bin/fedimint-cli-experimint/src/lib.rs`

**Interfaces:**
- Consumes: `stability_pool_client::StabilityPoolClientInit` (Task 4).

- [ ] **Step 1: Write the failing test change**

In `bin/fedimint-cli-experimint/src/lib.rs`, the tests module pins the client module set (currently six kinds). Add `multi_sig_stability_pool` in sorted position (between `mintv2` and `usdt` — check the test's actual ordering convention and match it) and update the "six kinds"/count language in the test's doc comment.

- [ ] **Step 2: Run it, verify it fails**

```bash
cargo test -p fedimint-cli-experimint registry_carries 2>&1 | tail -10
```

Expected: FAIL.

- [ ] **Step 3: Add the dep and attach**

`bin/fedimint-cli-experimint/Cargo.toml`:

```toml
stability-pool-client = { workspace = true }
```

In `experimint_modules` in `src/lib.rs`:

```rust
        // Fedi's stability pool v2 client — the server side of multispend
        // lives in fedimintd-experimint; this makes its accounts drivable
        // from `module multi_sig_stability_pool`.
        .attach_module(stability_pool_client::StabilityPoolClientInit::default())
```

(If `StabilityPoolClientInit` has no `Default`, construct it the way fedi's bridge does — check `~/projects/fedi-experimint/crates/modules/stability-pool/client/src/lib.rs:66` for its fields and use the zero-config construction; it is a plain marker struct or carries only optional knobs.)

Update the six-kinds prose in the module docs at the top of `lib.rs` and in `info.rs` if it enumerates kinds.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p fedimint-cli-experimint 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add bin/fedimint-cli-experimint Cargo.toml Cargo.lock && git commit -m "feat(cli): register the stability pool v2 client"
```

### Task 8: Docs + nix

**Files:**
- Modify: `README.md`, `docs/mainnet-deployment.md`, `flake.nix`

**Interfaces:**
- Consumes: env var names from Task 3, `spv2_init()` parameters from Task 6.

- [ ] **Step 1: README**

- Module-family table: add a row for spv2 (sourced from `elsiribot/fedi` branch `experimint`, not `modules/`).
- Intended-topology table: add `multi_sig_stability_pool` — "multisig accounts + threshold transfers (Fedi multispend); seeker/provider BTC↔fiat stabilization".
- `set-local-params` example: add `--module multi_sig_stability_pool`.
- Note the module is opt-in via `FM_ENABLE_MODULE_SPV2`.

- [ ] **Step 2: Mainnet runbook**

In `docs/mainnet-deployment.md`'s env var table add `FM_ENABLE_MODULE_SPV2`, `FM_SPV2_TEST_PARAMS`, `FM_SPV2_CYCLE_DURATION_SECS`, and a warning: with `OracleConfig::Aggregate` every guardian polls six public exchange APIs (CEX.io, Yadio, Bitstamp, Kraken, Coinbase, Gemini) over HTTPS for BTC/USD — a new outbound dependency; guardians behind egress filters must allow those hosts or price fetch degrades (module warns and retries).

- [ ] **Step 3: Nix cargoHash**

`Cargo.lock` changed, so the packaged builds' `cargoHash` in `flake.nix` is stale. Refresh it the way commit `b107513` did: set the hash to `""` (or `lib.fakeHash`), run `nix build .#fedimintd-experimint 2>&1 | grep 'got:'`, paste the reported hash, rebuild to verify. Repeat for each package output that vendors the workspace (check `flake.nix` for how many distinct `cargoHash` values exist — the CLI/price-keeper may share one).

- [ ] **Step 4: Commit**

```bash
git add README.md docs/mainnet-deployment.md flake.nix && git commit -m "docs: spv2 module in the README, runbook and nix hash"
```

### Task 9: Full verification

- [ ] **Step 1: Whole workspace**

```bash
cargo check --workspace --all-targets 2>&1 | tail -3 && cargo test --workspace 2>&1 | tail -10
```

Expected: clean check; all tests pass. Watch for the known `bdk_electrum` resolution ditch (workspace `Cargo.toml` NOTE): if `cargo` re-resolved it above 0.23.0, run `cargo update -p bdk_electrum --precise 0.23.0`.

- [ ] **Step 2: Binary smoke test**

```bash
cargo run -p fedimintd-experimint -- --help 2>&1 | grep -i "FM_SPV2\|FM_ENABLE_MODULE_SPV2"
```

Expected: the three documented env vars appear (via `get_documented_env_vars`).

- [ ] **Step 3: Final commit if anything moved**

```bash
git status --porcelain   # expect clean, else add+commit leftovers with an explanatory message
```

## Self-review notes

- Spec coverage: fork-check decision (no task — already done, recorded in spec), branch + dep switch (T1), port common/server/client (T2–4), push (T5), workspace deps + attach + params + env vars (T6), CLI (T7), tests (T6/T7 TDD steps), README/runbook/oracle note (T8), seven-kind pin (T6). Complete.
- The one deliberate non-literal element: `<SPV2_REV>` cannot exist until Task 5 pushes; Task 6 states the substitution explicitly.
- Client port (T4) is compile-driven by necessity — the delta is additive per the fork diff, and the task pins the reference implementation (`fedimint-amm-client`) and the invariants (no encoding changes) instead of guessing code.
