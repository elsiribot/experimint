//! The AMM module integration tests crate.
//!
//! `faucet` is a minimal, test-only issuing module used to give the `amm`
//! module a second `AmountUnit` to trade against `mintv2`'s BITCOIN — see
//! `faucet`'s module doc comment for why it exists (it is smaller than a
//! second `mintv2`, not a workaround for one being impossible) and why it must
//! never be linked into a real federation.
//!
//! `fixtures` builds the four-module `Fixtures` (`mintv2`, `faucet`, `amm`,
//! `dummy`) the integration tests in `tests/` share — see `fixtures`'s own
//! doc comment for why `dummy` is there too.

pub mod faucet;
pub mod fixtures;
