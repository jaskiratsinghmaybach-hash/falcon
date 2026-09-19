use anyhow::{Context, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::str::FromStr;

use super::TOKEN_PROGRAM_ID;

pub const ORCA_WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

/// Ticks per tick array in a Whirlpool.
const TICK_ARRAY_SIZE: i32 = 88;

/// Anchor discriminator: first 8 bytes of sha256("global:swap").
const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

/// Price bounds - used as "no limit" values depending on swap direction.
const MIN_SQRT_PRICE: u128 = 4_295_048_016;
const MAX_SQRT_PRICE: u128 = 79_226_673_515_401_279_992_447_579_055;

pub struct SwapAccounts {
    pub whirlpool: Pubkey,
    pub token_owner_account_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_owner_account_b: Pubkey,
    pub token_vault_b: Pubkey,
    pub token_authority: Pubkey,
}

fn program_id() -> Result<Pubkey> {
    Pubkey::from_str(ORCA_WHIRLPOOL_PROGRAM).context("Invalid Whirlpool program ID")
}

/// Round a tick index down to the start index of the tick array containing it.
fn tick_array_start_index(tick_index: i32, tick_spacing: u16) -> i32 {
    let ticks_in_array = TICK_ARRAY_SIZE * tick_spacing as i32;
    tick_index.div_euclid(ticks_in_array) * ticks_in_array
}

fn derive_tick_array_pda(whirlpool: &Pubkey, start_tick_index: i32) -> Result<Pubkey> {
    let start_str = start_tick_index.to_string();
    let (pda, _) = Pubkey::find_program_address(
        &[b"tick_array", whirlpool.as_ref(), start_str.as_bytes()],
        &program_id()?,
    );
    Ok(pda)
}

fn derive_oracle_pda(whirlpool: &Pubkey) -> Result<Pubkey> {
    let (pda, _) = Pubkey::find_program_address(&[b"oracle", whirlpool.as_ref()], &program_id()?);
    Ok(pda)
}

/// Three tick arrays, walking in the direction the price will move.
/// a_to_b = price moves down, so we walk to lower start indices.
fn derive_tick_arrays(
    whirlpool: &Pubkey,
    tick_current_index: i32,
    tick_spacing: u16,
    a_to_b: bool,
) -> Result<[Pubkey; 3]> {
    let ticks_in_array = TICK_ARRAY_SIZE * tick_spacing as i32;
    let start = tick_array_start_index(tick_current_index, tick_spacing);
    let offsets: [i32; 3] = if a_to_b { [0, -1, -2] } else { [0, 1, 2] };

    Ok([
        derive_tick_array_pda(whirlpool, start + offsets[0] * ticks_in_array)?,
        derive_tick_array_pda(whirlpool, start + offsets[1] * ticks_in_array)?,
        derive_tick_array_pda(whirlpool, start + offsets[2] * ticks_in_array)?,
    ])
}

/// Orca Whirlpool `swap` instruction.
/// `a_to_b` = true means spending token A to receive token B.
pub fn build_swap_instruction(
    accounts: &SwapAccounts,
    tick_current_index: i32,
    tick_spacing: u16,
    amount_in: u64,
    minimum_amount_out: u64,
    a_to_b: bool,
) -> Result<Instruction> {
    let program = program_id()?;
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID).context("Invalid Token program ID")?;

    let tick_arrays = derive_tick_arrays(
        &accounts.whirlpool,
        tick_current_index,
        tick_spacing,
        a_to_b,
    )?;
    let oracle = derive_oracle_pda(&accounts.whirlpool)?;

    // No price limit: clamp to the extreme in whichever direction we're moving.
    let sqrt_price_limit: u128 = if a_to_b {
        MIN_SQRT_PRICE
    } else {
        MAX_SQRT_PRICE
    };

    let mut data = SWAP_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    data.push(1u8); // amount_specified_is_input = true
    data.push(a_to_b as u8);

    let account_metas = vec![
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(accounts.token_authority, true),
        AccountMeta::new(accounts.whirlpool, false),
        AccountMeta::new(accounts.token_owner_account_a, false),
        AccountMeta::new(accounts.token_vault_a, false),
        AccountMeta::new(accounts.token_owner_account_b, false),
        AccountMeta::new(accounts.token_vault_b, false),
        AccountMeta::new(tick_arrays[0], false),
        AccountMeta::new(tick_arrays[1], false),
        AccountMeta::new(tick_arrays[2], false),
        AccountMeta::new(oracle, false),
    ];

    tracing::debug!(
        "Orca swap: a_to_b={}, tick_current={}, tick_arrays={:?}, oracle={}",
        a_to_b,
        tick_current_index,
        tick_arrays,
        oracle
    );

    Ok(Instruction {
        program_id: program,
        accounts: account_metas,
        data,
    })
}
