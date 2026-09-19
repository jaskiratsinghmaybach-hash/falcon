use anyhow::{Context, Result};
use borsh::BorshDeserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use super::PriceUpdate;

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

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

/// Everything the executor needs to build a Whirlpool swap instruction.
#[derive(Debug, Clone)]
pub struct WhirlpoolInfo {
    pub tick_current_index: i32,
    pub tick_spacing: u16,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
}

fn load_whirlpool(client: &RpcClient, pool_id: &str) -> Result<Whirlpool> {
    let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid Whirlpool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch Whirlpool account")?;

    // Anchor accounts carry an 8-byte discriminator before the struct data.
    let data = &account.data[8..];

    Whirlpool::try_from_slice(data).context("Failed to deserialize Whirlpool")
}

pub fn fetch_pool_info(client: &RpcClient, pool_id: &str) -> Result<WhirlpoolInfo> {
    let whirlpool = load_whirlpool(client, pool_id)?;

    Ok(WhirlpoolInfo {
        tick_current_index: whirlpool.tick_current_index,
        tick_spacing: whirlpool.tick_spacing,
        token_mint_a: whirlpool.token_mint_a,
        token_vault_a: whirlpool.token_vault_a,
        token_mint_b: whirlpool.token_mint_b,
        token_vault_b: whirlpool.token_vault_b,
    })
}

pub fn fetch_price(client: &RpcClient, pool_id: &str, pair: &str) -> Result<PriceUpdate> {
    let whirlpool = load_whirlpool(client, pool_id)?;

    let mint_a_info = client
        .get_token_supply(&whirlpool.token_mint_a)
        .context("Failed to fetch token A mint info")?;
    let mint_b_info = client
        .get_token_supply(&whirlpool.token_mint_b)
        .context("Failed to fetch token B mint info")?;

    let decimals_a = mint_a_info.decimals as i32;
    let decimals_b = mint_b_info.decimals as i32;

    // Price MUST come from sqrt_price, not vault ratio. Whirlpools are concentrated-liquidity:
    // vault balances include out-of-range liquidity that doesn't reflect the tradeable price.
    let sqrt_price_f64 = whirlpool.sqrt_price as f64 / (2f64.powi(64));
    let raw_price = sqrt_price_f64 * sqrt_price_f64;

    let decimal_adjustment = 10f64.powi(decimals_a - decimals_b);
    let price_b_per_a = raw_price * decimal_adjustment; // token B per 1 token A, UI decimals

    let vault_a_balance = client
        .get_token_account_balance(&whirlpool.token_vault_a)
        .context("Failed to fetch token vault A balance")?;
    let vault_b_balance = client
        .get_token_account_balance(&whirlpool.token_vault_b)
        .context("Failed to fetch token vault B balance")?;

    let vault_a_amount = vault_a_balance
        .ui_amount
        .context("No ui_amount for vault A")?;
    let vault_b_amount = vault_b_balance
        .ui_amount
        .context("No ui_amount for vault B")?;

    // Normalize: base_liquidity = non-SOL reserve, quote_liquidity = SOL reserve,
    // price = SOL per unit of the other token.
    let (base_liquidity, quote_liquidity, price) =
        if whirlpool.token_mint_a.to_string() == WSOL_MINT {
            (vault_b_amount, vault_a_amount, 1.0 / price_b_per_a)
        } else if whirlpool.token_mint_b.to_string() == WSOL_MINT {
            (vault_a_amount, vault_b_amount, price_b_per_a)
        } else {
            (vault_a_amount, vault_b_amount, price_b_per_a)
        };

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
