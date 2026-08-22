# `fedimint-amm` — Module Specification

**Status:** Draft v0.1 · **Target:** Fedimint ≥ v0.11 with multi-unit core (`AmountUnit` / `Amounts`)
**Prerequisite modules:** one `mintv2` instance per tradable unit

---

## 1. Executive summary

`fedimint-amm` is a Fedimint server/client module that provides constant-product automated market makers over the `AmountUnit`s a federation issues. A single module instance hosts **many pools, one per unit pair**, created permissionlessly by whoever supplies the first liquidity. It gives a federation continuous, deterministic, operator-free pricing between its assets — for example Bitcoin msats and a bridged USD unit — without requiring any member to run a market-making bot.

**Why one instance can host many pools.** `mintv2` carries one `amount_unit` per instance because `MintConfigPrivate` holds a DKG'd `tbs_sks` keyset per denomination — adding an asset means a new key ceremony. That constraint is a keygen artifact and does not generalise. `AmmConfigPrivate` is empty; this module holds no key material, so pools are ordinary database records and adding a pair costs nothing but a first deposit.

**Why an AMM rather than the existing `fedimint-swap` order book.** Guardians are passive consensus infrastructure; they do not quote. An escrow order book only works when some member actively posts and refreshes offers. A bonding curve quotes continuously with nobody on duty, which matches the operational reality of a small community federation.

**The core design decision.** Fedimint processes all transaction inputs before any output, and neither hook can see its siblings. A swap therefore cannot be expressed as a paired module input + module output. Instead, the entire swap is a **single module input** that declares the outgoing leg in `TransactionItemAmounts::amounts` and the incoming leg in `TransactionItemAmounts::fees`. Because the funding verifier checks each unit independently and treats `fees` purely as a coverage requirement, this balances correctly, evaluates the curve exactly once, and is atomic — a failed transaction rolls the whole database transaction back.

**Solvency.** The module reports its reserves to `Audit` as a liability, mirroring `fedimint-swap`. Because every nominal quantity the module absorbs is one a `mintv2` instance simultaneously stops owing, the federation's collapsed `net_assets()` sum is conserved at zero across all module operations. Bearer LP shares require one extra receivable line to preserve this.

**Phasing.** Phase 1 ships the pool registry with account-based LP positions and sequential pricing. Phase 2 adds bearer LP shares via a per-pool `mintv2` unit. Phase 3 optionally adds two-phase uniform-price batch clearing.

**Principal risk.** Not the swap math — the audit reporting. `net_assets() >= 0` is an `assert!` evaluated after the engine's commit point, summed globally with zero slack, so a one-msat over-reported liability panics every guardian simultaneously. Correct rounding direction is load-bearing for federation liveness, not just for pool accounting.

---

## 2. Scope

### In scope (Phase 1)

- Many pools per module instance, one per unordered unit pair, keyed by `PoolId`.
- Permissionless pool creation over any two units in a DKG-fixed allowlist.
- Constant-product invariant with a per-pool configurable proportional fee.
- Atomic single-transaction swaps in both directions.
- Liquidity provision and withdrawal with account-based (pubkey-keyed) share tracking.
- Slippage protection (`min_out`) enforced server-side.
- Solvency reporting via `ServerModule::audit`.
- Client module with quote API and swap/deposit/withdraw operations.

### Out of scope

- Adding new *units* after DKG. A unit only exists if a `mintv2` instance issues it, and that instance needs its own DKG keyset. New pools over existing units are permissionless; new units are not.
- Concentrated liquidity and dynamic fees.
- Multi-hop routing on the server. Several pools in one instance make A→B→C natural, but routing stays a client concern in Phase 1; each hop is its own input.
- Any external price oracle. Pricing is endogenous; no consensus item carries a price.
- Cross-federation swaps.

### Non-goals, stated explicitly

This module is **not trust-minimized relative to the federation**. Reserves are federation-held funds. The guarantee it provides is that pricing is deterministic, auditable, and not subject to guardian discretion — a governance property, not a custody one. Documentation must not imply otherwise.

---

## 3. Platform mechanics this design depends on

Each of these was verified against the tree; implementers should re-verify against their pinned version.

