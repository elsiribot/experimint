//! A minimal, test-only issuing module ("ammfaucet") used exclusively to give
//! `fedimint-amm-tests` a second `AmountUnit` to trade against `mintv2`'s
//! BITCOIN, since a federation on the pinned rev cannot host two `mintv2`
//! instances (spec §3.2: `ModuleInitRegistry` allows only one instance per
//! `ModuleKind`, and `mintv2` hardcodes `AmountUnit::BITCOIN` with no
//! config-gen params channel to override it).
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
