use anyhow::{Context, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::str::FromStr;

use super::TOKEN_PROGRAM_ID;

pub const ORCA_WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

pub const TICK_ARRAY_SIZE: i32 = 88;

const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

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

/// Rounds a tick index down to the start index of its containing tick
/// array. Public: the realtime scanner needs this same rounding to know
/// which tick-array accounts to subscribe to, and must use the identical
/// rule the executor uses or the two can disagree about which array a
/// given tick falls in.
pub fn tick_array_start_index(tick_index: i32, tick_spacing: u16) -> i32 {
    let ticks_in_array = TICK_ARRAY_SIZE * tick_spacing as i32;
    tick_index.div_euclid(ticks_in_array) * ticks_in_array
}

/// Start index of `tick_array_0` for a swap in direction `a_to_b`. For
/// b->a swaps the Whirlpool program searches a range shifted down by one
/// `tick_spacing`, so a current tick in the last spacing of an array needs
/// the NEXT array as `tick_array_0` (same rule as Orca's SDK
/// `getTickArrayPublicKeys`: shift = a_to_b ? 0 : tick_spacing). Without it
/// those swaps fail on-chain with an invalid tick array sequence.
pub fn first_tick_array_start_index(
    tick_current_index: i32,
    tick_spacing: u16,
    a_to_b: bool,
) -> i32 {
    let shift = if a_to_b { 0 } else { tick_spacing as i32 };
    tick_array_start_index(tick_current_index + shift, tick_spacing)
}

/// Derives a tick array PDA for a given start tick index. Public for the
/// same reason as `tick_array_start_index` - the scanner must derive the
/// exact same PDAs the executor will submit on-chain, or a quote/execution
/// mismatch (the kind Phase 4 exists to eliminate) becomes possible again.
pub fn derive_tick_array_pda(whirlpool: &Pubkey, start_tick_index: i32) -> Result<Pubkey> {
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

/// Derives the exact 3 tick array PDAs the legacy Orca `swap` instruction
/// will submit for a trade in direction `a_to_b`, starting from the array
/// containing `tick_current_index`. This is a hard on-chain constraint of
/// that instruction (see `SwapAccounts` / `build_swap_instruction` below) -
/// not a Falcon design choice - so the quote engine must use these exact 3
/// arrays (for the trade's actual direction) or it can price liquidity the
/// submitted transaction can never reach. Public so the scanner's tick-array
/// subscription logic and the analyzer's quote-time array selection both
/// derive from this single source of truth instead of duplicating the rule.
pub fn derive_tick_arrays(
    whirlpool: &Pubkey,
    tick_current_index: i32,
    tick_spacing: u16,
    a_to_b: bool,
) -> Result<[Pubkey; 3]> {
    let ticks_in_array = TICK_ARRAY_SIZE * tick_spacing as i32;
    let start = first_tick_array_start_index(tick_current_index, tick_spacing, a_to_b);
    let offsets: [i32; 3] = if a_to_b { [0, -1, -2] } else { [0, 1, 2] };

    Ok([
        derive_tick_array_pda(whirlpool, start + offsets[0] * ticks_in_array)?,
        derive_tick_array_pda(whirlpool, start + offsets[1] * ticks_in_array)?,
        derive_tick_array_pda(whirlpool, start + offsets[2] * ticks_in_array)?,
    ])
}

/// Orca Whirlpool `swap` instruction. `a_to_b` = spending token A to receive token B.
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

    let sqrt_price_limit: u128 = if a_to_b {
        MIN_SQRT_PRICE
    } else {
        MAX_SQRT_PRICE
    };

    let mut data = SWAP_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    data.push(1u8);
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

    Ok(Instruction {
        program_id: program,
        accounts: account_metas,
        data,
    })
}

#[cfg(test)]
mod tick_array_shift_tests {
    use super::*;

    #[test]
    fn a_to_b_uses_array_containing_current_tick() {
        // spacing 64 -> 5632 ticks per array.
        assert_eq!(first_tick_array_start_index(5631, 64, true), 0);
        assert_eq!(first_tick_array_start_index(-1, 64, true), -5632);
    }

    #[test]
    fn b_to_a_shifts_by_one_spacing() {
        assert_eq!(first_tick_array_start_index(5000, 64, false), 0);
        // Last spacing of array 0: b->a must start at the next array.
        assert_eq!(first_tick_array_start_index(5568, 64, false), 5632);
        assert_eq!(first_tick_array_start_index(5631, 64, false), 5632);
        assert_eq!(first_tick_array_start_index(-65, 64, false), -5632);
        assert_eq!(first_tick_array_start_index(-64, 64, false), 0);
    }
}
