pub mod orca;
pub mod raydium;
pub mod raydium_cpmm;
pub mod realtime;

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use std::sync::mpsc;

use crate::analyzer;
use crate::config::DecoderType;

/// Raw execution-domain reserves and fee parameters for one pool side of a pair,
/// plus f64 fields kept ONLY for human-readable logging/display (see logger.rs).
///
/// Numeric boundary rule (see numeric-migration deliverable notes): everything
/// under "raw execution state" below is deterministic integer data and is what
/// the Analyzer and Executor must use for any trading decision or instruction.
/// The `price`/`base_liquidity`/`quote_liquidity` f64 fields are DISPLAY-ONLY
/// convenience values for logs; never feed them back into calculations that
/// approve, size, or execute a trade.
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub dex: String,
    pub pair: String,

    // --- raw execution state (integer, deterministic) ---
    /// Raw base-token reserve, smallest units (i.e. accounting for the base
    /// mint's decimals), from the vault this PriceUpdate was fetched from.
    pub base_reserve_raw: u64,
    /// Raw quote-token reserve, smallest units. Quote is always the WSOL side
    /// for this bot's pairs (see scanner-specific fetch_price for orientation).
    pub quote_reserve_raw: u64,
    /// Base-token mint decimals (needed to convert base_reserve_raw for display
    /// or when constructing instruction amounts that must be in base units).
    pub base_decimals: u8,
    /// Quote-token (WSOL) mint decimals. Always 9 for SOL, but carried
    /// explicitly rather than hardcoded so the type stays honest about units.
    pub quote_decimals: u8,
    /// Trading fee as an exact integer ratio (numerator / denominator), taken
    /// directly from each protocol's own fee parameters - never derived via f64.
    pub fee_numerator: u64,
    pub fee_denominator: u64,

    // --- presentation-only (f64) - logging/display, NEVER execution ---
    pub price: f64,
    pub base_liquidity: f64,
    pub quote_liquidity: f64,
    pub fee_pct: f64,

    pub timestamp: std::time::SystemTime,
}

#[derive(Debug)]
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
    decoder: DecoderType,
    _trade_size_hint: f64,
    max_price_age_secs: u64,
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

    tracing::info!(
        "Real-time WebSocket subscriptions started for {} (Raydium mode: {:?})",
        pair,
        decoder
    );

    // Pre-cache static pool contexts on boot to eliminate redundant RPC round-trips
    let cpmm_ctx = match decoder {
        DecoderType::Cpmm => match raydium_cpmm::CpmmStaticContext::load(&client, raydium_pool_id) {
            Ok(ctx) => {
                tracing::info!("Pre-cached Raydium CPMM static pool metadata");
                Some(ctx)
            }
            Err(e) => {
                tracing::warn!("Failed to pre-cache CPMM metadata, falling back to dynamic fetch: {}", e);
                None
            }
        },
        DecoderType::Amm => None,
    };

    let orca_ctx = match orca::OrcaStaticContext::load(&client, orca_pool_id) {
        Ok(ctx) => {
            tracing::info!("Pre-cached Orca Whirlpool static pool metadata");
            Some(ctx)
        }
        Err(e) => {
            tracing::warn!("Failed to pre-cache Orca metadata, falling back to dynamic fetch: {}", e);
            None
        }
    };

    let mut last_raydium: Option<PriceUpdate> = None;
    let mut last_orca: Option<PriceUpdate> = None;

    // Seed both sides once via a normal RPC call so we have a baseline before
    // the first WebSocket event arrives.
    match decoder {
        DecoderType::Cpmm => {
            if let Some(ctx) = &cpmm_ctx {
                if let Ok(p) = ctx.fetch_price_with_context(&client, pair) {
                    last_raydium = Some(p);
                }
            } else if let Ok(p) = raydium_cpmm::fetch_price(&client, raydium_pool_id, pair) {
                last_raydium = Some(p);
            }
        }
        DecoderType::Amm => {
            if let Ok(p) = raydium::fetch_price(&client, raydium_pool_id, pair) {
                last_raydium = Some(p);
            }
        }
    }
    if let Some(ctx) = &orca_ctx {
        if let Ok(p) = ctx.fetch_price_with_context(&client, pair) {
            last_orca = Some(p);
        }
    } else if let Ok(p) = orca::fetch_price(&client, orca_pool_id, pair) {
        last_orca = Some(p);
    }

    for event in rx {
        tracing::info!(
            "WebSocket event received from {:?} ({} bytes)",
            event.source,
            event.update.data.len()
        );

        let decoded_price = match event.source {
            Source::Raydium => match decoder {
                DecoderType::Cpmm => {
                    if let Some(ctx) = &cpmm_ctx {
                        ctx.fetch_price_with_context(&client, pair).ok()
                    } else {
                        raydium_cpmm::fetch_price(&client, raydium_pool_id, pair).ok()
                    }
                }
                DecoderType::Amm => match raydium::decode_amm_info(&event.update.data) {
                    Ok(_amm_info) => raydium::fetch_price(&client, raydium_pool_id, pair).ok(),
                    Err(e) => {
                        tracing::warn!("Failed to decode Raydium AMM update: {}", e);
                        None
                    }
                },
            },
            Source::Orca => match orca::decode_whirlpool(&event.update.data) {
                Ok(whirlpool) => {
                    if let Some(ctx) = &orca_ctx {
                        ctx.price_from_whirlpool(&client, &whirlpool, pair).ok()
                    } else {
                        orca::fetch_price(&client, orca_pool_id, pair).ok()
                    }
                }
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
            match analyzer::find_best_opportunity_with_staleness(r, o, max_price_age_secs) {
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
                    tracing::info!(
                        "[{}] Opportunity check: {:?} | {} price: {:.10}, {} price: {:.10}",
                        pair,
                        reason,
                        r.dex,
                        r.price,
                        o.dex,
                        o.price
                    );

                    let (buy_dex, sell_dex) = if r.price < o.price {
                        (r.dex.as_str(), o.dex.as_str())
                    } else {
                        (o.dex.as_str(), r.dex.as_str())
                    };
                    // Display-only spread for the rejection log row. This is
                    // NOT used for any decision (the decision already happened
                    // inside find_best_opportunity using integer bps math) -
                    // it's purely so the CSV shows roughly how close the
                    // market was, using the same f64 price fields as the
                    // rest of the display-only log columns.
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
                        reason.as_display_pct(),
                        &format!("{:?}", reason),
                    );
                }
            }
        }
    }

    Ok(())
}
