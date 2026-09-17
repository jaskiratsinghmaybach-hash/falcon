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

pub fn fetch_price(client: &RpcClient, pool_id: &str, pair: &str) -> Result<PriceUpdate> {
    let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid Whirlpool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch Whirlpool account")?;

    let data = &account.data[8..];

    let whirlpool = Whirlpool::try_from_slice(data).context("Failed to deserialize Whirlpool")?;

    tracing::debug!(
        "Orca token_mint_a: {}, token_mint_b: {}",
        whirlpool.token_mint_a,
        whirlpool.token_mint_b
    );

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

    // Normalize: base_liquidity = non-SOL token reserve, quote_liquidity = SOL reserve,
    // price = SOL per unit of the other token. Consistent across every DEX module.
    let (base_liquidity, quote_liquidity, price) =
        if whirlpool.token_mint_a.to_string() == WSOL_MINT {
            (
                vault_b_amount,
                vault_a_amount,
                vault_a_amount / vault_b_amount,
            )
        } else if whirlpool.token_mint_b.to_string() == WSOL_MINT {
            (
                vault_a_amount,
                vault_b_amount,
                vault_b_amount / vault_a_amount,
            )
        } else {
            (
                vault_a_amount,
                vault_b_amount,
                vault_b_amount / vault_a_amount,
            )
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
