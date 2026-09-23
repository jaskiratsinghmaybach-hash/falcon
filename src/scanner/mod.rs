pub mod orca;
pub mod raydium;
pub mod raydium_cpmm;
pub mod realtime;

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Keypair};
use std::sync::mpsc;
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

enum Source {
    Raydium,
    Orca,
}

struct RealtimeEvent {
    source: Source,
    update: realtime::AccountUpdate,
}

/// Real-time event-driven loop: reacts the instant either pool's account changes,
/// instead of polling on a timer. Decodes directly from the WebSocket payload.
pub async fn run_realtime_loop(
    ws_url: &str,
    rpc_url: &str,
    pair: &str,
    raydium_pool_id: &str,
    orca_pool_id: &str,
    _trade_size_hint: f64,
) -> Result<()> {
    let client = RpcClient::new(rpc_url.to_string());
    let (tx, rx) = mpsc::channel::<RealtimeEvent>();

    let ws_url_raydium = ws_url.to_string();
    let raydium_pool_id_owned = raydium_pool_id.to_string();
    let tx_raydium = tx.clone();
    std::thread::spawn(move || {
        let (inner_tx, inner_rx) = mpsc::channel();
        let ws_url_clone = ws_url_raydium.clone();
        let pool_clone = raydium_pool_id_owned.clone();
        std::thread::spawn(move || {
            if let Err(e) = realtime::subscribe_to_account(&ws_url_clone, &pool_clone, inner_tx) {
                tracing::error!("Raydium subscription ended: {}", e);
            }
        });
        for update in inner_rx {
            if tx_raydium
                .send(RealtimeEvent {
                    source: Source::Raydium,
                    update,
                })
                .is_err()
            {
                break;
            }
        }
    });

    let ws_url_orca = ws_url.to_string();
    let orca_pool_id_owned = orca_pool_id.to_string();
    let tx_orca = tx;
    std::thread::spawn(move || {
        let (inner_tx, inner_rx) = mpsc::channel();
        let ws_url_clone = ws_url_orca.clone();
        let pool_clone = orca_pool_id_owned.clone();
        std::thread::spawn(move || {
            if let Err(e) = realtime::subscribe_to_account(&ws_url_clone, &pool_clone, inner_tx) {
                tracing::error!("Orca subscription ended: {}", e);
            }
        });
        for update in inner_rx {
            if tx_orca
                .send(RealtimeEvent {
                    source: Source::Orca,
                    update,
                })
                .is_err()
            {
                break;
            }
        }
    });

    tracing::info!("Real-time WebSocket subscriptions started for {}", pair);

    let mut last_raydium: Option<PriceUpdate> = None;
    let mut last_orca: Option<PriceUpdate> = None;

    // Seed both sides once via a normal RPC call so we have a baseline before
    // the first WebSocket event arrives.
    if let Ok(p) = raydium_cpmm::fetch_price(&client, raydium_pool_id, pair) {
        last_raydium = Some(p);
    }
    if let Ok(p) = orca::fetch_price(&client, orca_pool_id, pair) {
        last_orca = Some(p);
    }

    for event in rx {
        let decoded_price = match event.source {
            Source::Raydium => match raydium_cpmm::decode_pool_state(&event.update.data) {
                Ok(_pool_state) => raydium_cpmm::fetch_price(&client, raydium_pool_id, pair).ok(),
                Err(e) => {
                    tracing::warn!("Failed to decode Raydium update: {}", e);
                    None
                }
            },
            Source::Orca => match orca::decode_whirlpool(&event.update.data) {
                Ok(_whirlpool) => orca::fetch_price(&client, orca_pool_id, pair).ok(),
                Err(e) => {
                    tracing::warn!("Failed to decode Orca update: {}", e);
                    None
                }
            },
        };

        match event.source {
            Source::Raydium => {
                if let Some(p) = decoded_price {
                    last_raydium = Some(p);
                }
            }
            Source::Orca => {
                if let Some(p) = decoded_price {
                    last_orca = Some(p);
                }
            }
        }

        if let (Some(r), Some(o)) = (&last_raydium, &last_orca) {
            match analyzer::find_best_opportunity(r, o) {
                Ok(opp) => {
                    tracing::warn!(
                        "REAL-TIME OPPORTUNITY: buy on {} @ {:.10}, sell on {} @ {:.10}, NET PROFIT: {:.4}%",
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
                        "APPROVED_REALTIME",
                    );
                }
                Err(reason) => {
                    tracing::debug!("No opportunity: {:?}", reason);
                }
            }
        }
    }

    Ok(())
}
