# SPv2 (multispend) for fedimintd-experimint — design

Date: 2026-09-02
Status: approved

## Goal

Give `fedimintd-experimint` the server-side half of Fedi's "multispend"
feature: the **stability pool v2 module** (`KIND = "multi_sig_stability_pool"`,
`ModuleConsensusVersion(2, 2)`), exactly as deployed by Fedi today.

Multispend's group coordination (invitations, votes, withdrawal requests) is
Matrix-event-based and lives entirely in Fedi's client bridge
(`crates/multispend`, msgtype `xyz.fedi.multispend`); it has **no fedimintd
component** and is out of scope. What the federation itself provides — and what
this design adds — is spv2's multisig `Account` (pubkey set + threshold) and
threshold-validated `SignedTransferRequest`s, plus the full seeker/provider
stabilization machinery that shares the module.

## Fork situation (checked, 2026-09-02)

- experimint pins `elsiribot/fedimint` rev `51d011a` (0.12.0-alpha).
- Fedi pins `fedibtc/fedimint` tag `v0.11.0-fedi10` (`476bdf9`).
- Merge-base: `14f6399`. Of the 86 fedi-fork commits since it, 53 are
  patch-equivalent to upstream commits our branch already carries. The 33
  remaining are client-side fixes, devimint tweaks, gateway hacks, CI/version
  bumps, and iroh workarounds (contrary to our websocket federation). **None
  touch `fedimintd`, `fedimint-server`, or any `*-server` module.**

Decision: keep `elsiribot/fedimint @ 51d011a` unchanged; no rebase.

The relevant API delta our fork has over fedi's (`ServerModule` itself is
untouched; `AmountUnit` exists on both sides):

- `ServerModuleInit` gained `type Params` (typed per-instance config-gen
  params) and a `params: &Self::Params` argument on `trusted_dealer_gen` /
  `distributed_gen`; new defaulted hooks `provided_assets`, `required_assets`,
  `asset_param_field`, `get_documented_env_vars`, `default_config_gen_params`,
  `parse_params`.
- `fedimint-client-module` deltas in `module/init.rs`, `module/mod.rs`,
  `transaction/builder.rs`, `sm/state.rs`, `transaction/sm.rs`.

Shared-crypto versions unify: `secp256k1 0.29.0`, `bitcoin 0.32.x`,
`rand 0.8` on both sides.

## Approach: git dependency on an `elsiribot/fedi` branch

Not vendoring. The module source stays in the fedi tree, on a branch of the
existing `elsiribot/fedi` fork, so future rebases onto fedi upstream stay
cheap.

### Repo 1 — `elsiribot/fedi`, branch `experimint`

Branched off current fedi master (`cb458841e`, spv2 as deployed today).

1. Switch the workspace `fedimint-*` deps from
   `git = "https://github.com/fedibtc/fedimint", tag = "v0.11.0-fedi10"` to
   `git = "https://github.com/elsiribot/fedimint", rev = "51d011a47769c91aabe2ed6f1f62e91e53c50283"`
   — URL and rev **byte-identical** to experimint's entries, otherwise cargo
   treats them as distinct sources and `fedimint-core` splits in two.
2. Adapt `crates/modules/stability-pool/{common,server,client}` to the
   0.12-alpha API: `type Params = ()` on `StabilityPoolInit` (its config comes
   from the init struct's own fields), thread the `params` argument through
   `trusted_dealer_gen`/`distributed_gen`, adapt the client to the
   `fedimint-client-module` deltas. Consensus logic, `KIND`, encodings, DB
   layout and `CONSENSUS_VERSION` stay byte-identical — wire/DB compatibility
   with Fedi's deployment is a hard requirement.
3. Other workspace members (bridge, matrix, ffi, multispend) are allowed to
   not build on this branch. Cargo only resolves the packages experimint
   depends on; keeping the diff minimal beats keeping the whole workspace
   green.
4. Push the branch to `elsiribot/fedi`.

### Repo 2 — experimint

5. Add `stability-pool-common`, `stability-pool-server`,
   `stability-pool-client` as `{ git = "https://github.com/elsiribot/fedi", rev = <pinned> }`
   workspace deps (rev-pinned, matching house style for the fedimint pin).
6. Attach in `experimint_modules()` with Fedi's deployed parameters, verbatim
   from their `fedimintd/src/main.rs`:
   - `oracle_config: OracleConfig::Aggregate`
   - `cycle_duration: 600s`
   - `collateral_ratio: { provider: 1, seeker: 1 }`
   - `min_allowed_seek: 100_000 msat`, `min_allowed_provide: 100_000 msat`
   - `max_allowed_provide_fee_rate_ppb: 2000`
   - `min_allowed_cancellation_bps: 100`
   Opt-in like `usdt`/`mintv2`/`walletv2`: `is_enabled_by_default()` false,
   pre-ticked via `FM_ENABLE_MODULE_SPV2`; a test-params env var
   (`FM_SPV2_TEST_PARAMS`-style) selects `OracleConfig::Mock` + 15s cycle,
   mirroring Fedi; a cycle-duration override env var mirrors
   `FEDI_STABILITY_POOL_V2_CYCLE_DURATION_SECS`.
7. Register `stability-pool-client` in `fedimint-cli-experimint` so the CLI
   can drive accounts, consistent with `amm`/`usdt`.
8. Tests: module-set test expects seven kinds
   (`amm, lnv2, meta, multi_sig_stability_pool, mintv2, usdt, walletv2` —
   sorted); coexistence test covers the new kind.
9. Docs: README module table + topology, mainnet runbook gains the new env
   vars and a note that `OracleConfig::Aggregate` makes every guardian poll
   six public exchange APIs (Kraken, Coinbase, Gemini, Bitstamp, CEX.io,
   Yadio) for BTC/USD — a new outbound network dependency.

## Explicitly out of scope

- `stability-pool-old` (v1) — separate `KIND`, kept by Fedi only for legacy
  federations; multispend is v2-only.
- Fedi's spv2 `tests` crate — devimint-based, pulls Fedi's `devfed`; porting
  it is a bigger job than the module. The module's own unit tests (incl.
  proptests in `common`) come along on the branch.
- The Matrix multispend layer (`crates/multispend`, rpc-types, bridge).

## Risks

- **Two-fork drift**: the `experimint` branch of fedi must track two upstreams
  (fedi master for module fixes, our fedimint pin for API). Accepted; the
  git-dep approach is what keeps rebases tractable.
- **Client-module delta size**: the client port is the least-scoped part
  (~1.8k LOC lib + services against a changed client API). If
  `stability-pool-client` turns out to need deep surgery, the server module
  lands first and the CLI wiring follows.
- **Oracle availability**: Aggregate oracle needs outbound HTTPS from
  guardians; failure degrades price fetch (module tolerates transient
  failures by design — it warns and retries).
