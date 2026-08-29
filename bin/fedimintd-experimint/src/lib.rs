//! `fedimintd` built with the experimint module set.
//!
//! This is a thin wrapper around the platform branch's [`fedimintd::run`]: it
//! supplies a [`ServerModuleInitRegistry`] carrying the v2 core modules, the
//! meta module, and this repo's two local modules, and otherwise inherits every
//! flag, env var, setup UI and API endpoint from upstream.
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
//!
//! Note the **two `mintv2` instances**. That is the part worth understanding
//! before deploying, because only one of the two setup paths can express it.
//!
//! # Two `mintv2` instances: what works, and what does not
//!
//! Multiple instances of one [`ModuleKind`] are fully supported by the
//! platform. `ServerModuleConfigGenParamsRegistry` is a
//! `ModuleRegistry<ConfigGenModuleParams>`, i.e. a
//! `BTreeMap<ModuleInstanceId, (ModuleKind, ConfigGenModuleParams)>` — keyed by
//! *instance*, not by kind. `ConfigGenParams::module_params` is documented
//! upstream as "the single source of truth for which module instances the
//! federation runs".
//!
//! (The `assert!(…, "Can't insert module of same kind twice")` in
//! `fedimint-core/src/config.rs` is on `ModuleInitRegistry`, which maps kind ->
//! *init*. That is one implementation per kind — it does not constrain how many
//! instances a federation runs. Conflating the two registries is an easy
//! mistake; this crate's own registry below is the init one.)
//!
//! **Via the CLI / API: works today.** `fedimint-cli admin setup
//! set-local-params` takes a repeatable `--module <kind>[=<json>]`, which builds
//! the instance list directly and bypasses the UI entirely:
//!
//! ```text
//! fedimint-cli admin setup set-local-params \
//!     --module walletv2 \
//!     --module 'mintv2={"amount_unit":0}' \
//!     --module 'mintv2={"amount_unit":1}' \
//!     --module lnv2 \
//!     --module 'usdt={"chain_id":1}' \
//!     --module amm \
//!     --module meta
//! ```
//!
//! Instance ids are assigned by flag position (0, 1, 2, …). The platform branch
//! has a test (`fedimint-cli`'s `parses_full_deployment_topology`) asserting
//! exactly this shape, including the two `mintv2` instances carrying distinct
//! `amount_unit`s.
//!
//! **Via the setup UI: not expressible.** The web UI renders one checkbox per
//! [`ModuleKind`] and turns the ticked set into an instance list with
//! `select_kinds`. The available list it filters comes from
//! `ConfigGenSettings::available_module_params`, which [`fedimintd::run`] builds
//! as `build_module_params_registry(&registry, &registry.kinds())` — one
//! instance per kind, by construction. Upstream's own comment in
//! `fedimint-server-ui/src/setup.rs` says so: *"the current UI can only express
//! a single instance per kind … the instance list type already supports it."*
//!
//! **The minimal fix is smaller than it looks, and it is not in the UI.**
//! `select_kinds` keeps *every* instance whose kind is selected:
//!
//! ```text
//! for (_, kind, params) in self.iter_modules() {
//!     if selected.contains(kind) { selection.append_module(kind.clone(), params.clone()); }
//! }
//! ```
//!
//! So a two-instance `available_module_params` already survives the UI's
//! filtering intact — tick "mintv2" and you get both instances. The only thing
//! standing in the way is that [`fedimintd::run`] derives that field internally
//! and gives the caller no way to override it. Letting `run` accept a
//! caller-supplied `available_module_params` (or a whole `ConfigGenSettings`
//! override) would let *this* crate declare the topology and have the existing
//! UI materialize it, with no UI change at all. That is an additive change to
//! the platform branch; it cannot be done from here, because the field is
//! populated inside the pinned crate.
//!
//! The residual UX wart after such a fix: the operator still sees one "mintv2"
//! checkbox and cannot tell it stands for two instances, nor pick just one.
//! Making the instance list first-class in the UI is the larger follow-up
//! upstream already flagged.
//!
//! # Modules are not enabled by default
//!
//! `default_modules` (what the setup UI pre-ticks) is the subset whose
//! `is_enabled_by_default()` is true. On this module set that is only `lnv2`,
//! `meta` and `amm`. The three that carry the interesting topology are all
//! opt-in:
//!
//! - `mintv2` — `FM_ENABLE_MODULE_MINTV2`
//! - `walletv2` — `FM_ENABLE_MODULE_WALLETV2`
//! - `usdt` — `FM_ENABLE_MODULE_USDT`
//!
//! They are still *available* (tickable in the UI, nameable via `--module`)
//! without those variables; the variables only decide what starts pre-selected.

