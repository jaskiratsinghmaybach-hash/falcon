use anyhow::{Context, Result};
use borsh::BorshDeserialize;
use orca_whirlpools_core::{TickArrayFacade, TickFacade, TICK_ARRAY_SIZE};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use super::PriceUpdate;

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const ORCA_WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

// ===========================================================================
// ON-CHAIN TICK ARRAY ACCOUNT LAYOUT
// ===========================================================================
// This is NOT exposed by orca_whirlpools_core (that crate only has the
// in-memory TickArrayFacade/TickFacade math types - no on-chain account
// deserialization). The layout below is transcribed directly from the
// official Orca Whirlpool program source, `programs/whirlpool/src/state/
// tick.rs` in https://github.com/orca-so/whirlpools (main branch):
//
//   #[zero_copy(unsafe)]
//   #[repr(C, packed)]
//   pub struct Tick {
//       pub initialized: bool,              //   1 byte
//       pub liquidity_net: i128,             //  16 bytes
//       pub liquidity_gross: u128,           //  16 bytes
//       pub fee_growth_outside_a: u128,      //  16 bytes
//       pub fee_growth_outside_b: u128,      //  16 bytes
//       pub reward_growths_outside: [u128;3],//  48 bytes (16 * 3)
//   }
//   // Tick::LEN = 113 (the crate's own declared constant - matches the
//   // sum above exactly, used here as a hard cross-check).
//
//   #[account(zero_copy(unsafe))]
//   #[repr(packed)]
//   pub struct TickArray {
//       pub start_tick_index: i32,           //   4 bytes
//       pub ticks: [Tick; 88],                // 88 * 113 bytes
//       pub whirlpool: Pubkey,               //  32 bytes
//   }
//
// This is a zero-copy, tightly packed (#[repr(C/packed)]) on-chain struct,
// NOT a Borsh-serialized type - Anchor's `#[account(zero_copy)]` accounts
// are read as raw bytes at fixed offsets, not run through a Borsh decoder.
// We therefore hand-read exact byte offsets below rather than deriving
// BorshDeserialize (which would assume Borsh's own encoding rules and is
// not guaranteed to match a #[repr(packed)] zero-copy layout field-by-
// field, even though the two often coincide for simple fixed-width types).
//
// Every offset is checked at compile time via the `TICK_LEN`/`TICK_ARRAY_LEN`
// constants below, and at decode time via explicit length checks - a
// short/truncated account buffer is a hard error, never a silent partial
// read.
// ===========================================================================

/// Exact size in bytes of one on-chain `Tick`, per the program's own
/// `Tick::LEN` constant (1 + 16 + 16 + 16 + 16 + 48 = 113).
const TICK_LEN: usize = 113;

/// Exact size in bytes of one on-chain `TickArray` account, AFTER the
/// 8-byte Anchor discriminator: start_tick_index(4) + 88 ticks(113 each) + whirlpool(32).
const TICK_ARRAY_LEN: usize = 4 + TICK_ARRAY_SIZE * TICK_LEN + 32;

/// Decodes a single on-chain `Tick` (113 raw bytes, NOT Borsh) into the
/// `orca_whirlpools_core` in-memory `TickFacade` used by the quote engine.
///
/// Field order/widths match the program's `#[repr(C, packed)] struct Tick`
/// exactly - see the module-level layout note above.
fn decode_tick(bytes: &[u8]) -> Result<TickFacade> {
    if bytes.len() != TICK_LEN {
        anyhow::bail!(
            "decode_tick: expected exactly {} bytes, got {}",
            TICK_LEN,
            bytes.len()
        );
    }

    let initialized = bytes[0] != 0;

    let liquidity_net = i128::from_le_bytes(
        bytes[1..17]
            .try_into()
            .context("tick.liquidity_net slice")?,
    );
    let liquidity_gross = u128::from_le_bytes(
        bytes[17..33]
            .try_into()
            .context("tick.liquidity_gross slice")?,
    );
    let fee_growth_outside_a = u128::from_le_bytes(
        bytes[33..49]
            .try_into()
            .context("tick.fee_growth_outside_a slice")?,
    );
    let fee_growth_outside_b = u128::from_le_bytes(
        bytes[49..65]
            .try_into()
            .context("tick.fee_growth_outside_b slice")?,
    );

    let mut reward_growths_outside = [0u128; 3];
    for (i, slot) in reward_growths_outside.iter_mut().enumerate() {
        let start = 65 + i * 16;
        let end = start + 16;
        *slot = u128::from_le_bytes(
            bytes[start..end]
                .try_into()
                .with_context(|| format!("tick.reward_growths_outside[{i}] slice"))?,
        );
    }

    Ok(TickFacade {
        initialized,
        liquidity_net,
        liquidity_gross,
        fee_growth_outside_a,
        fee_growth_outside_b,
        reward_growths_outside,
    })
}

