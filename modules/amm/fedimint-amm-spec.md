# `fedimint-amm` — Module Specification

**Status:** v1, implementation-ready · **Date:** 2026-08-22
**Target:** fedimint master, pinned at `4794ee166afc191e0125c092893bd8f080939b53` (2026-08-22, workspace `0.13.0-alpha`, edition 2024)
**Supersedes:** `fedimint-amm-module-spec.md` (draft v0.1)
**Prerequisite:** one `mintv2` instance per tradable unit

---

## 1. Governing principle

**Uniswap V2 is the reference implementation.** Every formula, constant and guard is a mechanical translation of `UniswapV2Pair.sol` / `UniswapV2Library.sol` / `UniswapV2Router02.sol`. Where Fedimint's transaction model forces a deviation, the deviation is listed in §2 with its reason. Nothing else deviates.

This is deliberate. The swap math and the share accounting are the parts of an AMM that are catastrophic when subtly wrong and boring when right; they have been audited to death in the reference. Our novelty budget is spent entirely on the Fedimint integration.

---

## 2. Uniswap V2 parity

### 2.1 Matched exactly

| Reference | Ours |
| --- | --- |
| `x · y = k` constant product | same |
| `getAmountOut`: `997·in·R_out / (1000·R_in + 997·in)` | same, `u128` (§7.1) |
| Fee 0.30%, taken from the input, retained as reserve | same, `997/1000` default (§7.1) |
| `MINIMUM_LIQUIDITY = 1000` burned on first mint | same (§7.2) |
| First mint `sqrt(a₀·a₁) − MINIMUM_LIQUIDITY` | same (§7.2) |
| Later mint `min(a₀·S/r₀, a₁·S/r₁)` | same (§7.2) |
| Burn `aᵢ = liquidity · rᵢ / S` | same (§7.3) |
| `k` non-decreasing, asserted after every swap | same, but rejects rather than panics (§7.1) |
| Router guards `amountOutMin` / `amountAMin` / `amountBMin` | `min_out` / `min_lo` / `min_hi`, enforced server-side (§6) |
| One pair per unordered token pair, many pairs per factory | one `Pool` per unordered `AmountUnit` pair, many per module instance (§4) |

### 2.2 Forced deviations

| # | Reference behaviour | Ours | Why |
| --- | --- | --- | --- |
| D1 | Swap is one atomic call | Swap is **two transactions** | Core processes all inputs before all outputs; see §3.1. This is the single deepest deviation and everything in §5 follows from it. |
| D2 | LP tokens are transferable ERC-20 | LP positions are records keyed by pubkey | No bearer-token primitive available without a per-pool DKG; see §12.1. |
| D3 | `uint256` arithmetic | `u128` with a `2^58` reserve cap | No 256-bit integer in the dependency tree. The cap is derived in §7.1 and is unreachable in practice. |
| D4 | `sync()` and `skim()` | neither | The reference needs them because ERC-20 balances can be sent to a pair directly, diverging from stored reserves. Our reserves are mutated only by module handlers, so no divergence exists. |
| D5 | `price0CumulativeLast` TWAP oracle | none | Out of scope. No consensus item carries a price (§10). |
| D6 | Protocol fee (`feeTo`, `kLast`, ⅙ of growth) | none | The entire fee accrues to LPs. A guardian-directed cut is a governance question, deferred to §15. |
| D7 | Flash swaps via `uniswapV2Call` callback | none | No callback primitive; structurally absent, which removes the largest on-chain exploit class outright. |
| D8 | LP balance is a single fungible number | positions fragment, one per deposit | Follows from the recovery-safe key derivation in §8. |
| D9 | Fee fixed at 0.30% in bytecode | `fee_per_mille` configurable per pool | Federations trade pairs of wildly differing volatility. Setting it to `3` reproduces the reference exactly. |

---

## 3. Platform mechanics

All verified against the pinned rev. Re-verify on any bump.

| # | Mechanic | Location | Consequence |
| --- | --- | --- | --- |
| P1 | Inputs processed, then signatures validated, then outputs processed | `fedimint-server/src/consensus/transaction.rs:51-113` | §3.1 |
| P2 | `AmountUnit(u64)`, `BITCOIN = 0` | `fedimint-core/src/module/mod.rs:77,86` | Units are opaque ids |
| P3 | `Amounts(BTreeMap<AmountUnit, Amount>)`, never stores zero entries | `mod.rs:114` | Never insert an explicit zero leg (§7.3) |
| P4 | `TransactionItemAmounts { amounts, fees }`; `InputMeta { amount, pub_key }` | `mod.rs:252`, `mod.rs:57` | |
| P5 | Per-unit funding: `input[u] ≥ output[u] + fees[u]`, surplus forfeited from `CoreConsensusVersion 2.1` | `transaction.rs:157-196` | §6.3 depends on this |
| P6 | `CORE_CONSENSUS_VERSION == 2.1` | `fedimint-core/src/module/version.rs:94` | Overpay is live |
| P7 | `assert!(net_assets >= 0)` after every consensus item, before `commit_tx_result` | `fedimint-server/src/consensus/engine.rs:1058-1067` | §9 |
| P8 | `calculate_net_assets` = `try_fold(0i64, i64::checked_add)` behind `.expect(…)` | `fedimint-core/src/module/audit.rs:146-150` | Saturating clamps are forbidden (§9.2) |
| P9 | `Audit::add_items` names each item `format!("{key:?}")` | `audit.rs:44` | Audited keys are Debug-printed into the summary (§9.2) |
| P10 | Server modules have **no** `AmountUnit` API; units are declared client-side via `PrimaryModuleSupport` | `fedimint-server-core/src/lib.rs`, `fedimint-client-module/src/module/mod.rs:924-934` | §11 |
| P11 | `ClientInput { input, keys, amounts }` — amount is client-side, not wire | `fedimint-client-module/src/transaction/builder.rs:31-35` | §6.1 |
| P12 | `verify_input_submission` / `verify_output_submission` receive a dbtx, run in Submission mode only | `transaction.rs:53-64, 85-95` | §6.4 |
| P13 | `mintv2` carries one `amount_unit` per instance and rejects all others | `modules/fedimint-mintv2-common/src/config.rs:36,49`; `-client/src/lib.rs:417,432,517` | One mint instance per tradable unit |
| P14 | `module_root_secret` is namespaced per module instance | `fedimint-client-module/src/module/init.rs:114` | §8 |

