use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
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

/// Builds a Raydium AMM v4 SwapBaseInV2 instruction (8 accounts, no OpenBook/market accounts).
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

pub fn check_wallet_atas(
    client: &RpcClient,
    wallet: &Pubkey,
    other_token_mint: &Pubkey,
) -> Result<(Pubkey, bool, Pubkey, bool)> {
    let wsol_mint = Pubkey::from_str(WSOL_MINT).context("Invalid WSOL mint")?;

    let wsol_ata = get_associated_token_address(wallet, &wsol_mint);
    let other_ata = get_associated_token_address(wallet, other_token_mint);

    let wsol_exists = client.get_account(&wsol_ata).is_ok();
    let other_exists = client.get_account(&other_ata).is_ok();

    tracing::info!("WSOL ATA: {} (exists: {})", wsol_ata, wsol_exists);
    tracing::info!("Other token ATA: {} (exists: {})", other_ata, other_exists);

    Ok((wsol_ata, wsol_exists, other_ata, other_exists))
}

/// Builds a full transaction (ATA creation if needed + wrap SOL + Raydium swap) and runs it
/// through simulateTransaction. Does NOT send anything - purely a dry run against real chain state.
pub fn simulate_raydium_swap(
    client: &RpcClient,
    payer: &Keypair,
    amm_pool: &Pubkey,
    amm_authority: &Pubkey,
    pool_coin_token_account: &Pubkey,
    pool_pc_token_account: &Pubkey,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Result<()> {
    let wallet = payer.pubkey();
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)?;
    let wsol_mint = Pubkey::from_str(WSOL_MINT).context("Invalid WSOL mint")?;

    let (wsol_ata, wsol_exists, other_ata, other_exists) =
        check_wallet_atas(client, &wallet, output_mint)?;

    let (user_source, user_destination) = if *input_mint == wsol_mint {
        (wsol_ata, other_ata)
    } else {
        (other_ata, wsol_ata)
    };

    let mut instructions = vec![];

    if !wsol_exists {
        instructions.push(create_associated_token_account(
            &wallet,
            &wallet,
            &wsol_mint,
            &token_program,
        ));
    }
    if !other_exists {
        instructions.push(create_associated_token_account(
            &wallet,
            &wallet,
            output_mint,
            &token_program,
        ));
    }

    // If we're spending SOL (input_mint is WSOL), we need to actually fund the WSOL
    // token account: transfer lamports into it, then sync_native so the token balance
    // reflects that transfer. Native SOL and "SOL sitting in a WSOL token account" are
    // not the same thing - this step bridges the two.
    if *input_mint == wsol_mint {
        instructions.push(system_instruction::transfer(&wallet, &wsol_ata, amount_in));
        instructions.push(spl_token::instruction::sync_native(
            &token_program,
            &wsol_ata,
        )?);
    }

    let swap_accounts = RaydiumSwapAccounts {
        amm_pool: *amm_pool,
        amm_authority: *amm_authority,
        pool_coin_token_account: *pool_coin_token_account,
        pool_pc_token_account: *pool_pc_token_account,
        user_source,
        user_destination,
        user_owner: wallet,
    };

    let swap_ix = build_raydium_swap_instruction(&swap_accounts, amount_in, minimum_amount_out)?;
    instructions.push(swap_ix);

    let recent_blockhash = client
        .get_latest_blockhash()
        .context("Failed to fetch recent blockhash")?;

    let message = Message::new(&instructions, Some(&wallet));
    let mut tx = Transaction::new_unsigned(message);
    tx.sign(&[payer], recent_blockhash);

    tracing::info!(
        "Simulating transaction with {} instructions...",
        tx.message.instructions.len()
    );

    let sim_result = client
        .simulate_transaction(&tx)
        .context("Failed to simulate transaction")?;

    tracing::info!("Simulation result: {:#?}", sim_result.value);

    if let Some(err) = &sim_result.value.err {
        tracing::warn!("Simulation returned an error: {:?}", err);
    } else {
        tracing::info!("Simulation succeeded with no error!");
    }

    if let Some(logs) = &sim_result.value.logs {
        for log in logs {
            tracing::info!("LOG: {}", log);
        }
    }

    Ok(())
}
