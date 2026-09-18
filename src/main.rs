mod analyzer;
mod config;
mod executor;
mod scanner;

use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer;
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

    // Temporary check: does our wallet have the token accounts needed to trade RAY/SOL?
    let client = RpcClient::new(config.helius_rpc_url.clone());
    let ray_mint = Pubkey::from_str("4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R")?;
    executor::check_wallet_atas(&client, &config.keypair.pubkey(), &ray_mint)?;

    scanner::run_polling_loop(
        &config.helius_rpc_url,
        &config.pair,
        &config.raydium_pool_id,
        &config.orca_pool_id,
    )
    .await?;

    Ok(())
}