**`fedimint-swap-*` does not exist on master**, and there is no AMM, DEX or liquidity-pool code anywhere in the tree. The draft's structural precedent is gone; `fedimint-mintv2-*` and `fedimint-lnv2-*` are the models to follow.

### 3.1 The rule everything derives from

Core processes **all inputs, then signatures, then all outputs** (P1). An input *provides* value; an output *consumes* it. Neither can see its siblings, and there is no end-of-transaction hook.

> **A module input may provide value only by destroying a pre-existing, authenticated record. Anything that creates a record must be an output.**

Violating this gives an input that mints value on the strength of a sibling output which runs afterwards and may simply be absent — the pool pays out for free, and nothing in core catches it.

This is why the draft's single-input swap needed the `fees` channel as a "consume" channel, and why, having rejected that (D1 rationale), the swap must span two transactions.

---

## 4. Crate layout

```
modules/amm/
  fedimint-amm-common/   types, config, errors, curve math (no I/O)
  fedimint-amm-server/   ServerModule, DB schema, audit, API
  fedimint-amm-client/   ClientModule, state machines, recovery, quotes
  fedimint-amm-tests/    devimint integration tests
```

All curve and share arithmetic lives in `common` as pure functions, so client quotes and server settlement run the same code path. Dependencies mirror `modules/fedimint-dummy-*`: `fedimint-core` everywhere, `fedimint-server-core` in the server, `fedimint-client-module` in the client, `fedimint-testing` + `fedimint-client` in tests.

---

## 5. Data model

```rust
#[repr(u8)]
pub enum DbKeyPrefix {
    Pool       = 0x01,  // PoolId                -> Pool
    LpPosition = 0x02,  // (PoolId, PublicKey)   -> LpPosition
    Balance    = 0x03,  // (PublicKey, AmountUnit) -> BalanceEntry
}

/// Canonical unordered pair. MUST decode with lo < hi; see §5.1.
pub struct PoolId { lo: AmountUnit, hi: AmountUnit }

pub struct Pool {
    pub reserve_lo: Amount,
    pub reserve_hi: Amount,
    pub total_shares: u64,   // includes the unassigned MINIMUM_LIQUIDITY
}

pub struct LpPosition { pub shares: u64, pub tweak: [u8; 16] }
pub struct BalanceEntry { pub amount: Amount, pub tweak: [u8; 16] }
```

Pools are independent, so operations on different pairs never contend. `LpPosition` is keyed `(PoolId, PublicKey)` so an audit or API prefix scan enumerates a pool's positions.

Shares are a plain `u64`, not denominated in any `AmountUnit`.

`tweak` is stored on the two pubkey-keyed records so recovery can find them without replaying session history (§8).

### 5.1 Canonical `PoolId` — a consensus-correctness trap

`PoolId` is `Decodable`, so a hand-rolled client can encode it with `lo > hi`. If that is accepted, one unit pair gets **two** `Pool` records: liquidity splits, quotes disagree with reality, and `PoolId::new(a, b)` on the client resolves to only one of them.

Enforce `lo < hi` **in the `Decodable` impl**, not in each handler, and test that a swapped-order encoding is rejected.

```rust
impl PoolId {
    pub fn new(x: AmountUnit, y: AmountUnit) -> Option<Self> {
        match x.cmp(&y) {
            Ordering::Equal   => None,
            Ordering::Less    => Some(Self { lo: x, hi: y }),
            Ordering::Greater => Some(Self { lo: y, hi: x }),
        }
    }
}
```

---

## 6. Transaction items

Four items, assigned to input or output purely by §3.1.

| Item | Kind | Why that kind |
| --- | --- | --- |
| `SwapV0` | output | creates a `Balance` → must consume |
| `DepositV0` | output | creates an `LpPosition` → must consume |
| `ClaimBalanceV0` | input | destroys a `Balance` that already exists |
| `WithdrawV0` | input | destroys an `LpPosition` that already exists |

