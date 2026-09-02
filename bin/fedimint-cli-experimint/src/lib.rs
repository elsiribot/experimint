//! `fedimint-cli` built with the experimint client module set.
//!
//! The client-side counterpart of `fedimintd-experimint`, and a thin wrapper in
//! the same sense: it supplies the client module inits for the kinds that
//! binary's federations run, and otherwise inherits every flag, subcommand and
//! output format from the platform branch's [`FedimintCli`]. The one verb it
//! serves itself is [`info`]; see that module for why it is intercepted from
//! argv rather than registered.
//!
//! Without it there is no way to drive `amm` or `usdt` from a command line at
//! all — a stock `fedimint-cli` links neither module, so `fedimint-cli module
//! amm ...` resolves to a module the client cannot instantiate.
//!
//! `README.md` next to this file is the operator-facing version of what
//! follows: same topology, with the invocations spelled out.
//!
//! # The intended federation topology
//!
//! | Instance | Purpose |
//! | --- | --- |
//! | `walletv2` | on-chain BTC peg-in/peg-out |
//! | `mintv2` (`amount_unit: 0`) | BTC ecash |
//! | `mintv2` (`amount_unit: 1`) | USDT ecash |
//! | `usdt` | USDT-on-EVM peg-in/peg-out (issues `AmountUnit::new_custom(1)`) |
//! | `amm` | constant-product market between units 0 and 1 |
//! | `lnv2` | Lightning |
//! | `meta` | guardian-published metadata |
//! | `multi_sig_stability_pool` | Fedi's stability pool v2 (multispend) |
//!
//! Deliberately omits the v1 `mint`/`wallet`/`ln` client modules that
//! upstream's `FedimintCli::with_default_modules` also attaches, for the same
//! reason `fedimintd-experimint` omits their server halves: a multi-unit
//! federation is a v2-only story, and the v1 modules predate `AmountUnit`.
//! `fedimint-cli`'s own v1-era top-level subcommands resolve their module by
//! kind, which against this topology fails in two different ways. `spend`,
//! `reissue` and friends ask for kind `mint` and report "No modules found of
//! kind mint"; they would fail identically even if their inits were attached,
//! since the federation runs no v1 instances, so the message is expected rather
//! than a symptom. `info` is the one that does *not* fail: on the pinned rev it
//! falls back from `mint`/`wallet` to `mintv2`/`walletv2` and prints a plausible
//! answer, having resolved the mint to the lowest-id `mintv2` instance — the
//! bitcoin one. Being wrong quietly is worse than failing loudly, which is why
//! [`info`] replaces it.
//!
//! # Two `mintv2` instances
//!
//! The topology above runs two instances of one kind, which affects the client
//! in two places.
//!
//! **Registration is unaffected.** [`ClientModuleInitRegistry`] maps kind ->
//! *init* — one implementation per kind, which is why `attach` asserts on a
//! duplicate kind. How many *instances* of a kind exist is decided by the
//! federation's config, and the client builds one module per config entry from
//! the single init. Same distinction as on the server side.
//!
//! **Primary-module selection resolves by unit, not by kind.** Funding inputs
//! and change/claim outputs are routed by `Client::primary_module_for_unit`,
//! over the modules that declared `PrimaryModuleSupport::Selected`;
//! `fedimint-mintv2-client` declares exactly `[self.cfg.amount_unit]`. So the
//! unit-0 mint is primary for bitcoin and the unit-1 mint for USDT, with
//! nothing to configure and no ambiguity to break. This is what lets `amm`
//! (which is primary for nothing, spec P10) fund one leg of a swap from each
//! mint.
//!
//! **Addressing one specific instance is by id.** `fedimint-cli module <kind>`
//! resolves through `Client::get_first_instance`, which returns the *lowest*
//! instance id of that kind — so `module mintv2` always reaches the BTC mint
//! and can never reach the USDT one. The pinned platform rev has no
//! `Client::get_module_by_instance`; what it does have is
//! `Client::get_module_client_dyn(instance_id)`, which is exactly what
//! `ClientCmd::Module` dispatches through, and a `ModuleSelector` that parses
//! an all-digits argument as an instance id. So the USDT mint is reachable as
//! `fedimint-cli-experimint module <id>` with the id `module` (no argument) or
//! [`info`] lists — under the topology above, `2`. The kind form is a
//! convenience that is only unambiguous for the kinds this federation runs
//! once.

pub mod info;

use fedimint_cli::FedimintCli;
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client_module::module::init::ClientModuleInit;

