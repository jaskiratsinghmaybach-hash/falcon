use anyhow::{Context, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::str::FromStr;

pub const RAYDIUM_CPMM_PROGRAM: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

const SWAP_BASE_INPUT_DISCRIMINATOR: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];

pub struct SwapAccounts {
    pub payer: Pubkey,
    pub amm_config: Pubkey,
    pub pool_state: Pubkey,
    pub input_token_account: Pubkey,
    pub output_token_account: Pubkey,
    pub input_vault: Pubkey,
    pub output_vault: Pubkey,
    pub input_token_program: Pubkey,
    pub output_token_program: Pubkey,
    pub input_token_mint: Pubkey,
    pub output_token_mint: Pubkey,
    pub observation_state: Pubkey,
}

fn program_id() -> Result<Pubkey> {
    Pubkey::from_str(RAYDIUM_CPMM_PROGRAM).context("Invalid CPMM program ID")
}

fn derive_authority() -> Result<Pubkey> {
    let (pda, _) = Pubkey::find_program_address(&[b"vault_and_lp_mint_auth_seed"], &program_id()?);
    Ok(pda)
}

/// Raydium CPMM SwapBaseInput.
pub fn build_swap_instruction(
    accounts: &SwapAccounts,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Result<Instruction> {
    let program = program_id()?;
    let authority = derive_authority()?;

    let mut data = SWAP_BASE_INPUT_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());

    let account_metas = vec![
        AccountMeta::new_readonly(accounts.payer, true),
        AccountMeta::new_readonly(authority, false),
        AccountMeta::new_readonly(accounts.amm_config, false),
        AccountMeta::new(accounts.pool_state, false),
        AccountMeta::new(accounts.input_token_account, false),
        AccountMeta::new(accounts.output_token_account, false),
        AccountMeta::new(accounts.input_vault, false),
        AccountMeta::new(accounts.output_vault, false),
        AccountMeta::new_readonly(accounts.input_token_program, false),
        AccountMeta::new_readonly(accounts.output_token_program, false),
        AccountMeta::new_readonly(accounts.input_token_mint, false),
        AccountMeta::new_readonly(accounts.output_token_mint, false),
        AccountMeta::new(accounts.observation_state, false),
    ];

    Ok(Instruction {
        program_id: program,
        accounts: account_metas,
        data,
    })
}