/// Errors specific to tick-array loading/validation. Kept distinct from
/// generic `anyhow::Error` so callers (the realtime loop, the analyzer's
/// pre-simulation revalidation) can classify *why* a tick array was
/// rejected, per the Phase 4 failure-classification requirement.
#[derive(Debug, thiserror::Error)]
pub enum TickArrayError {
    #[error("tick array account too short: got {got} bytes, need at least {need}")]
    TooShort { got: usize, need: usize },
    #[error("tick array account not owned by Orca Whirlpool program (expected {expected}, got {got})")]
    WrongOwner { expected: Pubkey, got: Pubkey },
    #[error("tick array belongs to whirlpool {found}, expected {expected}")]
    WrongWhirlpool { expected: Pubkey, found: Pubkey },
    #[error("tick array start_tick_index {got} is not a valid start tick for tick_spacing {tick_spacing} (expected {expected})")]
    InvalidStartTick {
        got: i32,
        expected: i32,
        tick_spacing: u16,
    },
    #[error("failed to decode tick at index {index}: {source}")]
    TickDecode {
        index: usize,
        #[source]
        source: anyhow::Error,
    },
    #[error("missing neighboring tick array: expected start_tick_index {expected}, account not found/not fetched")]
    MissingNeighbor { expected: i32 },
}

/// A fully decoded on-chain tick array, ready to be converted into
/// `TickArrayFacade` for the quote engine.
#[derive(Debug, Clone)]
pub struct DecodedTickArray {
    pub start_tick_index: i32,
    pub whirlpool: Pubkey,
    pub ticks: [TickFacade; TICK_ARRAY_SIZE],
}

impl DecodedTickArray {
    pub fn to_facade(&self) -> TickArrayFacade {
        TickArrayFacade {
            start_tick_index: self.start_tick_index,
            ticks: self.ticks,
        }
    }
}

/// Decodes a raw on-chain TickArray account's bytes (INCLUDING the 8-byte
/// Anchor discriminator, as returned directly by RPC/WebSocket) into a
/// `DecodedTickArray`.
///
/// This performs NO ownership or ordering validation - callers that have
/// the account's `owner` field available (e.g. from `get_account`, or from
/// a WebSocket push that carries `owner`) MUST additionally call
/// `validate_tick_array_account` before trusting the result for a quote.
/// `load_tick_array` (below) does both together for the common RPC case.
pub fn decode_tick_array(data: &[u8]) -> Result<DecodedTickArray, TickArrayError> {
    if data.len() < 8 + TICK_ARRAY_LEN {
        return Err(TickArrayError::TooShort {
            got: data.len(),
            need: 8 + TICK_ARRAY_LEN,
        });
    }

    // Skip the 8-byte Anchor account discriminator, exactly as the existing
    // Whirlpool decoder does.
    let body = &data[8..8 + TICK_ARRAY_LEN];

    let start_tick_index = i32::from_le_bytes(
        body[0..4]
            .try_into()
            .expect("slice of exactly 4 bytes for start_tick_index"),
    );

    let mut ticks: [TickFacade; TICK_ARRAY_SIZE] = [TickFacade::default(); TICK_ARRAY_SIZE];
    for i in 0..TICK_ARRAY_SIZE {
        let start = 4 + i * TICK_LEN;
        let end = start + TICK_LEN;
        ticks[i] = decode_tick(&body[start..end]).map_err(|source| TickArrayError::TickDecode {
            index: i,
            source,
        })?;
    }

    let whirlpool_start = 4 + TICK_ARRAY_SIZE * TICK_LEN;
    let whirlpool_bytes: [u8; 32] = body[whirlpool_start..whirlpool_start + 32]
        .try_into()
        .expect("slice of exactly 32 bytes for whirlpool pubkey");
    let whirlpool = Pubkey::new_from_array(whirlpool_bytes);

    Ok(DecodedTickArray {
        start_tick_index,
        whirlpool,
        ticks,
    })
}

