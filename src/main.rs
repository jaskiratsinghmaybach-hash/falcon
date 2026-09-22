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

    let raydium_price =
        scanner::raydium_cpmm::fetch_price(&client, &config.raydium_pool_id, &config.pair)?;
    let orca_price = scanner::orca::fetch_price(&client, &config.orca_pool_id, &config.pair)?;

    tracing::info!("[RaydiumCPMM] price: {:.10}", raydium_price.price);
    tracing::info!("[Orca] price: {:.10}", orca_price.price);

    match analyzer::find_best_opportunity(&raydium_price, &orca_price) {
        Ok(opp) => {
            tracing::warn!(
                "OPPORTUNITY: buy on {} @ {:.10}, sell on {} @ {:.10}, trade size: {:.2}, NET PROFIT: {:.4}%",
                opp.buy_dex, opp.buy_price, opp.sell_dex, opp.sell_price, opp.trade_size_base, opp.net_profit_pct
            );

            // Stale-state check: re-fetch both prices right before acting, since real
            // opportunities are momentary and market conditions can shift between
            // detection and execution.
            let fresh_raydium =
                scanner::raydium_cpmm::fetch_price(&client, &config.raydium_pool_id, &config.pair)?;
            let fresh_orca =
                scanner::orca::fetch_price(&client, &config.orca_pool_id, &config.pair)?;

            const MAX_DRIFT_PCT: f64 = 0.5;

            let raydium_fresh =
                analyzer::is_still_fresh(raydium_price.price, fresh_raydium.price, MAX_DRIFT_PCT);
            let orca_fresh =
                analyzer::is_still_fresh(orca_price.price, fresh_orca.price, MAX_DRIFT_PCT);

            if !raydium_fresh || !orca_fresh {
                tracing::warn!("ABORT: prices moved since detection (Raydium fresh: {}, Orca fresh: {}) - opportunity is stale", raydium_fresh, orca_fresh);
            } else {
                // ... existing simulation code continues here
            }
        }
        Err(reason) => {
            tracing::info!("No opportunity right now: {:?}", reason);
            tracing::info!("(Last night's approved spread was momentary - it may already be gone)");
        }
    }

    Ok(())
}
