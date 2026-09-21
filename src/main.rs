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

            let amount_in_base = opp.trade_size_base;
            let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")?;
            let amount_in_lamports = (amount_in_base * opp.buy_price * 1_000_000_000.0) as u64;

            if opp.buy_dex == "RaydiumCPMM" {
                let pool_info =
                    scanner::raydium_cpmm::fetch_pool_info(&client, &config.raydium_pool_id)?;

                tracing::info!("Simulating BUY leg on RaydiumCPMM...");
                executor::simulate_cpmm_swap(
                    &client,
                    &config.keypair,
                    &pool_info,
                    &wsol_mint,
                    amount_in_lamports,
                    1,
                )?;
            } else {
                let orca_pool = Pubkey::from_str(&config.orca_pool_id)?;
                let orca_info = scanner::orca::fetch_pool_info(&client, &config.orca_pool_id)?;

                tracing::info!("Simulating BUY leg on Orca...");
                executor::simulate_orca_swap(
                    &client,
                    &config.keypair,
                    &orca_pool,
                    &orca_info,
                    &wsol_mint,
                    amount_in_lamports,
                    1,
                )?;
            }
        }
        Err(reason) => {
            tracing::info!("No opportunity right now: {:?}", reason);
            tracing::info!("(Last night's approved spread was momentary - it may already be gone)");
        }
    }

    Ok(())
}

