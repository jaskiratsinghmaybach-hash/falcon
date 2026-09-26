use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

const LOG_PATH: &str = "opportunity_log.csv";

/// Writes the CSV header if the file doesn't exist yet.
pub fn ensure_header() -> Result<()> {
    if !Path::new(LOG_PATH).exists() {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(LOG_PATH)
            .context("Failed to create log file")?;

        writeln!(
            file,
            "timestamp,pair,raydium_price,orca_price,base_liq_raydium,quote_liq_raydium,base_liq_orca,quote_liq_orca,buy_dex,sell_dex,raw_spread_pct,fee_adjusted_pct,net_after_slippage_pct,net_profit_pct,status"
        )?;
    }
    Ok(())
}

/// Logs one poll's result, whether it was an approved opportunity or a rejection reason.
#[allow(clippy::too_many_arguments)]
pub fn log_row(
    pair: &str,
    raydium_price: f64,
    orca_price: f64,
    raydium_base_liq: f64,
    raydium_quote_liq: f64,
    orca_base_liq: f64,
    orca_quote_liq: f64,
    buy_dex: &str,
    sell_dex: &str,
    raw_spread_pct: f64,
    fee_adjusted_pct: f64,
    net_after_slippage_pct: f64,
    net_profit_pct: f64,
    status: &str,
) -> Result<()> {
    let mut file = OpenOptions::new()
        .append(true)
        .open(LOG_PATH)
        .context("Failed to open log file for appending")?;

    let timestamp = chrono::Utc::now().to_rfc3339();

    writeln!(
        file,
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        timestamp,
        pair,
        raydium_price,
        orca_price,
        raydium_base_liq,
        raydium_quote_liq,
        orca_base_liq,
        orca_quote_liq,
        buy_dex,
        sell_dex,
        raw_spread_pct,
        fee_adjusted_pct,
        net_after_slippage_pct,
        net_profit_pct,
        status
    )?;

    Ok(())
}
