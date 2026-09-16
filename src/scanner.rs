use anyhow::{Context, Result};
use borsh::BorshDeserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

const RAYDIUM_SOL_USDC_POOL: &str = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2";

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

pub fn fetch_pool_price(rpc_url: &str) -> Result<f64> {
    let client = RpcClient::new(rpc_url.to_string());

    let pool_pubkey =
        Pubkey::from_str(RAYDIUM_SOL_USDC_POOL).context("Invalid pool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch pool account")?;

    let amm_info = AmmInfo::try_from_slice(&account.data)
        .context("Failed to deserialize AmmInfo - layout may have changed")?;

    tracing::info!(
        "Pool status: {}, coin_decimals: {}, pc_decimals: {}",
        amm_info.status,
        amm_info.coin_decimals,
        amm_info.pc_decimals
    );
    tracing::info!("Coin vault: {}", amm_info.coin_vault);
    tracing::info!("PC vault: {}", amm_info.pc_vault);

    let coin_balance = client
        .get_token_account_balance(&amm_info.coin_vault)
        .context("Failed to fetch coin vault balance")?;
    let pc_balance = client
        .get_token_account_balance(&amm_info.pc_vault)
        .context("Failed to fetch pc vault balance")?;

    let coin_amount: f64 = coin_balance
        .ui_amount
        .context("No ui_amount for coin vault")?;
    let pc_amount: f64 = pc_balance.ui_amount.context("No ui_amount for pc vault")?;

    tracing::info!("Coin vault balance: {} SOL", coin_amount);
    tracing::info!("PC vault balance: {} USDC", pc_amount);

    let price = pc_amount / coin_amount;

    tracing::info!("Computed price: {} USDC per SOL", price);

    Ok(price)
}
