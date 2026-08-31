//! The `info` verb: every module instance, and the `AmountUnit`s it deals in.
//!
//! # Why this replaces upstream's `info` rather than extending it
//!
//! [`FedimintCli`](fedimint_cli::FedimintCli) already has an `info`, and on the
//! pinned platform rev it does not fail against a v2-only federation — it falls
//! back from the v1 `mint`/`wallet` modules to `mintv2`/`walletv2`. What it does
//! instead is resolve the mint with `get_first_module`, i.e. the *lowest*
//! `mintv2` instance, so against the two-mint topology this repo's federations
//! run it silently reports the bitcoin mint's notes and never mentions the USDT
//! mint at all. That is the blindness this verb exists to remove.
//!
//! Upstream exposes no seam for replacing it. `fedimint-cli`'s whole public API
//! is [`fedimint_cli::envs`] plus `FedimintCli`'s four methods; its `Opts` and
//! `Command` types live in a private module, `FedimintCli::new` calls
//! `Opts::parse()` on the process's own argv before returning, and there is no
//! accessor for what it parsed and no builder for adding a subcommand. So the
//! only seam is argv, which is what [`Intercept`] parses.
//!
//! The alternative — vendoring upstream's `Opts`/`Command` so this crate owns
//! the top-level parse — was rejected: it would have to be re-synchronised by
//! hand every time the platform rev moves a flag or a subcommand, and a missed
//! sync is a verb that silently disappears from this binary. Interception has
//! the opposite failure mode: everything this parser does not recognise is
//! handed to upstream untouched, so upstream keeps its full surface for free and
//! a platform change can only ever affect the one verb modelled here.
//!
//! # Where the units come from
//!
//! Every unit reported here is read back from a module's own declaration, never
//! from a kind -> unit table kept in this file; see [`UnitSource`] for the three
//! declarations that exist and [`declared_units`] for the order they are tried
//! in. Two of the kinds this binary registers declare nothing on the pinned rev,
//! which the output states rather than papers over.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::builder::BoolishValueParser;
use clap::{Args, Parser, Subcommand};
use fedimint_amm_common::config::AmmClientConfig;
use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_cli::envs::{FM_CLIENT_DIR_ENV, FM_IROH_ENABLE_DHT_ENV, FM_USE_TOR_ENV};
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client::secret::RootSecretStrategy as _;
use fedimint_client::{Client, ClientHandleArc, RootSecret};
use fedimint_client_module::module::PrimaryModuleSupport;
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::Amount;
use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::core::{ModuleInstanceId, ModuleKind};
use fedimint_core::db::Database;
use fedimint_core::module::AmountUnit;
use fedimint_logging::TracingSetup;
use serde::Serialize;
use serde_json::Value;

use crate::experimint_modules;

/// The argv shapes this binary serves itself, ahead of `FedimintCli`.
///
/// Deliberately much narrower than upstream's parser: it models `info` and the
/// four global flags that change what `info` does, and nothing else. Every other
/// argv — including one this parser rejects outright — becomes
/// [`Verb::Upstream`] or a parse error, both of which mean "let upstream parse
/// the real argv itself". Nothing is rewritten or forwarded, so upstream sees
/// exactly the command line the user typed.
///
/// The consequence worth knowing is that combining `info` with a global flag not
/// listed here (`--password`, `--db-backend`, ...) falls through to upstream's
/// `info`. Those flags select guardian authentication or a non-RocksDb backend,
/// neither of which this verb offers, so the rule is "`info` plus an unmodelled
/// flag is upstream's `info`" rather than a silent mis-parse.
#[derive(Debug, Parser)]
#[command(
    name = "fedimint-cli-experimint",
    // `--help` and a bare `help` subcommand must reach upstream: its parser is
    // the one that knows the full verb list, and a help page rendered from this
    // struct would advertise `info` as the only thing the binary can do.
    disable_help_flag = true,
    disable_help_subcommand = true
)]
struct Intercept {
    #[command(flatten)]
    global: GlobalOpts,
    #[command(subcommand)]
    command: Verb,
}

/// The subset of upstream's global flags that reach [`open_client`].
///
/// Names, long forms and environment variables are kept identical to upstream's
/// so that one invocation cannot mean two different things depending on which
/// half of the binary served it. The environment variable names are imported
/// from [`fedimint_cli::envs`] rather than spelled out, so they cannot drift.
#[derive(Debug, Args)]
struct GlobalOpts {
    /// The working directory of the client containing the config and db.
    #[arg(long = "data-dir", env = FM_CLIENT_DIR_ENV)]
    data_dir: Option<PathBuf>,

