//! The multispend module integration tests crate.
//!
//! `faucet` is a minimal, test-only issuing module, copied (server half
//! only) from `fedimint-amm-tests`'s own `faucet` module, used to give these
//! tests a second `AmountUnit` independent of `mintv2`'s BITCOIN — see
//! `faucet`'s module doc comment for why it is a copy rather than a
//! dependency.
//!
//! `fixtures` builds the four-module `Fixtures` (`mintv2`, `dummy`,
//! `multispend`, `faucet`) the integration tests in `tests/` share — see
//! `fixtures`'s own doc comment for why `dummy` is there too, and why
//! `multispend` and `faucet` are registered server-only.

pub mod faucet;
pub mod fixtures;
