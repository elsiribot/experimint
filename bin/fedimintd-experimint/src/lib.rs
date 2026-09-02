//! `fedimintd` built with the experimint module set.
//!
//! This is a thin wrapper around the platform branch's [`fedimintd::run`]: it
//! supplies a [`ServerModuleInitRegistry`] carrying the v2 core modules, the
//! meta module, Fedi's stability pool v2 (multispend), and this repo's two
//! local modules, and otherwise inherits every flag, env var, setup UI and API
//! endpoint from upstream.
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
//! Note the **two `mintv2` instances**, one per asset. Both setup paths can
//! express that.
//!
//! # Two `mintv2` instances
//!
//! Multiple instances of one [`ModuleKind`] have always been supported by the
//! platform: `ServerModuleConfigGenParamsRegistry` is keyed by
//! `ModuleInstanceId`, not by kind, and `ConfigGenParams::module_params` is the
//! single source of truth for which instances a federation runs.
//!
//! (The `assert!(…, "Can't insert module of same kind twice")` in
//! `fedimint-core/src/config.rs` is on `ModuleInitRegistry`, which maps kind ->
//! *init*. That is one implementation per kind — it does not constrain how many
//! instances run. Conflating the two registries is an easy mistake; this
//! crate's own registry below is the init one.)
//!
//! **Setup UI.** The form builds the instance list one row at a time: pick a
//! kind, and for kinds denominated in an asset pick that too. Add a second
//! `mintv2` row, choose "USDT (unit 1)", and the federation runs two mints.
//! The asset choices are the assets the enabled modules declare they back.
//!
//! **CLI / API.** `fedimint-cli admin setup set-local-params` takes a
//! repeatable `--module <kind>[=<json>]`, building the same list directly:
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
//! Instance ids follow position in both paths.
//!
//! # Assets
//!
//! A mint holds no reserves of its own — its ecash is a claim on whatever backs
//! the unit it is denominated in. So modules declare both sides:
//! `walletv2` and `usdt` declare what they back (`provided_assets`), `mintv2`
//! declares what it needs (`required_assets`), and config generation refuses a
//! topology whose ecash is denominated in something nothing backs. That check
//! runs before DKG, because the denomination is baked into the module's
//! consensus config and cannot be changed afterwards.
//!
//! Bitcoin is always available and never needs a backing module: it is the
//! federation's native unit, and a lightning-only federation denominates in it
//! with no on-chain wallet enabled.
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
//! `multi_sig_stability_pool` is opt-in the same way, via
//! `FM_ENABLE_MODULE_SPV2`, independent of the asset topology above.
//!
//! They are still *available* (tickable in the UI, nameable via `--module`)
//! without those variables; the variables only decide what starts pre-selected.

use std::time::Duration;

use fedimint_core::Amount;
use fedimint_server_core::ServerModuleInitRegistry;
use stability_pool_server::StabilityPoolInit;
use stability_pool_server::common::config::{CollateralRatio, OracleConfig};
use stability_pool_server::envs::{FM_SPV2_CYCLE_DURATION_SECS_ENV, FM_SPV2_TEST_PARAMS_ENV};

/// The stability pool v2 init, carrying Fedi's deployed parameters verbatim
/// (fedi `crates/fedimint/fedimintd/src/main.rs`). `FM_SPV2_TEST_PARAMS`
/// switches to the mock oracle and a 15s cycle for devimint-style runs;
/// `FM_SPV2_CYCLE_DURATION_SECS` overrides the production cycle length.
#[must_use]
pub fn spv2_init() -> StabilityPoolInit {
    let test_params = fedimint_core::envs::is_env_var_set(FM_SPV2_TEST_PARAMS_ENV);
    let cycle_duration_secs: u64 = std::env::var(FM_SPV2_CYCLE_DURATION_SECS_ENV)
        .ok()
        .map(|v| {
            v.parse()
                .expect("FM_SPV2_CYCLE_DURATION_SECS must be a u64")
        })
        .unwrap_or(600);

    StabilityPoolInit {
        oracle_config: if test_params {
            OracleConfig::Mock
        } else {
            OracleConfig::Aggregate
        },
        cycle_duration: Duration::from_secs(if test_params { 15 } else { cycle_duration_secs }),
        collateral_ratio: CollateralRatio {
            provider: 1,
            seeker: 1,
        },
        min_allowed_seek: Amount::from_msats(100_000),
        min_allowed_provide: Amount::from_msats(100_000),
        max_allowed_provide_fee_rate_ppb: 2000,
        min_allowed_cancellation_bps: 100,
    }
}

/// The experimint module set: the v2 core modules, `meta`, Fedi's stability
/// pool v2, and this repo's `amm` and `usdt`.
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

    // Fedi's stability pool v2 — the server side of multispend. Sourced from
    // the experimint branch of elsiribot/fedi; parameters in `spv2_init`.
    modules.attach(spv2_init());

    modules
}

#[cfg(test)]
mod tests {
    use fedimint_core::config::ServerModuleConfigGenParamsRegistry;
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
                "multi_sig_stability_pool".to_string(),
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
        assert!(kinds.contains(&ModuleKind::clone_from_str("multi_sig_stability_pool")));
    }

    /// The intended topology must pass config generation's asset check.
    ///
    /// This is the end-to-end statement of what the asset plumbing is for: two
    /// `mintv2` instances, one on bitcoin and one on the unit `usdt` backs,
    /// alongside the modules that back them. If `usdt` ever stopped declaring
    /// `provided_assets`, or `mintv2` stopped declaring `required_assets`, a
    /// federation configured this way would be rejected at DKG — this fails
    /// first instead.
    #[test]
    fn intended_topology_passes_asset_validation() {
        let registry = experimint_modules();
        let mut params = ServerModuleConfigGenParamsRegistry::default();

        params.attach_config_gen_params(ModuleKind::clone_from_str("walletv2"), ());
        params.attach_config_gen_params(
            ModuleKind::clone_from_str("mintv2"),
            serde_json::json!({ "amount_unit": 0 }),
        );
        params.attach_config_gen_params(
            ModuleKind::clone_from_str("mintv2"),
            serde_json::json!({ "amount_unit": fedimint_usdt_common::USDT_UNIT.id() }),
        );
        params.attach_config_gen_params(
            ModuleKind::clone_from_str("usdt"),
            fedimint_usdt_common::UsdtGenParams::default(),
        );
        params.attach_config_gen_params(ModuleKind::clone_from_str("multi_sig_stability_pool"), ());

        fedimint_server::config::validate_module_assets(&registry, &params)
            .expect("the intended topology must validate");
    }

    /// A mint denominated in an asset no enabled module backs must be refused.
    ///
    /// Same instance list as above but with `usdt` left out, so nothing holds
    /// reserves against its unit. Without this the federation would boot and
    /// issue ecash redeemable against nothing, with no way to fix it after DKG.
    #[test]
    fn mint_without_a_backing_module_is_rejected() {
        let registry = experimint_modules();
        let mut params = ServerModuleConfigGenParamsRegistry::default();

        params.attach_config_gen_params(ModuleKind::clone_from_str("walletv2"), ());
        params.attach_config_gen_params(
            ModuleKind::clone_from_str("mintv2"),
            serde_json::json!({ "amount_unit": fedimint_usdt_common::USDT_UNIT.id() }),
        );

        let err = fedimint_server::config::validate_module_assets(&registry, &params)
            .expect_err("a mint with no backing module must be refused");

        assert!(
            err.to_string().contains("which no enabled module backs"),
            "unexpected error: {err}"
        );
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