    /// Activate usage of Tor as the Connector when building the Client.
    #[arg(long, env = FM_USE_TOR_ENV, value_parser = BoolishValueParser::new())]
    use_tor: bool,

    /// Enable using DHT name resolution in Iroh.
    #[arg(long, env = FM_IROH_ENABLE_DHT_ENV, value_parser = BoolishValueParser::new())]
    iroh_enable_dht: Option<bool>,

    /// Activate more verbose logging, for full control use the RUST_LOG env
    /// variable.
    #[arg(short = 'v', long)]
    verbose: bool,
}

#[derive(Debug, Subcommand)]
enum Verb {
    /// Describe the joined federation: every module instance, its id and kind,
    /// and the amount units it deals in.
    ///
    /// Replaces `fedimint-cli`'s `info`, which resolves the mint by kind and so
    /// reports only the lowest-id `mintv2` instance — against a federation with
    /// one mint per unit that is a balance for one asset and silence about the
    /// rest.
    ///
    /// Units are read back from the modules themselves: a mint reports the unit
    /// it is primary for, `amm` its configured allowlist, `usdt` the constant
    /// its own consensus logic credits in. `units_source` names which of those
    /// answered, and is `undeclared` for a module that publishes no unit at all
    /// rather than being filled in from a table.
    ///
    /// Balances are per unit, in that unit's own base denomination, and are
    /// attributed to the instance that is primary for the unit.
    Info {
        // Declared by hand, with a doc comment short enough to render as help
        // text, because clap propagates the top-level `disable_help_flag` to
        // every subcommand and that setting is not optional here: `--help`
        // typed on its own has to reach upstream's parser, which is the only
        // one that knows the full verb list.
        /// Print this help page.
        #[allow(dead_code, reason = "clap consumes it via `ArgAction::Help`")]
        #[arg(short = 'h', long, action = clap::ArgAction::Help)]
        help: Option<bool>,
    },

    /// Everything else, handed back to `fedimint-cli` untouched.
    ///
    /// The captured argv is deliberately not read on the delegation path:
    /// upstream re-parses the process's own argv, so this crate never becomes a
    /// forwarding layer that could reorder, drop or re-quote a flag. clap still
    /// requires an external subcommand to bind its arguments somewhere, and the
    /// tests read the binding to pin that the capture is raw.
    #[allow(dead_code, reason = "clap requires the binding; only tests read it")]
    #[command(external_subcommand)]
    Upstream(Vec<OsString>),
}

/// Which module API said what an instance's [`AmountUnit`]s are.
///
/// Reported next to the units so that an empty list stays distinguishable from a
/// module that has no way to answer. No variant here is a kind -> unit table:
/// each is a value the module itself publishes, so a module that changes its
/// unit changes what `info` prints, with nothing to keep in sync by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum UnitSource {
    /// `ClientModule::supports_being_primary` returned `Selected`. The
    /// authoritative source where it exists: it is the same value
    /// `Client::primary_module_for_unit` routes funding by, so a module
    /// answering here is answering with what actually decides behaviour.
    /// `fedimint-mintv2-client` returns its configured `amount_unit`.
    PrimaryModuleSupport,

    /// `supports_being_primary` returned `Any`: the module offers to be primary
    /// for whatever unit it is asked about, so there is no list to enumerate and
    /// `units` is empty for a reason opposite to `undeclared`.
    AnyUnit,

    /// The module's own client config, as agreed by the guardians at config-gen:
    /// `amm`'s `units` allowlist.
    ModuleConfig,

    /// A constant the module's `-common` crate exports as its unit of account:
    /// `usdt`'s `USDT_UNIT`.
    ModuleConstant,

    /// Nothing on the pinned platform rev declares this instance's units.
    /// `walletv2` and `lnv2` transact in bitcoin by construction, but neither
    /// says so through any client-side API, and inventing the answer here is
    /// exactly the table this command refuses to keep.
    Undeclared,
}

/// Whether this binary registered a client module for an instance's kind.
///
/// Same distinction `fedimint-cli`'s bare `module` verb draws: an instance the
/// federation runs but this client cannot build is listed, since its id is still
/// part of the topology, but nothing can be asked of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ModuleStatus {
    Active,
    UnsupportedByClient,
}

