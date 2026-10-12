use clap::Parser as _;
use lightspeed_provider_smolvm::{
    ProviderArgs, Store, envd, reconcile::reconcile, server, smolvm::SmolvmCli, telemetry,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let args = ProviderArgs::parse();
    telemetry::init(args.log_format);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %args.config.display(),
        "starting provider"
    );
    let config = args.load().await?;
    tracing::info!(
        state_dir = %config.state_dir().display(),
        smolvm = %config.smolvm.binary.display(),
        templates = ?config.templates.keys().collect::<Vec<_>>(),
        envd_sha256 = config.envd_binary.as_ref().map(|binary| binary.sha256.as_str()),
        "loaded config"
    );
    let store = Store::open(config.state_dir()).await?;
    // Fail fast on a missing or mismatched envd rather than at first create.
    if let Some(binary) = &config.envd_binary {
        envd::stage(&envd::EnvdMount::new(config.state_dir(), binary)).await?;
    }
    let backend = SmolvmCli::new(&config, store.clone());
    let report = reconcile(&config, &store, &backend).await?;
    tracing::info!(?report, "reconciled ownership store with smolvm");
    server::serve(server::App::new(config, backend, store)).await
}
