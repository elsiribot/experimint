use std::process::exit;

use fedimint_cli::FedimintCli;
use fedimint_cli_experimint::{experimint_modules, info};
use fedimint_core::fedimint_build_code_version_env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Must come first: `FedimintCli::new` parses argv and initializes tracing as
    // a side effect of construction, so once it has run there is nothing left to
    // intercept. `try_handle` returning `None` leaves both untouched, which is
    // what makes delegation invisible to upstream.
    if let Some(result) = info::try_handle().await {
        // Reproduces `FedimintCli::run`'s output contract rather than using
        // `?`: pretty JSON on stdout either way, errors as a `{"error": ...}`
        // object rather than a Rust panic message, and `exit(1)` on failure. A
        // caller must not be able to tell which half of the binary served the
        // command.
        match result {
            Ok(value) => println!("{}", pretty(&value)),
            Err(err) => {
                println!(
                    "{}",
                    pretty(&serde_json::json!({ "error": format!("{err:#}") }))
                );

                exit(1);
            }
        }

        return Ok(());
    }

    // `run` prints its own errors and `exit(1)`s, which is why it returns `()`
    // rather than a `Result`.
    experimint_modules(FedimintCli::new(fedimint_build_code_version_env!())?)
        .run()
        .await;

    Ok(())
}

fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).expect("a `serde_json::Value` is serializable")
}
