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

    tracing::info!("Falcon initialized and ready");

    scanner::run_polling_loop(
        &config.helius_rpc_url,
        &config.pair,
        &config.raydium_pool_id,
        &config.orca_pool_id,
    )
    .await?;

    Ok(())
}
