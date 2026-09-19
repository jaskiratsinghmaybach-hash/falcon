mod analyzer;
mod config;
mod executor;
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

    tracing::info!("Falcon starting up");

    let config = config::Config::load()?;
    let client = RpcClient::new(config.helius_rpc_url.clone());

    let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")?;

    let amount_in = 1_000_000; // 0.001 SOL in lamports
    let minimum_amount_out = 1; // no slippage protection for this dry run

    // --- Orca side test ---
    let orca_pool = Pubkey::from_str(&config.orca_pool_id)?;
    let orca_info = scanner::orca::fetch_pool_info(&client, &config.orca_pool_id)?;
    tracing::info!(
        "Orca pool: tick_current={}, tick_spacing={}, mint_a={}, mint_b={}",
        orca_info.tick_current_index,
        orca_info.tick_spacing,
        orca_info.token_mint_a,
        orca_info.token_mint_b
    );

    match executor::simulate_orca_swap(
        &client,
        &config.keypair,
        &orca_pool,
        &orca_info,
        &wsol_mint,
        amount_in,
        minimum_amount_out,
    ) {
        Ok(_) => tracing::info!("Orca simulation call completed"),
        Err(e) => tracing::error!("Orca simulation setup failed: {}", e),
    }

    Ok(())
}
