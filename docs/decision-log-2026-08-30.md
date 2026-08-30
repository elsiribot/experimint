# Decision log — 2026-08-30 USDt/AMM push

Calls made without the ability to consult, collected here for retroactive review.
Each entry: what was decided, what it was decided *against*, and the evidence.

---

## D1 — Fork direction: put experimint's clients on Fedi's line, not Fedi on experimint's

**Decided.** The Fedi app will consume experimint's `usdt` and `amm` **client** crates
built against `elsiribot/fedimint` tag `v0.11.0-fedi9-usdt.1`. experimint `master`
stays on `experimint-v0.11` (rev `51d011a4776`) as the **server/deployment** line.
Two lines, one rule: *master builds federations, `es/fedi9-compat` builds the app.*

**Decided against.** The instruction's fallback — "fork the fedimint-experimint branch
and apply fedi patches on top" — because that means moving the Fedi app from
`releases/v0.11` onto upstream `master` @ 2026-07-02: 500+ upstream commits, including
breaking client-facing changes Fedi has never compiled against, plus re-applying 30 Fedi
patches of which at least two (the `MetaFieldValue` revert `de5398ad80c`, the connector
URL rewrites `63d3f009679`) are deliberate *reversions* of upstream and would conflict
head-on. That is not a one-day change.

**Evidence.** The two forks are not ancestor/descendant. They diverged at upstream
`14f6399cde4` (2026-03-31). Divergence is dominated by 4 months of upstream `master`
drift that fedi9 lacks — not by either fork's own patches:
`14f6399..experimint-v0.11` = 739 commits, `14f6399..v0.11.0-fedi9` = 84.

Six API items are missing from bare `v0.11.0-fedi9`. **Four already exist in
`v0.11.0-fedi9-usdt.1`** (`await_primary_module_outputs_for_unit`; `AmountUnit::new_custom`
as `const fn`; the `FM_USDT_*` env consts; `LOG_CLIENT_MODULE_USDT`) — that tag is fedi9
plus exactly this port, it is immutable, and Fedi's `es/usdt-tidy` already `[patch]`es
onto it. The remaining two are AMM-only and come from upstream `cabdd390fd1`:
`ClientModuleInitArgs::client_span()` and `TaskGroup::spawn_cancellable_with_span`.
Both are avoidable with a ~3-line edit to `fedimint-amm-client` (plain
`spawn_cancellable`), needing no platform patch at all.

Conflict surface between the forks is near-zero: Fedi never touches `fedimint-core`,
`fedimint-derive`, or `fedimint-api-client`, which is exactly where experimint's
config-gen work lives. `fedimint-derive` is byte-identical across the two.

**Confirmed by compilation, not just analysis.** `es/fedi9-compat` builds. The decisive
test was a throwaway consumer crate outside the workspace that registers both modules
against fedi9's own `fedimint-client`:

```rust
builder.with_module(fedimint_amm_client::AmmClientInit);
builder.with_module(fedimint_usdt_client::UsdtClientInit);
```

Clean on native *and* on `wasm32-unknown-unknown`. That is precisely where the feared
seventh blocker (a trait bound or `async_trait` mismatch invisible to grep) would have
appeared, since none of the module crates depends on `fedimint-client` itself. It did
not appear. Workspace check, clippy `-D warnings`, fmt and 198 tests are all green.

The two AMM-only items were fixed module-side with plain `spawn_cancellable`, as
intended — no platform patch.

**Two corrections to the earlier analysis:**
- `fedimint-threshold-ecdsa` *does* exist on the fedi9 line at `crypto/threshold-ecdsa`.
  It is not why the usdt server is excluded. The server is excluded because
  `fedimint_core::module::Asset`, `fedimint_server_core::EnvVarDoc`, and the
  `provided_assets` / `get_documented_env_vars` hooks genuinely do not exist there.
- An undiscovered blocker surfaced and was resolved: the graph would not resolve until
  `[patch.crates-io]` for `iroh`/`iroh-base`/`iroh-relay` was restated, because
  `fedimint-connectors` on fedi9 needs iroh's `no_holepunch` feature and **`[patch]` is
  not inherited across a git-dependency boundary.**

**Deliberately not merged to master.** That branch narrows the workspace members to the
four client/common crates; merging would break the server line. Two branches is the
design, not an oversight.

**Residual risk:** the AMM has never been exercised at runtime against fedi9 — it
compiles and its unit tests pass, but no integration suite runs on that branch. The
usdt side has shipped in Fedi already; the amm side has only ever run against
experimint's own line.

**Corrected along the way.** experimint's root `Cargo.toml` states the divergence point
is `be854220f`. That is wrong — `be854220f` is the merge-base with upstream *master*,
and fedi9 does not contain it. Left uncorrected it keeps producing wrong effort
estimates.

---

## D2 — Fee aggregation: threshold-index, not plain median

