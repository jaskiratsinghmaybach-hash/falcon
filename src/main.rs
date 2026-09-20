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

    let client = RpcClient::new(config.helius_rpc_url.clone());
    let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")?;

    let pool_info = scanner::raydium_cpmm::fetch_pool_info(&client, &config.raydium_pool_id)?;

    // Tiny test amount - proving the instruction mechanics work, not a real trade.
    let amount_in = 1_000_000; // 0.001 SOL
    let minimum_amount_out = 1;

    match executor::simulate_cpmm_swap(
        &client,
        &config.keypair,
        &pool_info,
        &wsol_mint,
        amount_in,
        minimum_amount_out,
    ) {
        Ok(_) => tracing::info!("CPMM simulation completed"),
        Err(e) => tracing::error!("CPMM simulation failed: {}", e),
    }

    Ok(())
}
