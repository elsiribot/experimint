# `es/fedi9-compat` — the client line

`master` builds experimint against the **experimint platform branch** of
fedimint. The Fedi app builds against **Fedi's** fedimint. Those are two
different API generations, and neither side can adopt the other's wholesale.
This branch resolves that by building only the crates the app actually needs —
the module **clients** — against Fedi's line.

**The rule.** `master` is the server/deployment line: it is what guardians run,
what `bin/fedimintd-experimint` links, and where the consensus code, the
integration suites and the EVM e2e suites live. `es/fedi9-compat` is the client
line: it exists so the Fedi app can depend on `fedimint-{usdt,amm}-{client,common}`.
Module logic changes land on `master` first and are carried here; nothing
lands here that `master` does not also carry.

## Fork topology

The two fedimint lines are **siblings**, not one ahead of the other:

```
                        upstream fedimint master
                                 |
      14f6399cde4 (2026-03-31) --+--  common ancestor of both lines
         |                       |
         |  +739 commits         |  +171 commits
         v                       v
  elsiribot/fedimint       elsiribot/fedimint
  experimint-v0.11         v0.11.0-fedi9-usdt.1
  rev 51d011a4776          commit 32aa628c102
  workspace 0.12.0-alpha   workspace 0.11.0-rc.1
         ^                       ^
         |                       |
   master pins this        this branch pins this
                           (and so does the Fedi app,
                            via its own [patch] table)
```

`v0.11.0-fedi9-usdt.1` is `fedibtc/fedimint`'s `v0.11.0-fedi9` plus the usdt
module and four platform items (below). Fedi's `es/usdt-tidy` `[patch]`es its
entire fedimint tree onto that same tag, which is what makes it the meeting
point: an app consuming these crates resolves **one** `fedimint-core`, not two.

Verify the topology yourself in a checkout of `elsiribot/fedimint`:

```
$ git merge-base 51d011a4776 v0.11.0-fedi9-usdt.1
14f6399cde44827454fbc3d9d7f314d7a8678f63
$ git rev-list --count 14f6399cde4..51d011a4776    # experimint side
739
$ git rev-list --count 14f6399cde4..v0.11.0-fedi9-usdt.1
171
```

`master`'s root `Cargo.toml` used to name `be854220f` as the divergence point.
That was wrong: `be854220f` (2026-07-02) is on the experimint side of the split
and is **not** an ancestor of fedi9 (`git merge-base --is-ancestor be854220f
v0.11.0-fedi9-usdt.1` fails). It is the merge-base with *upstream master*, a
different question. The comment is corrected on this branch.

## Why the direction is client-side

Moving Fedi onto the experimint line means moving it across 739 commits of
upstream fedimint. Moving experimint's clients onto Fedi's line is a six-symbol
problem, four of which are already solved in the pinned tag. That asymmetry is
the whole argument.

## The six API items

Six symbols the client crates use are absent from bare `v0.11.0-fedi9`.

Four are **present in `v0.11.0-fedi9-usdt.1`**, so nothing here has to work
around them:

| Item | Used by |
| --- | --- |
| `Client::await_primary_module_outputs_for_unit` | `usdt-client/src/lib.rs` |
| `AmountUnit::new_custom` as a `const fn` | `pub const USDT_UNIT` in `usdt-common/src/lib.rs` |
| `FM_USDT_*` env consts | `usdt-common/src/lib.rs` |
| `LOG_CLIENT_MODULE_USDT` | `usdt-client/src/{evm,states}.rs` |

Two are **AMM-only** and absent, both introduced upstream in `cabdd390fd1`,
which fedi9 predates. They are fixed on the module side rather than by touching
the platform:

| Item | Was used at | Now |
| --- | --- | --- |
| `ClientModuleInitArgs::client_span()` | `amm-client/src/lib.rs` `init` | dropped, along with the `AmmClientModule::client_span` field |
| `TaskGroup::spawn_cancellable_with_span` | `amm-client/src/lib.rs` `start` | `TaskGroup::spawn_cancellable` |

The only behavioural consequence: the AMM's recovered-balance claim sweep roots
its own tracing tree instead of nesting under the client's span. Its events
still carry the task name. Nothing in the sweep's logic depends on the span.

## What differs from `master`

Six files, of which two (this document and a banner at the top of the README)
are prose.

**`Cargo.toml`**

- Every fedimint dependency moves from `rev = "51d011a4776"` to
  `tag = "v0.11.0-fedi9-usdt.1"`. All of them together — a mixed pin resolves
  the crates as distinct source packages and the trait impls stop lining up.
- Workspace `members` shrinks to the four client/common crates; the rest move
  to `exclude` (see below).
- A `[patch.crates-io]` table for `iroh`/`iroh-base`/`iroh-relay`. `[patch]` is
  not inherited from a git dependency, and `fedimint-connectors` on Fedi's line
  needs `iroh`'s `no_holepunch` feature, which only the fork has. Without it the
  graph does not resolve at all. This table matters only when experimint is the
  workspace root; a downstream consumer needs its own (the Fedi app has one,
  pointing at a different iroh fork, and that one wins there).