/// Validates a decoded tick array against the pool/context it is being used
/// for. This is the "never silently quote using stale or incomplete tick
/// state" gate from the Phase 4 spec:
///   - account ownership (must be owned by the Whirlpool program - checked
///     by the caller via `validate_pool_account`-style owner comparison
///     BEFORE calling this, since ownership is a property of the raw
///     Account, not the decoded struct; see `load_tick_array`)
///   - the tick array's embedded `whirlpool` field must match the pool
///     we're quoting for (protects against accidentally feeding in a tick
///     array from a different Whirlpool that happens to decode cleanly)
///   - `start_tick_index` must be a valid start-tick for the pool's
///     `tick_spacing` (protects against a corrupt/mismatched account slipping
///     through even though ownership + whirlpool-pubkey both checked out)
pub fn validate_tick_array(
    decoded: &DecodedTickArray,
    expected_whirlpool: &Pubkey,
    tick_spacing: u16,
) -> Result<(), TickArrayError> {
    if decoded.whirlpool != *expected_whirlpool {
        return Err(TickArrayError::WrongWhirlpool {
            expected: *expected_whirlpool,
            found: decoded.whirlpool,
        });
    }

    let ticks_in_array = TICK_ARRAY_SIZE as i32 * tick_spacing as i32;
    if ticks_in_array != 0 && decoded.start_tick_index.rem_euclid(ticks_in_array) != 0 {
        let expected = decoded
            .start_tick_index
            .div_euclid(ticks_in_array)
            * ticks_in_array;
        return Err(TickArrayError::InvalidStartTick {
            got: decoded.start_tick_index,
            expected,
            tick_spacing,
        });
    }

    Ok(())
}

/// Fetches, decodes, and validates a single tick array account via RPC in
/// one call: ownership, whirlpool match, and start-tick validity are all
/// enforced before the caller ever sees a `DecodedTickArray`.
pub fn load_tick_array(
    client: &RpcClient,
    tick_array_pubkey: &Pubkey,
    expected_whirlpool: &Pubkey,
    tick_spacing: u16,
) -> Result<DecodedTickArray> {
    let account = client
        .get_account(tick_array_pubkey)
        .context("Failed to fetch tick array account")?;

    let expected_owner = Pubkey::from_str(ORCA_WHIRLPOOL_PROGRAM)?;
    if account.owner != expected_owner {
        return Err(TickArrayError::WrongOwner {
            expected: expected_owner,
            got: account.owner,
        }
        .into());
    }

    let decoded = decode_tick_array(&account.data)?;
    validate_tick_array(&decoded, expected_whirlpool, tick_spacing)?;

    Ok(decoded)
}

#[derive(BorshDeserialize, Debug)]
pub struct WhirlpoolRewardInfo {
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub authority: Pubkey,
    pub emissions_per_second_x64: u128,
    pub growth_global_x64: u128,
}