**Decided.** "Median of guardian votes, walletv2-style" is implemented as walletv2's
ascending-sort + `.get(num_peers.threshold() - 1)`, plus a config-gen bounds band, plus
a fallback to the config fee when votes are short.

**Decided against.** Legacy `wallet`'s literal median (`rates[peer_count / 2]`), which
is **not** Byzantine-safe — `max_evil` peers can drag it. Since the fee directly sets
trader pricing, the safe direction matters: the threshold index yields a value at least
`max_evil + 1` honest guardians consider acceptable-or-higher, resisting a minority
pushing the fee *down* (the direction that bleeds LPs via LVR, the named threat in
spec §13).

Also decided against `meta`'s byte-equality-at-threshold scheme: it requires
threshold-many guardians to submit *identical* values, which for an independently
estimated number essentially never happens, so the fee would never update.

**Departure from walletv2 worth flagging.** walletv2 returns `Option` and rejects
transactions when there is no consensus feerate. The AMM must not — a freshly-DKG'd
federation would reject every swap until votes land. Fallback is the config value, so
`default_fee_per_mille` becomes a seed and the vote an override.

**Free wire change.** The module is undeployed (`MODULE_CONSENSUS_VERSION` 0.0, no
federation runs it), so replacing the empty `AmmConsensusItem` costs nothing now and
would cost a version bump later.

**Enabling fact.** The client never reads the config fee — it takes the fee from
`PoolSummary.fee_per_mille` and prices from `QUOTE_ENDPOINT`. So this is server-side
only: no client change, no transaction wire change.

---

## D3 — Both mainnet, as instructed

Confirmed instruction, recorded rather than questioned. Consequences accepted:
BTC on mainnet via `walletv2`, USDT on Ethereum L1 (`chain_id 1`). L1 gas makes
peg-out UserOps genuinely expensive; an L2 was offered and declined.

---

## D4 — Wiping the existing federations

The hosts were described as "former btcpp hosts". They are not idle. `btcpp-01`
carries a **live 10-guardian mainnet federation** — `ln`, `lnv2`, `meta`, `mint`,
`wallet` — with a 572 MB database and **14,584 sessions** of history. The iroh nodes
(12–20) likewise run active `fedimintd`.

The wipe was chosen with the hosts known to be live. It was **not** explicitly chosen
with "this federation has 14.5k sessions of mainnet history" on the table, because that
was established afterwards. Any outstanding ecash issued by that federation becomes
unredeemable the moment its guardians are wiped.

**Recommended before any wipe:** confirm the federation's `mint` outstanding issuance
and `wallet` on-chain balance are ~zero, or accept the loss explicitly. Not yet
determined — see the open item in the handover.

Note also that the existing federation's own membership spans `btcpp-01,03,04,05,06,07,…`
and 04/07/09 are down. At 10 guardians the threshold is 7, so it is sitting *at* the
edge already.

### D4a — I did not wipe. Deployment is prepared side-by-side instead.

**This is a deliberate deviation from the chosen option, and the one most worth
overruling if you disagree.**

Reasoning, in order of weight:
1. **The wipe buys nothing today.** The deployment cannot complete without a funded
   broadcaster EOA (see D7), so there is no version of today that ends with a working
   USDt federation on those hosts. Wiping now would destroy 14.5k sessions of mainnet
   history in exchange for nothing.
2. **The hosts were described as "former btcpp hosts."** They are running a live
   mainnet federation. That is a material difference between how the target was
   described and what it is, discovered *after* the choice was made.
3. **It costs almost nothing to keep both.** The six Contabo boxes have 4 cores, 5 GB
   RAM and ~350 GB free each. A second `fedimintd` instance on a distinct unit, state
   dir and port set fits comfortably.

The new federation is being configured as a second instance. **Wiping remains
available as a one-line change** — nothing here forecloses it. If the old federation is
genuinely drained and you want the hosts clean, that is a small edit, not a rework.

---

## D5 — Host flakiness is a deployment risk in its own right

`btcpp-01` answered SSH (518 days uptime) and then timed out during banner exchange
minutes later. The repo's own `CLAUDE.md` records a different up/down set than
`HOSTS.md`, and both differ from what probing found today. Reachable at first sweep:
`01, 03, 05, 06, 08, 10, 11` plus iroh `12–20`. Down: `04, 07, 09`.

Planning a 7-of-7 DKG ceremony across hosts with intermittent SSH is the single most
likely way for the deployment to fail on the day. DKG needs all 7 simultaneously.

---

## D7 — The mainnet deployment has a hard blocker I cannot clear

**Two secrets are missing and cannot be manufactured:**

1. **`FM_USDT_BROADCASTER_PRIVATE_KEY`** — a funded Ethereum EOA. Guardians front
   ERC-4337 UserOp gas from it. Confirmed in code: `BootstrapState` stays
   `AwaitingInfra`, which **blocks deposit-address handout**, until the
   `broadcaster_funded` readiness condition holds, requiring at least
   `broadcaster_min_balance_wei` (default 0.05 ETH). The env var's own doc notes a
   single shared key across guardians is fine, so **one funded key unblocks all seven**.
