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
        (vault_a_raw, vault_b_raw, decimals_a, decimals_b)
    };

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
        clmm_sqrt_price_q64: whirlpool.sqrt_price,
        clmm_liquidity: whirlpool.liquidity,
        clmm_is_a_wsol: is_a_wsol,
    })
}

#[derive(Debug, Clone)]
pub struct OrcaStaticContext {
    pub pool_id: Pubkey,
    pub token_mint_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_vault_b: Pubkey,
    pub decimals_a: u8,
    pub decimals_b: u8,
    pub is_a_wsol: bool,
}

impl OrcaStaticContext {
    pub fn load(client: &RpcClient, pool_id: &str) -> Result<Self> {
        let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid Whirlpool pubkey format")?;
        let whirlpool = load_whirlpool(client, pool_id)?;
        let mint_a_info = client
            .get_token_supply(&whirlpool.token_mint_a)
            .context("Failed to fetch token A mint info")?;
        let mint_b_info = client
            .get_token_supply(&whirlpool.token_mint_b)
            .context("Failed to fetch token B mint info")?;
        let is_a_wsol = whirlpool.token_mint_a.to_string() == WSOL_MINT;

        Ok(Self {
            pool_id: pool_pubkey,
            token_mint_a: whirlpool.token_mint_a,
            token_mint_b: whirlpool.token_mint_b,
            token_vault_a: whirlpool.token_vault_a,
            token_vault_b: whirlpool.token_vault_b,
            decimals_a: mint_a_info.decimals,
            decimals_b: mint_b_info.decimals,
            is_a_wsol,
        })
    }

    /// Still-live fallback path (initial seed before any WebSocket push has
    /// arrived). NOT used in the steady-state hot path - see
    /// `price_from_raw_reserves` / `price_from_whirlpool_and_reserves`.
    pub fn fetch_price_with_context(&self, client: &RpcClient, pair: &str) -> Result<PriceUpdate> {
        let whirlpool = load_whirlpool(client, &self.pool_id.to_string())?;
        self.price_from_whirlpool(client, &whirlpool, pair)
    }

    pub fn price_from_whirlpool(&self, client: &RpcClient, whirlpool: &Whirlpool, pair: &str) -> Result<PriceUpdate> {
        let vault_a_balance = client
            .get_token_account_balance(&whirlpool.token_vault_a)
            .context("Failed to fetch token vault A balance")?;
        let vault_b_balance = client
            .get_token_account_balance(&whirlpool.token_vault_b)
            .context("Failed to fetch token vault B balance")?;

        let vault_a_raw: u64 = vault_a_balance
            .amount
            .parse()
            .context("Failed to parse raw amount for vault A")?;
        let vault_b_raw: u64 = vault_b_balance
            .amount
            .parse()
            .context("Failed to parse raw amount for vault B")?;

        Ok(self.price_from_whirlpool_and_reserves(whirlpool.sqrt_price, whirlpool.liquidity, whirlpool.fee_rate, vault_a_raw, vault_b_raw, pair))
    }

    /// Hot-path constructor for a plain reserve update (e.g. only a vault
    /// account changed, sqrt_price unchanged) - reuses the last-known
    /// sqrt_price/fee_rate passed in by the caller. No RPC, no client.
    pub fn price_from_raw_reserves(
        &self,
        sqrt_price: u128,
        liquidity: u128,
        fee_rate: u16,
        vault_a_raw: u64,
        vault_b_raw: u64,
        pair: &str,
    ) -> PriceUpdate {
        self.price_from_whirlpool_and_reserves(sqrt_price, liquidity, fee_rate, vault_a_raw, vault_b_raw, pair)
    }

    /// Hot-path constructor: builds a `PriceUpdate` purely from values already
    /// in hand (either just decoded from a WebSocket push, or cached from the
    /// last update) - no RPC call, no client.
    pub fn price_from_whirlpool_and_reserves(
        &self,
        sqrt_price: u128,
        liquidity: u128,
        fee_rate: u16,
        vault_a_raw: u64,
        vault_b_raw: u64,
        pair: &str,
    ) -> PriceUpdate {
        let (base_reserve_raw, quote_reserve_raw, base_decimals, quote_decimals) = if self.is_a_wsol {
            (vault_b_raw, vault_a_raw, self.decimals_b, self.decimals_a)
        } else {
            (vault_a_raw, vault_b_raw, self.decimals_a, self.decimals_b)
        };

        let sqrt_price_f64 = sqrt_price as f64 / (2f64.powi(64));
        let raw_price = sqrt_price_f64 * sqrt_price_f64;
        let decimal_adjustment = 10f64.powi(self.decimals_a as i32 - self.decimals_b as i32);
        let price_b_per_a = raw_price * decimal_adjustment;

        let vault_a_ui = vault_a_raw as f64 / 10f64.powi(self.decimals_a as i32);
        let vault_b_ui = vault_b_raw as f64 / 10f64.powi(self.decimals_b as i32);

        let (base_liquidity, quote_liquidity, price) = if self.is_a_wsol {
            (vault_b_ui, vault_a_ui, 1.0 / price_b_per_a)
        } else {
            (vault_a_ui, vault_b_ui, price_b_per_a)
        };

        let fee_numerator = fee_rate as u64;
        const ORCA_FEE_RATE_DENOMINATOR: u64 = 1_000_000;
        let fee_denominator = ORCA_FEE_RATE_DENOMINATOR;
        let fee_pct = (fee_numerator as f64 / fee_denominator as f64) * 100.0;

        PriceUpdate {
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
            clmm_sqrt_price_q64: sqrt_price,
            clmm_liquidity: liquidity,
            clmm_is_a_wsol: self.is_a_wsol,
        }
    }
}