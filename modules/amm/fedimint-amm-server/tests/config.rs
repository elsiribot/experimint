use fedimint_amm_common::KIND;
use fedimint_amm_common::config::{AmmConfig, AmmConfigConsensus, ConfigError};
use fedimint_amm_server::AmmInit;
use fedimint_core::PeerId;
use fedimint_core::config::TypedServerModuleConfig;
use fedimint_server_core::{ConfigGenModuleArgs, ServerModuleInit};

fn args() -> ConfigGenModuleArgs {
    ConfigGenModuleArgs {
        network: bitcoin::Network::Regtest,
        disable_base_fees: false,
    }
}

#[test]
fn init_reports_our_module_kind() {
    assert_eq!(AmmInit::kind(), KIND);
}

/// `trusted_dealer_gen` must emit a config that passes `validate()` for every
/// peer, and all peers must agree on it.
#[test]
fn trusted_dealer_gen_emits_a_valid_config_for_every_peer() {
    let peers = (0..4).map(PeerId::from).collect::<Vec<_>>();
    let configs = AmmInit.trusted_dealer_gen(&peers, &args(), &());
    assert_eq!(configs.len(), 4);

    let mut consensus = Vec::new();
    for (_peer, cfg) in configs {
        let cfg: AmmConfig = cfg.to_typed().expect("must decode as AmmConfig");
        assert_eq!(cfg.consensus.validate(), Ok(()));
        consensus.push(cfg.consensus);
    }
    // Every peer must derive byte-identical consensus config.
    assert!(consensus.windows(2).all(|w| w[0] == w[1]));
}

/// The generator must never emit an empty unit set, which `validate` rejects.
#[test]
fn empty_units_are_rejected_by_validation() {
    let cfg = AmmConfigConsensus {
        units: Default::default(),
        default_fee_per_mille: 3,
        fee_overrides: Default::default(),
        min_fee_per_mille: 1,
        max_fee_per_mille: 50,
    };
    assert_eq!(cfg.validate(), Err(ConfigError::NoUnits));
}

/// The generated fee band must actually admit the generated default fee.
/// A band that excludes it is legal (`validate` only requires
/// `min <= max < 1000`) and the server clamps rather than fails, so nothing
/// downstream would notice a generator that shipped a default nobody could
/// ever charge.
#[test]
fn generated_default_fee_lies_inside_the_generated_band() {
    let peers = (0..4).map(PeerId::from).collect::<Vec<_>>();
    let configs = AmmInit.trusted_dealer_gen(&peers, &args(), &());
    let cfg: AmmConfig = configs[&PeerId::from(0)]
        .clone()
        .to_typed()
        .expect("must decode as AmmConfig");

    assert!(
        cfg.consensus
            .fee_in_band(cfg.consensus.default_fee_per_mille)
    );
    assert_eq!(
        cfg.consensus
            .clamp_fee_to_band(cfg.consensus.default_fee_per_mille),
        cfg.consensus.default_fee_per_mille
    );
}

/// An inverted band must fail the ceremony rather than reach the server,
/// where `clamp_fee_to_band` would panic on it.
#[test]
fn validate_config_rejects_an_inverted_fee_band() {
    let peers = (0..4).map(PeerId::from).collect::<Vec<_>>();
    let configs = AmmInit.trusted_dealer_gen(&peers, &args(), &());
    let peer0 = PeerId::from(0);

    let mut bad: AmmConfig = configs[&peer0].clone().to_typed().unwrap();
    bad.consensus.min_fee_per_mille = bad.consensus.max_fee_per_mille + 1;
    assert!(AmmInit.validate_config(&peer0, bad.to_erased()).is_err());
}

/// `get_client_config` must project the consensus config field-for-field.
#[test]
fn get_client_config_projects_consensus_fields() {
    let peers = (0..4).map(PeerId::from).collect::<Vec<_>>();
    let configs = AmmInit.trusted_dealer_gen(&peers, &args(), &());
    let cfg = &configs[&PeerId::from(0)];

    let client_cfg = AmmInit
        .get_client_config(&cfg.consensus)
        .expect("client config projection must succeed");

    let typed: AmmConfig = cfg.clone().to_typed().unwrap();
    assert_eq!(client_cfg.units, typed.consensus.units);
    assert_eq!(
        client_cfg.default_fee_per_mille,
        typed.consensus.default_fee_per_mille
    );
    assert_eq!(client_cfg.fee_overrides, typed.consensus.fee_overrides);
}

/// `validate_config` must accept a valid config and reject an invalid one.
#[test]
fn validate_config_calls_validate() {
    let peers = (0..4).map(PeerId::from).collect::<Vec<_>>();
    let configs = AmmInit.trusted_dealer_gen(&peers, &args(), &());
    let peer0 = PeerId::from(0);
    let cfg = configs[&peer0].clone();
    assert!(AmmInit.validate_config(&peer0, cfg).is_ok());

    let mut bad: AmmConfig = configs[&peer0].clone().to_typed().unwrap();
    bad.consensus.units.clear();
    assert!(AmmInit.validate_config(&peer0, bad.to_erased()).is_err());
}