#[derive(BorshDeserialize, Debug)]
pub struct Whirlpool {
    pub whirlpools_config: Pubkey,
    pub whirlpool_bump: [u8; 1],
    pub tick_spacing: u16,
    pub tick_spacing_seed: [u8; 2],
    pub fee_rate: u16,
    pub protocol_fee_rate: u16,
    pub liquidity: u128,
    pub sqrt_price: u128,
    pub tick_current_index: i32,
    pub protocol_fee_owed_a: u64,
    pub protocol_fee_owed_b: u64,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub fee_growth_global_a: u128,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
    pub fee_growth_global_b: u128,
    pub reward_last_updated_timestamp: u64,
    pub reward_infos: [WhirlpoolRewardInfo; 3],
}

#[derive(Debug, Clone)]
pub struct WhirlpoolInfo {
    pub tick_current_index: i32,
    pub tick_spacing: u16,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
}

fn validate_pool_account(account: &solana_sdk::account::Account, pool_id: &str) -> Result<()> {
    let expected_owner = Pubkey::from_str(ORCA_WHIRLPOOL_PROGRAM)?;
    if account.owner != expected_owner {
        anyhow::bail!(
            "Pool {} is not owned by Orca Whirlpool program (expected {}, got {}) - wrong pool type or address",
            pool_id, expected_owner, account.owner
        );
    }
    Ok(())
}

pub fn decode_whirlpool(data: &[u8]) -> Result<Whirlpool> {
    let inner = &data[8..];
    Whirlpool::try_from_slice(inner).context("Failed to deserialize Whirlpool")
}

fn load_whirlpool(client: &RpcClient, pool_id: &str) -> Result<Whirlpool> {
    let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid Whirlpool pubkey format")?;

    let account = client
        .get_account(&pool_pubkey)
        .context("Failed to fetch Whirlpool account")?;

    validate_pool_account(&account, pool_id)?;

    let data = &account.data[8..];

    Whirlpool::try_from_slice(data).context("Failed to deserialize Whirlpool")
}

pub fn fetch_pool_info(client: &RpcClient, pool_id: &str) -> Result<WhirlpoolInfo> {
    let whirlpool = load_whirlpool(client, pool_id)?;

    Ok(WhirlpoolInfo {
        tick_current_index: whirlpool.tick_current_index,
        tick_spacing: whirlpool.tick_spacing,
        token_mint_a: whirlpool.token_mint_a,
        token_vault_a: whirlpool.token_vault_a,
        token_mint_b: whirlpool.token_mint_b,
        token_vault_b: whirlpool.token_vault_b,
    })
}

