use anyhow::{Context, Result};
use borsh::BorshDeserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use super::PriceUpdate;

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const RAYDIUM_CPMM_PROGRAM: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

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

fn validate_pool_account(account: &solana_sdk::account::Account, pool_id: &str) -> Result<()> {
    let expected_owner = Pubkey::from_str(RAYDIUM_CPMM_PROGRAM)?;
    if account.owner != expected_owner {
        anyhow::bail!(
            "Pool {} is not owned by Raydium CPMM program (expected {}, got {}) - wrong pool type or address",
            pool_id, expected_owner, account.owner
        );
    }
    Ok(())
}

fn load_pool_state(client: &RpcClient, pool_id: &str) -> Result<PoolState> {
    let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid CPMM pool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch CPMM pool account")?;

    validate_pool_account(&account, pool_id)?;

    let data = &account.data[8..];

    PoolState::try_from_slice(data).context("Failed to deserialize CPMM PoolState")
}

/// Decodes PoolState from raw account bytes (no RPC call) - used by both the
/// RPC-based fetch path and the WebSocket real-time path.
pub fn decode_pool_state(data: &[u8]) -> Result<PoolState> {
    let inner = &data[8..];
    PoolState::try_from_slice(inner).context("Failed to deserialize CPMM PoolState")
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

    // Raw on-chain smallest-unit amounts, straight from the RPC's `amount` string
    // field (NOT `ui_amount`, which is already a lossy f64 division by decimals).
    // This is the execution-domain source of truth for reserves.
    let vault_0_raw: u64 = vault_0_balance
        .amount
        .parse()
        .context("Failed to parse raw amount for vault 0")?;
    let vault_1_raw: u64 = vault_1_balance
        .amount
        .parse()
        .context("Failed to parse raw amount for vault 1")?;

    let amm_config_account = client
        .get_account(&pool_state.amm_config)
        .context("Failed to fetch AmmConfig account")?;
    let amm_config = AmmConfig::try_from_slice(&amm_config_account.data[8..])
        .context("Failed to deserialize AmmConfig")?;

    // CPMM's trade_fee_rate is already an exact integer rate out of 1_000_000
    // (e.g. 2500 => 0.25%). Keep it as an exact integer ratio for execution;
    // the f64 percent below is derived ONLY for display.
    const CPMM_FEE_RATE_DENOMINATOR: u64 = 1_000_000;
    let fee_numerator = amm_config.trade_fee_rate;
    let fee_denominator = CPMM_FEE_RATE_DENOMINATOR;
    let fee_pct = (fee_numerator as f64 / fee_denominator as f64) * 100.0;

    // Presentation-only f64 conversions for logging (never fed back into
    // trading decisions - see PriceUpdate's doc comment).
    let vault_0_ui = vault_0_balance.ui_amount.unwrap_or(0.0);
    let vault_1_ui = vault_1_balance.ui_amount.unwrap_or(0.0);

    let is_token_0_wsol = pool_state.token_0_mint.to_string() == WSOL_MINT;

    let (base_reserve_raw, quote_reserve_raw, base_decimals, quote_decimals) = if is_token_0_wsol
    {
        (
            vault_1_raw,
            vault_0_raw,
            pool_state.mint_1_decimals,
            pool_state.mint_0_decimals,
        )
    } else {
        // Covers both "token_1 is WSOL" and the neither-is-WSOL fallback that
        // the original code also treated identically (token_0 = base).
        (
            vault_0_raw,
            vault_1_raw,
            pool_state.mint_0_decimals,
            pool_state.mint_1_decimals,
        )
    };

    let (base_liquidity, quote_liquidity, price) = if is_token_0_wsol {
        (vault_1_ui, vault_0_ui, vault_0_ui / vault_1_ui)
    } else {
        (vault_0_ui, vault_1_ui, vault_1_ui / vault_0_ui)
    };

    Ok(PriceUpdate {
        dex: "RaydiumCPMM".to_string(),
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

#[derive(Debug, Clone)]
pub struct CpmmStaticContext {
    pub pool_id: Pubkey,
    pub token_0_vault: Pubkey,
    pub token_1_vault: Pubkey,
    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,
    pub mint_0_decimals: u8,
    pub mint_1_decimals: u8,
    pub fee_numerator: u64,
    pub fee_denominator: u64,
    pub fee_pct: f64,
    pub is_token_0_wsol: bool,
}

impl CpmmStaticContext {
    pub fn load(client: &RpcClient, pool_id: &str) -> Result<Self> {
        let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid CPMM pool pubkey format")?;
        let pool_state = load_pool_state(client, pool_id)?;
        let amm_config_account = client
            .get_account(&pool_state.amm_config)
            .context("Failed to fetch AmmConfig account")?;
        let amm_config = AmmConfig::try_from_slice(&amm_config_account.data[8..])
            .context("Failed to deserialize AmmConfig")?;

        const CPMM_FEE_RATE_DENOMINATOR: u64 = 1_000_000;
        let fee_numerator = amm_config.trade_fee_rate;
        let fee_denominator = CPMM_FEE_RATE_DENOMINATOR;
        let fee_pct = (fee_numerator as f64 / fee_denominator as f64) * 100.0;
        let is_token_0_wsol = pool_state.token_0_mint.to_string() == WSOL_MINT;

        Ok(Self {
            pool_id: pool_pubkey,
            token_0_vault: pool_state.token_0_vault,
            token_1_vault: pool_state.token_1_vault,
            token_0_mint: pool_state.token_0_mint,
            token_1_mint: pool_state.token_1_mint,
            mint_0_decimals: pool_state.mint_0_decimals,
            mint_1_decimals: pool_state.mint_1_decimals,
            fee_numerator,
            fee_denominator,
            fee_pct,
            is_token_0_wsol,
        })
    }

    pub fn fetch_price_with_context(&self, client: &RpcClient, pair: &str) -> Result<PriceUpdate> {
        let vault_0_balance = client
            .get_token_account_balance(&self.token_0_vault)
            .context("Failed to fetch token 0 vault balance")?;
        let vault_1_balance = client
            .get_token_account_balance(&self.token_1_vault)
            .context("Failed to fetch token 1 vault balance")?;

        let vault_0_raw: u64 = vault_0_balance
            .amount
            .parse()
            .context("Failed to parse raw amount for vault 0")?;
        let vault_1_raw: u64 = vault_1_balance
            .amount
            .parse()
            .context("Failed to parse raw amount for vault 1")?;

        let (base_reserve_raw, quote_reserve_raw, base_decimals, quote_decimals) = if self.is_token_0_wsol {
            (vault_1_raw, vault_0_raw, self.mint_1_decimals, self.mint_0_decimals)
        } else {
            (vault_0_raw, vault_1_raw, self.mint_0_decimals, self.mint_1_decimals)
        };

        let vault_0_ui = vault_0_balance.ui_amount.unwrap_or(0.0);
        let vault_1_ui = vault_1_balance.ui_amount.unwrap_or(0.0);

        let (base_liquidity, quote_liquidity, price) = if self.is_token_0_wsol {
            (vault_1_ui, vault_0_ui, vault_0_ui / vault_1_ui)
        } else {
            (vault_0_ui, vault_1_ui, vault_1_ui / vault_0_ui)
        };

        Ok(PriceUpdate {
            dex: "RaydiumCPMM".to_string(),
            pair: pair.to_string(),
            base_reserve_raw,
            quote_reserve_raw,
            base_decimals,
            quote_decimals,
            fee_numerator: self.fee_numerator,
            fee_denominator: self.fee_denominator,
            price,
            base_liquidity,
            quote_liquidity,
            fee_pct: self.fee_pct,
            timestamp: std::time::SystemTime::now(),
        })
    }
}
