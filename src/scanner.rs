// Scanner: watches DEX pools, emits price updates

use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

// Well-known Raydium AMM v4 SOL/USDC pool - verifying this is live as our first test
const RAYDIUM_SOL_USDC_POOL: &str = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2";

pub fn check_pool_exists(rpc_url: &str) -> Result<()> {
    let client = RpcClient::new(rpc_url.to_string());

    let pool_pubkey =
        Pubkey::from_str(RAYDIUM_SOL_USDC_POOL).context("Invalid pool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch pool account - it may not exist or RPC failed")?;

    tracing::info!(
        "Pool account found. Owner: {}, Data length: {} bytes, Lamports: {}",
        account.owner,
        account.data.len(),
        account.lamports
    );

    Ok(())
}
