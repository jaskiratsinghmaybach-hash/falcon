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

/// What a successful simulation reports.
#[derive(Debug, Clone, Copy, Default)]
pub struct SimReport {
    pub units_consumed: Option<u64>,
}

fn simulate(client: &RpcClient, payer: &Keypair, instructions: Vec<Instruction>) -> Result<()> {
    simulate_with_blockhash(client, payer, instructions, None).map(|_| ())
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
) -> Result<SimReport> {
    let recent_blockhash = match cached_blockhash {
        Some(h) => h,
        None => client
            .get_latest_blockhash()
            .context("Failed to fetch recent blockhash")?,
    };

    // Program of each instruction, in submission order. Message compilation
    // preserves instruction order, so the index inside a chain-reported
    // `InstructionError` indexes this list. Needed to know WHICH program's
    // error code we are looking at (codes collide across programs).
    let instruction_programs: Vec<Pubkey> = instructions.iter().map(|ix| ix.program_id).collect();

    let message = Message::new(&instructions, Some(&payer.pubkey()));

    let mut tx = Transaction::new_unsigned(message);

    tx.sign(&[payer], recent_blockhash);

    tracing::info!(
        "Simulating transaction with {} instructions...",
        tx.message.instructions.len()
    );

    // Transport failure is a failed simulation (typed as `rpc_error`).
    let sim = client.simulate_transaction(&tx).map_err(|e| {
        anyhow::Error::new(crate::failure::SimulationFailure::Rpc(format!(
            "Failed to simulate transaction: {e}"
        )))
    })?;

    if let Some(logs) = &sim.value.logs {
        for (index, log) in logs.iter().enumerate() {
            tracing::info!("LOG[{}]: {}", index, log);
        }
    }

    tracing::info!("Compute units consumed: {:?}", sim.value.units_consumed);

    // Simulation truth gate: success requires the RPC call to have succeeded
    // (we are past it) AND `value.err == None`. Any chain-reported error is a
    // failed simulation, returned as a typed, classifiable failure.
    if let Err(failure) =
        crate::failure::simulation_outcome(sim.value.err.clone(), &instruction_programs)
    {
        tracing::error!(
            failure_class = %failure.class(),
            "SIMULATION FAILED: {}",
            failure
        );

        return Err(failure.into());
    }

    tracing::info!("SIMULATION SUCCESS: transaction executed successfully with no error");

    Ok(SimReport {
        units_consumed: sim.value.units_consumed,
    })
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
    orca_current_tick: Option<i32>,
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
            let current_tick = orca_current_tick.ok_or_else(|| {
                anyhow::anyhow!("Executor fail closed: live Orca current tick missing")
            })?;

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
                current_tick,
                info.tick_spacing,
                amount_in,
                minimum_amount_out,
                a_to_b,
            )
        }

        other => bail!("Unknown DEX: {other}"),
    }
}

/// Slippage tolerance applied to each leg's expected output when deriving its
/// `minimum_amount_out`. The sell leg is additionally floored at break-even
/// plus fees plus the required profit margin (see `sell_minimum_out`).
const SLIPPAGE_TOLERANCE_BPS: u32 = 100;

/// Minimum SOL the sell leg must return. The chain enforces this, so a trade
/// that would lose money after fees reverts (costing only the tx fee) instead
/// of completing. `relax_profit_floor` is the FORCE_SIM plumbing test only.
pub fn sell_minimum_out(
    expected_sell_raw: u64,
    amount_in_lamports: u64,
    min_profit_lamports: i128,
    relax_profit_floor: bool,
) -> Result<u64> {
    if relax_profit_floor {
        return Ok(1);
    }

    let base = analyzer::calculate_minimum_out_raw(expected_sell_raw, SLIPPAGE_TOLERANCE_BPS)
        .context("Failed to compute sell-leg minimum_out")?;

    let fixed_cost = crate::config::fixed_tx_cost_lamports(
        crate::config::TX_SIGNATURES,
        crate::config::COMPUTE_UNIT_LIMIT,
        crate::config::COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
    )
    .context("fixed tx cost overflow")?;

    let floor =
        crate::risk::sell_leg_floor_lamports(amount_in_lamports, fixed_cost, min_profit_lamports)
            .context("sell-leg floor overflow")?;

    Ok(base.max(floor))
}

