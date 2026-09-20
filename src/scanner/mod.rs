pub mod orca;
pub mod raydium;
pub mod raydium_cpmm;

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Keypair};
use std::time::Duration;

use crate::analyzer;

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

#[allow(clippy::too_many_arguments)]
pub async fn run_polling_loop(
    rpc_url: &str,
    pair: &str,
    raydium_pool_id: &str,
    orca_pool_id: &str,
    _payer: &Keypair,
    _other_token_mint: &Pubkey,
    _other_token_decimals: u32,
    _trade_size_base: f64,
) -> Result<()> {
    let client = RpcClient::new(rpc_url.to_string());

    loop {
        let raydium_result = raydium_cpmm::fetch_price(&client, raydium_pool_id, pair);
        let orca_result = orca::fetch_price(&client, orca_pool_id, pair);

        match (&raydium_result, &orca_result) {
            (Ok(r), Ok(o)) => {
                tracing::info!(
                    "[{}] {} price: {:.10} (base liq: {:.2}, quote liq: {:.2})",
                    r.dex,
                    r.pair,
                    r.price,
                    r.base_liquidity,
                    r.quote_liquidity
                );
                tracing::info!(
                    "[{}] {} price: {:.10} (base liq: {:.2}, quote liq: {:.2})",
                    o.dex,
                    o.pair,
                    o.price,
                    o.base_liquidity,
                    o.quote_liquidity
                );

                match analyzer::find_best_opportunity(r, o) {
                    Ok(opp) => {
                        tracing::warn!(
                            "REAL OPPORTUNITY: buy on {} @ {:.10}, sell on {} @ {:.10}, NET PROFIT: {:.4}%",
                            opp.buy_dex, opp.buy_price, opp.sell_dex, opp.sell_price, opp.net_profit_pct
                        );

                        let _ = crate::logger::log_row(
                            pair,
                            r.price,
                            o.price,
                            r.base_liquidity,
                            r.quote_liquidity,
                            o.base_liquidity,
                            o.quote_liquidity,
                            &opp.buy_dex,
                            &opp.sell_dex,
                            opp.raw_spread_pct,
                            opp.fee_adjusted_spread_pct,
                            opp.net_spread_after_slippage_pct,
                            opp.net_profit_pct,
                            "APPROVED",
                        );
                    }
                    Err(reason) => {
                        tracing::info!("No opportunity: {:?}", reason);

                        let (buy_dex, sell_dex) = if r.price < o.price {
                            (r.dex.as_str(), o.dex.as_str())
                        } else {
                            (o.dex.as_str(), r.dex.as_str())
                        };
                        let raw_spread_pct = ((r.price.max(o.price) - r.price.min(o.price))
                            / r.price.min(o.price))
                            * 100.0;

                        let _ = crate::logger::log_row(
                            pair,
                            r.price,
                            o.price,
                            r.base_liquidity,
                            r.quote_liquidity,
                            o.base_liquidity,
                            o.quote_liquidity,
                            buy_dex,
                            sell_dex,
                            raw_spread_pct,
                            0.0,
                            0.0,
                            0.0,
                            &format!("{:?}", reason),
                        );
                    }
                }
            }
            (Err(e), _) => tracing::error!("Raydium fetch failed: {}", e),
            (_, Err(e)) => tracing::error!("Orca fetch failed: {}", e),
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