| Mechanic | Location | Consequence for this module |
| --- | --- | --- |
| `AmountUnit(u64)`, id `0` reserved for native BTC | `fedimint-core/src/module/mod.rs` | Units are opaque ids; the module stores two of them in consensus config |
| `Amounts(BTreeMap<AmountUnit, Amount>)` | same | Inputs/outputs declare per-unit vectors, not scalars |
| `TransactionItemAmounts { amounts, fees }` | same | Two independent channels per item — the basis of the single-input swap |
| Per-unit funding check | `fedimint-server/src/consensus/transaction.rs`, `FundingVerifier::verify_funding` | BTC inputs cannot fund a USD output; each unit must balance on its own |
| Overpay permitted from `CoreConsensusVersion 2.1` | same | Surplus input value in a unit is forfeited, not refunded |
| `fees` is coverage-only | only referenced inside `FundingVerifier` | Nothing collects or routes fees; a module declaring a fee is solely responsible for recording where that value went |
| Inputs processed before outputs; no sibling visibility | `process_transaction_with_dbtx` | Paired input+output swaps are unsound |
| Whole consensus item in one dbtx, `ignore_uncommitted()`, early return on error | `fedimint-server/src/consensus/engine.rs` | Transactions are all-or-nothing across module writes |
| `assert!(audit.net_assets().milli_sat >= 0)` after every item, past the commit point | same | Audit misreporting is a federation-wide panic, not a rejection |
| `mintv2::audit` reports issuance as negative, unit-blind | `modules/fedimint-mintv2-server/src/lib.rs` | Audit collapses all units into one msat-typed scalar |
| Median consensus timestamp | `modules/fedimint-swap-server/src/lib.rs` | Reusable precedent for deadlines; adopt rather than reinvent |

---

## 4. Crate layout