/// Something client module inits can be attached to, one at a time.
///
/// [`experimint_modules`] is written against this rather than against
/// [`FedimintCli`] directly because `FedimintCli` exposes no way to read back
/// the module set it was handed — so a test pinning the set this binary
/// promises would have nothing to inspect. A [`ClientModuleInitRegistry`]
/// does, and takes the same inits under the same bounds, so the list itself
/// exists in exactly one place and the test observes the same sequence of
/// attachments the binary makes.
pub trait ClientModuleSink: Sized {
    #[must_use]
    fn attach_module<T>(self, init: T) -> Self
    where
        T: ClientModuleInit + 'static + Send + Sync;
}

impl ClientModuleSink for FedimintCli {
    fn attach_module<T>(self, init: T) -> Self
    where
        T: ClientModuleInit + 'static + Send + Sync,
    {
        self.with_module(init)
    }
}

impl ClientModuleSink for ClientModuleInitRegistry {
    fn attach_module<T>(mut self, init: T) -> Self
    where
        T: ClientModuleInit + 'static + Send + Sync,
    {
        self.attach(init);
        self
    }
}

/// Attaches the experimint client module set: the v2 core modules, `meta`,
/// Fedi's stability pool v2, and this repo's `amm` and `usdt`.
///
/// One init per kind — see the module docs on why that is unrelated to how many
/// *instances* of a kind the federation ends up running.
#[must_use]
pub fn experimint_modules<S: ClientModuleSink>(sink: S) -> S {
    sink
        // Core v2 modules.
        .attach_module(fedimint_mintv2_client::MintClientInit)
        .attach_module(fedimint_walletv2_client::WalletClientInit)
        .attach_module(fedimint_lnv2_client::LightningClientInit::default())
        // Guardian-published metadata.
        .attach_module(fedimint_meta_client::MetaClientInit)
        // Fedi's stability pool v2 client — the server side of multispend
        // lives in fedimintd-experimint; this makes its accounts drivable
        // from `module multi_sig_stability_pool`.
        .attach_module(stability_pool_client::StabilityPoolClientInit::default())
        // Local modules. Both are built with their `cli` feature on, which is
        // what puts their verbs behind `module amm` / `module usdt`.
        .attach_module(fedimint_amm_client::AmmClientInit)
        .attach_module(fedimint_usdt_client::UsdtClientInit)
}

#[cfg(test)]
mod tests {
    use fedimint_core::core::ModuleKind;

    use super::*;

    /// The client must carry exactly the seven kinds this binary promises.
    ///
    /// Pins the set so that dropping a module (or silently gaining one) is a
    /// test failure rather than a CLI that reports "Module not found" against a
    /// federation that does run the module.
    #[test]
    fn registry_carries_the_experimint_module_set() {
        let kinds: Vec<String> = experimint_modules(ClientModuleInitRegistry::new())
            .kinds()
            .iter()
            .map(ToString::to_string)
            .collect();

        assert_eq!(
            kinds,
            vec![
                "amm".to_string(),
                "lnv2".to_string(),
                "meta".to_string(),
                "mintv2".to_string(),
                "multi_sig_stability_pool".to_string(),
                "usdt".to_string(),
                "walletv2".to_string(),
            ],
            "unexpected module set (kinds() is sorted)"
        );
    }

    /// The client set must match `fedimintd-experimint`'s server set.
    ///
    /// A kind on one side only is a federation the other half cannot talk to:
    /// a server module with no client is an instance this CLI reports as
    /// `UnsupportedByClient`, and a client module with no server is a verb that
    /// can never resolve. Compared against that crate's own registry rather
    /// than a second hand-written list, since a hand-written one drifts in
    /// exactly the case this test exists to catch.
    #[test]
    fn client_and_server_module_sets_agree() {
        assert_eq!(
            experimint_modules(ClientModuleInitRegistry::new()).kinds(),
            fedimintd_experimint::experimint_modules().kinds(),
        );
    }

    /// `amm` and `usdt` must both be present and distinct.
    ///
    /// `attach` panics on a duplicate kind, so a regression that made the two
    /// module families collide would fail here rather than at the first
    /// `module amm` invocation.
    #[test]
    fn local_modules_coexist() {
        let kinds = experimint_modules(ClientModuleInitRegistry::new()).kinds();

        assert!(kinds.contains(&ModuleKind::clone_from_str("amm")));
        assert!(kinds.contains(&ModuleKind::clone_from_str("usdt")));
    }
}