```rust
pub enum AmmOutput {
    /// Sell `amount_in` of `unit_in` into the pool; credit the proceeds
    /// as a Balance claimable by `recipient_pk`.
    SwapV0 {
        unit_in: AmountUnit,
        unit_out: AmountUnit,      // MUST differ from unit_in
        amount_in: Amount,
        min_out: Amount,           // router-equivalent amountOutMin
        recipient_pk: PublicKey,
        tweak: [u8; 16],           // §8
    },
    /// Add liquidity; create the pool if absent.
    DepositV0 {
        pool: PoolId,
        amount_lo: Amount,
        amount_hi: Amount,
        min_shares: u64,           // router-equivalent slippage guard
        owner_pk: PublicKey,
        tweak: [u8; 16],
    },
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}

pub enum AmmInput {
    /// Claim the entire balance. Full claims only; see §6.1.
    ClaimBalanceV0 { pubkey: PublicKey, unit: AmountUnit },
    /// Burn shares, withdraw both sides pro rata.
    WithdrawV0 {
        pool: PoolId,
        owner_pk: PublicKey,
        shares: u64,
        min_lo: Amount,            // router-equivalent amountAMin
        min_hi: Amount,            // router-equivalent amountBMin
    },
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}
```

`amounts` returned per item:

- `SwapV0` → `{unit_in: amount_in}`
- `DepositV0` → `{lo: amount_lo, hi: amount_hi}`
- `ClaimBalanceV0` → `{unit: stored_amount}`, `pub_key = pubkey`
- `WithdrawV0` → `{lo: da, hi: db}`, `pub_key = owner_pk`, omitting any leg that floors to zero (P3)

`fees` is always empty. This module declares no fee to core; the swap fee is retained as reserve exactly as in the reference.

### 6.1 `ClaimBalanceV0` carries no amount, and claims are all-or-nothing

**The wire does not need the amount.** `ClientInput` carries it client-side for the builder's arithmetic (P11); the value core uses comes from the `InputMeta` that `process_input` returns.

**A declared amount is a second source of truth, hence an error path.** Omitting it deletes the declared-vs-stored check, its error variant, and the tests for both.

**Full claims keep the table bounded.** Balances are never garbage-collected — there is no deadline and no sweep, by design (§6.3). Partial claims would leave permanent residue, often too small to express in `mintv2` denominations. Full claims delete the record every time, so the table holds only live, in-flight swaps.

**The race resolves in our favour.** A balance is monotonically non-decreasing between the client building the claim and consensus processing it: only the owner can debit, but anyone can credit (`SwapV0` names an arbitrary `recipient_pk`). A full sweep therefore comes out **≥** what the client planned, and surplus inputs are silently forfeited under P5/P6 — so the transaction still succeeds, burning only a windfall the client never knew it had. The trader's own funds are never at risk, and crediting a stranger's balance costs the attacker real money for no gain.

Two obligations follow:

- The client **re-reads the balance immediately before building the claim**, so a gift is captured rather than forfeited.
- **We depend on overpay-permitted (P5, P6).** Below `2.1`, `verify_funding` requires exact equality per unit, and a gifted balance would make every claim attempt fail — a real DoS. Master is `2.1`. This is a standing assumption with a test, not folklore.

Retry semantics fall out clean: if a claim lands and the client resubmits, the second attempt finds no balance and is rejected outright.

### 6.2 Transaction shapes

**Swap, Tx1 — hand over A**

| Item | Kind | `amounts` |
| --- | --- | --- |
| `mintv2-A` note spend | input | `{A: dx + change}` |
| `amm` `SwapV0` | output | `{A: dx}` |
| `mintv2-A` change | output | `{A: change}` |

**Swap, Tx2 — take B**

| Item | Kind | `amounts` |
| --- | --- | --- |
| `amm` `ClaimBalanceV0` | input | `{B: dy}` |
| `mintv2-B` blinded notes | output | `{B: dy}` |

**Deposit** — `mintv2-A` and `mintv2-B` inputs fund one `amm` `DepositV0` output.
**Withdraw** — one `amm` `WithdrawV0` input funds `mintv2-A` and `mintv2-B` outputs.

Note that **each transaction touches exactly one unit on the mint side** for swaps, so two `mintv2` instances never have to co-fund a single transaction. Given P10 and the unit-parameterised client builder, that is a meaningful simplification and an independent argument for D1.

### 6.3 Properties of the two-transaction swap

- **`SwapV0` carries no authorisation, correctly.** Outputs return no `pub_key`; Tx1 is authorised entirely by the note spend. Anyone may swap into anyone's `recipient_pk`, which is harmless and yields pay-to-swap for free.
- **Never submitting Tx2 costs the pool nothing.** The pool already holds its A. The balance sits indefinitely: no deadline, no expiry sweep, no refund path, no reserves earmarked away from other traders. This is the entire payoff of choosing balances over pending offers, and it is why no consensus item is needed (§10).
- **Multi-hop costs one extra transaction, not two.** A→B→C is Tx1 `SwapV0(A→B)`; Tx2 `ClaimBalance(B)` + `SwapV0(B→C)`; Tx3 `ClaimBalance(C)` + notes. Purely client-side; the server needs no routing logic.
- **Full exit from fragmented positions is one transaction.** Several `WithdrawV0` inputs may share a transaction, since core collects a `pub_key` per input and validates the whole set at `transaction.rs:81`.

### 6.4 Submission-time rejection

Implement `verify_output_submission` and `verify_input_submission` (P12) to re-check `min_out`, `min_shares`, `min_lo`/`min_hi`, balance existence and reserve caps against the current dbtx. These run only in Submission mode and are not consensus-binding, but they turn a doomed transaction into an immediate client-visible error instead of a wasted consensus round.

