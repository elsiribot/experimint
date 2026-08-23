//! The AMM module integration tests crate.
//!
//! `faucet` is a minimal, test-only issuing module used to give the `amm`
//! module a second `AmountUnit` to trade against `mintv2`'s BITCOIN (spec
//! §3.2) — see `faucet`'s module doc comment for why it exists and why it
//! must never be linked into a real federation.
//!
//! `fixtures` builds the three-module `Fixtures` (`mintv2` + `faucet` +
//! `amm`) the integration tests in `tests/` share.

pub mod faucet;
pub mod fixtures;
