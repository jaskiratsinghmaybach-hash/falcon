mod analyzer;
mod config;
mod executor;
mod logger;
mod scanner;

use solana_client::rpc_client::RpcClient;
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

    let jup_mint = Pubkey::from_str("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263")?;
    let amm_authority = Pubkey::from_str("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1")?;

    let diag_pubkey = Pubkey::from_str(&config.raydium_pool_id)?;
    let diag_client = RpcClient::new(config.helius_rpc_url.clone());
    let diag_account = diag_client.get_account(&diag_pubkey)?;
    tracing::info!(
        "Raydium pool account - owner: {}, data length: {}",
        diag_account.owner,
        diag_account.data.len()
    );

    scanner::run_polling_loop(
        &config.helius_rpc_url,
        &config.pair,
        &config.raydium_pool_id,
        &config.orca_pool_id,
        &config.keypair,
        &jup_mint,      // now points to JUP mint
        5,              // JUP decimals
        &amm_authority, // NOTE: this will be wrong for the new pool - see below
        50_000_000.0,
    )
    .await?;

    Ok(())
}