#[derive(Debug, Serialize)]
struct ModuleInstanceInfo {
    /// Fixed by `--module` flag order at config generation, and the only way to
    /// address one specific instance of a kind the federation runs twice.
    id: ModuleInstanceId,
    kind: ModuleKind,
    status: ModuleStatus,
    units: BTreeSet<AmountUnit>,
    units_source: UnitSource,
}

#[derive(Debug, Serialize)]
struct UnitInfo {
    unit: AmountUnit,
    /// Instance that funds inputs and takes change in this unit. `None` means
    /// some module deals in the unit but no module offered to be primary for
    /// it, which leaves the unit unspendable.
    primary_module: Option<ModuleInstanceId>,
    /// Held balance, in the unit's own base denomination — msats for bitcoin,
    /// whatever the issuing module counts in otherwise. `Amount` carries no
    /// unit, so this number is only meaningful next to `unit`. `None` exactly
    /// when `primary_module` is, since the balance is the primary module's.
    balance: Option<Amount>,
}

#[derive(Debug, Serialize)]
struct Info {
    federation_id: FederationId,
    federation_name: Option<String>,
    modules: Vec<ModuleInstanceInfo>,
    /// Every unit any instance declared, deduplicated and ascending by id.
    units: Vec<UnitInfo>,
}

/// Runs `info` if that is what argv asks for, otherwise `None` for "upstream's".
///
/// Must be called before `FedimintCli::new`, which parses argv and initialises
/// tracing as a side effect of construction. Returning `None` leaves both of
/// those still to happen, which is what makes delegation transparent.
pub async fn try_handle() -> Option<anyhow::Result<Value>> {
    match Intercept::try_parse() {
        Ok(Intercept {
            global,
            command: Verb::Info { .. },
        }) => Some(run(global).await),

        // `disable_help_flag` and `disable_help_subcommand` leave `info` as the
        // only place this parser can be asked for a help page — the binary's own
        // `--help` fails as an unknown argument instead, and reaches upstream
        // that way. So a `DisplayHelp` here is unambiguously `info --help`, and
        // must be printed rather than delegated: delegating would render
        // upstream's page for the command this one replaces. `Error::exit`
        // prints to stdout and exits `0` for this kind, which is clap's own
        // convention and what every other verb of this binary already does.
        Err(err) if err.kind() == clap::error::ErrorKind::DisplayHelp => err.exit(),

        Ok(_) | Err(_) => None,
    }
}

async fn run(global: GlobalOpts) -> anyhow::Result<Value> {
    // Same base levels upstream picks, and for the same reason: the client emits
    // its progress at `debug`, so `-v` is what turns a hung API call into
    // something diagnosable.
    TracingSetup::default()
        .with_base_level(if global.verbose { "debug" } else { "info" })
        .init()
        .context("tracing initializes")?;

    let client = open_client(&global).await?;

    let info = describe(&client).await?;

    Ok(serde_json::to_value(info).expect("Info is serializable"))
}

/// Opens the client the data dir has already joined to a federation.
///
/// `fedimint-cli`'s equivalent is a private method on `FedimintCli`, so this is
/// the same sequence over the public `fedimint-client` API, narrowed to what a
/// read-only command needs: no admin credentials, because `info` calls no
/// authenticated endpoint, and no `--federation-secret-hex`, because that flag
/// exists to seed a database that has *not* joined yet.
async fn open_client(global: &GlobalOpts) -> anyhow::Result<ClientHandleArc> {
    let data_dir = global
        .data_dir
        .as_ref()
        .context("`--data-dir=` argument not set.")?;

    // Same filename and same default-on backend as upstream. A data dir written
    // by `fedimint-cli-experimint join` is therefore readable here and vice
    // versa; `--db-backend` is not modelled, so a redb data dir falls through to
    // upstream's `info` rather than being misread as RocksDb.
    let db: Database = fedimint_rocksdb::RocksDb::build(data_dir.join("client.db"))
        .open()
        .await
        .context("could not open rocksdb database")?
        .into();

    // The secret is not what `info` reads, but opening a client derives every
    // module's keys from it, so there is no lighter way in.
    let entropy = Client::load_decodable_client_secret_opt::<Vec<u8>>(&db)
        .await?
        .context("Encoded client secret not present in DB")?;

    let root_secret = RootSecret::StandardDoubleDerive(
        Bip39RootSecretStrategy::<12>::to_root_secret(&Mnemonic::from_entropy(&entropy)?),
    );

    let iroh_enable_dht = global.iroh_enable_dht.unwrap_or(true);

    let mut builder = Client::builder()
        .await?
        .with_iroh_enable_dht(iroh_enable_dht);

    // The same registry the rest of the binary hands `FedimintCli`, so an
    // instance is `UnsupportedByClient` here exactly when `module <id>` would
    // also refuse it.
    builder.with_module_inits(experimint_modules(ClientModuleInitRegistry::new()));

    let connectors = ConnectorRegistry::build_from_client_defaults()
        .iroh_pkarr_dht(iroh_enable_dht)
        .ws_force_tor(global.use_tor)
        .bind()
        .await?;

    Ok(Arc::new(builder.open(connectors, db, root_secret).await?))
}

