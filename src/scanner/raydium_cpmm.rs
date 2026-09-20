use anyhow::{Context, Result};
use borsh::BorshDeserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use super::PriceUpdate;

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

#[derive(BorshDeserialize, Debug)]
pub struct PoolState {
    pub amm_config: Pubkey,
    pub pool_creator: Pubkey,
    pub token_0_vault: Pubkey,
    pub token_1_vault: Pubkey,
    pub lp_mint: Pubkey,
    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,
    pub token_0_program: Pubkey,
    pub token_1_program: Pubkey,
    pub observation_key: Pubkey,
    pub auth_bump: u8,
    pub status: u8,
    pub lp_mint_decimals: u8,
    pub mint_0_decimals: u8,
    pub mint_1_decimals: u8,
    pub lp_supply: u64,
    pub protocol_fees_token_0: u64,
    pub protocol_fees_token_1: u64,
    pub fund_fees_token_0: u64,
    pub fund_fees_token_1: u64,
    pub open_time: u64,
    pub recent_epoch: u64,
    pub creator_fee_on: u8,
    pub enable_creator_fee: bool,
    pub padding1: [u8; 6],
    pub creator_fees_token_0: u64,
    pub creator_fees_token_1: u64,
    pub padding: [u64; 28],
}

#[derive(BorshDeserialize, Debug)]
pub struct AmmConfig {
    pub bump: u8,
    pub disable_create_pool: bool,
    pub index: u16,
    pub trade_fee_rate: u64,
    pub protocol_fee_rate: u64,
    pub fund_fee_rate: u64,
    pub create_pool_fee: u64,
    pub protocol_owner: Pubkey,
    pub fund_owner: Pubkey,
    pub creator_fee_rate: u64,
    pub padding: [u64; 15],
}

/// Everything the executor needs to build a CPMM swap instruction.
#[derive(Debug, Clone)]
pub struct CpmmPoolInfo {
    pub pool_state: Pubkey,
    pub amm_config: Pubkey,
    pub token_0_vault: Pubkey,
    pub token_1_vault: Pubkey,
    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,
    pub token_0_program: Pubkey,
    pub token_1_program: Pubkey,
    pub observation_key: Pubkey,
}

fn load_pool_state(client: &RpcClient, pool_id: &str) -> Result<PoolState> {
    let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid CPMM pool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch CPMM pool account")?;

    // Anchor accounts carry an 8-byte discriminator before the struct data.
    let data = &account.data[8..];

    PoolState::try_from_slice(data).context("Failed to deserialize CPMM PoolState")
}

pub fn fetch_pool_info(client: &RpcClient, pool_id: &str) -> Result<CpmmPoolInfo> {
    let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid CPMM pool pubkey format")?;
    let pool_state = load_pool_state(client, pool_id)?;

    Ok(CpmmPoolInfo {
        pool_state: pool_pubkey,
        amm_config: pool_state.amm_config,
        token_0_vault: pool_state.token_0_vault,
        token_1_vault: pool_state.token_1_vault,
        token_0_mint: pool_state.token_0_mint,
        token_1_mint: pool_state.token_1_mint,
        token_0_program: pool_state.token_0_program,
        token_1_program: pool_state.token_1_program,
        observation_key: pool_state.observation_key,
    })
}

pub fn fetch_price(client: &RpcClient, pool_id: &str, pair: &str) -> Result<PriceUpdate> {
    let pool_state = load_pool_state(client, pool_id)?;

    tracing::debug!(
        "CPMM mint_0_decimals: {}, mint_1_decimals: {}",
        pool_state.mint_0_decimals,
        pool_state.mint_1_decimals
    );

    tracing::debug!(
        "CPMM token_0_mint: {}, token_1_mint: {}",
        pool_state.token_0_mint,
        pool_state.token_1_mint
    );

    let vault_0_balance = client
        .get_token_account_balance(&pool_state.token_0_vault)
        .context("Failed to fetch token 0 vault balance")?;
    let vault_1_balance = client
        .get_token_account_balance(&pool_state.token_1_vault)
        .context("Failed to fetch token 1 vault balance")?;

    let vault_0_amount = vault_0_balance
        .ui_amount
        .context("No ui_amount for vault 0")?;
    let vault_1_amount = vault_1_balance
        .ui_amount
        .context("No ui_amount for vault 1")?;

    // CPMM's fee is stored on the shared AmmConfig account, not PoolState itself.
    let amm_config_account = client
        .get_account(&pool_state.amm_config)
        .context("Failed to fetch AmmConfig account")?;
    let amm_config = AmmConfig::try_from_slice(&amm_config_account.data[8..])
        .context("Failed to deserialize AmmConfig")?;

    // trade_fee_rate is in units of 1/1_000_000 of volume.
    let fee_pct = (amm_config.trade_fee_rate as f64 / 1_000_000.0) * 100.0;

    // Normalize: base_liquidity = non-SOL reserve, quote_liquidity = SOL reserve,
    // price = SOL per unit of the other token. Vault ratio is valid here since CPMM
    // is a true constant-product AMM (unlike Orca's concentrated liquidity).
    let (base_liquidity, quote_liquidity, price) =
        if pool_state.token_0_mint.to_string() == WSOL_MINT {
            (
                vault_1_amount,
                vault_0_amount,
                vault_0_amount / vault_1_amount,
            )
        } else if pool_state.token_1_mint.to_string() == WSOL_MINT {
            (
                vault_0_amount,
                vault_1_amount,
                vault_1_amount / vault_0_amount,
            )
        } else {
            (
                vault_0_amount,
                vault_1_amount,
                vault_1_amount / vault_0_amount,
            )
        };

    Ok(PriceUpdate {
        dex: "RaydiumCPMM".to_string(),
        pair: pair.to_string(),
        price,
        base_liquidity,
        quote_liquidity,
        fee_pct,
        timestamp: std::time::SystemTime::now(),
    })
}