---

## 7. Arithmetic

`Amount` is `u64` base units. Every intermediate is `u128`. **No floating point on any consensus-reachable path.** (`audit.rs:80` uses `f64` for display formatting only, never control flow.)

### 7.1 Swap — `getAmountOut`

```rust
pub const MAX_RESERVE: u64 = 1 << 58;

fn amount_out(reserve_in: u64, reserve_out: u64, amount_in: u64, fee_per_mille: u16)
    -> Result<u64, AmmError>
{
    require amount_in > 0 && reserve_in > 0 && reserve_out > 0
    require amount_in >= units[unit_in].min_swap_in          // anti-dust (§11)
    require amount_in <= MAX_RESERVE && reserve_in <= MAX_RESERVE && reserve_out <= MAX_RESERVE
    require fee_per_mille < 1000

    let in_with_fee = amount_in as u128 * (1000 - fee_per_mille as u128);
    let numerator   = in_with_fee * reserve_out as u128;
    let denominator = reserve_in as u128 * 1000 + in_with_fee;
    let out         = numerator / denominator;               // floor

    require out > 0                 // reject dust that would round to nothing
    require out < reserve_out       // never drain
    u64::try_from(out)
}
```

State update: `reserve_in += amount_in` — the **full** amount, fee included — and `reserve_out -= out`. Nothing collects the fee; it stays as reserve and lifts `k`, paying LPs through a rising share price rather than a transfer. This is the reference behaviour verbatim.

**Why `MAX_RESERVE = 2^58`.** The largest intermediate is `numerator = 997 · amount_in · reserve_out`, and `997 < 2^10`, so safety requires `amount_in · reserve_out < 2^118`. Capping both at `2^58` gives `997 · 2^116 < 2^126` — four bits of headroom. The same bound covers `da · total_shares ≤ 2^116` (§7.2), `isqrt(da · db) ≤ 2^58`, and the `k` product (`≤ 2^116`).

`2^58` msats is ≈ 2.88 × 10⁶ BTC, two orders of magnitude above the entire supply, so the cap is unreachable for BTC and still ≈ 2.9 × 10¹⁷ base units for any other unit. Per-mille rather than ppm is what buys the headroom: ppm would cost 20 bits instead of 10 and force the cap down to `2^54`.

`SwapV0` and `DepositV0` **reject** any operation that would push a reserve above `MAX_RESERVE`. This makes every downstream `u128` expression total by construction rather than by hope.

**Backstop.** After every swap, check `reserve_in_new · reserve_out_new ≥ reserve_in_old · reserve_out_old` in `u128`. On violation, **return an error so the transaction is rejected** — never `panic!` or `assert!`. The reference can revert; we cannot, because a panic here is a federation halt (P7).

### 7.2 Deposit — `mint`

```
if total_shares == 0:                                    // pool creation
    minted   = isqrt(da as u128 * db as u128)            // floor
    require minted > MINIMUM_LIQUIDITY                   // == 1000
    total_shares = minted
    to_owner = minted - MINIMUM_LIQUIDITY                // assigned to nobody, ever
else:
    minted   = min(da as u128 * total_shares / reserve_lo,
                   db as u128 * total_shares / reserve_hi)    // floor
    require minted > 0
    total_shares = total_shares.checked_add(u64::try_from(minted)?)?
    to_owner = minted
require to_owner >= min_shares
reserve_lo += da; reserve_hi += db                       // both ≤ MAX_RESERVE
LpPosition[(pool, owner_pk)] = LpPosition { shares: to_owner, tweak }
```

**The `min()` forces deposits at the current ratio**; excess on the over-supplied side is donated to existing LPs. That is reference behaviour and a genuine UX footgun, so it belongs in client docs — but it needs no second mechanism, because `min_shares` guards both failure modes at once. A ratio move between quote and landing drops `minted`; an unbalanced pair drops `minted` by the same arithmetic. One guard, two protections.

`MINIMUM_LIQUIDITY` shares are credited to no `LpPosition` and are unwithdrawable forever. Besides defeating the first-depositor share-price inflation attack, this means **`total_shares` can never return to zero**, so the `total_shares == 0` branch runs exactly once per pool, no later deposit can divide by zero, and `Pool` records are created once and never deleted. It also makes pool creation cost a permanent two-sided deposit — a natural anti-spam bond.

Pool creation is otherwise **permissionless within the DKG unit allowlist**: validation is `lo != hi`, both units present in `units`, and `minted > MINIMUM_LIQUIDITY`.

`isqrt` is an explicit integer Newton iteration in `common`, never `f64::sqrt`, differentially tested against a reference across a `u128` sample.

### 7.3 Withdraw — `burn`

```
require 0 < shares <= LpPosition[(pool, owner_pk)].shares
da = reserve_lo as u128 * shares / total_shares           // floor
db = reserve_hi as u128 * shares / total_shares           // floor
require da >= min_lo && db >= min_hi
require da > 0 || db > 0
position.shares -= shares; total_shares -= shares
reserve_lo -= da; reserve_hi -= db
if position.shares == 0 { delete record }
```

Both payouts floor, so rounding dust stays with the pool — the correct direction, and why deposit-then-immediate-withdraw can never return more than went in.

