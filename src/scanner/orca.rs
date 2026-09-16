use anyhow::{Context, Result};
use borsh::BorshDeserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use super::PriceUpdate;

const ORCA_SOL_USDC_WHIRLPOOL: &str = "7qbRF6YsyGuLUVs6Y1q64bdVrfe4ZcUUz1JRdoVNUJnm";
const SOL_DECIMALS: i32 = 9;
const USDC_DECIMALS: i32 = 6;

#[derive(BorshDeserialize, Debug)]
pub struct WhirlpoolRewardInfo {
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub authority: Pubkey,
    pub emissions_per_second_x64: u128,
    pub growth_global_x64: u128,
}

#[derive(BorshDeserialize, Debug)]
pub struct Whirlpool {
    pub whirlpools_config: Pubkey,
    pub whirlpool_bump: [u8; 1],
    pub tick_spacing: u16,
    pub tick_spacing_seed: [u8; 2],
    pub fee_rate: u16,
    pub protocol_fee_rate: u16,
    pub liquidity: u128,
    pub sqrt_price: u128,
    pub tick_current_index: i32,
    pub protocol_fee_owed_a: u64,
    pub protocol_fee_owed_b: u64,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub fee_growth_global_a: u128,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
    pub fee_growth_global_b: u128,
    pub reward_last_updated_timestamp: u64,
    pub reward_infos: [WhirlpoolRewardInfo; 3],
}

pub fn fetch_price(client: &RpcClient, pool_id: &str, pair: &str) -> Result<PriceUpdate> {
    let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid Whirlpool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch Whirlpool account")?;

    let data = &account.data[8..];

    let whirlpool = Whirlpool::try_from_slice(data).context("Failed to deserialize Whirlpool")?;

    let sqrt_price_f64 = whirlpool.sqrt_price as f64 / (2f64.powi(64));
    let raw_price = sqrt_price_f64 * sqrt_price_f64;

    let decimal_adjustment = 10f64.powi(SOL_DECIMALS - USDC_DECIMALS);
    let price = raw_price * decimal_adjustment;

    let vault_a_balance = client
        .get_token_account_balance(&whirlpool.token_vault_a)
        .context("Failed to fetch token vault A balance")?;
    let vault_b_balance = client
        .get_token_account_balance(&whirlpool.token_vault_b)
        .context("Failed to fetch token vault B balance")?;

    let base_liquidity = vault_a_balance
        .ui_amount
        .context("No ui_amount for vault A")?;
    let quote_liquidity = vault_b_balance
        .ui_amount
        .context("No ui_amount for vault B")?;

    // fee_rate is in hundredths of a basis point: fee_rate / 1_000_000 = fee as a fraction
    let fee_pct = whirlpool.fee_rate as f64 / 10_000.0;

    Ok(PriceUpdate {
        dex: "Orca".to_string(),
        pair: pair.to_string(),
        price,
        base_liquidity,
        quote_liquidity,
        fee_pct,
        timestamp: std::time::SystemTime::now(),
    })
}
