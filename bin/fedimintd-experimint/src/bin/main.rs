use std::convert::Infallible;

use fedimint_core::fedimint_build_code_version_env;

#[tokio::main]
async fn main() -> anyhow::Result<Infallible> {
    fedimintd::run(
        fedimintd_experimint::experimint_modules(),
        fedimint_build_code_version_env!(),
        // Vendor suffix, appended to the reported version to distinguish this
        // binary's module set from a stock `fedimintd`. Informational only:
        // compatibility and consensus config generation use the normalized
        // `x.y.z` release version.
        Some("experimint"),
    )
    .await
}
