pub mod jito;
pub mod kamino;
pub mod orca;
pub mod raydium;
pub mod raydium_cpmm;

use anyhow::{bail, Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    hash::Hash,
    instruction::Instruction,
    message::Message,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::{Transaction, VersionedTransaction},
};
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::{create_associated_token_account, create_associated_token_account_idempotent},
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
    cached_blockhash: Option<Hash>,
) -> Result<()> {
    let recent_blockhash = resolve_blockhash(client, cached_blockhash)?;

    // Program of each instruction, in submission order. Message compilation
    // preserves instruction order, so the index inside a chain-reported
    // `InstructionError` indexes this list. Needed to know WHICH program's
    // error code we are looking at (codes collide across programs).
    let instruction_programs: Vec<Pubkey> = instructions.iter().map(|ix| ix.program_id).collect();

    let message = Message::new(&instructions, Some(&payer.pubkey()));

    let mut tx = Transaction::new_unsigned(message);

    tx.sign(&[payer], recent_blockhash);

    simulate_signed(
        client,
        &VersionedTransaction::from(tx),
        &instruction_programs,
    )
}

/// Simulates an already-signed transaction and applies the simulation
/// truth gate (see `simulate_with_blockhash`).
fn simulate_signed(
    client: &RpcClient,
    tx: &VersionedTransaction,
    instruction_programs: &[Pubkey],
) -> Result<()> {
    tracing::info!(
        "Simulating transaction with {} instructions...",
        tx.message.instructions().len()
    );

    // Transport failure is a failed simulation (typed as `rpc_error`).
    let sim = client.simulate_transaction(tx).map_err(|e| {
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
        crate::failure::simulation_outcome(sim.value.err.clone(), instruction_programs)
    {
        tracing::error!(
            failure_class = %failure.class(),
            "SIMULATION FAILED: {}",
            failure
        );

        return Err(failure.into());
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

/// Slippage tolerance applied to the sell leg's quoted output. The on-chain
/// profit floor (see `sell_minimum_out_lamports`) is usually the binding
/// constraint; this only matters when it is stricter.
const SELL_LEG_SLIPPAGE_BPS: u32 = 100;

/// The sell leg's `minimum_amount_out`: the larger of the slippage-adjusted
/// quote and the PROFIT FLOOR (SOL in + every lamport of tx fees + the
/// configured minimum profit). Because both legs live in one transaction,
/// the floor makes the whole arbitrage revert on-chain unless it actually
/// nets at least `min_profit_lamports` after fees.
pub fn sell_minimum_out_lamports(
    amount_in_lamports: u64,
    expected_sell_out_lamports: u64,
    fixed_cost_lamports: u64,
    min_profit_lamports: u64,
) -> Result<u64> {
    let slippage_floor =
        analyzer::calculate_minimum_out_raw(expected_sell_out_lamports, SELL_LEG_SLIPPAGE_BPS)
            .context("Failed to compute sell-leg minimum_out")?;

    let profit_floor = amount_in_lamports
        .checked_add(fixed_cost_lamports)
        .and_then(|v| v.checked_add(min_profit_lamports))
        .and_then(|v| v.checked_add(1))
        .context("Profit floor overflow")?;

    Ok(slippage_floor.max(profit_floor))
}

/// Optional execution features layered onto the atomic transaction.
#[derive(Debug, Clone, Default)]
pub struct ExecExtras {
    /// POOL mode: fund the buy leg with a Kamino flash loan instead of the
    /// wallet. Borrow and repay sit in the same transaction as both legs.
    pub flash: Option<kamino::KaminoReserve>,
    /// Jito tip (lamports) appended as the last instruction; 0 = no tip.
    pub jito_tip_lamports: u64,
    /// Address lookup tables; when non-empty the arb is sent as a v0
    /// transaction (needed to fit the flash-loan accounts in 1232 bytes).
    pub lookup_tables: Vec<AddressLookupTableAccount>,
}

/// Max serialized transaction size accepted by the cluster.
pub const MAX_TX_BYTES: usize = 1232;

/// Wire size of a transaction (signature count fits a 1-byte short_vec).
pub fn serialized_tx_size(tx: &VersionedTransaction) -> usize {
    1 + 64 * tx.signatures.len() + tx.message.serialize().len()
}

/// A fully built, signed atomic arbitrage transaction plus the program of
/// each instruction (in order) for failure classification.
pub struct BuiltArbTx {
    pub tx: VersionedTransaction,
    pub instruction_programs: Vec<Pubkey>,
}

/// Builds and signs the full atomic arbitrage transaction:
///
/// compute budget -> create WSOL ATA (idempotent) -> wrap SOL -> buy leg ->
/// sell leg (with on-chain profit floor) -> close WSOL ATA (unwrap back to
/// native SOL).
///
/// Every step is in ONE transaction, so either the whole round trip lands
/// with at least the minimum profit or nothing happens (only the fee is
/// spent on a revert).
#[allow(clippy::too_many_arguments)]
pub fn build_opportunity_instructions(
    wallet: Pubkey,
    opportunity: &Opportunity,
    raydium_ctx: &RaydiumPoolContext,
    orca_ctx: &OrcaPoolContext,
    other_token_mint: &Pubkey,
    ata_cache: &AtaCache,
    orca_state: Option<&crate::scanner::orca::OrcaQuoteState>,
    extras: &ExecExtras,
    // Dry-run plumbing test (FORCE_SIM): drop the sell-leg profit floor so a
    // deliberately unprofitable trade still simulates cleanly. Never set for
    // a transaction that may be broadcast.
    relax_profit_floor: bool,
) -> Result<Vec<Instruction>> {
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)?;
    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;

    let AtaCache {
        wsol_ata,
        other_ata,
        other_exists,
        ..
    } = *ata_cache;

    let amount_in_lamports = opportunity.trade_size_lamports;
    if amount_in_lamports == 0 {
        bail!("Executor fail closed: zero trade size");
    }

    let mut instructions = vec![
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(
            crate::config::COMPUTE_UNIT_LIMIT,
        ),
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(
            crate::config::COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        ),
        // The WSOL ATA is closed at the end of every arb, so it is
        // (re)created here every time. Idempotent: a no-op if it exists.
        create_associated_token_account_idempotent(&wallet, &wallet, &wsol_mint, &token_program),
    ];

    if !other_exists {
        instructions.push(create_associated_token_account_idempotent(
            &wallet,
            &wallet,
            other_token_mint,
            &token_program,
        ));
    }

    // Fund the buy leg: flash-borrow WSOL straight into the WSOL ATA (POOL
    // mode), or wrap the wallet's own SOL.
    let flash_borrow = match &extras.flash {
        Some(flash) => {
            let index = u8::try_from(instructions.len()).context("Too many instructions")?;
            instructions.push(flash.borrow_ix(wallet, wsol_ata, amount_in_lamports));
            Some((flash, index))
        }
        None => {
            instructions.push(system_instruction::transfer(
                &wallet,
                &wsol_ata,
                amount_in_lamports,
            ));
            instructions.push(spl_token::instruction::sync_native(
                &token_program,
                &wsol_ata,
            )?);
            None
        }
    };

    // Buy leg minimum == amount the sell leg spends, so the sell leg can
    // never try to spend more base token than the buy leg delivered.
    let buy_minimum_out_raw = analyzer::calculate_minimum_out_raw(
        opportunity.expected_output_after_buy_raw,
        crate::config::BUY_LEG_SLIPPAGE_BPS,
    )
    .context("Failed to compute buy-leg minimum_out")?;
    let sell_amount_in_raw = buy_minimum_out_raw;
    if sell_amount_in_raw == 0 {
        bail!("Executor fail closed: buy leg guarantees zero output");
    }

    let orca_is_involved = opportunity.buy_dex == "Orca" || opportunity.sell_dex == "Orca";
    let orca_current_tick = if orca_is_involved {
        let state = orca_state.ok_or_else(|| {
            anyhow::anyhow!(
                "Executor fail closed: live Orca state missing for opportunity involving Orca"
            )
        })?;

        if state.whirlpool_pubkey != orca_ctx.pool_id {
            bail!(
                "Executor fail closed: Orca state pool {} does not match context pool {}",
                state.whirlpool_pubkey,
                orca_ctx.pool_id
            );
        }

        if let crate::scanner::orca::CoherenceStatus::Incoherent { worst_drift } =
            state.coherence_status()
        {
            bail!(
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

    instructions.push(build_leg_instruction(
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
    )?);

    let sell_minimum_out = if relax_profit_floor {
        1
    } else {
        let fixed_cost = crate::config::fixed_tx_cost_lamports(
            crate::config::TX_SIGNATURES,
            crate::config::COMPUTE_UNIT_LIMIT,
            crate::config::COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        )
        .context("Fixed tx cost overflow")?;
        let flash_fee = match &flash_borrow {
            Some((flash, _)) => flash
                .flash_fee(amount_in_lamports)
                .context("Flash-loan fee overflow")?,
            None => 0,
        };
        let all_costs = fixed_cost
            .checked_add(flash_fee)
            .and_then(|v| v.checked_add(extras.jito_tip_lamports))
            .context("Cost overflow")?;
        sell_minimum_out_lamports(
            amount_in_lamports,
            opportunity.expected_output_after_sell_raw,
            all_costs,
            crate::config::min_profit_lamports(),
        )?
    };

    instructions.push(build_leg_instruction(
        &opportunity.sell_dex,
        raydium_ctx,
        orca_ctx,
        wallet,
        wsol_ata,
        other_ata,
        other_token_mint,
        sell_amount_in_raw,
        sell_minimum_out,
        orca_current_tick,
    )?);

    // Repay principal + fee from the WSOL ATA. klend checks this pairs with
    // the borrow at `index`; if the sell leg came up short, this fails and
    // the whole transaction reverts.
    if let Some((flash, index)) = flash_borrow {
        instructions.push(flash.repay_ix(wallet, wsol_ata, amount_in_lamports, index));
    }

    // Unwrap: closing the WSOL ATA returns every lamport in it (principal or
    // remaining profit + rent) to the wallet as native SOL.
    instructions.push(spl_token::instruction::close_account(
        &token_program,
        &wsol_ata,
        &wallet,
        &wallet,
        &[],
    )?);

    // Jito tip last: only paid if every instruction above succeeded.
    if extras.jito_tip_lamports > 0 {
        instructions.push(jito::tip_ix(
            &wallet,
            &jito::pick_tip_account(),
            extras.jito_tip_lamports,
        ));
    }

    tracing::info!(
        "Built atomic arb tx: buy on {} ({} lamports in, min {} raw out), sell on {} ({} raw in, min {} lamports out)",
        opportunity.buy_dex,
        amount_in_lamports,
        buy_minimum_out_raw,
        opportunity.sell_dex,
        sell_amount_in_raw,
        sell_minimum_out
    );

    Ok(instructions)
}

/// `build_opportunity_instructions` + sign with `payer` as fee payer.
#[allow(clippy::too_many_arguments)]
pub fn build_opportunity_transaction(
    payer: &Keypair,
    opportunity: &Opportunity,
    raydium_ctx: &RaydiumPoolContext,
    orca_ctx: &OrcaPoolContext,
    other_token_mint: &Pubkey,
    recent_blockhash: Hash,
    ata_cache: &AtaCache,
    orca_state: Option<&crate::scanner::orca::OrcaQuoteState>,
    extras: &ExecExtras,
    relax_profit_floor: bool,
) -> Result<BuiltArbTx> {
    let wallet = payer.pubkey();
    let instructions = build_opportunity_instructions(
        wallet,
        opportunity,
        raydium_ctx,
        orca_ctx,
        other_token_mint,
        ata_cache,
        orca_state,
        extras,
        relax_profit_floor,
    )?;

    let instruction_programs: Vec<Pubkey> = instructions.iter().map(|ix| ix.program_id).collect();
    let tx = if extras.lookup_tables.is_empty() {
        let message = Message::new(&instructions, Some(&wallet));
        let mut tx = Transaction::new_unsigned(message);
        tx.try_sign(&[payer], recent_blockhash)
            .context("Failed to sign arb transaction")?;
        VersionedTransaction::from(tx)
    } else {
        let message = v0::Message::try_compile(
            &wallet,
            &instructions,
            &extras.lookup_tables,
            recent_blockhash,
        )
        .context("Failed to compile v0 arb message")?;
        VersionedTransaction::try_new(VersionedMessage::V0(message), &[payer])
            .context("Failed to sign arb transaction")?
    };

    let size = serialized_tx_size(&tx);
    if size > MAX_TX_BYTES {
        bail!(
            "Executor fail closed: arb transaction is {size} bytes (max {MAX_TX_BYTES}); add an address lookup table (LOOKUP_TABLES)"
        );
    }

    Ok(BuiltArbTx {
        tx,
        instruction_programs,
    })
}

fn resolve_blockhash(client: &RpcClient, cached_blockhash: Option<Hash>) -> Result<Hash> {
    match cached_blockhash {
        Some(h) => Ok(h),
        None => client
            .get_latest_blockhash()
            .context("Failed to fetch recent blockhash"),
    }
}

/// Builds and simulates the full atomic arbitrage transaction.
/// Never sends anything - simulation only.
#[allow(clippy::too_many_arguments)]
pub fn simulate_opportunity(
    client: &RpcClient,
    payer: &Keypair,
    opportunity: &Opportunity,
    raydium_ctx: &RaydiumPoolContext,
    orca_ctx: &OrcaPoolContext,
    other_token_mint: &Pubkey,
    cached_blockhash: Option<Hash>,
    ata_cache: &AtaCache,
    orca_state: Option<&crate::scanner::orca::OrcaQuoteState>,
    extras: &ExecExtras,
    relax_profit_floor: bool,
) -> Result<()> {
    let built = build_opportunity_transaction(
        payer,
        opportunity,
        raydium_ctx,
        orca_ctx,
        other_token_mint,
        resolve_blockhash(client, cached_blockhash)?,
        ata_cache,
        orca_state,
        extras,
        relax_profit_floor,
    )?;
    simulate_signed(client, &built.tx, &built.instruction_programs)
}

/// Result of `execute_opportunity`.
#[derive(Debug)]
pub enum ExecutionOutcome {
    /// Simulation passed; live trading is off so nothing was sent.
    Simulated,
    /// Simulation passed and the exact simulated transaction was broadcast.
    Sent(solana_sdk::signature::Signature),
}

/// Builds the atomic arb transaction, simulates it (simulation truth gate),
/// and - only if `live` and the simulation succeeded - broadcasts the EXACT
/// transaction that was simulated. Landing is confirmed in the background.
#[allow(clippy::too_many_arguments)]
pub fn execute_opportunity(
    client: &RpcClient,
    payer: &Keypair,
    opportunity: &Opportunity,
    raydium_ctx: &RaydiumPoolContext,
    orca_ctx: &OrcaPoolContext,
    other_token_mint: &Pubkey,
    cached_blockhash: Option<Hash>,
    ata_cache: &AtaCache,
    orca_state: Option<&crate::scanner::orca::OrcaQuoteState>,
    extras: &ExecExtras,
    jito_sender: Option<&jito::JitoSender>,
    live: bool,
) -> Result<ExecutionOutcome> {
    let built = build_opportunity_transaction(
        payer,
        opportunity,
        raydium_ctx,
        orca_ctx,
        other_token_mint,
        resolve_blockhash(client, cached_blockhash)?,
        ata_cache,
        orca_state,
        extras,
        false,
    )?;

    simulate_signed(client, &built.tx, &built.instruction_programs)?;

    if !live {
        return Ok(ExecutionOutcome::Simulated);
    }

    let signature = match jito_sender {
        Some(jito) => jito.send(&built.tx)?,
        None => client
            .send_transaction_with_config(
                &built.tx,
                solana_client::rpc_config::RpcSendTransactionConfig {
                    // The identical transaction was just simulated; a second
                    // preflight only adds latency.
                    skip_preflight: true,
                    max_retries: Some(2),
                    ..Default::default()
                },
            )
            .context("Failed to send arb transaction")?,
    };

    tracing::warn!(
        "ARB SENT via {}: {} (expected net profit {} lamports)",
        if jito_sender.is_some() { "Jito" } else { "RPC" },
        signature,
        opportunity.net_profit_lamports
    );

    spawn_confirmation_watcher(client.url(), signature);

    Ok(ExecutionOutcome::Sent(signature))
}

/// Polls the signature status in the background and logs whether the arb
/// landed, reverted, or expired. Never blocks the hot path.
fn spawn_confirmation_watcher(rpc_url: String, signature: solana_sdk::signature::Signature) {
    std::thread::spawn(move || {
        let client = RpcClient::new(rpc_url);
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(750));
            match client.get_signature_statuses(&[signature]) {
                Ok(resp) => {
                    if let Some(Some(status)) = resp.value.first() {
                        match &status.err {
                            None => {
                                tracing::warn!("ARB LANDED: {} (slot {})", signature, status.slot);
                                return;
                            }
                            Some(err) => {
                                tracing::error!(
                                    "ARB REVERTED ON-CHAIN: {} (slot {}): {:?} - only the fee was spent",
                                    signature,
                                    status.slot,
                                    err
                                );
                                return;
                            }
                        }
                    }
                }
                Err(e) => tracing::debug!("Signature status poll failed for {}: {}", signature, e),
            }
        }
        tracing::warn!(
            "ARB NOT CONFIRMED within ~30s (likely dropped/expired): {}",
            signature
        );
    });
}

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
mod profit_floor_tests {
    use super::*;

    #[test]
    fn profit_floor_covers_principal_fees_and_min_profit() {
        // expected out barely above principal: the profit floor binds.
        let min_out = sell_minimum_out_lamports(1_000_000, 1_010_000, 13_750, 5_000).unwrap();
        assert_eq!(min_out, 1_000_000 + 13_750 + 5_000 + 1);
    }

    #[test]
    fn slippage_floor_binds_when_stricter() {
        // 1% below 2_000_000 = 1_980_000 > profit floor.
        let min_out = sell_minimum_out_lamports(1_000_000, 2_000_000, 13_750, 0).unwrap();
        assert_eq!(min_out, 1_980_000);
    }

    #[test]
    fn profit_floor_overflow_is_error() {
        assert!(sell_minimum_out_lamports(u64::MAX, 0, 1, 0).is_err());
    }
}

/// Live mainnet check of the full atomic transaction (no keys, nothing sent):
/// `cargo test mainnet_sim -- --ignored --nocapture`. Uses a SOL-rich system
/// account as fee payer with signature verification disabled.
/// Env overrides: MAINNET_RPC_URL, SIM_PAYER, SIM_RAYDIUM_CPMM_POOL, SIM_ORCA_POOL.
#[cfg(test)]
mod mainnet_sim_tests {
    use super::*;
    use crate::scanner::{orca as sorca, raydium_cpmm as scpmm};
    use solana_client::rpc_config::RpcSimulateTransactionConfig;
    use solana_sdk::commitment_config::CommitmentConfig;

    fn env_or(key: &str, default: &str) -> String {
        std::env::var(key).unwrap_or_else(|_| default.to_string())
    }

    fn simulate_unsigned(
        client: &RpcClient,
        payer: &Pubkey,
        ixs: &[Instruction],
    ) -> solana_client::rpc_response::RpcSimulateTransactionResult {
        let tx = Transaction::new_unsigned(Message::new(ixs, Some(payer)));
        client
            .simulate_transaction_with_config(
                &tx,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::confirmed()),
                    ..Default::default()
                },
            )
            .expect("simulate RPC failed")
            .value
    }

    fn v0_tx(
        ixs: &[Instruction],
        payer: &Pubkey,
        luts: &[AddressLookupTableAccount],
    ) -> VersionedTransaction {
        let msg = v0::Message::try_compile(payer, ixs, luts, Hash::default()).unwrap();
        VersionedTransaction {
            signatures: vec![solana_sdk::signature::Signature::default()],
            message: VersionedMessage::V0(msg),
        }
    }

    fn v0_size(ixs: &[Instruction], payer: &Pubkey, luts: &[AddressLookupTableAccount]) -> usize {
        serialized_tx_size(&v0_tx(ixs, payer, luts))
    }

    fn simulate_unsigned_v0(
        client: &RpcClient,
        payer: &Pubkey,
        ixs: &[Instruction],
        luts: &[AddressLookupTableAccount],
    ) -> solana_client::rpc_response::RpcSimulateTransactionResult {
        client
            .simulate_transaction_with_config(
                &v0_tx(ixs, payer, luts),
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::confirmed()),
                    ..Default::default()
                },
            )
            .expect("simulate RPC failed")
            .value
    }

    #[test]
    #[ignore]
    fn mainnet_sim_atomic_arb_round_trip() {
        let client = RpcClient::new_with_commitment(
            env_or("MAINNET_RPC_URL", "https://api.mainnet-beta.solana.com"),
            CommitmentConfig::confirmed(),
        );
        let payer: Pubkey = env_or("SIM_PAYER", "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9")
            .parse()
            .unwrap();
        let ray_id = env_or(
            "SIM_RAYDIUM_CPMM_POOL",
            "Q2sPHPdUWFMg7M7wwrQKLrn619cAucfRsmhVJffodSp",
        );
        let orca_id = env_or(
            "SIM_ORCA_POOL",
            "HsQGWEh3ib6w59rBh5n1jXmi8VXFBqKEjxozL6PGfcgb",
        );

        let raydium_ctx = RaydiumPoolContext::load_cpmm(&client, &ray_id).unwrap();
        let orca_ctx = OrcaPoolContext::load(&client, &orca_id).unwrap();

        // Raydium CPMM exact quote state.
        let ray_pk: Pubkey = ray_id.parse().unwrap();
        let pool_state =
            scpmm::decode_pool_state(&client.get_account(&ray_pk).unwrap().data).unwrap();
        let amm_config =
            scpmm::decode_amm_config(&client.get_account(&pool_state.amm_config).unwrap().data)
                .unwrap();
        let bal = |v: &Pubkey| -> u64 {
            client
                .get_token_account_balance(v)
                .unwrap()
                .amount
                .parse()
                .unwrap()
        };
        let (v0, v1) = (
            bal(&pool_state.token_0_vault),
            bal(&pool_state.token_1_vault),
        );
        scpmm::publish_quote_state(scpmm::quote_state_from_accounts(
            ray_pk,
            &pool_state,
            &amm_config,
            v0,
            v1,
        ));
        let ray_price = scpmm::CpmmStaticContext::from_loaded(ray_pk, &pool_state, &amm_config)
            .price_from_raw_reserves(v0, v1, "SIM");

        // Orca exact quote state: pool + 5-array window, one slot.
        let slot = client.get_slot().unwrap();
        let whirlpool =
            sorca::decode_whirlpool(&client.get_account(&orca_ctx.pool_id).unwrap().data).unwrap();
        let mut tick_arrays: Vec<sorca::SlottedTickArray> = sorca::subscription_tick_array_pdas(
            &orca_ctx.pool_id,
            whirlpool.tick_current_index,
            whirlpool.tick_spacing,
        )
        .unwrap()
        .iter()
        .filter_map(|pda| {
            sorca::load_tick_array(&client, pda, &orca_ctx.pool_id, whirlpool.tick_spacing).ok()
        })
        .map(|array| sorca::SlottedTickArray { array, slot })
        .collect();
        tick_arrays.sort_by_key(|s| s.array.start_tick_index);
        let orca_state = sorca::OrcaQuoteState {
            whirlpool_pubkey: orca_ctx.pool_id,
            whirlpool: whirlpool.clone(),
            tick_arrays,
            pool_slot: slot,
        };
        sorca::publish_orca_quote_state(orca_state.clone());
        let orca_price = sorca::OrcaStaticContext::load(&client, &orca_id)
            .unwrap()
            .price_from_whirlpool(&client, &whirlpool, "SIM")
            .unwrap();

        let opp = analyzer::build_forced_opportunity(&ray_price, &orca_price, 10_000_000)
            .expect("forced opportunity");
        println!(
            "buy {} -> sell {}: in {} lamports, buy out {} raw, sell out {} lamports, net {}",
            opp.buy_dex,
            opp.sell_dex,
            opp.trade_size_lamports,
            opp.expected_output_after_buy_raw,
            opp.expected_output_after_sell_raw,
            opp.net_profit_lamports
        );

        let wsol_mint: Pubkey = WSOL_MINT.parse().unwrap();
        let other_mint = if orca_ctx.info.token_mint_a == wsol_mint {
            orca_ctx.info.token_mint_b
        } else {
            orca_ctx.info.token_mint_a
        };
        let ata_cache = AtaCache {
            wsol_ata: get_associated_token_address(&payer, &wsol_mint),
            wsol_exists: false,
            other_ata: get_associated_token_address(&payer, &other_mint),
            other_exists: false,
        };

        // 1. Plumbing: full round trip with the profit floor relaxed must succeed.
        let ixs = build_opportunity_instructions(
            payer,
            &opp,
            &raydium_ctx,
            &orca_ctx,
            &other_mint,
            &ata_cache,
            Some(&orca_state),
            &ExecExtras::default(),
            true,
        )
        .unwrap();
        let sim = simulate_unsigned(&client, &payer, &ixs);
        for l in sim.logs.iter().flatten() {
            println!("  {l}");
        }
        assert!(
            sim.err.is_none(),
            "relaxed round trip failed: {:?}",
            sim.err
        );
        println!("relaxed round trip OK, CU used {:?}", sim.units_consumed);
        assert!(sim.units_consumed.unwrap_or(0) <= crate::config::COMPUTE_UNIT_LIMIT as u64);

        // 2. Atomic guard: with the profit floor on, an unprofitable trade
        //    must revert in the sell leg instead of landing at a loss.
        if opp.net_profit_lamports <= 0 {
            let ixs = build_opportunity_instructions(
                payer,
                &opp,
                &raydium_ctx,
                &orca_ctx,
                &other_mint,
                &ata_cache,
                Some(&orca_state),
                &ExecExtras::default(),
                false,
            )
            .unwrap();
            let sim = simulate_unsigned(&client, &payer, &ixs);
            println!("profit-floor sim err (expected): {:?}", sim.err);
            assert!(sim.err.is_some(), "unprofitable trade must revert");
        }

        // 3. Flash loan + Jito tip: borrow the trade size from Kamino, run
        //    both legs, repay, unwrap, tip.
        let reserve_id: Pubkey = env_or("SIM_KAMINO_RESERVE", kamino::DEFAULT_SOL_RESERVE)
            .parse()
            .unwrap();
        let flash = kamino::KaminoReserve::load(&client, &reserve_id).unwrap();
        let lut_key: Pubkey = kamino::MAIN_MARKET_LOOKUP_TABLE.parse().unwrap();
        let lut_data = client.get_account(&lut_key).unwrap().data;
        let luts = vec![AddressLookupTableAccount {
            key: lut_key,
            addresses: solana_sdk::address_lookup_table::state::AddressLookupTable::deserialize(
                &lut_data,
            )
            .unwrap()
            .addresses
            .to_vec(),
        }];
        let extras = ExecExtras {
            flash: Some(flash.clone()),
            jito_tip_lamports: 10_000,
            lookup_tables: luts.clone(),
        };
        let mut ixs = build_opportunity_instructions(
            payer,
            &opp,
            &raydium_ctx,
            &orca_ctx,
            &other_mint,
            &ata_cache,
            Some(&orca_state),
            &extras,
            true,
        )
        .unwrap();
        let repay_pos = ixs
            .iter()
            .rposition(|ix| ix.program_id == kamino::KLEND_PROGRAM_ID)
            .unwrap();
        // The live spread is usually negative; top up the WSOL ATA just
        // before the repay so the plumbing (borrow/repay pairing) is tested
        // independently of market conditions.
        let shortfall = 1_000_000u64;
        let wsol_ata = ata_cache.wsol_ata;
        ixs.insert(
            repay_pos,
            spl_token::instruction::sync_native(&spl_token::id(), &wsol_ata).unwrap(),
        );
        ixs.insert(
            repay_pos,
            system_instruction::transfer(&payer, &wsol_ata, shortfall),
        );
        let sim = simulate_unsigned_v0(&client, &payer, &ixs, &luts);
        for l in sim.logs.iter().flatten() {
            println!("  {l}");
        }
        assert!(
            sim.err.is_none(),
            "flash-loan round trip failed: {:?}",
            sim.err
        );
        println!(
            "flash-loan + Jito tip round trip OK, CU used {:?}",
            sim.units_consumed
        );
        let size = v0_size(&ixs, &payer, &luts);
        println!("serialized tx size: {size} bytes (limit 1232)");
        assert!(size <= 1232);

        // 4. Without the top-up an unprofitable flash-loan arb must revert.
        if opp.net_profit_lamports <= 0 {
            let ixs = build_opportunity_instructions(
                payer,
                &opp,
                &raydium_ctx,
                &orca_ctx,
                &other_mint,
                &ata_cache,
                Some(&orca_state),
                &extras,
                false,
            )
            .unwrap();
            let sim = simulate_unsigned_v0(&client, &payer, &ixs, &luts);
            println!("flash profit-floor sim err (expected): {:?}", sim.err);
            assert!(
                sim.err.is_some(),
                "unprofitable flash-loan trade must revert"
            );
        }
    }
}