`min_lo` / `min_hi` are the reference router's `amountAMin` / `amountBMin`. They are not paranoia: a large swap landing just before a withdrawal barely changes the position's *value* but changes its *composition* a lot, and receiving 99% of whatever was just dumped into the pool is real slippage.

`require da > 0 || db > 0` stops someone burning a position too small to pay out anything. A leg that floors to zero is **omitted** from the returned `amounts` rather than inserted as an explicit zero (P3).

### 7.4 The rule that protects the balance sheet

Audit conservation is **structural, not a rounding property**. A swap moves `reserve_out -= dy` and credits `balance += dy` — the same integer — so the liability delta is `+dx − dy + dy = +dx`, exactly offsetting the mintv2 liability released by the burned notes. It cancels for *any* value of `dy`, including a wrong one. Withdrawal is the same: the `da`/`db` debited from reserves are the identical integers returned in `amounts`.

So the operative rule is:

> **Never compute a value twice.** Compute it once; use that one binding for both the DB mutation and the returned `amounts`.

That is mechanically checkable in review and in tests. "Floor everywhere" is not — one can floor consistently and still halt the federation by recomputing a value on the reporting path. Floors still matter, but for pool economics: they keep the pool undrainable, keep subtraction from underflowing, and keep dust with the LPs.

---

## 8. Key derivation and recovery

Follow `fedimint-mintv2-client/src/issuance.rs` exactly.

```rust
// mintv2, issuance.rs:88-93
output_secret = root.child_key(ChildId(denomination)).tweak(&tweak)
```

The `tweak` is a **random `[u8; 16]`**, never an index, and it is published in the clear on the wire. Two properties follow, and we need both:

**Stateless derivation.** A counter makes derivation shared mutable state: two wallets restored from one seed both derive "the next" key and collide, and an old client still running quietly issues into slots the new one is using. A random tweak has no state to desync, so concurrent clients on one seed never collide.

**A private, cheap recovery prefilter.**

```rust
tweak_filter(root) = root.to_random_bytes()                  // issuance.rs:50-52, private per seed
grind_tweak:  loop { t = rand(); if check_tweak(t, filter) { return t } }   // :54-62
check_tweak:  sha256(tweak, filter)[0..2] == [0, 0]          // :64-71
```

Issuance grinds ~65 536 hashes (sub-millisecond) so the tweak carries a 16-bit tag only the seed-holder can test. Recovery then pays one cheap hash per candidate and a real derivation on ~1/65 536 of them. An outside observer cannot compute the filter, so the tag leaks no ownership.

### 8.1 Applied here

```rust
const CHILD_SWAP: u64 = 0;
const CHILD_LP:   u64 = 1;

recipient_pk = keypair(module_root.child_key(ChildId(CHILD_SWAP)).tweak(&tweak))
owner_pk     = keypair(module_root.child_key(ChildId(CHILD_LP)).tweak(&tweak))
```

Child ids need only be unique *within* the module: `module_root_secret` is already namespaced per module instance (P14), which is how mintv2 gets away with using a raw denomination as its `ChildId`. Our filter derives from our own module secret and is therefore uncorrelated with mintv2's.

### 8.2 Recovery is a table scan, not a history replay

mintv2 must scan historical outputs because notes are bearer and live client-side. Our `Balance` and `LpPosition` are **server-side records**, so we store the `tweak` in the record and expose a recovery endpoint streaming `(tweak, pubkey, …)`. Recovery becomes a scan of live state — for balances, only in-flight swaps — with no session replay at all. Grinding still earns its keep on `LpPosition`, which grows with a federation's LP count.

### 8.3 Accepted consequence

A second deposit grinds a fresh tweak and therefore creates a **new position rather than growing an existing one** (D8). Positions fragment and each withdraws via its own `WithdrawV0`, though they may share one transaction (§6.3). The cost is bytes and a little dust; the benefit is that an LP's deposits are mutually unlinkable.

### 8.4 Upstream note

`grind_tweak` / `check_tweak` / `tweak_filter` live in `fedimint-mintv2-client`, not a shared crate. Depending on it would couple our client to another module's client crate, so reimplement the ~15 lines in `fedimint-amm-client` — and propose upstream that they move to `fedimint-client-module` as a reusable recovery primitive.

---

## 9. Audit

Deliberately minimal. Upstream audit is expected to change; §9.2 records what we would otherwise have designed around, so it can be revisited then rather than rediscovered.

### 9.1 Implementation

Reserves and outstanding balances are both liabilities — obligations to hand notes back to someone. Report each unit as its own item so no item mixes units. Follows the `fedimint-lnv2-server` shape (`lib.rs:609-630`).

```rust
async fn audit(&self, dbtx: &mut DatabaseTransaction<'_>,
               audit: &mut Audit, module_instance_id: ModuleInstanceId) {
    audit.add_items(dbtx, module_instance_id, &PoolPrefix,
        |_, p| -i64::try_from(p.reserve_lo.msats).expect("bounded by MAX_RESERVE")).await;
    audit.add_items(dbtx, module_instance_id, &PoolPrefix,
        |_, p| -i64::try_from(p.reserve_hi.msats).expect("bounded by MAX_RESERVE")).await;
    audit.add_items(dbtx, module_instance_id, &BalancePrefix,
        |_, b| -i64::try_from(b.amount.msats).expect("bounded by MAX_RESERVE")).await;
    // LpPosition is deliberately NOT reported: shares are a claim against
    // reserves already counted above. Reporting them would double-count.
}
```