async fn describe(client: &Client) -> anyhow::Result<Info> {
    let config = client.config().await;

    let mut modules = Vec::new();
    let mut declared = BTreeSet::new();

    for (&id, module_cfg) in &config.modules {
        let instance = describe_instance(client, &config, id, &module_cfg.kind)?;

        declared.extend(instance.units.iter().copied());
        modules.push(instance);
    }

    let mut units = Vec::new();

    for unit in declared {
        // Routing and balance are the client's view of a unit rather than any
        // one module's, and both go through the primary module, so a unit no
        // module offered to be primary for has neither.
        let (primary_module, balance) = match client.primary_module_for_unit(unit) {
            Some((id, _)) => (Some(id), Some(client.get_balance_for_unit(unit).await?)),
            None => (None, None),
        };

        units.push(UnitInfo {
            unit,
            primary_module,
            balance,
        });
    }

    Ok(Info {
        federation_id: client.federation_id(),
        federation_name: config.global.federation_name().map(ToOwned::to_owned),
        modules,
        units,
    })
}

fn describe_instance(
    client: &Client,
    config: &ClientConfig,
    id: ModuleInstanceId,
    kind: &ModuleKind,
) -> anyhow::Result<ModuleInstanceInfo> {
    // An instance whose kind this binary did not register has no client module
    // to ask, and its config stays undecoded for want of a decoder, so both unit
    // sources that could answer for it are unavailable.
    let (status, (units, units_source)) = if client.has_module(id) {
        (
            ModuleStatus::Active,
            declared_units(client, config, id, kind)?,
        )
    } else {
        (
            ModuleStatus::UnsupportedByClient,
            (BTreeSet::new(), UnitSource::Undeclared),
        )
    };

    Ok(ModuleInstanceInfo {
        id,
        kind: kind.clone(),
        status,
        units,
        units_source,
    })
}

