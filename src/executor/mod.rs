use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
};
use spl_associated_token_account::get_associated_token_address;
use std::str::FromStr;

const RAYDIUM_AMM_V4_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

pub struct RaydiumSwapAccounts {
    pub amm_pool: Pubkey,
    pub amm_authority: Pubkey,
    pub pool_coin_token_account: Pubkey,
    pub pool_pc_token_account: Pubkey,
    pub user_source: Pubkey,
    pub user_destination: Pubkey,
    pub user_owner: Pubkey,
}

pub fn build_raydium_swap_instruction(
    accounts: &RaydiumSwapAccounts,
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

/// Checks whether the wallet has the ATAs needed to swap RAY <-> SOL, and reports
/// their derived addresses plus whether each currently exists on-chain.
pub fn check_wallet_atas(
    client: &RpcClient,
    wallet: &Pubkey,
    other_token_mint: &Pubkey,
) -> Result<()> {
    let wsol_mint = Pubkey::from_str(WSOL_MINT).context("Invalid WSOL mint")?;

    let wsol_ata = get_associated_token_address(wallet, &wsol_mint);
    let other_ata = get_associated_token_address(wallet, other_token_mint);

    let wsol_exists = client.get_account(&wsol_ata).is_ok();
    let other_exists = client.get_account(&other_ata).is_ok();

    tracing::info!("WSOL ATA: {} (exists: {})", wsol_ata, wsol_exists);
    tracing::info!("Other token ATA: {} (exists: {})", other_ata, other_exists);

    Ok(())
}

