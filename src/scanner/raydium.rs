use anyhow::{Context, Result};
use borsh::BorshDeserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use super::PriceUpdate;

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const RAYDIUM_AMM_V4_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

#[derive(BorshDeserialize, Debug)]
pub struct Fees {
    pub min_separate_numerator: u64,
    pub min_separate_denominator: u64,
    pub trade_fee_numerator: u64,
    pub trade_fee_denominator: u64,
    pub pnl_numerator: u64,
    pub pnl_denominator: u64,
    pub swap_fee_numerator: u64,
    pub swap_fee_denominator: u64,
}

#[derive(BorshDeserialize, Debug)]
pub struct StateData {
    pub need_take_pnl_coin: u64,
    pub need_take_pnl_pc: u64,
    pub total_pnl_pc: u64,
    pub total_pnl_coin: u64,
    pub pool_open_time: u64,
    pub padding: [u64; 2],
    pub orderbook_to_init_time: u64,
    pub swap_coin_in_amount: u128,
    pub swap_pc_out_amount: u128,
    pub swap_acc_pc_fee: u64,
    pub swap_pc_in_amount: u128,
    pub swap_coin_out_amount: u128,
    pub swap_acc_coin_fee: u64,
}

#[derive(BorshDeserialize, Debug)]
pub struct AmmInfo {
    pub status: u64,
    pub nonce: u64,
    pub order_num: u64,
    pub depth: u64,
    pub coin_decimals: u64,
    pub pc_decimals: u64,
    pub state: u64,
    pub reset_flag: u64,
    pub min_size: u64,
    pub vol_max_cut_ratio: u64,
    pub amount_wave: u64,
    pub coin_lot_size: u64,
    pub pc_lot_size: u64,
    pub min_price_multiplier: u64,
    pub max_price_multiplier: u64,
    pub sys_decimal_value: u64,
    pub fees: Fees,
    pub state_data: StateData,
    pub coin_vault: Pubkey,
    pub pc_vault: Pubkey,
    pub coin_vault_mint: Pubkey,
    pub pc_vault_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub open_orders: Pubkey,
    pub market: Pubkey,
    pub market_program: Pubkey,
    pub target_orders: Pubkey,
    pub padding1: [u64; 8],
    pub amm_owner: Pubkey,
    pub lp_amount: u64,
    pub client_order_id: u64,
    pub recent_epoch: u64,
    pub padding2: u64,
}

pub struct PoolVaults {
    pub coin_vault: Pubkey,
    pub pc_vault: Pubkey,
}

fn validate_pool_account(account: &solana_sdk::account::Account, pool_id: &str) -> Result<()> {
    let expected_owner = Pubkey::from_str(RAYDIUM_AMM_V4_PROGRAM)?;
    if account.owner != expected_owner {
        anyhow::bail!(
            "Pool {} is not owned by Raydium AMM v4 program (expected {}, got {}) - wrong pool type or address",
            pool_id, expected_owner, account.owner
        );
    }
    if account.data.len() != 752 {
        anyhow::bail!(
            "Pool {} has unexpected data length {} (expected 752 bytes for AmmInfo) - layout mismatch",
            pool_id, account.data.len()
        );
    }
    Ok(())
}

pub fn fetch_pool_vaults(client: &RpcClient, pool_id: &str) -> Result<PoolVaults> {
    let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid pool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch pool account")?;

    validate_pool_account(&account, pool_id)?;

    let amm_info =
        AmmInfo::try_from_slice(&account.data).context("Failed to deserialize AmmInfo")?;

    Ok(PoolVaults {
        coin_vault: amm_info.coin_vault,
        pc_vault: amm_info.pc_vault,
    })
}

pub fn fetch_price(client: &RpcClient, pool_id: &str, pair: &str) -> Result<PriceUpdate> {
    let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid pool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch pool account")?;

    validate_pool_account(&account, pool_id)?;

    let amm_info =
        AmmInfo::try_from_slice(&account.data).context("Failed to deserialize AmmInfo")?;

    let coin_balance = client
        .get_token_account_balance(&amm_info.coin_vault)
        .context("Failed to fetch coin vault balance")?;
    let pc_balance = client
        .get_token_account_balance(&amm_info.pc_vault)
        .context("Failed to fetch pc vault balance")?;

    // Raw on-chain smallest-unit amounts - execution-domain source of truth.
    let coin_raw: u64 = coin_balance
        .amount
        .parse()
        .context("Failed to parse raw amount for coin vault")?;
    let pc_raw: u64 = pc_balance
        .amount
        .parse()
        .context("Failed to parse raw amount for pc vault")?;

    let coin_decimals = amm_info.coin_decimals as u8;
    let pc_decimals = amm_info.pc_decimals as u8;

    tracing::debug!(
        "Raydium coin_vault_mint: {}, pc_vault_mint: {}",
        amm_info.coin_vault_mint,
        amm_info.pc_vault_mint
    );

    let is_pc_wsol = amm_info.pc_vault_mint.to_string() == WSOL_MINT;

    let (base_reserve_raw, quote_reserve_raw, base_decimals, quote_decimals) = if is_pc_wsol {
        (coin_raw, pc_raw, coin_decimals, pc_decimals)
    } else {
        // Covers both "coin is WSOL" and the neither-is-WSOL fallback, which
        // the original code also treated identically (pc = base side).
        (pc_raw, coin_raw, pc_decimals, coin_decimals)
    };

    // Presentation-only f64 for logging.
    let coin_amount_ui = coin_balance.ui_amount.unwrap_or(0.0);
    let pc_amount_ui = pc_balance.ui_amount.unwrap_or(0.0);

    let (base_liquidity, quote_liquidity, price) = if is_pc_wsol {
        (coin_amount_ui, pc_amount_ui, pc_amount_ui / coin_amount_ui)
    } else {
        (pc_amount_ui, coin_amount_ui, coin_amount_ui / pc_amount_ui)
    };

    // Already an exact integer ratio in the account data - use it directly,
    // never round-trip through f64 for the execution-domain value.
    let fee_numerator = amm_info.fees.swap_fee_numerator;
    let fee_denominator = amm_info.fees.swap_fee_denominator;
    let fee_pct = (fee_numerator as f64 / fee_denominator as f64) * 100.0;

    Ok(PriceUpdate {
        dex: "Raydium".to_string(),
        pair: pair.to_string(),
        base_reserve_raw,
        quote_reserve_raw,
        base_decimals,
        quote_decimals,
        fee_numerator,
        fee_denominator,
        price,
        base_liquidity,
        quote_liquidity,
        fee_pct,
        timestamp: std::time::SystemTime::now(),
    })
}

pub fn decode_amm_info(data: &[u8]) -> Result<AmmInfo> {
    if data.len() != 752 {
        anyhow::bail!(
            "Raydium AMM v4 account data length mismatch: expected 752, got {}",
            data.len()
        );
    }
    AmmInfo::try_from_slice(data).context("Failed to deserialize AmmInfo")
}