`MAX_RESERVE = 2^58 < i64::MAX` makes every `try_from` total, so the `expect` is justified by an invariant enforced at §7.1 rather than by optimism.

Conservation, per operation:

| Operation | mintv2 Δnet | amm Δnet | Σ |
| --- | --- | --- | --- |
| `SwapV0` (A→B) | `+dx` | `−dx + dy − dy` | **0** |
| `ClaimBalanceV0` | `−dy` | `+dy` | **0** |
| `DepositV0` | `+da + db` | `−da − db` | **0** |
| `WithdrawV0` | `−da − db` | `+da + db` | **0** |

Collapsing every unit into one `i64` msat scalar is lossy in general but safe here: the module only ever *relocates* nominal quantity between itself and a `mintv2` instance and never originates any, so each quantity appears twice with opposite sign.

### 9.2 Known limitations, to revisit when upstream audit changes

- **`audit` runs for every module after every consensus item** (P7), so scanning `Balance` per-row is O(live balances) on the hot path. Because balances are never garbage-collected, an attacker can create and abandon them, imposing a permanent scan cost on the whole federation. Each row costs them a real swap of at least `min_swap_in`, permanently abandoned, so it is a self-limiting bond — but the cost lands on the federation. The fix, if needed, is mintv2's pattern: an aggregate `TotalLiability: AmountUnit -> Amount` (`mintv2-server/src/lib.rs:492-503` audits an `IssuanceCounter`, not its notes), making audit O(units). Deferred because it introduces a redundant source of truth.
- **Audited keys leak.** `add_items` names each item `format!("{key:?}")` (P9), so auditing `Balance` per-row Debug-prints every in-flight swap's recipient pubkey into the `AuditSummary` guardians expose over the admin API. The aggregate above would also fix this.
- **A saturating clamp is the one forbidden implementation.** `calculate_net_assets` is `try_fold(0i64, i64::checked_add)` behind `.expect(…)` (P8), so `unwrap_or(i64::MAX)` converts a misreport into a summing overflow and panics — precisely the halt. `MAX_RESERVE` is what makes the question moot.
- **Severity.** The draft claimed the assert sits past the commit point. It does not — it is at `engine.rs:1058`, before `commit_tx_result` at `:1067`. This changes nothing: every guardian runs the same deterministic code and panics together, and a restart re-processes the same item and panics again. Unrecoverable without a code change either way.
- **Upstream ask.** `AuditItem` carrying an `AmountUnit`, with a per-unit assert, would remove this entire class of federation-halting bugs.

---

## 10. Consensus items

**None.** The draft carried a median `Timestamp` item to support deadline logic; nothing in this design has a deadline, because a balance is always claimable and no reserves are ever earmarked. Adding it now would be dead consensus surface.

**No price consensus item exists and none should be added.** Pricing is endogenous; an oracle would create a manipulation surface this design otherwise does not have.

---

## 11. Configuration

```rust
pub struct AmmConfigConsensus {
    /// Units this federation permits trading, with per-unit dust thresholds.
    /// A unit is only reachable if some mintv2 instance issues it — a setup
    /// requirement this module cannot verify (P13).
    pub units: BTreeMap<AmountUnit, UnitParams>,
    /// Applied to any pool without an explicit override. Default 3 (= 0.30%,
    /// the reference value).
    pub default_fee_per_mille: u16,
    pub fee_overrides: BTreeMap<PoolId, u16>,
}

pub struct UnitParams {
    /// Minimum accepted swap input. Anti-dust and anti-spam; a DoS control,
    /// not a privacy control (§13.1).
    pub min_swap_in: Amount,
}

pub struct AmmClientConfig {
    pub units: BTreeMap<AmountUnit, UnitParams>,
    pub default_fee_per_mille: u16,
    pub fee_overrides: BTreeMap<PoolId, u16>,
}

pub struct AmmConfigPrivate;  // empty — this module holds no key material
```

`MINIMUM_LIQUIDITY` is the reference's hardcoded `1000`, not configurable.

**DKG validation.** `units` non-empty; every fee `< 1000`; every `min_swap_in` non-zero; every `PoolId` in `fee_overrides` canonical (§5.1) with both units in `units`. The federation must additionally run a `mintv2` instance per listed unit, which this module cannot check — surface it as a guardian-UI setup checklist item.

**Adding units** requires a `mintv2` DKG plus an `AmmConfigConsensus` change, i.e. a coordinated federation upgrade. Adding *pairs* over existing units requires nothing.

Because `AmmConfigPrivate` is empty, this module holds no key material and pools are ordinary database records — which is why one instance can host many pools while a `mintv2` instance hosts exactly one unit (P13).

**Consensus version** starts at `0.0`. Any change to the encoded shape of an input, output or stored record requires a bump plus a `get_database_migrations` entry.

---

## 12. Client module

Units are declared client-side (P10) and `ClientModule` is unit-parameterised throughout — `create_final_inputs_and_outputs(…, unit, …)`, `get_balance(dbtx, unit)`, `get_balances(dbtx)`.

**Operations**

| Operation | Shape |
| --- | --- |
| `swap(unit_in, unit_out, amount_in, max_slippage_bps)` | quote → Tx1 → await accept → re-read balance → Tx2 → reissue notes |
| `deposit(pool, amount_lo, amount_hi, max_slippage_bps)` | quote shares → build → submit → await |
| `withdraw(position, shares, max_slippage_bps)` | build → submit → await → reissue both sides |

