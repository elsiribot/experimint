//! Shared `fedimint-testing` fixture for the AMM integration tests: a
//! federation running `mintv2` (issuing `AmountUnit::BITCOIN`), the test-only
//! `faucet` module (issuing a second, custom `AmountUnit` — spec §3.2), `amm`
//! itself, and `dummy`. Mirrors the `fixtures()` helper every
//! `fedimint-*-tests` crate at the pinned rev defines (e.g.
//! `fedimint-mintv2-tests/tests/tests.rs:107-111`).
//!
//! `dummy` is included purely to bootstrap BITCOIN into a wallet the same way
//! `fedimint-mintv2-tests`'s own `issue_ecash` helper does
//! (`DummyClientModule::create_input`, an unconditional "value from nothing"
//! input `fedimint-dummy-server` accepts with no balance check at all — see
//! its `process_input`). `mintv2` has no such primitive of its own; a real
//! federation gets BITCOIN from Lightning or an on-chain peg-in, neither of
//! which `fedimint-testing`'s in-process federation runs. `dummy`'s presence
//! here has nothing to do with the "second unit" problem `faucet` solves —
//! it only ever mints `AmountUnit::BITCOIN`.
//!
//! `dummy` *is* registered as a primary-module candidate for every unit —
//! `DummyClientInit::supports_being_primary` returns `PrimaryModuleSupport::
//! Any { priority: LOW }` (`fedimint-dummy-client/src/lib.rs:154-158`), a
//! wildcard match at the lowest priority. It never actually wins that role
//! here, though: `mintv2` and `faucet` both register a `Selected` match for
//! their own unit at the same `LOW` priority, and
//! `ClientBuilder::primary_modules_for_unit` orders wildcard candidates after
//! specific ones within a priority tier (`fedimint-client/src/client.rs`'s
//! own comment: "within same priority, wildcard matches come last") — see
//! this crate's own
//! `faucet_is_primary_for_its_unit_and_mintv2_for_bitcoin` test, which
//! asserts exactly this ordering rather than dummy's absence from
//! consideration.

use fedimint_amm_client::AmmClientInit;
use fedimint_amm_server::AmmInit;
use fedimint_dummy_client::DummyClientInit;
use fedimint_dummy_server::DummyInit;
use fedimint_mintv2_client::MintClientInit;
use fedimint_mintv2_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;

use crate::faucet::client::FaucetClientInit;
use crate::faucet::server::FaucetInit;

pub fn fixtures() -> Fixtures {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit);
    let fixtures = fixtures.with_module(FaucetClientInit, FaucetInit);
    let fixtures = fixtures.with_module(AmmClientInit, AmmInit);
    fixtures.with_module(DummyClientInit, DummyInit)
}

/// A federation built from [`fixtures`] with `mintv2`'s base fee disabled.
///
/// `mintv2` charges a non-zero `fee_consensus` on issuance/reissue by
/// default in `fedimint-testing` (`enable_mint_fees: true`,
/// `fedimint-testing/src/federation.rs:281`) — the same reason
/// `fedimint-mintv2-tests`'s own `issue_ecash` helper deliberately
/// over-funds by a margin before spending exactly a smaller, round amount.
/// This module's tests instead disable the fee outright
/// (`FederationTestBuilder::disable_mint_fees`), so a mint/deposit/swap of
/// exactly `X` sats charges no *fee* on the BTC leg. That alone does not
/// make `X` leave the wallet and the pool holding exactly `X`, though:
/// `mintv2`'s client-held notes still bottom out at a 512-msat denomination
/// floor regardless of this setting
/// (`fedimint-mintv2-common::config::client_denominations`, `9..42`), so an
/// `X` not aligned to that grid silently forfeits its remainder on issuance
/// or reissue independent of any fee — see the `dust_free_sats` helper in
/// `tests/tests.rs`, which exists precisely to keep every BTC amount these
/// tests assert on exactly representable. This affects only `mintv2`'s BTC
/// leg; the test faucet charges no fee either way and has no
/// note-denomination concept at all (spec-mandated: "no cryptography" —
/// balances are a plain per-key `Amount`, not a set of fixed-size notes).
pub async fn new_federation() -> FederationTest {
    fixtures()
        .new_fed_builder(0)
        .disable_mint_fees()
        .build()
        .await
}
