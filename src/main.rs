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

    tracing::info!("Falcon initialized and ready");

    let client = RpcClient::new(config.helius_rpc_url.clone());

    // Fetch the pool's real vault addresses first.
    let vaults = scanner::raydium::fetch_pool_vaults(&client, &config.raydium_pool_id)?;
    tracing::info!(
        "Pool coin_vault: {}, pc_vault: {}",
        vaults.coin_vault,
        vaults.pc_vault
    );

    // Stage 3 test: simulate a tiny Raydium swap (buying RAY with a small amount of SOL)
    // using our known, verified pool accounts. This does NOT send a real transaction.
    let ray_mint = Pubkey::from_str("4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R")?;
    let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")?;
    let amm_pool = Pubkey::from_str(&config.raydium_pool_id)?;
    let amm_authority = Pubkey::from_str("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1")?;

    let amount_in = 1_000_000; // 0.001 SOL in lamports, a tiny test amount
    let minimum_amount_out = 1; // essentially no slippage protection for this dry-run test

    match executor::simulate_raydium_swap(
        &client,
        &config.keypair,
        &amm_pool,
        &amm_authority,
        &vaults.coin_vault,
        &vaults.pc_vault,
        &wsol_mint,
        &ray_mint,
        amount_in,
        minimum_amount_out,
    ) {
        Ok(_) => tracing::info!("Simulation call completed"),
        Err(e) => tracing::error!("Simulation setup failed: {}", e),
    }

    Ok(())
}
