mod analyzer;
mod config;
mod executor;
mod logger;
mod scanner;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    logger::ensure_header()?;

    tracing::info!("Falcon starting up");

    let config = config::Config::load()?;

    tracing::info!("Falcon initialized and ready - starting real-time engine");

    scanner::run_realtime_loop(
        &config.helius_ws_url,
        &config.helius_rpc_url,
        &config.pair,
        &config.raydium_pool_id,
        &config.orca_pool_id,
        50_000_000.0,
    )
    .await?;

    Ok(())
}