- `[profile.dev.package]` drops the cggmp21 threshold-ECDSA entries and
  `librocksdb-sys`. Those crates reach the graph only via
  `fedimint-usdt-server`, and cargo warns on every build for a profile spec that
  matches nothing.
- The divergence-point comment is corrected.

**`Cargo.lock`** — re-resolved (−4741/+426 lines). The gateway/LDK/rocksdb
subtree disappears with the excluded crates.

**`modules/amm/fedimint-amm-client/src/lib.rs`** — the two span call sites above.

**`.github/workflows/ci.yml`** — the `bin` lane is dropped (nothing to link
here) and the `test` lane is unit-tests-only. The `wasm` lane is unchanged and
is the lane that matters on this branch.

No other module source changes. In particular `usdt-client`, `usdt-common` and
`amm-common` compile against Fedi's line **unmodified**.

## Excluded crates

| Crate | Why |
| --- | --- |
| `fedimint-usdt-server` | Does not compile against fedi9. Needs `fedimint_core::module::Asset`, `fedimint_server_core::EnvVarDoc`, and the `ServerModuleInit::{provided_assets, get_documented_env_vars}` trait methods — none exist on that line. |
| `fedimint-amm-server` | Compiles against fedi9 today, but is excluded anyway: it is deployment-side code, it can never be assembled into a `fedimintd` on this branch, and keeping it a member would impose a two-line compatibility constraint on consensus code for no benefit. |
| `bin/fedimintd-experimint` | Needs both servers plus fedi9-absent `fedimintd` setup surface. |
| `fedimint-{usdt,amm}-tests` | Need `fedimint-testing`/`devimint` and the server crates. |

`fedimint-threshold-ecdsa` **does** exist on the fedi9 line (`crypto/threshold-ecdsa`,
same package name), so it is not the reason the usdt server is out — the
`ServerModuleInit` surface is.

## Verification

All commands from the worktree root, inside `nix develop --accept-flake-config`.

```bash
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
cargo test --workspace --locked

# The gate that actually decides whether the mobile app can ship these:
cargo check --locked --target wasm32-unknown-unknown \
    -p fedimint-amm-common -p fedimint-amm-client \
    -p fedimint-usdt-common -p fedimint-usdt-client
```

All pass, with no warnings. 198 unit tests, 0 failures.

### The consumer probe

Checking the crates in isolation does not prove an app can *register* them:
`ClientBuilder::with_module` is where a mismatched `ClientModuleInit` bound, a
`DynClientConfig` shape or an `async_trait` vs `async_trait_maybe_send!` split
would surface, and none of those crates depends on `fedimint-client`. So that
step is proved separately, with a throwaway crate outside this workspace:

```toml
# Cargo.toml — its own workspace, path deps into this branch
fedimint-amm-client  = { path = ".../modules/amm/fedimint-amm-client" }
fedimint-usdt-client = { path = ".../modules/usdt/fedimint-usdt-client" }
fedimint-client = { git = "https://github.com/elsiribot/fedimint", tag = "v0.11.0-fedi9-usdt.1" }
# plus the same [patch.crates-io] iroh rows
```

```rust
pub fn register(builder: &mut fedimint_client::ClientBuilder) {
    builder.with_module(fedimint_amm_client::AmmClientInit);
    builder.with_module(fedimint_usdt_client::UsdtClientInit);
}
```

`cargo check` and `cargo check --target wasm32-unknown-unknown` both pass on
that crate. That is the actual answer to "can the Fedi app consume these".

## Encoding compatibility

The consensus encoding is unaffected by the pin move, so a record encoded by a
client on this branch decodes on a `master`-built server:

```
$ git diff --stat v0.11.0-fedi9-usdt.1 51d011a4776 -- fedimint-derive/src/
                                     # empty: byte-identical
$ git diff --stat v0.11.0-fedi9-usdt.1 51d011a4776 -- fedimint-core/src/encoding/
 fedimint-core/src/encoding/mod.rs | 32 ++++++++++++--------------
```

and all 32 of those lines are inside `#[cfg(test)]` — test structs hoisted out
of their test fns. No wire behaviour differs.

One latent trap to keep in mind. `fedimint_core::module::Amounts` derives
`Encodable`/`Decodable` on the experimint line but **not** on fedi9. Every
current use in the client/common crates is an in-memory `TransactionItemAmounts`
value, never a field of an encoded record, which is why this branch builds. The
day someone puts an `Amounts` inside a `#[derive(Encodable)]` type in a client
or common crate, this branch stops compiling — and that is the desired outcome,
because such a record would not round-trip on Fedi's line at all.

## Keeping the branch alive

- Rebase onto `master` rather than merging; the diff is meant to stay this
  small.
- Bump the fedimint tag only in lockstep with Fedi's `[patch]` table. A tag
  Fedi does not also pin defeats the purpose.
- After any rebase or bump, the wasm gate above is the check that must pass.
- Fedi consumes these crates by **replacing** its
  `fedimint-usdt-{client,common}` dependency rows — which today point at the
  fork's vendored snapshot under `modules/fedimint-usdt-*` in the tag — with
  rows pointing at this branch. Depending on both at once would put two
  same-named, type-incompatible packages in one graph.