/// Builds the full atomic arbitrage instruction list (compute budget, any
/// missing ATAs, wrap, buy leg, sell leg). Simulation and broadcast both use
/// exactly this list, so what is simulated is what is sent.
#[allow(clippy::too_many_arguments)]
fn build_opportunity_instructions(
    wallet: Pubkey,
    opportunity: &Opportunity,
    raydium_ctx: &RaydiumPoolContext,
    orca_ctx: &OrcaPoolContext,
    other_token_mint: &Pubkey,
    ata_cache: &AtaCache,
    orca_state: Option<&crate::scanner::orca::OrcaQuoteState>,
    min_profit_lamports: i128,
    relax_profit_floor: bool,
) -> Result<Vec<Instruction>> {
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)?;

    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;

    let AtaCache {
        wsol_ata,
        wsol_exists,
        other_ata,
        other_exists,
    } = *ata_cache;

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

    let buy_minimum_out_raw = analyzer::calculate_minimum_out_raw(
        opportunity.expected_output_after_buy_raw,
        SLIPPAGE_TOLERANCE_BPS,
    )
    .context("Failed to compute buy-leg minimum_out")?;

    let orca_is_involved = opportunity.buy_dex == "Orca" || opportunity.sell_dex == "Orca";
    let orca_current_tick = if orca_is_involved {
        let state = orca_state.ok_or_else(|| {
            anyhow::anyhow!(
                "Executor fail closed: live Orca state missing for opportunity involving Orca"
            )
        })?;

        if state.whirlpool_pubkey != orca_ctx.pool_id {
            anyhow::bail!(
                "Executor fail closed: Orca state pool {} does not match context pool {}",
                state.whirlpool_pubkey,
                orca_ctx.pool_id
            );
        }

        if let crate::scanner::orca::CoherenceStatus::Incoherent { worst_drift } =
            state.coherence_status()
        {
            anyhow::bail!(
                "Executor fail closed: Orca state incoherent with worst slot drift {}",
                worst_drift
            );
        }

        let orca_is_buy = opportunity.buy_dex == "Orca";
        let a_to_b = analyzer::orca_leg_a_to_b(state, orca_is_buy);

        let _ = analyzer::select_tick_arrays_for_direction(
            &state.tick_arrays,
            state.whirlpool.tick_current_index,
            state.whirlpool.tick_spacing,
            a_to_b,
        )
        .map_err(|e| anyhow::anyhow!("Executor fail closed: required tick arrays missing: {e}"))?;

        Some(state.whirlpool.tick_current_index)
    } else {
        None
    };

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
        orca_current_tick,
    )?;

    instructions.push(buy_ix);

    instructions.insert(
        0,
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(
            crate::config::COMPUTE_UNIT_LIMIT,
        ),
    );

    instructions.insert(
        1,
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(
            crate::config::COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        ),
    );

    let sell_minimum_out_lamports = sell_minimum_out(
        opportunity.expected_output_after_sell_raw,
        amount_in_lamports,
        min_profit_lamports,
        relax_profit_floor,
    )?;

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
        orca_current_tick,
    )?;

    instructions.push(sell_ix);

    tracing::info!(
        "Built atomic opportunity tx: buy on {} ({} lamports SOL in), sell on {} ({} raw units token in, floor {} lamports out)",
        opportunity.buy_dex,
        amount_in_lamports,
        opportunity.sell_dex,
        other_token_amount_raw,
        sell_minimum_out_lamports
    );

    Ok(instructions)
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
    cached_blockhash: Option<solana_sdk::hash::Hash>,
    ata_cache: &AtaCache,
    orca_state: Option<&crate::scanner::orca::OrcaQuoteState>,
    min_profit_lamports: i128,
    // Dry-run plumbing test (FORCE_SIM): drop the sell-leg breakeven floor so
    // a deliberately unprofitable trade still simulates cleanly. Simulation
    // only - nothing is ever broadcast by this function.
    relax_profit_floor: bool,
) -> Result<SimReport> {
    let instructions = build_opportunity_instructions(
        payer.pubkey(),
        opportunity,
        raydium_ctx,
        orca_ctx,
        other_token_mint,
        ata_cache,
        orca_state,
        min_profit_lamports,
        relax_profit_floor,
    )?;

    simulate_with_blockhash(client, payer, instructions, cached_blockhash)
}