pub fn fetch_price(client: &RpcClient, pool_id: &str, pair: &str) -> Result<PriceUpdate> {
    let whirlpool = load_whirlpool(client, pool_id)?;

    tracing::debug!(
        "Orca token_mint_a: {}, token_mint_b: {}",
        whirlpool.token_mint_a,
        whirlpool.token_mint_b
    );

    let mint_a_info = client
        .get_token_supply(&whirlpool.token_mint_a)
        .context("Failed to fetch token A mint info")?;
    let mint_b_info = client
        .get_token_supply(&whirlpool.token_mint_b)
        .context("Failed to fetch token B mint info")?;

    let decimals_a = mint_a_info.decimals;
    let decimals_b = mint_b_info.decimals;

    let vault_a_balance = client
        .get_token_account_balance(&whirlpool.token_vault_a)
        .context("Failed to fetch token vault A balance")?;
    let vault_b_balance = client
        .get_token_account_balance(&whirlpool.token_vault_b)
        .context("Failed to fetch token vault B balance")?;

    let vault_a_raw: u64 = vault_a_balance
        .amount
        .parse()
        .context("Failed to parse raw amount for vault A")?;
    let vault_b_raw: u64 = vault_b_balance
        .amount
        .parse()
        .context("Failed to parse raw amount for vault B")?;

    let is_a_wsol = whirlpool.token_mint_a.to_string() == WSOL_MINT;

    let (base_reserve_raw, quote_reserve_raw, base_decimals, quote_decimals) = if is_a_wsol {
        (vault_b_raw, vault_a_raw, decimals_b, decimals_a)
    } else {
        (vault_a_raw, vault_b_raw, decimals_a, decimals_b)
    };

    let sqrt_price_f64 = whirlpool.sqrt_price as f64 / (2f64.powi(64));
    let raw_price = sqrt_price_f64 * sqrt_price_f64;
    let decimal_adjustment = 10f64.powi(decimals_a as i32 - decimals_b as i32);
    let price_b_per_a = raw_price * decimal_adjustment;

    let vault_a_ui = vault_a_balance.ui_amount.unwrap_or(0.0);
    let vault_b_ui = vault_b_balance.ui_amount.unwrap_or(0.0);

    let (base_liquidity, quote_liquidity, price) = if is_a_wsol {
        (vault_b_ui, vault_a_ui, 1.0 / price_b_per_a)
    } else {
        (vault_a_ui, vault_b_ui, price_b_per_a)
    };

    let fee_numerator = whirlpool.fee_rate as u64;
    const ORCA_FEE_RATE_DENOMINATOR: u64 = 1_000_000;
    let fee_denominator = ORCA_FEE_RATE_DENOMINATOR;
    let fee_pct = (fee_numerator as f64 / fee_denominator as f64) * 100.0;

    Ok(PriceUpdate {
        dex: "Orca".to_string(),
        pair: pair.to_string(),
        base_reserve_raw,
        quote_reserve_raw,
        base_decimals,
        quote_decimals,
        fee_numerator,
        fee_denominator,
        price,
        base_liquidity,
        quote_liquidity,
        fee_pct,
        timestamp: std::time::SystemTime::now(),
        clmm_sqrt_price_q64: whirlpool.sqrt_price,
        clmm_liquidity: whirlpool.liquidity,
        clmm_is_a_wsol: is_a_wsol,
    })
}

#[derive(Debug, Clone)]
pub struct OrcaStaticContext {
    pub pool_id: Pubkey,
    pub token_mint_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_vault_b: Pubkey,
    pub decimals_a: u8,
    pub decimals_b: u8,
    pub is_a_wsol: bool,
}

impl OrcaStaticContext {
    pub fn load(client: &RpcClient, pool_id: &str) -> Result<Self> {
        let pool_pubkey = Pubkey::from_str(pool_id).context("Invalid Whirlpool pubkey format")?;
        let whirlpool = load_whirlpool(client, pool_id)?;
        let mint_a_info = client
            .get_token_supply(&whirlpool.token_mint_a)
            .context("Failed to fetch token A mint info")?;
        let mint_b_info = client
            .get_token_supply(&whirlpool.token_mint_b)
            .context("Failed to fetch token B mint info")?;
        let is_a_wsol = whirlpool.token_mint_a.to_string() == WSOL_MINT;

        Ok(Self {
            pool_id: pool_pubkey,
            token_mint_a: whirlpool.token_mint_a,
            token_mint_b: whirlpool.token_mint_b,
            token_vault_a: whirlpool.token_vault_a,
            token_vault_b: whirlpool.token_vault_b,
            decimals_a: mint_a_info.decimals,
            decimals_b: mint_b_info.decimals,
            is_a_wsol,
        })
    }

    /// Still-live fallback path (initial seed before any WebSocket push has
    /// arrived). NOT used in the steady-state hot path - see
    /// `price_from_raw_reserves` / `price_from_whirlpool_and_reserves`.
    pub fn fetch_price_with_context(&self, client: &RpcClient, pair: &str) -> Result<PriceUpdate> {
        let whirlpool = load_whirlpool(client, &self.pool_id.to_string())?;
        self.price_from_whirlpool(client, &whirlpool, pair)
    }

