//! A minimal, test-only issuing module ("msfaucet") used exclusively to give
//! `fedimint-multispend-tests` a second `AmountUnit` to credit into a
//! `multispend` account alongside `mintv2`'s BITCOIN — spec §3.1 requires an
//! account be able to hold balances in any `AmountUnit`, and exercising that
//! for real needs a second unit actually issuable in this federation, not
//! just a second literal `AmountUnit::new_custom(_)` value handed to
//! `MultispendOutput::Credit` with nothing backing it (which the funding
//! verifier would reject as unbalanced).
//!
//! Copied (server half only) from `fedimint-amm-tests::faucet`, per the Task
//! 6 brief's explicit instruction to reuse that approach rather than invent
//! another. It is a copy, not a dependency, for two reasons:
//!
//! 1. `fedimint-amm-tests` is a sibling module's integration-test crate, not
//!    a shared library — depending on it here would pull in the whole `amm`
//!    module graph (`amm-client`, `amm-server`, transitively `mintv2` a
//!    second time) for one unit-issuing shim, and would make this crate's
//!    tests fail to compile if `amm`'s own tests broke for an unrelated
//!    reason. Fedimint module test suites are conventionally siblings, not
//!    dependents of one another.
//! 2. Every multispend test in this crate assembles transactions by hand
//!    (spec §9.3 — this module has no client, and the real client can't use
//!    `TransactionBuilder` either, since the witness must sign a txid that
//!    doesn't exist until after `build()`). `fedimint-amm-tests::faucet`'s
//!    *client* half (`client/`, `client/input_sm.rs`, `client/output_sm.rs`)
//!    exists only to drive `TransactionBuilder`, which nothing here uses —
//!    so only the server half (`common`, `server`) is copied, and this
//!    module is registered via `Fixtures::with_server_only_module`, exactly
//!    like `multispend` itself.
//!
//! Renamed `ModuleKind` from `"ammfaucet"` to `"msfaucet"` on the copy so a
//! federation inspecting its own consensus (or a person reading a log) never
//! sees an `amm`-branded module name in a multispend-only fixture; the two
//! kinds never coexist in one federation, so nothing depends on them
//! matching.
//!
//! **This module is test-only. It must never be a dependency of
//! `fedimint-multispend-{common,server}`, and it must never ship in a real
//! federation.** It has no cryptography, no DKG key material, and no blind
//! signatures — anyone can mint themselves unlimited funds via
//! [`common::FaucetOutput::MintV0`] for free (see [`server`]'s module doc
//! comment for why that is nonetheless solvent under the global audit
//! assert, and why it is fine at all: a real deployment would never link
//! this crate in).

pub mod common;
pub mod server;
