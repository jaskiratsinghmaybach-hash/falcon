pub mod orca;
pub mod raydium;

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub dex: String,
    pub pair: String,
    pub price: f64,
    pub base_liquidity: f64,
    pub quote_liquidity: f64,
    pub timestamp: std::time::SystemTime,
}

pub async fn run_polling_loop(rpc_url: &str) -> Result<()> {
    let client = RpcClient::new(rpc_url.to_string());

    loop {
        match raydium::fetch_price(&client) {
            Ok(update) => {
                tracing::info!(
                    "[{}] {} price: {:.4} (base liq: {:.2}, quote liq: {:.2})",
                    update.dex,
                    update.pair,
                    update.price,
                    update.base_liquidity,
                    update.quote_liquidity
                );
            }
            Err(e) => tracing::error!("Raydium fetch failed: {}", e),
        }

        match orca::fetch_price(&client) {
            Ok(update) => {
                tracing::info!(
                    "[{}] {} price: {:.4} (base liq: {:.2}, quote liq: {:.2})",
                    update.dex,
                    update.pair,
                    update.price,
                    update.base_liquidity,
                    update.quote_liquidity
                );
            }
            Err(e) => tracing::error!("Orca fetch failed: {}", e),
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