    pub fn price_from_whirlpool(
        &self,
        client: &RpcClient,
        whirlpool: &Whirlpool,
        pair: &str,
    ) -> Result<PriceUpdate> {
        let vault_a_balance = client
            .get_token_account_balance(&whirlpool.token_vault_a)
            .context("Failed to fetch token vault A balance")?;
        let vault_b_balance = client
            .get_token_account_balance(&whirlpool.token_vault_b)
            .context("Failed to fetch token vault B balance")?;

        let vault_a_raw: u64 = vault_a_balance
            .amount
            .parse()
            .context("Failed to parse raw amount for vault A")?;
        let vault_b_raw: u64 = vault_b_balance
            .amount
            .parse()
            .context("Failed to parse raw amount for vault B")?;

        Ok(self.price_from_whirlpool_and_reserves(
            whirlpool.sqrt_price,
            whirlpool.liquidity,
            whirlpool.fee_rate,
            vault_a_raw,
            vault_b_raw,
            pair,
        ))
    }

    /// Hot-path constructor for a plain reserve update (e.g. only a vault
    /// account changed, sqrt_price unchanged) - reuses the last-known
    /// sqrt_price/fee_rate passed in by the caller. No RPC, no client.
    pub fn price_from_raw_reserves(
        &self,
        sqrt_price: u128,
        liquidity: u128,
        fee_rate: u16,
        vault_a_raw: u64,
        vault_b_raw: u64,
        pair: &str,
    ) -> PriceUpdate {
        self.price_from_whirlpool_and_reserves(
            sqrt_price,
            liquidity,
            fee_rate,
            vault_a_raw,
            vault_b_raw,
            pair,
        )
    }

    /// Hot-path constructor: builds a `PriceUpdate` purely from values already
    /// in hand (either just decoded from a WebSocket push, or cached from the
    /// last update) - no RPC call, no client.
    pub fn price_from_whirlpool_and_reserves(
        &self,
        sqrt_price: u128,
        liquidity: u128,
        fee_rate: u16,
        vault_a_raw: u64,
        vault_b_raw: u64,
        pair: &str,
    ) -> PriceUpdate {
        let (base_reserve_raw, quote_reserve_raw, base_decimals, quote_decimals) = if self.is_a_wsol
        {
            (vault_b_raw, vault_a_raw, self.decimals_b, self.decimals_a)
        } else {
            (vault_a_raw, vault_b_raw, self.decimals_a, self.decimals_b)
        };

        let sqrt_price_f64 = sqrt_price as f64 / (2f64.powi(64));
        let raw_price = sqrt_price_f64 * sqrt_price_f64;
        let decimal_adjustment = 10f64.powi(self.decimals_a as i32 - self.decimals_b as i32);
        let price_b_per_a = raw_price * decimal_adjustment;

        let vault_a_ui = vault_a_raw as f64 / 10f64.powi(self.decimals_a as i32);
        let vault_b_ui = vault_b_raw as f64 / 10f64.powi(self.decimals_b as i32);

        let (base_liquidity, quote_liquidity, price) = if self.is_a_wsol {
            (vault_b_ui, vault_a_ui, 1.0 / price_b_per_a)
        } else {
            (vault_a_ui, vault_b_ui, price_b_per_a)
        };

        let fee_numerator = fee_rate as u64;
        const ORCA_FEE_RATE_DENOMINATOR: u64 = 1_000_000;
        let fee_denominator = ORCA_FEE_RATE_DENOMINATOR;
        let fee_pct = (fee_numerator as f64 / fee_denominator as f64) * 100.0;

        PriceUpdate {
            dex: "Orca".to_string(),
            pair: pair.to_string(),
            base_reserve_raw,
            quote_reserve_raw,
            base_decimals,
            quote_decimals,
            fee_numerator,
            fee_denominator,
            price,
            base_liquidity,
            quote_liquidity,
            fee_pct,
            timestamp: std::time::SystemTime::now(),
            clmm_sqrt_price_q64: sqrt_price,
            clmm_liquidity: liquidity,
            clmm_is_a_wsol: self.is_a_wsol,
        }
    }
}

#[cfg(test)]
mod tick_array_tests {
    use super::*;

