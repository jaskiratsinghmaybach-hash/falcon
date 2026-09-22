pub mod orca;
pub mod raydium;
pub mod raydium_cpmm;

use anyhow::{bail, Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::Instruction, message::Message, pubkey::Pubkey, signature::Keypair, signer::Signer,
    system_instruction, transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use std::str::FromStr;

use crate::analyzer::{self, Opportunity};
use crate::scanner::orca::WhirlpoolInfo;

pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Known accounts for a specific Raydium pool. Loaded once per run from live chain state.
pub struct RaydiumPoolContext {
    pub pool_id: Pubkey,
    pub amm_authority: Pubkey,
    pub coin_vault: Pubkey,
    pub pc_vault: Pubkey,
}

/// Known accounts for a specific Orca pool. Loaded once per run from live chain state.
pub struct OrcaPoolContext {
    pub pool_id: Pubkey,
    pub info: WhirlpoolInfo,
}

pub fn simulate_cpmm_swap(
    client: &RpcClient,
    payer: &Keypair,
    pool_info: &crate::scanner::raydium_cpmm::CpmmPoolInfo,
    input_mint: &Pubkey,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Result<()> {
    let wallet = payer.pubkey();
    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;

    let output_mint = if pool_info.token_0_mint == *input_mint {
        pool_info.token_1_mint
    } else {
        pool_info.token_0_mint
    };

    let non_sol_mint = if pool_info.token_0_mint == wsol_mint {
        pool_info.token_1_mint
    } else {
        pool_info.token_0_mint
    };

    let (wsol_ata, wsol_exists, other_ata, other_exists) =
        check_wallet_atas(client, &wallet, &non_sol_mint)?;

    let (input_token_account, output_token_account) = if *input_mint == wsol_mint {
        (wsol_ata, other_ata)
    } else {
        (other_ata, wsol_ata)
    };

    let (input_vault, output_vault) = if pool_info.token_0_mint == *input_mint {
        (pool_info.token_0_vault, pool_info.token_1_vault)
    } else {
        (pool_info.token_1_vault, pool_info.token_0_vault)
    };

    let (input_token_program, output_token_program) = if pool_info.token_0_mint == *input_mint {
        (pool_info.token_0_program, pool_info.token_1_program)
    } else {
        (pool_info.token_1_program, pool_info.token_0_program)
    };

    let mut instructions = vec![];
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)?;

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

    let accounts = raydium_cpmm::SwapAccounts {
        payer: wallet,
        amm_config: pool_info.amm_config,
        pool_state: pool_info.pool_state,
        input_token_account,
        output_token_account,
        input_vault,
        output_vault,
        input_token_program,
        output_token_program,
        input_token_mint: *input_mint,
        output_token_mint: output_mint,
        observation_state: pool_info.observation_key,
    };

    instructions.push(raydium_cpmm::build_swap_instruction(
        &accounts,
        amount_in,
        minimum_amount_out,
    )?);

    simulate(client, payer, instructions)
}

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

    let non_sol_mint = if pool.token_mint_a == wsol_mint {
        pool.token_mint_b
    } else {
        pool.token_mint_a
    };

    let (wsol_ata, wsol_exists, other_ata, other_exists) =
        check_wallet_atas(client, &wallet, &non_sol_mint)?;

    let (token_owner_account_a, token_owner_account_b) = if pool.token_mint_a == wsol_mint {
        (wsol_ata, other_ata)
    } else {
        (other_ata, wsol_ata)
    };

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

/// Builds a single swap instruction for the named DEX, spending `input_mint` for the
/// other token. Direction-agnostic: works whether SOL or the other token is being spent.
fn build_leg_instruction(
    dex_name: &str,
    raydium_ctx: &RaydiumPoolContext,
    orca_ctx: &OrcaPoolContext,
    wallet: Pubkey,
    wsol_ata: Pubkey,
    other_ata: Pubkey,
    input_mint: &Pubkey,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Result<Instruction> {
    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;

    match dex_name {
        "Raydium" => {
            let (user_source, user_destination) = if *input_mint == wsol_mint {
                (wsol_ata, other_ata)
            } else {
                (other_ata, wsol_ata)
            };

            let accounts = raydium::SwapAccounts {
                amm_pool: raydium_ctx.pool_id,
                amm_authority: raydium_ctx.amm_authority,
                pool_coin_token_account: raydium_ctx.coin_vault,
                pool_pc_token_account: raydium_ctx.pc_vault,
                user_source,
                user_destination,
                user_owner: wallet,
            };

            raydium::build_swap_instruction(&accounts, amount_in, minimum_amount_out)
        }
        "Orca" => {
            let info = &orca_ctx.info;
            let (token_owner_account_a, token_owner_account_b) = if info.token_mint_a == wsol_mint {
                (wsol_ata, other_ata)
            } else {
                (other_ata, wsol_ata)
            };

            let a_to_b = info.token_mint_a == *input_mint;

            let accounts = orca::SwapAccounts {
                whirlpool: orca_ctx.pool_id,
                token_owner_account_a,
                token_vault_a: info.token_vault_a,
                token_owner_account_b,
                token_vault_b: info.token_vault_b,
                token_authority: wallet,
            };

            orca::build_swap_instruction(
                &accounts,
                info.tick_current_index,
                info.tick_spacing,
                amount_in,
                minimum_amount_out,
                a_to_b,
            )
        }
        other => bail!("Unknown DEX: {other}"),
    }
}

/// Builds and simulates the full atomic arb transaction: buy on `opportunity.buy_dex`,
/// sell on `opportunity.sell_dex`, direction chosen entirely by the Analyzer's output.
/// Never sends anything - simulation only.
#[allow(clippy::too_many_arguments)]
pub fn simulate_opportunity(
    client: &RpcClient,
    payer: &Keypair,
    opportunity: &Opportunity,
    raydium_ctx: &RaydiumPoolContext,
    orca_ctx: &OrcaPoolContext,
    other_token_mint: &Pubkey,
    other_token_decimals: u32,
) -> Result<()> {
    let wallet = payer.pubkey();
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)?;
    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;

    let (wsol_ata, wsol_exists, other_ata, other_exists) =
        check_wallet_atas(client, &wallet, other_token_mint)?;

    let amount_in_sol_ui = opportunity.trade_size_base * opportunity.buy_price;
    let amount_in_lamports = (amount_in_sol_ui * 1_000_000_000.0) as u64;

    // Estimate what the buy leg produces, using the buy-side pool's reserves, so we can
    // feed a real amount into the sell leg rather than guessing.

    // Use the Analyzer's own computed output from the buy leg, not a re-derived guess -
    // this is the exact amount the buy leg is expected to produce, so the sell leg
    // spends exactly that, keeping Analyzer and Executor in agreement.
    let other_token_amount_raw =
        (opportunity.expected_output_after_buy * 10f64.powi(other_token_decimals as i32)) as u64;

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
            other_token_mint,
            &token_program,
        ));
    }

    // Fund WSOL for the buy leg.
    instructions.push(system_instruction::transfer(
        &wallet,
        &wsol_ata,
        amount_in_lamports,
    ));
    instructions.push(spl_token::instruction::sync_native(
        &token_program,
        &wsol_ata,
    )?);

    // Leg 1: buy - spend SOL, receive the other token.
    const SLIPPAGE_TOLERANCE_PCT: f64 = 1.0; // 1% tolerance below expected output

    let buy_minimum_out_ui = analyzer::calculate_minimum_out(
        opportunity.expected_output_after_buy,
        SLIPPAGE_TOLERANCE_PCT,
    );
    let buy_minimum_out_raw = (buy_minimum_out_ui * 10f64.powi(other_token_decimals as i32)) as u64;

    let buy_ix = build_leg_instruction(
        &opportunity.buy_dex,
        raydium_ctx,
        orca_ctx,
        wallet,
        wsol_ata,
        other_ata,
        &wsol_mint,
        amount_in_lamports,
        buy_minimum_out_raw,
    )?;
    instructions.push(buy_ix);

    // Leg 2: sell - spend the other token (amount = what we expect to have received),
    // receive SOL back.
    let sell_minimum_out_sol = analyzer::calculate_minimum_out(
        opportunity.expected_output_after_sell,
        SLIPPAGE_TOLERANCE_PCT,
    );
    let sell_minimum_out_lamports = (sell_minimum_out_sol * 1_000_000_000.0) as u64;

    let sell_ix = build_leg_instruction(
        &opportunity.sell_dex,
        raydium_ctx,
        orca_ctx,
        wallet,
        wsol_ata,
        other_ata,
        other_token_mint,
        other_token_amount_raw,
        sell_minimum_out_lamports,
    )?;
    instructions.push(sell_ix);

    tracing::info!(
        "Built atomic opportunity tx: buy on {} ({} lamports SOL in), sell on {} ({} raw units token in)",
        opportunity.buy_dex, amount_in_lamports, opportunity.sell_dex, other_token_amount_raw
    );

    simulate(client, payer, instructions)
}