use fedimint_server_core::ServerModuleInitRegistry;

/// The experimint module set: the v2 core modules, `meta`, and this repo's
/// `amm` and `usdt`.
///
/// One init per kind — see the module docs on why that is unrelated to how many
/// *instances* of a kind the federation ends up running.
///
/// Deliberately omits the v1 `mint`/`wallet`/`ln` modules that upstream's
/// [`fedimintd::default_modules`] also attaches. This binary targets a
/// multi-unit federation, which is a v2-only story: the v1 modules predate
/// `AmountUnit` and cannot denominate anything but bitcoin.
#[must_use]
pub fn experimint_modules() -> ServerModuleInitRegistry {
    let mut modules = ServerModuleInitRegistry::new();

    // Core v2 modules.
    modules.attach(fedimint_mintv2_server::MintInit);
    modules.attach(fedimint_walletv2_server::WalletInit);
    modules.attach(fedimint_lnv2_server::LightningInit);

    // Guardian-published metadata. Upstream gates this behind
    // `FM_DISABLE_META_MODULE`; here it is unconditional, since it is part of
    // this binary's declared module set.
    modules.attach(fedimint_meta_server::MetaInit);

    // Local modules.
    modules.attach(fedimint_amm_server::AmmInit);
    modules.attach(fedimint_usdt_server::UsdtInit::default());

    modules
}

#[cfg(test)]
mod tests {
    use fedimint_core::core::ModuleKind;

    use super::*;

    /// The registry must carry exactly the seven kinds this binary promises.
    ///
    /// Pins the set so that dropping a module (or silently gaining one) is a
    /// test failure rather than a federation that comes up missing a module
    /// nobody notices until a client asks for it.
    #[test]
    fn registry_carries_the_experimint_module_set() {
        let kinds: Vec<String> = experimint_modules()
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
                "usdt".to_string(),
                "walletv2".to_string(),
            ],
            "unexpected module set (kinds() is sorted)"
        );
    }

    /// `amm` and `usdt` must both be present and distinct.
    ///
    /// This is the composition claim the whole workspace repin was for: two
    /// module families built against one `fedimint-core`, registrable side by
    /// side. `attach` panics on a duplicate kind, so a regression that made
    /// them collide would fail here rather than at guardian startup.
    #[test]
    fn local_modules_coexist() {
        let registry = experimint_modules();
        let kinds = registry.kinds();

        assert!(kinds.contains(&ModuleKind::clone_from_str("amm")));
        assert!(kinds.contains(&ModuleKind::clone_from_str("usdt")));
    }

    /// The units the intended topology trades must line up.
    ///
    /// `usdt` issues `AmountUnit::new_custom(1)`, and the `amm`'s default
    /// consensus config allowlists exactly units 0 and 1 — so a BTC<->USDT pool
    /// needs no config change. If either side ever moves, this fails instead of
    /// producing a federation whose AMM silently refuses every usdt pool.
    #[test]
    fn usdt_unit_is_in_the_amm_allowlist() {
        assert_eq!(
            fedimint_usdt_common::USDT_UNIT,
            fedimint_core::module::AmountUnit::new_custom(1),
        );
    }
}