**State machines.** The swap is the only multi-step operation: `Tx1Submitted → Tx1Accepted → Tx2Submitted → Done`, with `Tx1Rejected` and `Tx2Rejected` terminal. There is no refund state and no timeout, because a balance created by Tx1 is permanently claimable — a crash between the two steps is resumed, never unwound. Deposit and withdraw are single-transaction: `Submitted → Accepted | Rejected`.

**Slippage.** The client computes `min_out` from a fresh quote and the user's tolerance, and must re-quote immediately before submitting rather than reuse a cached figure. Same for `min_shares` and `min_lo`/`min_hi`.

**Recovery** re-derives candidate keys per §8 and scans the two recovery endpoints. Any unclaimed `Balance` found is claimed; any `LpPosition` found is restored to the position list.

**API endpoints** (server)

- `POOLS_ENDPOINT` → every `PoolId` with reserves, `total_shares`, effective fee.
- `QUOTE_ENDPOINT(unit_in, unit_out, amount_in)` → `{ amount_out, effective_price, price_impact_per_mille }`, computed with the same `common` function the server settles with.
- `BALANCE_RECOVERY_ENDPOINT` → stream of `(tweak, pubkey, unit, amount)`.
- `LP_RECOVERY_ENDPOINT` → stream of `(tweak, pool, pubkey, shares)`.

### 12.1 Documented limitations

- **A swap is two transactions.** The client hides this behind one `OperationId`, but it is user-visible as two confirmations, and the UI should say so rather than appear stalled.
- **LP positions are keyed by pubkey and fully visible to guardians**, including size and pool. Bearer shares would fix it and are not planned (§15). State this plainly in user docs.
- **Unbalanced deposits donate the excess** to existing LPs (§7.2).

---

## 13. Threat model

### Structurally absent

- **Flash loans** — no borrowing primitive and no callback (D7). The largest on-chain exploit class cannot be expressed.
- **Oracle manipulation** — no oracle (§10).
- **Malicious token contracts** — units are federation-configured at DKG; there is no permissionless listing.
- **Reserve/balance divergence** — no `sync`/`skim` needed (D4).

### Applies

| Threat | Mechanism | Mitigation |
| --- | --- | --- |
| Guardian front-running | Guardians see submitted transactions before ordering them | Not fully solvable. `min_out` bounds the loss to the trader's stated tolerance. Document the residual exposure honestly. |
| Censorship | A guardian withholds a swap | Standard threshold assumption; client submits to multiple peers |
| LVR / adverse selection | Anyone with a faster external price picks off stale reserves | Inherent to the reference design. `fee_per_mille` must reflect the pair's volatility; a pool with one-directional flow will bleed. Surface realised fee income vs. reserve drift in the guardian UI. |
| Dust / spam griefing | Many tiny swaps, each flooring output to zero | `min_swap_in`; reject `amount_in` below it and `out == 0` |
| First-depositor inflation | Mint 1 share, donate to inflate share price | `MINIMUM_LIQUIDITY` burn (§7.2) |
| Ratio shift on deposit | Deposit lands after a large swap and mints fewer shares | `min_shares` |
| Composition shift on withdraw | Withdrawal lands after a large swap | `min_lo` / `min_hi` |
| Reserve drain via rounding | Accumulated sub-unit errors | Floor everywhere plus the `k` post-condition (§7.1) |
| Split-pool via non-canonical `PoolId` | Two records for one pair | Enforced in `Decodable` (§5.1) |
| Abandoned-balance audit cost | Ungarbage-collected balances inflate the audit scan | §9.2; bonded by `min_swap_in` |
| Audit-induced halt | Misreported liability trips the global assert | §7.4, §9; lifecycle test in §14 |

### 13.1 Swap privacy

**Both legs of a swap are e-cash**, so a swap links two anonymous events rather than a user to an amount. Inputs are notes blind-signed at some earlier issuance; outputs are blinded messages signed without being seen. A guardian observes "someone exchanged `dx` of A for `dy` of B" with no handle on either end. This is the same posture as the Lightning module, which likewise reveals a payment amount in the clear.

The draft's proposed quantisation ladder on `amount_in` is **dropped**. Its rationale — that `amount_out`'s tier decomposition fingerprints the resulting note bundle — describes a general property of Chaumian e-cash that applies equally to an LN receive or a peg-in of any non-round amount, and is not amplified here: guardians see `dy` directly in the transaction, so deriving it from public reserves adds nothing. `min_swap_in` is retained purely as an anti-dust control and must not be documented as a privacy parameter.

What remains binding:

- **`recipient_pk` must be freshly ground per swap** (§8). It is the one persistent-identity handle in the transaction and must never be derived from, or reused across, LP positions, other swaps, or any wallet identity key. This is a correctness requirement of the client, and it is tested.
- **Reserves are a public accumulator.** Unlike the mint and LN modules, this module must expose reserves so clients can quote. Anyone polling `POOLS_ENDPOINT` can infer the size and direction of every swap. That is inherent — a quotable curve cannot hide its own state — and it leaks trade sizes to the public rather than to guardians alone. It identifies no one, but it should be stated rather than discovered.
- **Client note handling.** Reissue swap proceeds into the wallet's general note pool rather than holding them as an isolated bundle.

