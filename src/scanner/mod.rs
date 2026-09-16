pub mod orca;
pub mod raydium;

use crate::analyzer;
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
    pub fee_pct: f64,
    pub timestamp: std::time::SystemTime,
}

pub async fn run_polling_loop(
    rpc_url: &str,
    pair: &str,
    raydium_pool_id: &str,
    orca_pool_id: &str,
) -> Result<()> {
    let client = RpcClient::new(rpc_url.to_string());

    loop {
        let raydium_result = raydium::fetch_price(&client, raydium_pool_id, pair);
        let orca_result = orca::fetch_price(&client, orca_pool_id, pair);

        match (&raydium_result, &orca_result) {
            (Ok(r), Ok(o)) => {
                tracing::info!(
                    "[{}] {} price: {:.4} (base liq: {:.2}, quote liq: {:.2})",
                    r.dex,
                    r.pair,
                    r.price,
                    r.base_liquidity,
                    r.quote_liquidity
                );
                tracing::info!(
                    "[{}] {} price: {:.4} (base liq: {:.2}, quote liq: {:.2})",
                    o.dex,
                    o.pair,
                    o.price,
                    o.base_liquidity,
                    o.quote_liquidity
                );

                match analyzer::find_opportunity(r, o) {
                    Ok(opp) => {
                        tracing::info!(
                            "OPPORTUNITY: buy on {} @ {:.4}, sell on {} @ {:.4}, raw spread: {:.4}%, fee-adjusted: {:.4}%",
                            opp.buy_dex, opp.buy_price, opp.sell_dex, opp.sell_price, opp.raw_spread_pct, opp.fee_adjusted_spread_pct
                        );
                    }
                    Err(reason) => {
                        tracing::info!("No opportunity: {:?}", reason);
                    }
                }
            }
            (Err(e), _) => tracing::error!("Raydium fetch failed: {}", e),
            (_, Err(e)) => tracing::error!("Orca fetch failed: {}", e),
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