/// Outcome of one broadcast attempt. `pnl_lamports` is the measured change in
/// wallet SOL + WSOL value (negative = the fee or a loss), never an estimate.
#[derive(Debug, Clone)]
pub struct SendReport {
    pub signature: solana_sdk::signature::Signature,
    /// The transaction reached `confirmed` commitment (successfully OR with an
    /// on-chain error - a reverted tx still lands and pays its fee).
    pub landed: bool,
    /// On-chain error, if the transaction landed but failed.
    pub tx_error: Option<String>,
    pub pnl_lamports: i128,
}

/// Wallet value in lamports: native SOL plus the WSOL ATA's token balance,
/// both read at `confirmed` commitment (the default RpcClient commitment is
/// `finalized`, which lags and would make a fresh trade look like zero).
pub fn wallet_value_lamports(
    client: &RpcClient,
    wallet: &Pubkey,
    wsol_ata: &Pubkey,
) -> Result<i128> {
    use solana_sdk::commitment_config::CommitmentConfig;
    let confirmed = CommitmentConfig::confirmed();

    let sol = client
        .get_balance_with_commitment(wallet, confirmed)
        .context("Failed to read wallet SOL balance")?
        .value;

    // A missing WSOL ATA is a legitimate zero balance.
    let wsol = match client.get_token_account_balance_with_commitment(wsol_ata, confirmed) {
        Ok(resp) => resp.value.amount.parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    };

    Ok(sol as i128 + wsol as i128)
}