---

## 14. Testing

**Differential test against the reference.** Port a corpus of `getAmountOut` / `mint` / `burn` vectors from the Uniswap V2 test suite and assert bit-identical results within `MAX_RESERVE`. This is the highest-value test in the module: it is what makes "mechanical translation" a checkable claim rather than an aspiration.

**Property tests on `common`:**

- `k` non-decreasing under any sequence of swaps, at reserves spanning `1` to `MAX_RESERVE`.
- A round trip A→B→A always returns strictly less than it started with, for every non-zero input.
- `deposit(da, db)` followed immediately by `withdraw(all_minted)` returns `≤ da` and `≤ db` on both sides.
- `amount_out` never returns `≥ reserve_out`, and never returns `0` without erroring.
- `total_shares` never reaches zero after creation.
- `isqrt` agrees with a reference across a `u128` sample.
- No input within the caps overflows `u128` — fuzzed, not merely argued.

**Encoding tests:** non-canonical `PoolId` rejected (§5.1); `#[encodable_default]` round-trips for unknown variants; decoders fuzzed.

**Audit lifecycle test.** Drive first deposit → second deposit at a shifted ratio → swaps both directions → partial withdraw → full withdraw, asserting after **every step** that summed `net_assets()` across the `amm` instance and both `mintv2` instances is exactly `0`. Repeat at extreme ratios (`10^6 : 1`) and at minimum viable reserves.

**Overpay-dependency test.** Credit a balance from a second party between build and submit; assert the claim still succeeds and forfeits only the surplus (§6.1). This test fails loudly if P5/P6 ever change.

**Recovery test.** Derive positions and balances, wipe client state, recover from seed alone, assert everything is found and claimable. Include a concurrent-client case: two clients on one seed both swapping, asserting no key collision (§8).

**Integration tests** against `devimint`: full construction through real `mintv2` instances, verifying the per-unit funding check accepts §6.2's shapes and rejects malformed variants — missing claim leg, wrong unit, `min_out` violation, non-existent balance.

**Determinism test:** run curve and audit paths on x86-64 and aarch64 in CI and compare byte-identical output.

---

## 15. Out of scope

Deliberately not planned, as distinct from "not yet built":

- **Bearer LP shares.** The draft's Phase 2 issued shares as a per-pool `AmountUnit` via `mintv2`. That contradicts permissionless pool creation: a new unit needs a DKG key ceremony, so a permissionlessly-created pool could never get a share unit, and bearer shares would silently restrict pool creation to coordinated upgrades. Permissionless pools were kept; positions stay account-based and LP privacy is a documented limitation (§12.1), not a roadmap item. A future bearer scheme would need this module's own threshold keyset with the pool encoded in the signed message — a substantially larger project.
- **Batch clearing.** Two-phase uniform-price clearing would remove the front-running incentive, but it costs atomicity, reintroduces refund state machines, and buys no privacy the e-cash legs do not already provide (§13.1). Revisit only if guardian front-running proves to be a real problem in practice.
- **Server-side multi-hop routing.** Composes client-side at one transaction per hop (§6.3).
- **Concentrated liquidity, dynamic fees, TWAP oracle, protocol fee.** D5, D6, D9.
- **Cross-federation swaps.**
- **Adding units after DKG.** Adding *pairs* over existing units is permissionless; adding units is not (§11).

### 15.1 Non-goal, stated explicitly

This module is **not trust-minimised relative to the federation**. Reserves are federation-held funds. What it guarantees is that pricing is deterministic, auditable, and not subject to guardian discretion — a governance property, not a custody one. Documentation must not imply otherwise.

---

## 16. Open questions

1. **Fee governance.** A fee fixed at DKG is inflexible for a pair whose volatility changes. Is a guardian-voted fee (median consensus item, bounded range, rate-limited) worth reintroducing the consensus item §10 removed?
2. **Reserve seeding.** Who provides initial liquidity, and does the federation want to seed pools from a treasury? That implies a privileged deposit path with governance implications worth deciding deliberately rather than by omission.
3. **Per-unit audit.** Would upstream accept `AuditItem` carrying an `AmountUnit` (§9.2)? It would make the global assert per-unit and remove a class of federation-halting bugs across the ecosystem.
4. **Recovery primitives upstream.** Should `grind_tweak` / `check_tweak` / `tweak_filter` move to `fedimint-client-module` (§8.4)?
5. **`MAX_RESERVE` for non-BTC units.** `2^58` is unreachable for BTC but is a real ceiling for a fine-grained unit. Should it be per-unit configurable at DKG rather than a global constant?

---

## 17. References

**In-tree** (pinned rev): `fedimint-core/src/module/mod.rs` · `fedimint-core/src/module/audit.rs` · `fedimint-server/src/consensus/transaction.rs` · `fedimint-server/src/consensus/engine.rs` · `fedimint-client-module/src/transaction/builder.rs` · `fedimint-client-module/src/module/init.rs` · `modules/fedimint-mintv2-client/src/issuance.rs` · `modules/fedimint-mintv2-server/src/lib.rs` · `modules/fedimint-lnv2-server/src/lib.rs` · `modules/fedimint-dummy-*`

**Reference implementation:** `UniswapV2Pair.sol`, `UniswapV2Library.sol`, `UniswapV2Router02.sol` (Uniswap V2 core/periphery).
