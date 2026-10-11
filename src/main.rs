use clap::Parser as _;
use lightspeed_provider_smolvm::{
    ProviderArgs, Store, reconcile::reconcile, server, smolvm::SmolvmCli,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let config = ProviderArgs::parse().load().await?;
    let store = Store::open(config.state_dir()).await?;
    let backend = SmolvmCli::new(&config, store.clone());
    let report = reconcile(&config, &store, &backend).await?;
    tracing::info!(?report, "reconciled ownership store with smolvm");
    server::serve(server::App::new(config, backend, store)).await
}
