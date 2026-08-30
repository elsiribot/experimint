use fedimint_cli::FedimintCli;
use fedimint_cli_experimint::experimint_modules;
use fedimint_core::fedimint_build_code_version_env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `FedimintCli::new` parses argv and initializes tracing, so it must run
    // before anything that wants either; `run` prints its own errors and
    // `exit(1)`s, which is why it returns `()` rather than a `Result`.
    experimint_modules(FedimintCli::new(fedimint_build_code_version_env!())?)
        .run()
        .await;

    Ok(())
}
