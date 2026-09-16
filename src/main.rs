mod analyzer;
mod config;
mod executor;
mod scanner;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    tracing::info!("Falcon starting up");

    let config = config::Config::load()?;

    scanner::check_pool_exists(&config.helius_rpc_url)?;

    tracing::info!("Falcon initialized and ready");

    Ok(())
}
