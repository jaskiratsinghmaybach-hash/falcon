use anyhow::{Context, Result};
use borsh::BorshDeserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::{OnceLock, RwLock};

use super::PriceUpdate;

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const RAYDIUM_CPMM_PROGRAM: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";
const FEE_DENOMINATOR: u64 = 1_000_000;

// Raydium CPMM PoolStatus bit layout:
// bit 0 = deposit disabled
// bit 1 = withdraw disabled
// bit 2 = swap disabled
const SWAP_STATUS_BIT: u8 = 1 << 2;

#[derive(BorshDeserialize, Debug, Clone)]
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

#[derive(BorshDeserialize, Debug, Clone)]
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

#[derive(Debug, Clone)]
pub struct CpmmQuoteState {
    pub pool_id: Pubkey,

    pub token_0_vault: Pubkey,
    pub token_1_vault: Pubkey,
    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,

    pub token_0_program: Pubkey,
    pub token_1_program: Pubkey,

    pub vault_0_raw: u64,
    pub vault_1_raw: u64,

    pub protocol_fees_token_0: u64,
    pub protocol_fees_token_1: u64,
    pub fund_fees_token_0: u64,
    pub fund_fees_token_1: u64,
    pub creator_fees_token_0: u64,
    pub creator_fees_token_1: u64,

    pub trade_fee_rate: u64,
    pub protocol_fee_rate: u64,
    pub fund_fee_rate: u64,
    pub creator_fee_rate: u64,

    pub creator_fee_on: u8,
    pub enable_creator_fee: bool,

    pub status: u8,
    pub open_time: u64,

    pub is_token_0_wsol: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpmmQuoteError {
    ZeroReserve,
    ReserveFeeUnderflow,
    ArithmeticOverflow,
    InvalidCreatorFeeMode,
    InsufficientInputAfterFees,
    OutputTooLarge,
    Token2022Unsupported,
    SwapDisabled,
    PoolNotOpen,
}

impl std::fmt::Display for CpmmQuoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for CpmmQuoteError {}

static CURRENT_QUOTE_STATE: OnceLock<RwLock<Option<CpmmQuoteState>>> = OnceLock::new();

fn quote_state_store() -> &'static RwLock<Option<CpmmQuoteState>> {
    CURRENT_QUOTE_STATE.get_or_init(|| RwLock::new(None))
}

/// Publishes the newest combined PoolState + AmmConfig + vault snapshot.
///
/// Falcon currently monitors one configured Raydium CPMM pool, so the
/// analyzer reads this single canonical hot-path snapshot rather than
/// smuggling protocol-specific state through generic PriceUpdate fields.
pub fn publish_quote_state(state: CpmmQuoteState) {
    if let Ok(mut guard) = quote_state_store().write() {
        *guard = Some(state);
    }
}

pub fn current_quote_state() -> Option<CpmmQuoteState> {
    quote_state_store()
        .read()
        .ok()
        .and_then(|guard| guard.clone())
}

