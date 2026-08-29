//! A minimal, test-only issuing module ("ammfaucet") used exclusively to give
//! `fedimint-amm-tests` a second `AmountUnit` to trade against `mintv2`'s
//! BITCOIN.
//!
//! **It is not here because a second `mintv2` is impossible.** An earlier
//! revision of spec §3.2 claimed that, and it was wrong — instance registries
//! are keyed by `ModuleInstanceId`, and `Fixtures::with_extra_module_instance`
//! spins up a second `mintv2` on a different unit (see
//! `fedimint-usdt-tests`' `dual_mint_fixtures`, which does exactly that).
//!
//! It is here because it is *smaller*: a second issuing unit with no DKG, no
//! blind signatures and no key material, so the AMM's own tests exercise the
//! AMM rather than a second mint's ceremony. A dual-`mintv2` fixture would work
//! and would be closer to production; swapping to one is a reasonable
//! follow-up, not a correction.
//!
//! **This module is test-only. It must never be a dependency of
//! `fedimint-amm-{common,server,client}`, and it must never ship in a real
//! federation.** It has no cryptography, no DKG key material, and no blind
//! signatures — anyone can mint themselves unlimited funds via
//! [`common::FaucetOutput`] for free (see [`server`]'s module doc comment for
//! why that is nonetheless solvent under the global audit assert, and why it
//! is fine at all: a real deployment would never link this crate in).
//!
//! `common` holds the wire types shared by [`client`] and [`server`], mirror
//! image of how `fedimint-amm-common` relates to `-client`/`-server`.

pub mod client;
pub mod common;
pub mod server;