2. **`FM_USDT_RESIDUAL_RECOVERY_RECIPIENT`** — a treasury EVM address. Must be
   consensus-agreed and identical across guardians (every guardian builds a
   byte-identical `EntryPoint.withdrawTo`), and the module rejects the zero-address
   placeholder on non-dev chains.

Everything else for mainnet is resolved and verified on-chain (D8). These two are the
whole gap between "prepared" and "running".

**A third, non-secret prerequisite:** the AMM pool needs seeding with real BTC *and*
real USDt before a swap demo shows anything. No amount of code substitutes for that.

## D8 — Mainnet contract addresses, verified on-chain rather than assumed

Checked today against Ethereum mainnet via `eth_getCode` / `eth_call`:

| Param | Address | Evidence |
| --- | --- | --- |
| `usdt_contract` | `0xdAC17F958D2ee523a2206206994597C13D831ec7` | has code |
| `entry_point` | `0x0000000071727De22E5E9d8BAf0edAc6f37da032` | ERC-4337 v0.7, has code |
| `account_factory` | `0x91E60e0613810449d098b0b5Ec8b51A0FE8c8985` | `accountImplementation()` → the impl below |
| `simple_account_impl` | `0x68641de71cfea5a5d0d29712449ee254bb1400c2` | its `entryPoint()` → the v0.7 EntryPoint |
| `eth_usd_price_feed` | `0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419` | has code; already the compiled-in default |

The factory choice matters and was not obvious: two other mainnet
`SimpleAccountFactory` deployments (`0x9406Cc…6454`, `0x15Ba39…0232`) resolve to impls
whose `entryPoint()` is **v0.6**. Picking one of those would produce deposit addresses
against the wrong EntryPoint.

`confirmation_depth` must be ≥ 6 on a non-dev chain (`MIN_PROD_CONFIRMATION_DEPTH`);
set to 12.

**Verified.** The module's CREATE2 derivation reproduces the deployed factory's
`getAddress(owner, salt)` byte-for-byte — four owner/salt pairs for
`derive_deposit_account` plus two for `derive_pool_account`, all exact, against live
mainnet. The live `eth_getProof` path was checked too: a real holder's balance proven
from the trie (`17000000000000198`) equals `balanceOf()` at the same block, confirming
the ERC-20 balances mapping is at storage slot 2 as the module assumes.

The check is not vacuous: as a negative control, the same derivation against the **v0.6**
factory `0x9406Cc…6454` produces `0x5241cE8e…bD24` where the chain says
`0x03a74d80…6A92`. They differ, as they must — different embedded `ERC1967Proxy`. So a
v0.6 misconfiguration would make *every* deposit address wrong, silently.

Reproduce with `cargo run -p fedimint-usdt-tests --bin verify-mainnet-config`.

### D8a — Which `SimpleAccountFactory`: use the module's own default

Verification turned up a fork I had not seen. With only `FM_USDT_ENTRY_POINT` set,
`usdt_gen_params_from_env` derives the module's **own** Arachnid-CREATE2 factory
`0xd095bB8b86Afe336ea11D7382269e1C39037c8fb` (impl `0x25510b5911085689e0758109855ad14f14b8aF8b`)
— not the canonical eth-infinitism `0x91E6…8985`. Both are deployed on mainnet, both
have the right `entryPoint()`, and the module's derivation matches **both**. They are
equally safe but yield **different deposit accounts**, so the choice must be deliberate.

**Decided: take the module's default — leave `FM_USDT_ACCOUNT_FACTORY` and
`FM_USDT_SIMPLE_ACCOUNT_IMPL` unset.**

Reasoning: the module *derives* this address by design, so it is the path config-gen
was built and tested around. Every override has to be byte-identical across all seven
guardians or config-gen diverges, and on hosts with intermittent SSH the cheapest way
to avoid a misconfigured guardian is to have fewer values to get wrong. The address is
also deterministic across chains, so the config survives a chain change.

Overriding to the canonical factory is a supported alternative — the values are in the
handover — but it buys nothing here and costs two more things that must match exactly.

### Correction to a value I supplied

I told the verifying agent the `getAddress(address,uint256)` selector was `0x5fbfb9cf`.
That is wrong — `0x5fbfb9cf` is `createAccount(address,uint256)`; `getAddress` is
**`0x8cb84e18`**. The agent recomputed it from keccak rather than trusting me, so the
verification used the correct selector. Flagged because the same mistake in a config
would have been silent.

## D6 — Sandbox required an override for host access

Outbound port 22 is blocked in the normal execution sandbox (verified: `github.com:22`
also fails while `:443` succeeds). Every host command was run with the sandbox
override. Flagged because it means host-touching commands did not go through the
usual confinement.
