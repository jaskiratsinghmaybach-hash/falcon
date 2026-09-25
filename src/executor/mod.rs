pub mod orca;
pub mod raydium;
pub mod raydium_cpmm;

use anyhow::{bail, Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
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
pub enum RaydiumPoolContext {
    Amm {
        pool_id: Pubkey,
        amm_authority: Pubkey,
        coin_vault: Pubkey,
        pc_vault: Pubkey,
    },
    Cpmm {
        pool_info: crate::scanner::raydium_cpmm::CpmmPoolInfo,
    },
}

impl RaydiumPoolContext {
    pub fn load_amm(client: &RpcClient, pool_id: &str) -> Result<Self> {
        let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid Raydium pool pubkey")?;

        let vaults = crate::scanner::raydium::fetch_pool_vaults(client, pool_id)?;

        let amm_program = Pubkey::from_str("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8")?;

        let (amm_authority, _) = Pubkey::find_program_address(&[b"amm authority"], &amm_program);

        Ok(Self::Amm {
            pool_id: pool_pubkey,
            amm_authority,
            coin_vault: vaults.coin_vault,
            pc_vault: vaults.pc_vault,
        })
    }

    pub fn load_cpmm(client: &RpcClient, pool_id: &str) -> Result<Self> {
        let pool_info = crate::scanner::raydium_cpmm::fetch_pool_info(client, pool_id)?;

        Ok(Self::Cpmm { pool_info })
    }

    pub fn load(
        client: &RpcClient,
        pool_id: &str,
        decoder: crate::config::DecoderType,
    ) -> Result<Self> {
        match decoder {
            crate::config::DecoderType::Amm => Self::load_amm(client, pool_id),
            crate::config::DecoderType::Cpmm => Self::load_cpmm(client, pool_id),
        }
    }
}

/// Known accounts for a specific Orca pool. Loaded once per run from live chain state.
pub struct OrcaPoolContext {
    pub pool_id: Pubkey,
    pub info: WhirlpoolInfo,
}

impl OrcaPoolContext {
    /// Loads the Whirlpool's static account layout once at startup.
    pub fn load(client: &RpcClient, pool_id: &str) -> Result<Self> {
        let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid Orca Whirlpool pubkey")?;

        let info = crate::scanner::orca::fetch_pool_info(client, pool_id)?;

        Ok(Self {
            pool_id: pool_pubkey,
            info,
        })
    }
}

/// Cached ATA pubkeys + existence, computed once at startup.
#[derive(Debug, Clone, Copy)]
pub struct AtaCache {
    pub wsol_ata: Pubkey,
    pub wsol_exists: bool,
    pub other_ata: Pubkey,
    pub other_exists: bool,
}

impl AtaCache {
    /// One-time RPC check at startup.
    pub fn load(client: &RpcClient, wallet: &Pubkey, other_token_mint: &Pubkey) -> Result<Self> {
        let (wsol_ata, wsol_exists, other_ata, other_exists) =
            check_wallet_atas(client, wallet, other_token_mint)?;

        Ok(Self {
            wsol_ata,
            wsol_exists,
            other_ata,
            other_exists,
        })
    }
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
    simulate_with_blockhash(client, payer, instructions, None)
}

/// Simulate a transaction and enforce simulation truth.
///
/// IMPORTANT:
/// `RpcClient::simulate_transaction()` can return successfully at the RPC
/// transport level while the simulated transaction itself contains a
/// Solana program error in `sim.value.err`.
///
/// That program error is NOT a successful simulation.
///
/// Therefore:
///
///     RPC request success
///         !=
///     transaction simulation success
///
/// This function treats `sim.value.err` as a hard failure.
fn simulate_with_blockhash(
    client: &RpcClient,
    payer: &Keypair,
    instructions: Vec<Instruction>,
    cached_blockhash: Option<solana_sdk::hash::Hash>,
) -> Result<()> {
    let recent_blockhash = match cached_blockhash {
        Some(h) => h,
        None => client
            .get_latest_blockhash()
            .context("Failed to fetch recent blockhash")?,
    };

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

    if let Some(logs) = &sim.value.logs {
        for (index, log) in logs.iter().enumerate() {
            tracing::info!("LOG[{}]: {}", index, log);
        }
    }

    tracing::info!("Compute units consumed: {:?}", sim.value.units_consumed);

    if let Some(err) = &sim.value.err {
        tracing::error!(
            "SIMULATION FAILED: transaction returned a Solana execution error: {:?}",
            err
        );

        bail!("transaction simulation failed: {:?}", err);
    }

    tracing::info!("SIMULATION SUCCESS: transaction executed successfully with no error");

    Ok(())
}

/// Builds a single swap instruction for the named DEX, spending `input_mint`
/// for the other token. Direction-agnostic.
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
        "Raydium" | "RaydiumCPMM" => match raydium_ctx {
            RaydiumPoolContext::Amm {
                pool_id,
                amm_authority,
                coin_vault,
                pc_vault,
            } => {
                let (user_source, user_destination) = if *input_mint == wsol_mint {
                    (wsol_ata, other_ata)
                } else {
                    (other_ata, wsol_ata)
                };

                let accounts = raydium::SwapAccounts {
                    amm_pool: *pool_id,
                    amm_authority: *amm_authority,
                    pool_coin_token_account: *coin_vault,
                    pool_pc_token_account: *pc_vault,
                    user_source,
                    user_destination,
                    user_owner: wallet,
                };

                raydium::build_swap_instruction(&accounts, amount_in, minimum_amount_out)
            }

            RaydiumPoolContext::Cpmm { pool_info } => {
                let output_mint = if pool_info.token_0_mint == *input_mint {
                    pool_info.token_1_mint
                } else {
                    pool_info.token_0_mint
                };

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

                let (input_token_program, output_token_program) =
                    if pool_info.token_0_mint == *input_mint {
                        (pool_info.token_0_program, pool_info.token_1_program)
                    } else {
                        (pool_info.token_1_program, pool_info.token_0_program)
                    };

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

                raydium_cpmm::build_swap_instruction(&accounts, amount_in, minimum_amount_out)
            }
        },

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

/// Builds and simulates the full atomic arbitrage transaction.
///
/// Never sends anything - simulation only.
///
/// The analyzer's expected buy output is used directly as the sell input.
#[allow(clippy::too_many_arguments)]
pub fn simulate_opportunity(
    client: &RpcClient,
    payer: &Keypair,
    opportunity: &Opportunity,
    raydium_ctx: &RaydiumPoolContext,
    orca_ctx: &OrcaPoolContext,
    other_token_mint: &Pubkey,
    other_token_decimals: u32,
    cached_blockhash: Option<solana_sdk::hash::Hash>,
    ata_cache: &AtaCache,
) -> Result<()> {
    let wallet = payer.pubkey();

    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)?;

    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;

    let AtaCache {
        wsol_ata,
        wsol_exists,
        other_ata,
        other_exists,
    } = *ata_cache;

    let _ = other_token_decimals;

    let amount_in_lamports = opportunity.trade_size_lamports;

    let other_token_amount_raw = opportunity.expected_output_after_buy_raw;

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

    instructions.push(system_instruction::transfer(
        &wallet,
        &wsol_ata,
        amount_in_lamports,
    ));

    instructions.push(spl_token::instruction::sync_native(
        &token_program,
        &wsol_ata,
    )?);

    const SLIPPAGE_TOLERANCE_BPS: u32 = 100;

    let buy_minimum_out_raw = analyzer::calculate_minimum_out_raw(
        opportunity.expected_output_after_buy_raw,
        SLIPPAGE_TOLERANCE_BPS,
    )
    .context("Failed to compute buy-leg minimum_out")?;

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

    instructions.insert(
        0,
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(350_000),
    );

    instructions.insert(
        1,
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(25_000),
    );

    let base_sell_minimum_out = analyzer::calculate_minimum_out_raw(
        opportunity.expected_output_after_sell_raw,
        SLIPPAGE_TOLERANCE_BPS,
    )
    .context("Failed to compute sell-leg minimum_out")?;

    let breakeven_lamports = amount_in_lamports + 1_000;

    let sell_minimum_out_lamports = base_sell_minimum_out.max(breakeven_lamports);

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
        opportunity.buy_dex,
        amount_in_lamports,
        opportunity.sell_dex,
        other_token_amount_raw
    );

    simulate_with_blockhash(client, payer, instructions, cached_blockhash)
}