/// The units instance `id` deals in, and which declaration said so.
///
/// Tried in order of how load-bearing the answer is.
/// `supports_being_primary` comes first because it is the only unit declaration
/// the platform puts on `IClientModule` *and* the one the client routes funding
/// by, so it can never disagree with what the wallet actually does.
///
/// The two fallbacks exist because a module can deal in a unit without offering
/// to be primary for it. `amm` is primary for nothing by design (spec P10) yet
/// only accepts the units its config allowlists, and `usdt` holds the reserves
/// backing `USDT_UNIT` while the ecash denominated in it lives in a `mintv2`
/// instance. Both are still the module's own published value — change the
/// allowlist or the constant and this output follows — which is the property a
/// table written here would not have.
fn declared_units(
    client: &Client,
    config: &ClientConfig,
    id: ModuleInstanceId,
    kind: &ModuleKind,
) -> anyhow::Result<(BTreeSet<AmountUnit>, UnitSource)> {
    match client.get_module_client_dyn(id)?.supports_being_primary() {
        PrimaryModuleSupport::Selected { units, .. } => {
            return Ok((units, UnitSource::PrimaryModuleSupport));
        }
        PrimaryModuleSupport::Any { .. } => return Ok((BTreeSet::new(), UnitSource::AnyUnit)),
        PrimaryModuleSupport::None => {}
    }

    if *kind == fedimint_amm_common::KIND {
        let units = config
            .get_module::<AmmClientConfig>(id)?
            .units
            .keys()
            .copied()
            .collect();

        return Ok((units, UnitSource::ModuleConfig));
    }

    if *kind == fedimint_usdt_common::KIND {
        return Ok((
            BTreeSet::from([fedimint_usdt_common::USDT_UNIT]),
            UnitSource::ModuleConstant,
        ));
    }

    Ok((BTreeSet::new(), UnitSource::Undeclared))
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{Intercept, Verb};

    /// Turns an argv tail into the words [`Intercept`] sees, argv[0] included.
    fn parse(args: &[&str]) -> clap::error::Result<Intercept> {
        Intercept::try_parse_from(
            std::iter::once("fedimint-cli-experimint").chain(args.iter().copied()),
        )
    }

    /// `info` must be recognised whether the data dir comes before it as a flag
    /// or from the environment, since both are how upstream accepts it.
    #[test]
    fn info_is_intercepted() {
        for args in [
            vec!["info"],
            vec!["--data-dir", "/tmp/wallet", "info"],
            vec!["-v", "--data-dir", "/tmp/wallet", "info"],
            // Both spellings upstream uses: `--use-tor` takes no value,
            // `--iroh-enable-dht` requires one. Mirroring that is the point of
            // `GlobalOpts`, so it is worth pinning.
            vec!["--use-tor", "info"],
            vec!["--iroh-enable-dht", "false", "info"],
        ] {
            let parsed = parse(&args).expect("global flags and `info` parse");

            assert!(
                matches!(parsed.command, Verb::Info { .. }),
                "`{args:?}` did not reach the intercepted `info`"
            );
        }
    }

    /// Upstream verbs must survive interception byte for byte, hyphens
    /// included. `module <id> <verb> --flag` is the shape every module verb in
    /// this repo is driven with, and clap would be within its rights to reject
    /// `--unit-in` as an unknown flag if the catch-all did not take its
    /// arguments raw.
    #[test]
    fn upstream_verbs_fall_through_unchanged() {
        for args in [
            vec!["module"],
            vec!["module", "2", "balance"],
            vec![
                "module",
                "amm",
                "swap",
                "--unit-in",
                "0",
                "--amount-in",
                "1",
            ],
            vec!["join", "fed11qgqp"],
            vec!["version-hash"],
            vec!["dev", "api", "--help"],
        ] {
            let parsed = parse(&args).expect("an upstream verb parses as the catch-all");

            let Verb::Upstream(forwarded) = parsed.command else {
                panic!("`{args:?}` was intercepted instead of being left to upstream");
            };

            assert_eq!(
                forwarded, args,
                "the catch-all altered the argv it captured"
            );
        }
    }

    /// A global flag this parser does not model must produce an error, because
    /// an error is what makes the caller delegate. Silently accepting one would
    /// mean serving `info` while ignoring a flag the user asked for.
    #[test]
    fn unmodelled_globals_fail_to_parse() {
        for args in [
            vec!["--our-id", "0", "info"],
            vec!["--password", "hunter2", "info"],
            vec!["--db-backend", "cursed-redb", "info"],
            vec!["--help"],
            vec![],
        ] {
            parse(&args)
                .expect_err("an argv this parser does not model must fail so it falls through");
        }
    }

    /// `info --help` must render from this crate's doc comments, not upstream's
    /// one-line "Display wallet info (holdings, tiers)" — that description
    /// belongs to the command this one replaces.
    #[test]
    fn info_help_describes_this_command() {
        let err = parse(&["info", "--help"]).expect_err("--help short-circuits");

        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);

        let help = err.to_string();

        for phrase in ["units_source", "undeclared", "base denomination"] {
            assert!(
                help.contains(phrase),
                "`info --help` is missing `{phrase}`:\n{help}"
            );
        }
    }

    /// `try_handle` tells `info --help` apart from the binary's own `--help`
    /// only by the error kind, so the binary's own must not be `DisplayHelp`.
    /// If clap ever started rendering a page here, every `--help` would print
    /// this parser's one-verb usage instead of upstream's full command list.
    #[test]
    fn the_binarys_own_help_is_not_a_help_page() {
        for args in [
            vec!["--help"],
            vec!["-h"],
            vec!["help"],
            vec!["help", "info"],
        ] {
            let kind = parse(&args).err().map(|err| err.kind());

            assert_ne!(
                kind,
                Some(clap::error::ErrorKind::DisplayHelp),
                "`{args:?}` rendered a help page from this parser"
            );
        }
    }
}