/// LIVE ONLY. Builds the same atomic transaction that was just simulated,
/// broadcasts it, and waits until it lands or its blockhash expires, then
/// measures the real PnL from wallet balances.
///
/// Safety properties:
/// - atomic: both legs are in ONE transaction, so a failed leg reverts both;
/// - the sell leg carries an on-chain floor (see `sell_minimum_out`), so a
///   trade that would lose money reverts and costs only the fee;
/// - refuses to send if the wallet cannot cover size + fee + reserve;
/// - node-side preflight simulation stays ON.
#[allow(clippy::too_many_arguments)]
pub fn send_opportunity(
    client: &RpcClient,
    payer: &Keypair,
    opportunity: &Opportunity,
    raydium_ctx: &RaydiumPoolContext,
    orca_ctx: &OrcaPoolContext,
    other_token_mint: &Pubkey,
    ata_cache: &AtaCache,
    orca_state: Option<&crate::scanner::orca::OrcaQuoteState>,
    min_profit_lamports: i128,
    wallet_reserve_lamports: u64,
) -> Result<SendReport> {
    use solana_client::rpc_config::RpcSendTransactionConfig;
    use solana_sdk::commitment_config::{CommitmentConfig, CommitmentLevel};

    let wallet = payer.pubkey();

    let fixed_cost = crate::config::fixed_tx_cost_lamports(
        crate::config::TX_SIGNATURES,
        crate::config::COMPUTE_UNIT_LIMIT,
        crate::config::COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
    )
    .context("fixed tx cost overflow")?;

    let sol_balance = client
        .get_balance_with_commitment(&wallet, CommitmentConfig::confirmed())
        .context("Failed to read wallet balance before send")?
        .value;
    crate::risk::check_wallet_funds(
        sol_balance,
        opportunity.trade_size_lamports,
        fixed_cost,
        wallet_reserve_lamports,
    )?;

    let value_before = wallet_value_lamports(client, &wallet, &ata_cache.wsol_ata)?;

    let instructions = build_opportunity_instructions(
        wallet,
        opportunity,
        raydium_ctx,
        orca_ctx,
        other_token_mint,
        ata_cache,
        orca_state,
        min_profit_lamports,
        false,
    )?;

    // Fresh blockhash WITH its validity height: that height is how we know,
    // later, that an unconfirmed transaction can never land.
    let (blockhash, last_valid_block_height) = client
        .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
        .context("Failed to fetch blockhash for send")?;

    let message = Message::new(&instructions, Some(&wallet));
    let mut tx = Transaction::new_unsigned(message);
    tx.sign(&[payer], blockhash);

    let signature = client
        .send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: false,
                preflight_commitment: Some(CommitmentLevel::Confirmed),
                ..RpcSendTransactionConfig::default()
            },
        )
        .map_err(|e| anyhow::anyhow!("Broadcast rejected before landing: {e}"))?;

    tracing::warn!("LIVE: transaction broadcast, signature {signature}");

    // Wait for the outcome. Only conclude "dropped" once the blockhash has
    // expired - before that the transaction may still land, and guessing
    // would let the next trade run on an unknown wallet state.
    let mut landed = false;
    let mut tx_error: Option<String> = None;
    loop {
        match client.get_signature_statuses(&[signature]) {
            Ok(resp) => {
                if let Some(Some(status)) = resp.value.first() {
                    if status.satisfies_commitment(CommitmentConfig::confirmed()) {
                        landed = true;
                        tx_error = status.err.as_ref().map(|e| format!("{e:?}"));
                        break;
                    }
                }
            }
            Err(e) => tracing::warn!("signature status poll failed: {e}"),
        }

        match client.get_block_height_with_commitment(CommitmentConfig::confirmed()) {
            Ok(height) if height > last_valid_block_height => {
                // One last look: the status can race the height check.
                if let Ok(resp) = client.get_signature_statuses(&[signature]) {
                    if let Some(Some(status)) = resp.value.first() {
                        if status.satisfies_commitment(CommitmentConfig::confirmed()) {
                            landed = true;
                            tx_error = status.err.as_ref().map(|e| format!("{e:?}"));
                        }
                    }
                }
                break;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("block height poll failed: {e}"),
        }

        std::thread::sleep(std::time::Duration::from_millis(400));
    }

    let value_after = wallet_value_lamports(client, &wallet, &ata_cache.wsol_ata)?;
    let pnl_lamports = value_after - value_before;

    Ok(SendReport {
        signature,
        landed,
        tx_error,
        pnl_lamports,
    })
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

#[cfg(test)]
mod sell_floor_tests {
    use super::*;

    // fixed cost = 13_750 (see config cost model)
    #[test]
    fn floor_binds_when_profit_is_thin() {
        // 1 SOL in, expected out only 0.3% above: 1% slippage base (0.99*) is
        // BELOW break-even, so the fee+margin floor must win.
        let out = sell_minimum_out(1_003_000_000, 1_000_000_000, 50_000, false).unwrap();
        assert_eq!(out, 1_000_000_000 + 13_750 + 50_000);
    }

    #[test]
    fn slippage_base_binds_when_profit_is_fat() {
        let out = sell_minimum_out(1_100_000_000, 1_000_000_000, 50_000, false).unwrap();
        assert!(out > 1_000_000_000 + 13_750 + 50_000);
        assert!(out < 1_100_000_000);
    }

    #[test]
    fn floor_never_allows_a_net_loss() {
        for expected in [1_000_000_000u64, 1_000_100_000, 1_050_000_000] {
            let out = sell_minimum_out(expected, 1_000_000_000, 0, false).unwrap();
            assert!(out >= 1_000_000_000 + 13_750);
        }
    }

    #[test]
    fn relaxed_floor_is_only_for_force_sim() {
        assert_eq!(sell_minimum_out(5, 1_000_000_000, 50_000, true).unwrap(), 1);
    }
}
