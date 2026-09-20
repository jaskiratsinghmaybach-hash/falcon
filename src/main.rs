mod analyzer;
mod config;
mod executor;
mod logger;
mod scanner;

use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    logger::ensure_header()?;

    tracing::info!("Falcon starting up");

    let config = config::Config::load()?;

    tracing::info!("Falcon initialized and ready");

    let jup_mint = Pubkey::from_str("JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN")?;
    let amm_authority = Pubkey::from_str("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1")?;

    scanner::run_polling_loop(
        &config.helius_rpc_url,
        &config.pair,
        &config.raydium_pool_id,
        &config.orca_pool_id,
        &config.keypair,
        &jup_mint,      // now points to JUP mint
        6,              // JUP decimals
        &amm_authority, // NOTE: this will be wrong for the new pool - see below
        50.0,
    )
    .await?;

    Ok(())
}