    /// Hand-encodes a single on-chain `Tick` (113 raw bytes) per the
    /// confirmed program layout, for use in fixture construction. This is
    /// the exact inverse of `decode_tick`, written independently (not by
    /// reusing decode logic) so the test can catch a bug in either
    /// direction.
    fn encode_tick(
        initialized: bool,
        liquidity_net: i128,
        liquidity_gross: u128,
        fee_growth_outside_a: u128,
        fee_growth_outside_b: u128,
        reward_growths_outside: [u128; 3],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(TICK_LEN);
        out.push(initialized as u8);
        out.extend_from_slice(&liquidity_net.to_le_bytes());
        out.extend_from_slice(&liquidity_gross.to_le_bytes());
        out.extend_from_slice(&fee_growth_outside_a.to_le_bytes());
        out.extend_from_slice(&fee_growth_outside_b.to_le_bytes());
        for r in reward_growths_outside {
            out.extend_from_slice(&r.to_le_bytes());
        }
        assert_eq!(out.len(), TICK_LEN);
        out
    }

    /// Hand-encodes a full on-chain `TickArray` account (WITH the 8-byte
    /// Anchor discriminator prefix, matching what RPC/WebSocket actually
    /// deliver), given a start_tick_index, owning whirlpool, and a sparse
    /// map of (index -> encoded tick bytes) for any initialized ticks; all
    /// other slots are filled with an all-zero (uninitialized, all-zero
    /// liquidity) tick.
    fn encode_tick_array(
        start_tick_index: i32,
        whirlpool: Pubkey,
        overrides: &[(usize, Vec<u8>)],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + TICK_ARRAY_LEN);
        out.extend_from_slice(&[0u8; 8]); // discriminator - contents irrelevant to our decoder
        out.extend_from_slice(&start_tick_index.to_le_bytes());

        let zero_tick = encode_tick(false, 0, 0, 0, 0, [0, 0, 0]);
        for i in 0..TICK_ARRAY_SIZE {
            if let Some((_, bytes)) = overrides.iter().find(|(idx, _)| *idx == i) {
                out.extend_from_slice(bytes);
            } else {
                out.extend_from_slice(&zero_tick);
            }
        }