fn validate_pool_account(account: &solana_sdk::account::Account, pool_id: &str) -> Result<()> {
    let expected_owner = Pubkey::from_str(RAYDIUM_CPMM_PROGRAM)?;

    if account.owner != expected_owner {
        anyhow::bail!(
            "Pool {} is not owned by Raydium CPMM program (expected {}, got {}) - wrong pool type or address",
            pool_id,
            expected_owner,
            account.owner
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

    decode_pool_state(&account.data)
}

pub fn decode_pool_state(data: &[u8]) -> Result<PoolState> {
    if data.len() < 8 {
        anyhow::bail!(
            "Raydium CPMM PoolState account too short: {} bytes",
            data.len()
        );
    }

    PoolState::try_from_slice(&data[8..]).context("Failed to deserialize CPMM PoolState")
}

pub fn decode_amm_config(data: &[u8]) -> Result<AmmConfig> {
    if data.len() < 8 {
        anyhow::bail!(
            "Raydium CPMM AmmConfig account too short: {} bytes",
            data.len()
        );
    }

    AmmConfig::try_from_slice(&data[8..]).context("Failed to deserialize CPMM AmmConfig")
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

fn build_quote_state(
    pool_state: &PoolState,
    amm_config: &AmmConfig,
    vault_0_raw: u64,
    vault_1_raw: u64,
) -> CpmmQuoteState {
    CpmmQuoteState {
        pool_id: Pubkey::default(),

        token_0_vault: pool_state.token_0_vault,
        token_1_vault: pool_state.token_1_vault,
        token_0_mint: pool_state.token_0_mint,
        token_1_mint: pool_state.token_1_mint,

        token_0_program: pool_state.token_0_program,
        token_1_program: pool_state.token_1_program,

        vault_0_raw,
        vault_1_raw,

        protocol_fees_token_0: pool_state.protocol_fees_token_0,
        protocol_fees_token_1: pool_state.protocol_fees_token_1,
        fund_fees_token_0: pool_state.fund_fees_token_0,
        fund_fees_token_1: pool_state.fund_fees_token_1,
        creator_fees_token_0: pool_state.creator_fees_token_0,
        creator_fees_token_1: pool_state.creator_fees_token_1,

        trade_fee_rate: amm_config.trade_fee_rate,
        protocol_fee_rate: amm_config.protocol_fee_rate,
        fund_fee_rate: amm_config.fund_fee_rate,
        creator_fee_rate: amm_config.creator_fee_rate,

        creator_fee_on: pool_state.creator_fee_on,
        enable_creator_fee: pool_state.enable_creator_fee,

        status: pool_state.status,
        open_time: pool_state.open_time,

        is_token_0_wsol: pool_state.token_0_mint.to_string() == WSOL_MINT,
    }
}

fn with_pool_id(mut state: CpmmQuoteState, pool_id: Pubkey) -> CpmmQuoteState {
    state.pool_id = pool_id;
    state
}

pub fn quote_state_from_accounts(
    pool_id: Pubkey,
    pool_state: &PoolState,
    amm_config: &AmmConfig,
    vault_0_raw: u64,
    vault_1_raw: u64,
) -> CpmmQuoteState {
    with_pool_id(
        build_quote_state(pool_state, amm_config, vault_0_raw, vault_1_raw),
        pool_id,
    )
}

pub fn fetch_price(client: &RpcClient, pool_id: &str, pair: &str) -> Result<PriceUpdate> {
    let pool_state = load_pool_state(client, pool_id)?;

    let amm_config_account = client
        .get_account(&pool_state.amm_config)
        .context("Failed to fetch AmmConfig account")?;

    let amm_config = decode_amm_config(&amm_config_account.data)?;

    let vault_0_balance = client
        .get_token_account_balance(&pool_state.token_0_vault)
        .context("Failed to fetch token 0 vault balance")?;

    let vault_1_balance = client
        .get_token_account_balance(&pool_state.token_1_vault)
        .context("Failed to fetch token 1 vault balance")?;

    let vault_0_raw: u64 = vault_0_balance
        .amount
        .parse()
        .context("Failed to parse raw amount for vault 0")?;

    let vault_1_raw: u64 = vault_1_balance
        .amount
        .parse()
        .context("Failed to parse raw amount for vault 1")?;

    let pool_pubkey = Pubkey::from_str(pool_id)?;

    let quote_state = quote_state_from_accounts(
        pool_pubkey,
        &pool_state,
        &amm_config,
        vault_0_raw,
        vault_1_raw,
    );

    publish_quote_state(quote_state);

    let static_ctx = CpmmStaticContext::from_loaded(pool_pubkey, &pool_state, &amm_config);

    Ok(static_ctx.price_from_raw_reserves(vault_0_raw, vault_1_raw, pair))
}

#[derive(Debug, Clone)]
pub struct CpmmStaticContext {
    pub pool_id: Pubkey,

    pub token_0_vault: Pubkey,
    pub token_1_vault: Pubkey,

    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,

    pub token_0_program: Pubkey,
    pub token_1_program: Pubkey,

    pub mint_0_decimals: u8,
    pub mint_1_decimals: u8,

    pub fee_numerator: u64,
    pub fee_denominator: u64,
    pub fee_pct: f64,

    pub is_token_0_wsol: bool,

    pub amm_config: Pubkey,
    pub protocol_fee_rate: u64,
    pub fund_fee_rate: u64,
    pub creator_fee_rate: u64,
}

impl CpmmStaticContext {
    pub fn from_loaded(
        pool_pubkey: Pubkey,
        pool_state: &PoolState,
        amm_config: &AmmConfig,
    ) -> Self {
        let fee_numerator = amm_config.trade_fee_rate;

        Self {
            pool_id: pool_pubkey,

            token_0_vault: pool_state.token_0_vault,
            token_1_vault: pool_state.token_1_vault,

            token_0_mint: pool_state.token_0_mint,
            token_1_mint: pool_state.token_1_mint,

            token_0_program: pool_state.token_0_program,
            token_1_program: pool_state.token_1_program,

            mint_0_decimals: pool_state.mint_0_decimals,
            mint_1_decimals: pool_state.mint_1_decimals,

            fee_numerator,
            fee_denominator: FEE_DENOMINATOR,
            fee_pct: (fee_numerator as f64 / FEE_DENOMINATOR as f64) * 100.0,

            is_token_0_wsol: pool_state.token_0_mint.to_string() == WSOL_MINT,

            amm_config: pool_state.amm_config,
            protocol_fee_rate: amm_config.protocol_fee_rate,
            fund_fee_rate: amm_config.fund_fee_rate,
            creator_fee_rate: amm_config.creator_fee_rate,
        }
    }

    pub fn load(client: &RpcClient, pool_id: &str) -> Result<Self> {
        let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid CPMM pool pubkey format")?;

        let pool_state = load_pool_state(client, pool_id)?;

        let amm_config_account = client
            .get_account(&pool_state.amm_config)
            .context("Failed to fetch AmmConfig account")?;

        let amm_config = decode_amm_config(&amm_config_account.data)?;

        tracing::info!(
            "Raydium CPMM token programs: token0={}, token1={}",
            pool_state.token_0_program,
            pool_state.token_1_program
        );

        tracing::info!(
            "Raydium CPMM config: trade_fee_rate={}, protocol_fee_rate={}, fund_fee_rate={}, creator_fee_rate={}, creator_fee_on={}, creator_enabled={}",
            amm_config.trade_fee_rate,
            amm_config.protocol_fee_rate,
            amm_config.fund_fee_rate,
            amm_config.creator_fee_rate,
            pool_state.creator_fee_on,
            pool_state.enable_creator_fee
        );

        let legacy_token_program = spl_token::id();

        if pool_state.token_0_program != legacy_token_program
            || pool_state.token_1_program != legacy_token_program
        {
            tracing::warn!(
                "Raydium CPMM pool uses Token-2022 or another token program; exact transfer-fee quoting is disabled for this pool"
            );
        }

        Ok(Self::from_loaded(pool_pubkey, &pool_state, &amm_config))
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

        Ok(self.price_from_raw_reserves(vault_0_raw, vault_1_raw, pair))
    }

    pub fn price_from_raw_reserves(
        &self,
        vault_0_raw: u64,
        vault_1_raw: u64,
        pair: &str,
    ) -> PriceUpdate {
        let (base_reserve_raw, quote_reserve_raw, base_decimals, quote_decimals) =
            if self.is_token_0_wsol {
                (
                    vault_1_raw,
                    vault_0_raw,
                    self.mint_1_decimals,
                    self.mint_0_decimals,
                )
            } else {
                (
                    vault_0_raw,
                    vault_1_raw,
                    self.mint_0_decimals,
                    self.mint_1_decimals,
                )
            };

        let vault_0_ui = vault_0_raw as f64 / 10f64.powi(self.mint_0_decimals as i32);

        let vault_1_ui = vault_1_raw as f64 / 10f64.powi(self.mint_1_decimals as i32);

        let (base_liquidity, quote_liquidity, price) = if self.is_token_0_wsol {
            (vault_1_ui, vault_0_ui, vault_0_ui / vault_1_ui)
        } else {
            (vault_0_ui, vault_1_ui, vault_1_ui / vault_0_ui)
        };

        PriceUpdate {
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
            clmm_sqrt_price_q64: 0,
            clmm_liquidity: 0,
            clmm_is_a_wsol: false,
        }
    }
}

/// Returns whether Raydium currently considers swaps enabled.
///
/// Raydium disables swapping when bit 2 of `status` is set, and it also
/// rejects swaps before `open_time`.
pub fn is_swap_open(state: &CpmmQuoteState, unix_timestamp: u64) -> bool {
    state.status & SWAP_STATUS_BIT == 0 && unix_timestamp >= state.open_time
}

fn ensure_swap_open(state: &CpmmQuoteState, unix_timestamp: u64) -> Result<(), CpmmQuoteError> {
    if state.status & SWAP_STATUS_BIT != 0 {
        return Err(CpmmQuoteError::SwapDisabled);
    }

    if unix_timestamp < state.open_time {
        return Err(CpmmQuoteError::PoolNotOpen);
    }

    Ok(())
}

/// Exact Raydium CPMM base-input quote.
///
/// This mirrors the on-chain `CurveCalculator::swap_base_input` structure:
///
/// 1. Input transfer fee is applied before this function on-chain.
/// 2. If creator fee is on input, trade + creator rates are combined.
/// 3. The combined fee is ceiling-rounded.
/// 4. The combined fee is split into creator/trade portions.
/// 5. Effective reserves are raw vault balances minus accumulated
///    protocol/fund/creator fees.
/// 6. Constant-product output uses floor division.
/// 7. If creator fee is output-side, it is removed after the CP output.
///
/// Protocol/fund fees are bookkeeping portions of the trade fee; they are
/// not an additional deduction from the trader's output.
pub fn quote_output_amount(
    state: &CpmmQuoteState,
    amount_in: u64,
    spending_quote: bool,
) -> Result<u64, CpmmQuoteError> {
    if amount_in == 0 {
        return Ok(0);
    }

    let legacy_token_program = spl_token::id();

    if state.token_0_program != legacy_token_program
        || state.token_1_program != legacy_token_program
    {
        return Err(CpmmQuoteError::Token2022Unsupported);
    }

    let fees_0 = (state.protocol_fees_token_0 as u128)
        .checked_add(state.fund_fees_token_0 as u128)
        .ok_or(CpmmQuoteError::ArithmeticOverflow)?
        .checked_add(state.creator_fees_token_0 as u128)
        .ok_or(CpmmQuoteError::ArithmeticOverflow)?;

    let fees_1 = (state.protocol_fees_token_1 as u128)
        .checked_add(state.fund_fees_token_1 as u128)
        .ok_or(CpmmQuoteError::ArithmeticOverflow)?
        .checked_add(state.creator_fees_token_1 as u128)
        .ok_or(CpmmQuoteError::ArithmeticOverflow)?;

    let reserve_0 = (state.vault_0_raw as u128)
        .checked_sub(fees_0)
        .ok_or(CpmmQuoteError::ReserveFeeUnderflow)?;

    let reserve_1 = (state.vault_1_raw as u128)
        .checked_sub(fees_1)
        .ok_or(CpmmQuoteError::ReserveFeeUnderflow)?;

    if reserve_0 == 0 || reserve_1 == 0 {
        return Err(CpmmQuoteError::ZeroReserve);
    }

    let zero_for_one = if state.is_token_0_wsol {
        spending_quote
    } else {
        !spending_quote
    };

    let (reserve_in, reserve_out) = if zero_for_one {
        (reserve_0, reserve_1)
    } else {
        (reserve_1, reserve_0)
    };

    let creator_fee_rate = if state.enable_creator_fee {
        state.creator_fee_rate
    } else {
        0
    };

    let creator_fee_on_input = match state.creator_fee_on {
        0 => true,
        1 => zero_for_one,
        2 => !zero_for_one,
        _ => return Err(CpmmQuoteError::InvalidCreatorFeeMode),
    };

    let input_u128 = amount_in as u128;

    let input_less_fees = if creator_fee_on_input {
        let combined_rate = state
            .trade_fee_rate
            .checked_add(creator_fee_rate)
            .ok_or(CpmmQuoteError::ArithmeticOverflow)?;

        let total_fee = ceil_fee(input_u128, combined_rate)?;

        input_u128
            .checked_sub(total_fee)
            .ok_or(CpmmQuoteError::InsufficientInputAfterFees)?
    } else {
        let trade_fee = ceil_fee(input_u128, state.trade_fee_rate)?;

        input_u128
            .checked_sub(trade_fee)
            .ok_or(CpmmQuoteError::InsufficientInputAfterFees)?
    };

    if input_less_fees == 0 {
        return Ok(0);
    }

    let numerator = input_less_fees
        .checked_mul(reserve_out)
        .ok_or(CpmmQuoteError::ArithmeticOverflow)?;

    let denominator = reserve_in
        .checked_add(input_less_fees)
        .ok_or(CpmmQuoteError::ArithmeticOverflow)?;

    if denominator == 0 {
        return Err(CpmmQuoteError::ZeroReserve);
    }

    let output_swapped = numerator / denominator;

    let output = if creator_fee_on_input {
        output_swapped
    } else {
        let creator_fee = ceil_fee(output_swapped, creator_fee_rate)?;

        output_swapped
            .checked_sub(creator_fee)
            .ok_or(CpmmQuoteError::InsufficientInputAfterFees)?
    };

    u64::try_from(output).map_err(|_| CpmmQuoteError::OutputTooLarge)
}

fn ceil_fee(amount: u128, rate: u64) -> Result<u128, CpmmQuoteError> {
    if rate == 0 || amount == 0 {
        return Ok(0);
    }

    let numerator = amount
        .checked_mul(rate as u128)
        .ok_or(CpmmQuoteError::ArithmeticOverflow)?;

    let denominator = FEE_DENOMINATOR as u128;

    numerator
        .checked_add(denominator - 1)
        .ok_or(CpmmQuoteError::ArithmeticOverflow)
        .map(|v| v / denominator)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(
        reserve_0: u64,
        reserve_1: u64,
        trade_fee_rate: u64,
        creator_fee_rate: u64,
        creator_fee_on: u8,
        enable_creator_fee: bool,
    ) -> CpmmQuoteState {
        CpmmQuoteState {
            pool_id: Pubkey::default(),
            token_0_vault: Pubkey::default(),
            token_1_vault: Pubkey::default(),
            token_0_mint: Pubkey::default(),
            token_1_mint: Pubkey::default(),
            token_0_program: spl_token::id(),
            token_1_program: spl_token::id(),
            vault_0_raw: reserve_0,
            vault_1_raw: reserve_1,
            protocol_fees_token_0: 0,
            protocol_fees_token_1: 0,
            fund_fees_token_0: 0,
            fund_fees_token_1: 0,
            creator_fees_token_0: 0,
            creator_fees_token_1: 0,
            trade_fee_rate,
            protocol_fee_rate: 0,
            fund_fee_rate: 0,
            creator_fee_rate,
            creator_fee_on,
            enable_creator_fee,
            status: 0,
            open_time: 0,
            is_token_0_wsol: true,
        }
    }

    #[test]
    fn exact_cpmm_quote_uses_trade_fee_and_floor() {
        let s = state(1_000_000, 2_000_000, 2_500, 0, 0, false);

        let out = quote_output_amount(&s, 1_000, true).unwrap();

        // trade fee = ceil(1000 * 2500 / 1e6) = 3
        // effective input = 997
        // output = floor(997 * 2_000_000 / 1_000_997)
        assert_eq!(out, 1992);
    }

    #[test]
    fn effective_reserves_subtract_accumulated_fees() {
        let mut s = state(1_000_000, 2_000_000, 0, 0, 0, false);

        s.protocol_fees_token_0 = 10_000;

        let out = quote_output_amount(&s, 1_000, true).unwrap();

        assert_eq!(
            out,
            (1_000u128 * 2_000_000u128 / (990_000u128 + 1_000u128)) as u64
        );
    }

    #[test]
    fn creator_fee_on_input_uses_combined_fee_rounding() {
        // Raydium calculates:
        //
        // total_fee = ceil(amount * (trade + creator) / 1e6)
        //
        // rather than:
        //
        // ceil(amount * trade / 1e6)
        // + ceil(amount * creator / 1e6)
        //
        // For amount=2, trade=1, creator=1:
        // combined = ceil(4 / 1e6) = 1
        // separate = 1 + 1 = 2
        //
        // Therefore the exact Raydium curve input is 1.
        let s = state(1_000_000, 2_000_000, 1, 1, 0, true);

        let out = quote_output_amount(&s, 2, true).unwrap();

        let expected = 1u128 * 2_000_000u128 / (1_000_000u128 + 1u128);

        assert_eq!(out, expected as u64);
    }

    #[test]
    fn creator_fee_on_input_is_charged_before_curve() {
        let s = state(1_000_000, 2_000_000, 0, 10_000, 0, true);

        let out = quote_output_amount(&s, 1_000, true).unwrap();

        // combined fee = ceil(1000 * 1% / 1e6) = 10
        // creator fee split = 10
        // curve input = 990
        assert_eq!(
            out,
            (990u128 * 2_000_000u128 / (1_000_000u128 + 990u128)) as u64
        );
    }

    #[test]
    fn creator_fee_output_mode_is_removed_after_curve() {
        let s = state(1_000_000, 2_000_000, 0, 10_000, 1, true);

        let out = quote_output_amount(&s, 1_000, true);

        let raw = 1_000u128 * 2_000_000u128 / (1_000_000u128 + 1_000u128);

        let expected = raw - ((raw * 10_000 + 999_999) / 1_000_000);

        assert_eq!(out.unwrap(), expected as u64);
    }

    #[test]
    fn disabled_swap_is_detected() {
        let mut s = state(1_000_000, 2_000_000, 0, 0, 0, false);

        s.status = SWAP_STATUS_BIT;

        assert!(!is_swap_open(&s, 1_000));
        assert_eq!(
            ensure_swap_open(&s, 1_000),
            Err(CpmmQuoteError::SwapDisabled)
        );
    }

    #[test]
    fn pool_open_time_is_respected() {
        let mut s = state(1_000_000, 2_000_000, 0, 0, 0, false);

        s.open_time = 2_000;

        assert!(!is_swap_open(&s, 1_999));
        assert!(is_swap_open(&s, 2_000));
        assert_eq!(
            ensure_swap_open(&s, 1_999),
            Err(CpmmQuoteError::PoolNotOpen)
        );
    }

    #[test]
    fn token_2022_is_rejected_for_exact_quote() {
        let mut s = state(1_000_000, 2_000_000, 0, 0, 0, false);

        s.token_0_program = Pubkey::new_from_array([7u8; 32]);

        assert_eq!(
            quote_output_amount(&s, 1_000, true),
            Err(CpmmQuoteError::Token2022Unsupported)
        );
    }
}
