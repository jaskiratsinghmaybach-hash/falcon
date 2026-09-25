use anyhow::{Context, Result};
use borsh::BorshDeserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use super::PriceUpdate;

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const ORCA_WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

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

#[derive(Debug, Clone)]
pub struct WhirlpoolInfo {
    pub tick_current_index: i32,
    pub tick_spacing: u16,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
}

fn validate_pool_account(account: &solana_sdk::account::Account, pool_id: &str) -> Result<()> {
    let expected_owner = Pubkey::from_str(ORCA_WHIRLPOOL_PROGRAM)?;
    if account.owner != expected_owner {
        anyhow::bail!(
            "Pool {} is not owned by Orca Whirlpool program (expected {}, got {}) - wrong pool type or address",
            pool_id, expected_owner, account.owner
        );
    }
    Ok(())
}

pub fn decode_whirlpool(data: &[u8]) -> Result<Whirlpool> {
    let inner = &data[8..];
    Whirlpool::try_from_slice(inner).context("Failed to deserialize Whirlpool")
}

fn load_whirlpool(client: &RpcClient, pool_id: &str) -> Result<Whirlpool> {
    let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid Whirlpool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch Whirlpool account")?;

    validate_pool_account(&account, pool_id)?;

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

    tracing::debug!(
        "Orca token_mint_a: {}, token_mint_b: {}",
        whirlpool.token_mint_a,
        whirlpool.token_mint_b
    );

    let mint_a_info = client
        .get_token_supply(&whirlpool.token_mint_a)
        .context("Failed to fetch token A mint info")?;
    let mint_b_info = client
        .get_token_supply(&whirlpool.token_mint_b)
        .context("Failed to fetch token B mint info")?;

    let decimals_a = mint_a_info.decimals;
    let decimals_b = mint_b_info.decimals;

    let vault_a_balance = client
        .get_token_account_balance(&whirlpool.token_vault_a)
        .context("Failed to fetch token vault A balance")?;
    let vault_b_balance = client
        .get_token_account_balance(&whirlpool.token_vault_b)
        .context("Failed to fetch token vault B balance")?;

    // Raw on-chain smallest-unit amounts - execution-domain source of truth.
    // The Analyzer sizes trades and computes swap outputs from THESE reserves,
    // never from sqrt_price - so sqrt_price's f64 conversion below only ever
    // feeds the display `price` field, never a trading decision.
    let vault_a_raw: u64 = vault_a_balance
        .amount
        .parse()
        .context("Failed to parse raw amount for vault A")?;
    let vault_b_raw: u64 = vault_b_balance
        .amount
        .parse()
        .context("Failed to parse raw amount for vault B")?;

    let is_a_wsol = whirlpool.token_mint_a.to_string() == WSOL_MINT;

    let (base_reserve_raw, quote_reserve_raw, base_decimals, quote_decimals) = if is_a_wsol {
        (vault_b_raw, vault_a_raw, decimals_b, decimals_a)
    } else {
        // Covers both "B is WSOL" and the neither-is-WSOL fallback, which the
        // original code also treated identically (A = base side).
        (vault_a_raw, vault_b_raw, decimals_a, decimals_b)
    };

    // --- presentation-only from here down: f64 sqrt_price -> display price ---
    // TODO(numeric-migration): sqrt_price is Q64.64 fixed-point on-chain
    // (price = sqrt_price^2 / 2^128, base units). This f64 conversion is fine
    // for a human-readable log line, but if Orca-side execution math (e.g.
    // exact CLMM tick-crossing output) is implemented later, that MUST use
    // sqrt_price/liquidity directly in u128/i128 fixed-point per Orca's own
    // whirlpool math - not this display float. Left as a TODO per the
    // "preserve existing protocol-specific structure, don't invent exact CLMM
    // math here" instruction.
    let sqrt_price_f64 = whirlpool.sqrt_price as f64 / (2f64.powi(64));
    let raw_price = sqrt_price_f64 * sqrt_price_f64;
    let decimal_adjustment = 10f64.powi(decimals_a as i32 - decimals_b as i32);
    let price_b_per_a = raw_price * decimal_adjustment;

    let vault_a_ui = vault_a_balance.ui_amount.unwrap_or(0.0);
    let vault_b_ui = vault_b_balance.ui_amount.unwrap_or(0.0);

    let (base_liquidity, quote_liquidity, price) = if is_a_wsol {
        (vault_b_ui, vault_a_ui, 1.0 / price_b_per_a)
    } else {
        (vault_a_ui, vault_b_ui, price_b_per_a)
    };

    // Orca's fee_rate is stored in hundredths of a basis point, per Orca's own
    // docs: swap_fee = (input_amount * fee_rate) / 1_000_000. Use it directly
    // as an exact integer ratio.
    //
    // BUG FIX during numeric migration: the pre-migration code divided by
    // 10_000 here (treating fee_rate as plain basis points), which understated
    // fee_pct by 100x versus Orca's documented formula. Confirmed against
    // https://docs.orca.so/developers/architecture/whirlpool-fees. This only
    // affected the f64 display value in the old code, but since fee_pct also
    // fed straight into the old analyzer's fee-adjusted-spread math, it means
    // prior runs UNDERSTATED Orca's real fee in the profitability check. Worth
    // knowing when reviewing any historical opportunity_log.csv rows.
    let fee_numerator = whirlpool.fee_rate as u64;
    const ORCA_FEE_RATE_DENOMINATOR: u64 = 1_000_000;
    let fee_denominator = ORCA_FEE_RATE_DENOMINATOR;
    let fee_pct = (fee_numerator as f64 / fee_denominator as f64) * 100.0;

    Ok(PriceUpdate {
        dex: "Orca".to_string(),
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