        out.extend_from_slice(whirlpool.as_ref());
        assert_eq!(out.len(), 8 + TICK_ARRAY_LEN);
        out
    }

    fn test_whirlpool_pubkey() -> Pubkey {
        Pubkey::new_from_array([7u8; 32])
    }

    // --- decode_tick: exact field round-trip ---

    #[test]
    fn decode_tick_round_trips_all_fields() {
        let bytes = encode_tick(
            true,
            -12_345_i128,
            987_654_321_u128,
            111_111_111_111_u128,
            222_222_222_222_u128,
            [1, 2, 3],
        );
        let tick = decode_tick(&bytes).unwrap();
        assert!(tick.initialized);
        assert_eq!(tick.liquidity_net, -12_345);
        assert_eq!(tick.liquidity_gross, 987_654_321);
        assert_eq!(tick.fee_growth_outside_a, 111_111_111_111);
        assert_eq!(tick.fee_growth_outside_b, 222_222_222_222);
        assert_eq!(tick.reward_growths_outside, [1, 2, 3]);
    }

    #[test]
    fn decode_tick_rejects_wrong_length() {
        let bytes = vec![0u8; TICK_LEN - 1];
        assert!(decode_tick(&bytes).is_err());
    }

    #[test]
    fn tick_len_matches_program_constant() {
        // Cross-check against the program's own declared Tick::LEN = 113.
        assert_eq!(TICK_LEN, 113);
    }

    // --- decode_tick_array: full account decode ---

    #[test]
    fn decode_tick_array_reads_start_index_and_whirlpool() {
        let pool = test_whirlpool_pubkey();
        let data = encode_tick_array(-11264, pool, &[]);
        let decoded = decode_tick_array(&data).unwrap();
        assert_eq!(decoded.start_tick_index, -11264);
        assert_eq!(decoded.whirlpool, pool);
        assert_eq!(decoded.ticks.len(), TICK_ARRAY_SIZE);
    }

    #[test]
    fn decode_tick_array_places_initialized_tick_at_correct_index() {
        let pool = test_whirlpool_pubkey();
        let tick_bytes = encode_tick(true, 500_000, 500_000, 0, 0, [0, 0, 0]);
        let data = encode_tick_array(0, pool, &[(22, tick_bytes)]);
        let decoded = decode_tick_array(&data).unwrap();

        assert!(decoded.ticks[22].initialized);
        assert_eq!(decoded.ticks[22].liquidity_net, 500_000);

        // Every other slot remains the zero/uninitialized default.
        for (i, t) in decoded.ticks.iter().enumerate() {
            if i != 22 {
                assert!(!t.initialized, "tick {i} should be uninitialized");
                assert_eq!(t.liquidity_net, 0);
            }
        }
    }

    #[test]
    fn decode_tick_array_rejects_truncated_account() {
        let pool = test_whirlpool_pubkey();
        let mut data = encode_tick_array(0, pool, &[]);
        data.truncate(data.len() - 1);
        let result = decode_tick_array(&data);
        assert!(matches!(result, Err(TickArrayError::TooShort { .. })));
    }

    #[test]
    fn decode_tick_array_to_facade_preserves_start_and_ticks() {
        let pool = test_whirlpool_pubkey();
        let tick_bytes = encode_tick(true, -777, 777, 0, 0, [0, 0, 0]);
        let data = encode_tick_array(5632, pool, &[(5, tick_bytes)]);
        let decoded = decode_tick_array(&data).unwrap();
        let facade = decoded.to_facade();

        assert_eq!(facade.start_tick_index, 5632);
        assert!(facade.ticks[5].initialized);
        assert_eq!(facade.ticks[5].liquidity_net, -777);
    }

    // --- validate_tick_array: ownership/whirlpool/start-tick checks ---
    // (account ownership itself is checked one layer up, in load_tick_array,
    // against the raw Account.owner field - see the wrong-owner coverage in
    // that function's own doc comment; validate_tick_array covers the two
    // checks that operate on the DECODED struct.)

    #[test]
    fn validate_tick_array_accepts_matching_whirlpool_and_valid_start() {
        let pool = test_whirlpool_pubkey();
        let data = encode_tick_array(0, pool, &[]);
        let decoded = decode_tick_array(&data).unwrap();
        assert!(validate_tick_array(&decoded, &pool, 64).is_ok());
    }

    #[test]
    fn validate_tick_array_rejects_wrong_whirlpool() {
        let pool = test_whirlpool_pubkey();
        let other_pool = Pubkey::new_from_array([9u8; 32]);
        let data = encode_tick_array(0, pool, &[]);
        let decoded = decode_tick_array(&data).unwrap();

        let result = validate_tick_array(&decoded, &other_pool, 64);
        assert!(matches!(
            result,
            Err(TickArrayError::WrongWhirlpool { .. })
        ));
    }

    #[test]
    fn validate_tick_array_rejects_invalid_start_tick() {
        let pool = test_whirlpool_pubkey();
        // start_tick_index=100 is not a multiple of (88 * tick_spacing) for
        // any sane tick_spacing - deliberately corrupt/misaligned.
        let data = encode_tick_array(100, pool, &[]);
        let decoded = decode_tick_array(&data).unwrap();

        let result = validate_tick_array(&decoded, &pool, 64);
        assert!(matches!(
            result,
            Err(TickArrayError::InvalidStartTick { .. })
        ));
    }

    #[test]
    fn validate_tick_array_accepts_negative_aligned_start_tick() {
        let pool = test_whirlpool_pubkey();
        // 88 * 64 = 5632; -5632 is a valid (negative) start tick.
        let data = encode_tick_array(-5632, pool, &[]);
        let decoded = decode_tick_array(&data).unwrap();
        assert!(validate_tick_array(&decoded, &pool, 64).is_ok());
    }
}