Fork the structure of `fedimint-swap-*`, not `fedimint-dummy-*` (the dummy module's server-side consensus logic was removed in v0.11 and it is now a pure testing mock).

```
modules/
  fedimint-amm-common/     types, config, errors, curve math (no I/O)
  fedimint-amm-server/     ServerModule impl, DB schema, audit, API
  fedimint-amm-client/     ClientModule impl, state machines, quote helpers
  fedimint-amm-tests/      integration tests against devimint
```

All curve arithmetic lives in `common` as pure functions so client and server compute identical quotes from the same code path.

---

## 5. Configuration

```rust
pub struct AmmConfigConsensus {
    /// Units this federation permits trading, with per-unit dust thresholds.
    /// A unit is only reachable if some `mintv2` instance issues it; that is a
    /// setup requirement, not checkable from inside this module.
    pub units: BTreeMap<AmountUnit, UnitParams>,
    /// Applied to any pool without an explicit override.
    pub default_fee_ppm: u32,
    /// Per-pair fee overrides, e.g. a tighter fee for a stable pair.
    pub fee_overrides: BTreeMap<PoolId, u32>,
    /// Shares permanently burned on pool creation (first-depositor defence).
    pub minimum_liquidity: u64,
}

pub struct UnitParams {
    /// Minimum accepted swap input in this unit. Anti-dust and anti-spam;
    /// a DoS control, not a privacy control (§13.1).
    pub min_swap_in: Amount,
}

/// Canonical, unordered identifier for a pool. `AmountUnit: Ord`, so the pair
/// is stored sorted and (A,B) and (B,A) resolve to the same pool.
#[derive(Encodable, Decodable, Ord, PartialOrd, Eq, PartialEq, Clone, Copy)]
pub struct PoolId { lo: AmountUnit, hi: AmountUnit }

impl PoolId {
    pub fn new(x: AmountUnit, y: AmountUnit) -> Option<Self> {
        match x.cmp(&y) {
            Ordering::Equal => None,
            Ordering::Less => Some(Self { lo: x, hi: y }),
            Ordering::Greater => Some(Self { lo: y, hi: x }),
        }
    }
}

pub struct AmmClientConfig {
    pub units: BTreeMap<AmountUnit, UnitParams>,
    pub default_fee_ppm: u32,
    pub fee_overrides: BTreeMap<PoolId, u32>,
}

pub struct AmmConfigPrivate; // empty — this module holds no key material
```

**Validation at DKG.** `units` non-empty; every `fee_ppm < 1_000_000`; `minimum_liquidity >= 1_000`; every `min_swap_in` non-zero; every `PoolId` in `fee_overrides` has both units in `units`. The federation must additionally have a `mintv2` instance for each listed unit, which the module cannot verify — surface it as a setup checklist item in the guardian UI.

**Pool creation is permissionless within the allowlist.** There is no `CreatePool` output. A `DepositV0` naming a `PoolId` with no existing record creates it, subject to `minimum_liquidity`. Because those shares are never assigned to any `LpPosition`, they are unwithdrawable, so creating a pool costs the creator a permanent deposit on both sides — a natural anti-spam bond. `total_shares` never returns to zero, so pool records are created once and never deleted.

**Adding units** still requires a `mintv2` DKG for the new unit plus an `AmmConfigConsensus` change, i.e. a coordinated federation upgrade. Adding *pairs* over existing units requires nothing.

**Consensus version** starts at `0.0`. Any change to the encoded shape of an input, output, consensus item, or stored DB record requires a bump plus a `get_database_migrations` entry.

---

## 6. Database schema

```rust
#[repr(u8)]
pub enum DbKeyPrefix {
    Pool        = 0x01,  // PoolId              -> Pool
    LpPosition  = 0x02,  // (PoolId, PublicKey) -> u64 (shares)
    ConsensusTs = 0x03,  // PeerId              -> u64 (per-guardian timestamp votes)
}

pub struct Pool {
    /// Reserve held in PoolId::lo's unit.
    pub reserve_lo: Amount,
    /// Reserve held in PoolId::hi's unit.
    pub reserve_hi: Amount,
    /// Includes the unassigned `minimum_liquidity`.
    pub total_shares: u64,
}
```

One record per pool, read-modify-written on every operation touching that pool. Pools are independent, so operations on different pairs never contend — a useful property once several pairs are live.

`LpPosition` is keyed by the pair `(PoolId, PublicKey)` so a member's positions across pools enumerate under a `PoolId` prefix scan, which `audit` and the API both need.

Shares are plain `u64` and are **not** denominated in any `AmountUnit` in Phase 1.

---

## 7. Transaction types

### 7.1 Inputs

```rust
pub enum AmmInput {
    /// Complete swap in one item. `amounts` carries the outgoing leg,
    /// `fees` carries the incoming leg.
    SwapV0 {
        /// Unit the trader supplies. Pool is derived: PoolId::new(unit_in, unit_out).
        unit_in: AmountUnit,
        /// Unit the trader receives. MUST differ from unit_in.
        unit_out: AmountUnit,
        /// Exact quantity supplied, in unit_in. MUST be >= its min_swap_in.
        amount_in: Amount,
        /// Minimum acceptable output, in unit_out. Enforced server-side.
        min_out: Amount,
        /// Key the transaction must be signed by.
        trader_pk: PublicKey,
    },
    /// Burn LP shares and withdraw both sides pro rata.
    WithdrawV0 {
        pool: PoolId,
        shares: u64,
        owner_pk: PublicKey,
    },
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}
```

### 7.2 Outputs

```rust
pub enum AmmOutput {
    /// Deposit both sides and be credited LP shares.
    DepositV0 {
        /// Creates the pool if it does not exist, subject to minimum_liquidity.
        pool: PoolId,
        amount_lo: Amount,
        amount_hi: Amount,
        /// Minimum shares accepted; guards against a concurrent ratio move.
        min_shares: u64,
        owner_pk: PublicKey,
    },
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}
```

### 7.3 Resulting transaction shapes

**Swap, A → B** (e.g. BTC in, USD out):

| Item | `amounts` | `fees` |
| --- | --- | --- |
| `mintv2-A` input (notes) | `{A: dx}` | `{}` |
| `amm` input `SwapV0` | `{B: dy}` | `{A: dx}` |
| `mintv2-B` output (notes) | `{B: dy}` | `{}` |

Per unit: A — inputs `dx`, outputs+fees `dx`. B — inputs `dy`, outputs+fees `dy`. Balanced. Signed by `trader_pk`.

**Deposit:**

| Item | `amounts` | `fees` |
| --- | --- | --- |
| `mintv2-A` input | `{A: da}` | `{}` |
| `mintv2-B` input | `{B: db}` | `{}` |
| `amm` output `DepositV0` | `{A: da, B: db}` | `{}` |

**Withdraw:** `amm` input `WithdrawV0` declares `amounts: {A: da, B: db}`, funding two `mintv2` outputs. Signed by `owner_pk`.

### 7.4 Note on the `fees` channel

Using `fees` as a consume channel is mechanically sound — nothing in core collects or routes it — but it is a semantic stretch. Two consequences to accept knowingly:

- `TransactionError::UnbalancedTransaction` will report a swap's input leg under its `fee` field, which is confusing during debugging.
- If core ever begins routing declared fees to guardians or a treasury, this module silently loses its incoming leg.

**Action:** raise this with upstream before depending on it in production, and consider proposing an explicit `consumes` channel on `TransactionItemAmounts`.

---

## 8. Curve and arithmetic

All quantities are `u64` base units (msats for BTC-typed units). All intermediates are `u128`. **No floating point anywhere.**

### 8.1 Swap

```
fn amount_out(reserve_in, reserve_out, amount_in, fee_ppm) -> Option<u64>:
    require amount_in > 0, reserve_in > 0, reserve_out > 0
    in_eff   = (amount_in as u128) * (1_000_000 - fee_ppm) / 1_000_000   // floor
    require in_eff > 0
    numer    = (reserve_out as u128) * in_eff
    denom    = (reserve_in as u128) + in_eff
    out      = numer / denom                                             // floor
    require out < reserve_out                                            // never drain
    u64::try_from(out).ok()
```

State update: `reserve_in += amount_in` (the **full** amount, fee included), `reserve_out -= out`. The fee therefore accrues to the pool by raising `k`, and is never transferred anywhere.

Every division floors, which is always in the pool's favour. This is simultaneously the correct AMM rule and the safe audit rule — see §10.

**Post-condition, asserted:** `reserve_in_new * reserve_out_new >= reserve_in_old * reserve_out_old` computed in `u128`. On violation, return an error; do not settle. This is the backstop for any arithmetic mistake above it.

### 8.2 Deposit

```
if total_shares == 0:
    minted = isqrt(da as u128 * db as u128)                    // floor
    require minted > minimum_liquidity
    total_shares = minted
    minted_to_owner = minted - minimum_liquidity               // remainder unassigned forever
else:
    minted = min( da as u128 * total_shares / reserve_a,
                  db as u128 * total_shares / reserve_b )      // floor
    require minted > 0
    total_shares += minted
    minted_to_owner = minted
require minted_to_owner >= min_shares
reserve_a += da; reserve_b += db
```

The `min()` is what forces deposits at the current ratio; any excess on one side is a donation to existing LPs. Document this in the client so users are not surprised.

`minimum_liquidity` shares are credited to no `LpPosition` record and are unwithdrawable, which prevents the first-depositor share-price inflation attack. Because they are never assigned, they must still be counted in `total_shares` for audit consistency (§10).

### 8.3 Withdraw

```
require 0 < shares <= lp_position[owner]
da = reserve_a as u128 * shares / total_shares                 // floor
db = reserve_b as u128 * shares / total_shares                 // floor
lp_position[owner] -= shares
total_shares -= shares
reserve_a -= da; reserve_b -= db
if lp_position[owner] == 0: delete record
```

Flooring both payouts means dust remains with the pool. Correct direction.

---

## 9. Consensus items

```rust
pub enum AmmConsensusItem {
    /// Guardian's wall-clock seconds. Median-aggregated, monotonic.
    Timestamp(u64),
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}
```

Phase 1 uses this only to timestamp events for the client and to support future deadline logic. Reuse the aggregation approach already implemented in `fedimint-swap-server` rather than writing a new one: take the median across peers, clamp to be non-decreasing.

**No price consensus item exists and none should be added.** Pricing is endogenous. Introducing an oracle would create a manipulation surface that this design otherwise does not have.

---

## 10. Audit and solvency

### 10.1 The invariant to preserve

`Audit` collapses every `AmountUnit` into one `i64 milli_sat` field and `calculate_net_assets` sums them. This is safe here because the module only ever *relocates* nominal quantity between itself and `mintv2` instances — it never originates any. Every quantity therefore appears twice with opposite sign and the global sum is conserved.

Swap A → B:

| | Δ net |
| --- | --- |
| `mintv2-A` burns `dx` | `+dx` |
| `mintv2-B` issues `dy` | `−dy` |
| `amm` reserves as liability | `−dx + dy` |
| **total** | **0** |

### 10.2 Phase 1 implementation

Reserves are a liability. Report each side as its own `AuditItem` so no single item mixes units:

```rust
async fn audit(&self, dbtx: &mut DatabaseTransaction<'_>,
               audit: &mut Audit, module_instance_id: ModuleInstanceId) {
    // Two passes over every Pool record, one per unit, so that no single
    // AuditItem ever mixes units.
    audit.add_items(dbtx, module_instance_id, &PoolPrefix,
        |_k, p| -i64::try_from(p.reserve_lo.msats).unwrap_or(i64::MAX)).await;
    audit.add_items(dbtx, module_instance_id, &PoolPrefix,
        |_k, p| -i64::try_from(p.reserve_hi.msats).unwrap_or(i64::MAX)).await;
    // LpPosition records are NOT reported: shares are internal bookkeeping
    // against reserves already counted above. Reporting them would double-count.
}
```

Account-based shares report nothing. This is the main reason Phase 1 uses them.

### 10.3 Phase 2 — bearer LP shares

If shares become a third `AmountUnit` issued by a `mintv2` instance, that instance reports `−S` and nothing offsets it. The module must then additionally report **outstanding shares as a positive receivable**:

```rust
audit.add_items(dbtx, module_instance_id, &PoolPrefix,
    |_k, p| i64::try_from(p.total_shares).unwrap_or(i64::MAX)).await;
```

Counterintuitive — the module posts a positive for a claim against itself — but it is forced by conservation, and `S` cancels exactly regardless of magnitude. `minimum_liquidity` must be included in the reported `total_shares`, since the corresponding notes were issued.

### 10.4 Why this is the highest-severity area

`assert!(audit.net_assets()... >= 0)` runs after **every** consensus item, across all modules, positioned after the engine's `warn_uncommitted()` / `AcceptedItemKey` insertion — i.e. past the point of no return. It panics rather than rejecting. It is a global sum against zero with no slack.

Therefore: a one-msat over-reported liability from this module halts the entire federation. Every rounding decision in §8 must floor, and §11's tests must cover this explicitly.

---

## 11. Determinism requirements

Divergence between guardians is a liveness failure — Fedimint halts rather than forking. Mandatory:

- No floating point in any consensus-reachable path, including logging that could influence control flow.
- `u128` for all intermediates; `checked_*` on every operation; no `as` narrowing without `try_from`.
- `BTreeMap` / `BTreeSet` only. No `HashMap` iteration in consensus paths.
- No `SystemTime`, no RNG, no I/O inside `process_input`, `process_output`, `process_consensus_item`, or `audit`.
- `isqrt` must be an explicit integer Newton implementation in `common`, not a float-backed one. Pin and test it.
- Consensus item aggregation must be order-independent (median over a sorted `BTreeMap<PeerId, _>`).

---

## 12. Client module

Per `ClientModule` conventions: define input/output types and state machines; operations return an `OperationId` immediately and are driven by the shared `Executor`.

**Operations**

| Operation | Shape |
| --- | --- |
| `swap(side_in, amount_in, max_slippage_bps)` | quote → build tx → submit → await acceptance → reissue output notes |
| `deposit(amount_a, amount_b, max_slippage_bps)` | quote shares → build tx → submit → await |
| `withdraw(shares)` | build tx → submit → await → reissue both sides |

**State machines** are simple in Phase 1 because swaps are atomic: submit → `Accepted` | `Rejected`. There is no pending-claim state and therefore no refund path. This is the main ergonomic dividend of the single-input design and should be preserved unless Phase 3 forces otherwise.

**Slippage.** The client computes `min_out` from a fresh quote and the user's tolerance. It must re-quote immediately before submitting, not reuse a cached figure.

**API endpoints** (server):

- `RESERVES_ENDPOINT` → `Reserves` plus `total_shares`.
- `QUOTE_ENDPOINT(side_in, amount_in)` → `{ amount_out, effective_price, price_impact_ppm }`, computed with the same `common` function the server uses.
- `LP_POSITION_ENDPOINT(pubkey)` → shares and current redeemable amounts.

---

## 13. Threat model

### Does not apply here

- **Flash loans.** No borrowing primitive, no atomic composability with external lending. The largest on-chain exploit class is structurally absent.
- **Oracle manipulation.** No oracle.
- **Malicious token contracts.** Units are federation-configured at DKG; there is no permissionless listing.

### Applies

| Threat | Mechanism | Mitigation |
| --- | --- | --- |
| **Guardian front-running** | Guardians see submitted transactions before ordering them | Not fully solvable in Phase 1. `min_out` bounds the loss to the trader's stated tolerance. Phase 3 batch clearing removes the incentive by making reordering non-profitable. Document the residual exposure honestly. |
| **Censorship** | A guardian withholds a swap | Standard federation threshold assumption; client submits to multiple peers |
| **LVR / adverse selection** | Any member with a faster external price picks off stale reserves | Inherent. `fee_ppm` must be set with the pair's volatility in mind; a pool whose flow is one-directional will bleed. Surface realised fee income vs. reserve drift in the guardian UI so this is visible. |
| **Dust / spam griefing** | Many tiny swaps, each flooring output to zero | `min_swap_in_*` config; reject `amount_in` below it and `out == 0` |
| **First-depositor inflation** | Mint 1 share, then donate to inflate share price | `minimum_liquidity` burn |
| **Ratio-shift on deposit** | LP's deposit lands after a large swap and mints fewer shares than expected | `min_shares` on `DepositV0` |
| **Reserve drain via rounding** | Accumulated sub-unit errors | Floor everywhere + `k`-monotonicity post-condition |
| **Audit-induced halt** | Misreported liability trips the global assert | §10; lifecycle tests in §14 |

### 13.1 Swap privacy: why no quantisation is needed

**Both legs of a swap are e-cash, so the swap links two anonymous events rather than a user to an amount.** The inputs are notes that were blind-signed at some earlier issuance, so guardians cannot tell who held them. The output is a set of blinded messages they sign without seeing, so they cannot link the resulting notes to any later spend. What a guardian observes is "someone exchanged `dx` of A for `dy` of B" with no handle on either end.

This is the same posture as the Lightning module, which likewise reveals a payment amount in the clear and relies on the e-cash legs on either side to prevent that amount being attributed to a person. An AMM swap is not a special case and does not warrant a special mechanism.

An earlier draft proposed enforcing a quantisation ladder on `amount_in`. That is dropped. The reasoning behind it — that `amount_out`'s tier decomposition fingerprints the resulting note bundle — describes a general and well-understood property of Chaumian e-cash that applies equally to an LN receive or an on-chain peg-in of any non-round amount. It is not amplified by the AMM: guardians see `dy` directly in the transaction, so their ability to also *derive* it from public reserves adds nothing. Enforcing a ladder would have cost real UX and config surface for no marginal privacy.

`min_swap_in` is retained, but purely as an anti-dust and anti-griefing control (§13, threat table). It is not a privacy parameter and should not be documented as one.

**What does remain, and is binding:**

- **`trader_pk` MUST be freshly generated per swap.** It is the one persistent-identity handle in the transaction. It must never be derived from, or reused across, LP positions, other swaps, or any wallet-level identity key. A reused key would reintroduce exactly the linkage the e-cash legs otherwise prevent. Treat this as a correctness requirement of the client, and test it.
- **Reserves are a public accumulator.** Unlike the mint and LN modules, where amounts are visible only to guardians processing a transaction, this module must expose reserves through `RESERVES_ENDPOINT` for clients to quote against. Anyone polling it can therefore infer the size and direction of every swap. This is inherent — a quotable curve cannot hide its own state — and it leaks trade sizes to the public rather than to guardians alone. It identifies no one, but it should be stated rather than discovered.
- **Client note handling.** Reissue swap output into the wallet's general note pool rather than holding and later spending it as an isolated bundle. Standard e-cash hygiene, not an AMM-specific measure.

### 13.2 LP position privacy

Phase 1 LP positions are keyed by pubkey and therefore fully visible to guardians, including size and pool. State this plainly in user-facing docs. Phase 2's bearer shares fix it; nothing in Phase 1 mitigates it.

---

## 14. Testing requirements

**Unit / property tests on `common`:**

- `k` is non-decreasing under any sequence of swaps, at reserves spanning `1` to `u64::MAX / 2`.
- A round-trip swap (A→B→A) always returns strictly less than it started with, for all non-zero inputs.
- `deposit` followed immediately by `withdraw` of the same shares never returns more than was deposited on either side.
- `amount_out` never returns `>= reserve_out`.
- `isqrt` agrees with a reference implementation across the full `u128` range sample.

**Audit lifecycle test — the critical one:**

Drive a full sequence — first deposit, second deposit at a shifted ratio, swaps in both directions, partial withdraw, full withdraw — and assert after **every single step** that the summed `net_assets()` across the `amm` instance and both `mintv2` instances is exactly `0`. Repeat at extreme ratios (`1_000_000 : 1`) and at minimum viable reserves. Extend to the LP-share unit for Phase 2.

**Integration tests** against `devimint`: full transaction construction through real `mintv2` instances, verifying the per-unit funding check accepts the shapes in §7.3 and rejects malformed variants (missing incoming leg, wrong unit, `min_out` violation).

**Determinism test:** run the curve and audit paths on x86-64 and aarch64 in CI and compare byte-identical output.

**Fuzzing:** the input/output decoders, and `amount_out` / share math against overflow.

---

## 15. Phasing

| Phase | Content | Exit criterion |
| --- | --- | --- |
| **1** | Pool registry, account-based shares, atomic single-input swaps, audit as liability | Audit lifecycle test green; devimint integration green; guardian UI shows reserves and realised fees |
| **2** | Bearer LP shares as a per-pool unit via `mintv2`, receivable audit line | Lifecycle test extended to the share unit; LP privacy documented as achieved |
| **3** | Two-phase deposit/claim with uniform-price batch clearing per session | Only if guardian front-running proves to be a real problem in practice. It costs atomicity and reintroduces refund state machines, and it buys no privacy the e-cash legs do not already provide (§13.1) |

---

## 16. Open questions

1. **Upstream `fees` semantics.** Should this module depend on `fees` as a consume channel, or should an explicit channel be proposed to core first? Recommend raising before Phase 1 merge.
2. **Multi-unit audit.** The collapse is safe for this module but fragile across the ecosystem. Is there appetite upstream for `AuditItem` carrying an `AmountUnit`? That would let the assert become per-unit and remove the class of federation-halting reporting bugs entirely.
3. **Fee parameterisation.** Fixed `fee_ppm` at DKG is inflexible for a pair whose volatility changes. Is a guardian-voted fee (median consensus item, bounded range, rate-limited) worth the added surface in Phase 2?
4. **Interaction with `fedimint-swap`.** A federation running both gets an order book and a curve over the same units. Should the client route across them, or is that scope creep?
5. **Reserve seeding.** Who provides initial liquidity, and does the federation want the ability to seed the pool from a treasury? That implies a privileged deposit path, which has governance implications worth deciding deliberately rather than by omission.

---

## 17. References in-tree

- `fedimint-core/src/module/mod.rs` — `AmountUnit`, `Amounts`, `TransactionItemAmounts`
- `fedimint-core/src/module/audit.rs` — `Audit`, `AuditItem`, `calculate_net_assets`
- `fedimint-server/src/consensus/transaction.rs` — `FundingVerifier`, processing order
- `fedimint-server/src/consensus/engine.rs` — dbtx atomicity, post-item audit assert
- `modules/fedimint-swap-*` — closest structural precedent; cross-unit input/output pattern, median consensus timestamp, per-leg audit reporting
- `modules/fedimint-mintv2-server/src/lib.rs` — audit sign convention