use solana_sdk::hash::Hash;
use std::sync::{Arc, RwLock};

pub type BlockhashCache = Arc<RwLock<Hash>>;

pub fn spawn_blockhash_poller(rpc_url: String, interval_ms: u64) -> Result<BlockhashCache> {
    let client = RpcClient::new(rpc_url);

    let initial_hash = client
        .get_latest_blockhash()
        .context("Failed to get initial blockhash")?;

    let cache = Arc::new(RwLock::new(initial_hash));

    let cache_clone = Arc::clone(&cache);

    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));

        match client.get_latest_blockhash() {
            Ok(hash) => {
                if let Ok(mut lock) = cache_clone.write() {
                    *lock = hash;
                }
            }

            Err(e) => {
                tracing::warn!("Failed to refresh blockhash: {}", e);
            }
        }
    });

    Ok(cache)
}

pub fn ensure_wallet_atas(
    client: &RpcClient,
    payer: &Keypair,
    other_token_mint: &Pubkey,
) -> Result<(Pubkey, Pubkey)> {
    let wallet = payer.pubkey();

    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)?;

    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;

    let (wsol_ata, wsol_exists, other_ata, other_exists) =
        check_wallet_atas(client, &wallet, other_token_mint)?;

    let mut setup_ixs = Vec::new();

    if !wsol_exists {
        setup_ixs.push(create_associated_token_account(
            &wallet,
            &wallet,
            &wsol_mint,
            &token_program,
        ));
    }

    if !other_exists {
        setup_ixs.push(create_associated_token_account(
            &wallet,
            &wallet,
            other_token_mint,
            &token_program,
        ));
    }

    if !setup_ixs.is_empty() {
        tracing::info!("Pre-creating {} missing ATA(s)...", setup_ixs.len());

        let recent_blockhash = client.get_latest_blockhash()?;

        let message = Message::new(&setup_ixs, Some(&wallet));

        let mut tx = Transaction::new_unsigned(message);

        tx.sign(&[payer], recent_blockhash);

        client
            .send_and_confirm_transaction(&tx)
            .context("Failed to pre-create ATAs")?;

        tracing::info!("ATAs successfully pre-created and ready!");
    } else {
        tracing::info!("All necessary ATAs already exist!");
    }

    Ok((wsol_ata, other_ata))
}
