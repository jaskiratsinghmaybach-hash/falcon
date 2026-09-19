use anyhow::{Context, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::str::FromStr;

use super::TOKEN_PROGRAM_ID;

pub const RAYDIUM_AMM_V4_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

pub struct SwapAccounts {
    pub amm_pool: Pubkey,
    pub amm_authority: Pubkey,
    pub pool_coin_token_account: Pubkey,
    pub pool_pc_token_account: Pubkey,
    pub user_source: Pubkey,
    pub user_destination: Pubkey,
    pub user_owner: Pubkey,
}

/// Raydium AMM v4 SwapBaseInV2 (tag 16) - 8 accounts, no OpenBook/market accounts.
pub fn build_swap_instruction(
    accounts: &SwapAccounts,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Result<Instruction> {
    let program_id =
        Pubkey::from_str(RAYDIUM_AMM_V4_PROGRAM).context("Invalid Raydium program ID")?;
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID).context("Invalid Token program ID")?;

    let mut data = vec![16u8];
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());

    let account_metas = vec![
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new(accounts.amm_pool, false),
        AccountMeta::new_readonly(accounts.amm_authority, false),
        AccountMeta::new(accounts.pool_coin_token_account, false),
        AccountMeta::new(accounts.pool_pc_token_account, false),
        AccountMeta::new(accounts.user_source, false),
        AccountMeta::new(accounts.user_destination, false),
        AccountMeta::new_readonly(accounts.user_owner, true),
    ];

    Ok(Instruction {
        program_id,
        accounts: account_metas,
        data,
    })
}

