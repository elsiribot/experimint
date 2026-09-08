//! Shared `fedimint-testing` fixture for the multispend integration tests: a
//! federation running `mintv2` (issuing `AmountUnit::BITCOIN`, primary
//! module), `dummy` (bootstrapping BITCOIN from nothing), `multispend`
//! itself, and the test-only `msfaucet` (bootstrapping a second, custom
//! `AmountUnit`).
//!
//! `dummy` is included purely to bootstrap BITCOIN into a wallet, the same
//! way `fedimint-amm-tests`'s fixture does: `DummyInput` is an unconditional
//! "value from nothing" input `fedimint-dummy-server` accepts with no
//! balance check at all (see its `process_input`). `mintv2` has no such
//! primitive of its own — a real federation gets BITCOIN from Lightning or
//! an on-chain peg-in, neither of which `fedimint-testing`'s in-process
//! federation runs. `mintv2` is registered as the *primary* module purely so
//! `Fixtures::new_primary` (which every fixture needs some primary module
//! for) and `FederationTest::new_client` have something to build an ordinary
//! client against; no test in this crate ever spends a `mintv2` note by
//! hand — every deposit and withdrawal here goes through `dummy` (BITCOIN)
//! or `msfaucet` (the second unit), and every `multispend` transaction is
//! assembled by hand rather than through a client, per spec §9.3.
//!
//! `multispend` and `msfaucet` are both registered via
//! [`Fixtures::with_server_only_module`]: neither module has a client
//! (`multispend` never will — see the crate-level module doc comment on
//! `fedimint-multispend-server` — and `msfaucet`'s client half was
//! deliberately not copied, see `crate::faucet`'s module doc comment), and
//! every transaction touching either module in this crate's tests is
//! assembled by hand and submitted directly.

use fedimint_dummy_client::DummyClientInit;
use fedimint_dummy_server::DummyInit;
use fedimint_mintv2_client::MintClientInit;
use fedimint_mintv2_server::MintInit;
use fedimint_multispend_server::MultispendInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;

use crate::faucet::server::FaucetInit;

pub fn fixtures() -> Fixtures {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit);
    let fixtures = fixtures.with_module(DummyClientInit, DummyInit);
    let fixtures = fixtures.with_server_only_module(MultispendInit);
    fixtures.with_server_only_module(FaucetInit)
}

/// A federation built from [`fixtures`] with all four peers online.
pub async fn new_federation() -> FederationTest {
    fixtures().new_fed_builder(0).build().await
}
