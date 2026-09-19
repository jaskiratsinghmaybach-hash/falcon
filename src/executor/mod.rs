pub mod orca;
pub mod raydium;

use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::Instruction, message::Message, pubkey::Pubkey, signature::Keypair, signer::Signer,
    system_instruction, transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use std::str::FromStr;

pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

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

/// Signs and simulates a set of instructions. Never sends anything.
fn simulate(client: &RpcClient, payer: &Keypair, instructions: Vec<Instruction>) -> Result<()> {
    let recent_blockhash = client
        .get_latest_blockhash()
        .context("Failed to fetch recent blockhash")?;

    let message = Message::new(&instructions, Some(&payer.pubkey()));
    let mut tx = Transaction::new_unsigned(message);
    tx.sign(&[payer], recent_blockhash);

    tracing::info!(
        "Simulating transaction with {} instructions...",
        tx.message.instructions.len()
    );

    let sim = client
        .simulate_transaction(&tx)
        .context("Failed to simulate transaction")?;

    if let Some(err) = &sim.value.err {
        tracing::warn!("Simulation returned an error: {:?}", err);
    } else {
        tracing::info!("Simulation succeeded with no error!");
    }

    if let Some(logs) = &sim.value.logs {
        for log in logs {
            tracing::info!("LOG: {}", log);
        }
    }

    tracing::info!("Compute units consumed: {:?}", sim.value.units_consumed);

    Ok(())
}

/// Simulates a single Raydium swap (SOL -> other token).
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
    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;

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

    if *input_mint == wsol_mint {
        instructions.push(system_instruction::transfer(&wallet, &wsol_ata, amount_in));
        instructions.push(spl_token::instruction::sync_native(
            &token_program,
            &wsol_ata,
        )?);
    }

    let accounts = raydium::SwapAccounts {
        amm_pool: *amm_pool,
        amm_authority: *amm_authority,
        pool_coin_token_account: *pool_coin_token_account,
        pool_pc_token_account: *pool_pc_token_account,
        user_source,
        user_destination,
        user_owner: wallet,
    };

    instructions.push(raydium::build_swap_instruction(
        &accounts,
        amount_in,
        minimum_amount_out,
    )?);

    simulate(client, payer, instructions)
}

/// Simulates a single Orca Whirlpool swap (SOL -> other token).
pub fn simulate_orca_swap(
    client: &RpcClient,
    payer: &Keypair,
    whirlpool: &Pubkey,
    pool: &crate::scanner::orca::WhirlpoolInfo,
    input_mint: &Pubkey,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Result<()> {
    let wallet = payer.pubkey();
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)?;
    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;

    // Whichever mint isn't the input is the output.
    let other_mint = if pool.token_mint_a == *input_mint {
        pool.token_mint_b
    } else {
        pool.token_mint_a
    };

    let non_sol_mint = if pool.token_mint_a == wsol_mint {
        pool.token_mint_b
    } else {
        pool.token_mint_a
    };

    let (wsol_ata, wsol_exists, other_ata, other_exists) =
        check_wallet_atas(client, &wallet, &non_sol_mint)?;

    // Map our two ATAs onto the pool's A/B slots.
    let (token_owner_account_a, token_owner_account_b) = if pool.token_mint_a == wsol_mint {
        (wsol_ata, other_ata)
    } else {
        (other_ata, wsol_ata)
    };

    // a_to_b is true when we're spending the pool's token A.
    let a_to_b = pool.token_mint_a == *input_mint;

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
            &non_sol_mint,
            &token_program,
        ));
    }

    if *input_mint == wsol_mint {
        instructions.push(system_instruction::transfer(&wallet, &wsol_ata, amount_in));
        instructions.push(spl_token::instruction::sync_native(
            &token_program,
            &wsol_ata,
        )?);
    }

    tracing::info!(
        "Orca swap direction: spending {}, receiving {}, a_to_b={}",
        input_mint,
        other_mint,
        a_to_b
    );

    let accounts = orca::SwapAccounts {
        whirlpool: *whirlpool,
        token_owner_account_a,
        token_vault_a: pool.token_vault_a,
        token_owner_account_b,
        token_vault_b: pool.token_vault_b,
        token_authority: wallet,
    };

    instructions.push(orca::build_swap_instruction(
        &accounts,
        pool.tick_current_index,
        pool.tick_spacing,
        amount_in,
        minimum_amount_out,
        a_to_b,
    )?);

    simulate(client, payer, instructions)
}

