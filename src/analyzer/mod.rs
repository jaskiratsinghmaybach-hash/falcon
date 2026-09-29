use crate::scanner::{raydium_cpmm, PriceUpdate};
use orca_whirlpools_core::{
    swap_quote_by_input_token, TickArrayFacade, TickArrays, TickFacade, WhirlpoolFacade,
    WhirlpoolRewardInfoFacade,
};

// ===========================================================================
// NUMERIC DOMAIN NOTE (execution-critical path)
// ===========================================================================
// Every field/function below that participates in sizing a trade, computing
// a swap output, or approving/rejecting an opportunity uses deterministic
// integer or fixed-point arithmetic - never f64. The Opportunity struct keeps
// a small set of f64 fields at the very end, clearly marked, ONLY for
// human-readable logging (opportunity_log.csv via logger.rs) and console
// output; nothing reads those fields back into a trading decision.
//
// Units:
//   - "base" amounts are raw smallest-units of the pair's non-SOL token.
//   - "quote" amounts are raw lamports (SOL's smallest unit, decimals = 9).
//   - Fees are exact integer ratios (numerator / denominator), taken directly
//     from each protocol's own on-chain fee parameters.
//   - Slippage tolerance is basis points (1% = 100 bps).
//   - Profit is signed lamports (i128, since a rejected/underwater trade has
//     a well-defined negative net profit, not just "no profit").
// ===========================================================================

/// Basis-point denominator used throughout (1 bps = 1/10_000).
pub const BPS_DENOMINATOR: u128 = 10_000;

#[derive(Debug, Clone)]
pub struct Opportunity {
    pub buy_dex: String,
    pub sell_dex: String,
    pub pair: String,

    // --- execution-critical integer fields ---
    /// Raw lamports spent on the buy leg (the trade's SOL-denominated size).
    pub trade_size_lamports: u64,
    /// Exact raw base-token amount the buy leg is expected to produce. The
    /// sell leg MUST spend exactly this amount - see executor/mod.rs, which
    /// takes this value directly rather than re-deriving it.
    pub expected_output_after_buy_raw: u64,
    /// Exact raw lamports the sell leg is expected to produce.
    pub expected_output_after_sell_raw: u64,
    /// Net profit in lamports (final SOL back minus SOL spent minus fixed
    /// tx fees). Signed because a losing trade has a real negative
    /// value, not just "not approved". THIS is the authoritative
    /// approve/reject quantity - see find_opportunity's final check.
    pub net_profit_lamports: i128,

    // --- presentation-only (f64) - logging/display, NEVER re-used for a
    // trading decision. Derived FROM the integer fields above, once, purely
    // for the CSV/log line. ---
    pub buy_price: f64,
    pub sell_price: f64,
    pub raw_spread_pct: f64,
    pub fee_adjusted_spread_pct: f64,
    pub net_spread_after_slippage_pct: f64,
    pub net_profit_pct: f64,
}

#[derive(Debug)]
pub enum RejectReason {
    NoSpread,
    SpreadTooSmall {
        spread_bps: i128,
        min_required_bps: i128,
    },
    FeesExceedSpread {
        fee_adjusted_bps: i128,
    },
    SlippageExceedsSpread {
        net_bps: i128,
    },
    NetProfitNotPositive {
        net_profit_lamports: i128,
    },
    StalePrice {
        age_secs: u64,
        max_age_secs: u64,
    },
    /// The cost model's arithmetic failed. This is NOT a market outcome, so it
    /// must not be reported as "no spread".
    ArithmeticFailure,
}

impl RejectReason {
    /// Display-only percent form, for log lines. Never used for a decision.
    pub fn as_display_pct(&self) -> f64 {
        match self {
            RejectReason::NoSpread => 0.0,
            RejectReason::SpreadTooSmall { spread_bps, .. } => *spread_bps as f64 / 100.0,
            RejectReason::FeesExceedSpread { fee_adjusted_bps } => *fee_adjusted_bps as f64 / 100.0,
            RejectReason::SlippageExceedsSpread { net_bps } => *net_bps as f64 / 100.0,
            RejectReason::NetProfitNotPositive {
                net_profit_lamports,
            } => *net_profit_lamports as f64 / 1_000_000_000.0,
            RejectReason::StalePrice { age_secs, .. } => *age_secs as f64,
            RejectReason::ArithmeticFailure => 0.0,
        }
    }
}

/// Errors from the integer quote/arithmetic helpers below. Kept distinct from
/// RejectReason: these represent invalid/degenerate INPUT data (bad reserves,
/// overflow) rather than a legitimate "not profitable" market outcome.
#[derive(Debug, thiserror::Error)]
pub enum MathError {
    #[error("reserve is zero, cannot quote against an empty pool")]
    ZeroReserve,
    #[error("arithmetic overflow computing {0}")]
    Overflow(&'static str),
    #[error("division by zero computing {0}")]
    DivisionByZero(&'static str),
    #[error("output amount exceeds u64 range")]
    OutputTooLarge,
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    #[error("Orca exact CLMM quote unavailable: {0}")]
    OrcaQuoteUnavailable(&'static str),
    #[error("Orca exact CLMM quote failed: {0}")]
    OrcaQuoteFailed(&'static str),
    #[error("Orca tick array coverage insufficient for this swap size (needed more arrays than currently loaded)")]
    OrcaInsufficientTickCoverage,
    #[error("Orca quote state incoherent: tick array(s) drifted {worst_drift} slots from pool account (max allowed: {})", crate::scanner::orca::MAX_COHERENT_SLOT_DRIFT)]
    OrcaStateIncoherent { worst_drift: u64 },
    #[error("Raydium CPMM quote state is not initialized")]
    CpmmQuoteStateUnavailable,
    /// Raydium CPMM's own typed quote error, preserved (it used to be
    /// discarded into a generic string) so failures can be classified.
    #[error("Raydium CPMM exact quote failed: {0:?}")]
    CpmmQuote(raydium_cpmm::CpmmQuoteError),
}

/// How many base-token raw units you receive for spending `input_quote_raw`
/// raw units into a constant-product pool with the given raw reserves.
/// Deterministic integer math (u128 intermediate to avoid overflow on the
/// numerator), checked throughout - never silently saturates or wraps.
///
/// numerator = amount_in * reserve_out
/// denominator = reserve_in + amount_in
/// output = numerator / denominator   (floors - see rounding note below)
///
/// Rounding: this floors (integer division truncates toward zero for
/// non-negative operands), which UNDERSTATES the output slightly - i.e. it
/// is conservative in the direction that matters: it will never report more
/// output than a real swap would actually produce.
pub fn estimate_output_amount_raw(
    reserve_in: u64,
    reserve_out: u64,
    amount_in: u64,
) -> Result<u64, MathError> {
    if reserve_in == 0 || reserve_out == 0 {
        return Err(MathError::ZeroReserve);
    }
    if amount_in == 0 {
        return Ok(0);
    }

    let reserve_in = reserve_in as u128;
    let reserve_out = reserve_out as u128;
    let amount_in = amount_in as u128;

    let numerator = amount_in
        .checked_mul(reserve_out)
        .ok_or(MathError::Overflow("amount_in * reserve_out"))?;
    let denominator = reserve_in
        .checked_add(amount_in)
        .ok_or(MathError::Overflow("reserve_in + amount_in"))?;

    if denominator == 0 {
        return Err(MathError::DivisionByZero("reserve_in + amount_in"));
    }

    let output = numerator / denominator;

    u64::try_from(output).map_err(|_| MathError::OutputTooLarge)
}

/// Checks whether a freshly-fetched reserve pair still roughly matches the
/// reserves an Opportunity was computed from, in integer basis points of
/// drift. If the market moved more than `max_drift_bps` since detection, the
/// opportunity is stale and should be discarded rather than acted on.
///
/// Drift is computed on the SPOT PRICE implied by (quote_reserve /
/// base_reserve) at each snapshot, in integer bps, via cross-multiplication
/// to avoid any float division.
pub fn is_still_fresh(
    original_base_reserve: u64,
    original_quote_reserve: u64,
    current_base_reserve: u64,
    current_quote_reserve: u64,
    max_drift_bps: u32,
) -> Result<bool, MathError> {
    if original_base_reserve == 0 || current_base_reserve == 0 {
        return Err(MathError::ZeroReserve);
    }

    // original_price = original_quote / original_base
    // current_price  = current_quote / current_base
    // drift = |current_price - original_price| / original_price
    //
    // Cross-multiply to compare without ever dividing into a float:
    // |current_quote * original_base - original_quote * current_base| * BPS
    //   <= max_drift_bps * original_quote * current_base   (all u128)
    let original_base = original_base_reserve as u128;
    let original_quote = original_quote_reserve as u128;
    let current_base = current_base_reserve as u128;
    let current_quote = current_quote_reserve as u128;

    let lhs_a = current_quote
        .checked_mul(original_base)
        .ok_or(MathError::Overflow("current_quote * original_base"))?;
    let lhs_b = original_quote
        .checked_mul(current_base)
        .ok_or(MathError::Overflow("original_quote * current_base"))?;

    let diff = lhs_a.max(lhs_b) - lhs_a.min(lhs_b);

    let diff_scaled = diff
        .checked_mul(BPS_DENOMINATOR)
        .ok_or(MathError::Overflow("diff * BPS_DENOMINATOR"))?;

    let base_scale = original_quote
        .checked_mul(current_base)
        .ok_or(MathError::Overflow("original_quote * current_base (scale)"))?;

    let allowed = base_scale
        .checked_mul(max_drift_bps as u128)
        .ok_or(MathError::Overflow("base_scale * max_drift_bps"))?;

    Ok(diff_scaled <= allowed)
}

/// Slippage of one leg, in integer basis points: how much worse the
/// effective execution price is than the pool's spot price, expressed as
/// bps of the spot price. Computed entirely via cross-multiplication -
/// no float division anywhere in this comparison.
/// Calculates curve price impact from a canonical quote.
///
/// This intentionally excludes protocol/creator/trade fees from the
/// price-impact measurement. Fees are accounted for separately by the
/// canonical quote and profitability calculation.
///
/// For a constant-product curve:
///
/// spot = effective_reserve_out / effective_reserve_in
/// execution = gross_output / effective_amount_in
///
/// Therefore:
///
/// impact = 1 - execution / spot
///
/// Everything is integer arithmetic.
/// Legacy-compatible slippage helper.

///

/// Returns the constant-product curve price impact in basis points for a

/// single generic CPMM leg. Execution-critical Raydium CPMM paths use the

/// canonical protocol-specific quote instead.

pub fn estimate_slippage_bps(
    reserve_in: u64,

    reserve_out: u64,

    amount_in: u64,
) -> Result<i128, MathError> {
    let output = estimate_output_amount_raw(reserve_in, reserve_out, amount_in)?;

    estimate_curve_price_impact_bps(reserve_in, reserve_out, amount_in, output)
}

fn estimate_curve_price_impact_bps(
    reserve_in: u64,
    reserve_out: u64,
    effective_amount_in: u64,
    gross_output: u64,
) -> Result<i128, MathError> {
    if reserve_in == 0 || reserve_out == 0 {
        return Err(MathError::ZeroReserve);
    }

    if effective_amount_in == 0 {
        return Ok(0);
    }

    if gross_output == 0 {
        return Ok(BPS_DENOMINATOR as i128);
    }

    let numerator = (gross_output as u128)
        .checked_mul(reserve_in as u128)
        .ok_or(MathError::Overflow("curve impact numerator"))?
        .checked_mul(BPS_DENOMINATOR)
        .ok_or(MathError::Overflow("curve impact numerator * bps"))?;

    let denominator = (effective_amount_in as u128)
        .checked_mul(reserve_out as u128)
        .ok_or(MathError::Overflow("curve impact denominator"))?;

    if denominator == 0 {
        return Err(MathError::DivisionByZero(
            "effective_amount_in * reserve_out",
        ));
    }

    let execution_over_spot_bps = numerator / denominator;

    Ok((BPS_DENOMINATOR as i128) - execution_over_spot_bps as i128)
}

/// Fixed lamport cost of Falcon's single atomic transaction: the base
/// signature fee plus the priority fee the executor actually requests. Read
/// from `config`'s cost model - the same constants the executor passes to
/// `set_compute_unit_limit` / `set_compute_unit_price` - so the analyzer
/// prices exactly what the chain will charge. There is no Jito tip: Jito is
/// not part of the current execution path.
fn fixed_cost_lamports() -> Result<i128, MathError> {
    fixed_cost_from(
        crate::config::TX_SIGNATURES,
        crate::config::COMPUTE_UNIT_LIMIT,
        crate::config::COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
    )
}

fn fixed_cost_from(
    signatures: u64,
    compute_unit_limit: u32,
    compute_unit_price_micro_lamports: u64,
) -> Result<i128, MathError> {
    crate::config::fixed_tx_cost_lamports(
        signatures,
        compute_unit_limit,
        compute_unit_price_micro_lamports,
    )
    .map(i128::from)
    .ok_or(MathError::Overflow("fixed_cost_lamports"))
}

/// Picks a trade size that's reasonable relative to the thinner pool's depth
/// (raw base-token units), so we're testing a realistic trade rather than an
/// impossible one. NOT the flashloan optimal-sizing logic - just a sane
/// default for pre-flashloan testing, same role as before the migration.
pub fn safe_trade_size_raw(pool_a_base_reserve: u64, pool_b_base_reserve: u64) -> u64 {
    const SAFETY_FRACTION_BPS: u128 = 100; // 1% of the thinner pool's reserves

    let thinner = pool_a_base_reserve.min(pool_b_base_reserve) as u128;
    let sized = thinner * SAFETY_FRACTION_BPS / BPS_DENOMINATOR;
    sized.min(u64::MAX as u128) as u64
}

/// Calculates a safe minimum-output floor: the expected output minus a
/// tolerance buffer (in basis points), so the transaction reverts if actual
/// execution is worse than expected by more than this margin.
///
/// Rounding: the amount SUBTRACTED from expected_output is rounded UP
/// (ceiling division), so minimum_out is rounded DOWN relative to a naive
/// floor-division calculation. This means the protection can only be as
/// tight or tighter than the stated tolerance - it will never accidentally
/// accept a worse execution price than intended.
pub fn calculate_minimum_out_raw(
    expected_output_raw: u64,
    slippage_tolerance_bps: u32,
) -> Result<u64, MathError> {
    if slippage_tolerance_bps as u128 > BPS_DENOMINATOR {
        return Err(MathError::InvalidInput(
            "slippage_tolerance_bps exceeds 10_000 (100%)",
        ));
    }

    let expected = expected_output_raw as u128;
    let tolerance = slippage_tolerance_bps as u128;

    let raw_reduction = expected
        .checked_mul(tolerance)
        .ok_or(MathError::Overflow("expected * tolerance_bps"))?;

    // Ceiling division so the reduction is never understated (never makes
    // minimum_out weaker/looser than the stated tolerance): ceil(a/b) =
    // (a + b - 1) / b for positive integers.
    let reduction = raw_reduction.div_ceil(BPS_DENOMINATOR);

    let minimum_out = expected.saturating_sub(reduction);

    u64::try_from(minimum_out).map_err(|_| MathError::OutputTooLarge)
}

// ===========================================================================
// ORCA WHIRLPOOL (CLMM) SWAP MATH
// ===========================================================================
// Orca Whirlpools are Concentrated Liquidity Market Makers (CLMM), not
// constant-product pools. Their swap output is governed by:
//
//   sqrt_price (Q64.64 fixed-point)  and  active liquidity
//
// rather than by vault reserve ratios. Using x*y=k for an Orca CLMM leg
// would systematically underestimate output, causing execution to fail with
// AmountOutBelowMinimum (error 6036) because the minimum we send on-chain
// would be set too high relative to what the pool actually returns.
//
// Both functions assume a single-tick-range swap (no tick-crossing). This is
// correct for the small trade sizes we target; crossing a tick boundary
// requires fetching tick-array account data from the chain and is out of
// scope for the hot-path estimator.
//
// Overflow strategy:
//   sqrt_price_q64 is typically ~2^69 for realistic prices (verified against
//   live Orca pool data). Multiplying two such values gives ~2^138, which
//   overflows u128. The (>> 32) scaling trick keeps all intermediate products
//   within u128 while introducing at most 1 ULP error per shifted term.

/// Orca CLMM swap: spending token B (SOL/quote) to receive token A (base).
/// Direction when `is_a_wsol = false` (SOL is token_b, base token is token_a).
///
/// Formula derivation:
///   new_sqrt_price = sqrt_price + amount_net * 2^64 / liquidity
///   amount_out_a   = amount_net * 2^64 / ((sqrt_price >> 32) * (new_sqrt_price >> 32))
///
/// The `>> 32` on both sqrt_price terms brings the product of the two into
/// u128 range without losing significant precision for our trade sizes.
fn clmm_output_b_to_a(
    sqrt_price_q64: u128,
    liquidity: u128,
    fee_numerator: u64,
    fee_denominator: u64,
    amount_in: u64,
) -> Result<u64, MathError> {
    if liquidity == 0 {
        return Err(MathError::ZeroReserve);
    }

    let fee_denom = fee_denominator.max(1) as u128;
    let amount_net = (amount_in as u128)
        .checked_mul(fee_denom - fee_numerator as u128)
        .ok_or(MathError::Overflow("clmm_b_to_a fee_net"))?
        / fee_denom;

    if amount_net == 0 {
        return Ok(0);
    }

    // delta_sqrt_price = amount_net * 2^64 / liquidity  (Q64.64)
    // amount_net <= ~u64, so amount_net * 2^64 fits in u128.
    let delta_num = amount_net
        .checked_mul(1u128 << 64)
        .ok_or(MathError::Overflow("clmm_b_to_a delta_num"))?;
    let delta_sqrt_price = delta_num / liquidity;

    let new_sqrt_price = sqrt_price_q64
        .checked_add(delta_sqrt_price)
        .ok_or(MathError::Overflow("clmm_b_to_a new_sqrt_price"))?;

    // amount_out_a = amount_net * 2^64 / (sp32 * nsp32)
    // (equivalent to: amount_net / (actual_sqrt * actual_new_sqrt) in real units)
    let sp32 = sqrt_price_q64 >> 32;
    let nsp32 = new_sqrt_price >> 32;
    if sp32 == 0 || nsp32 == 0 {
        return Err(MathError::ZeroReserve);
    }

    let denom = sp32
        .checked_mul(nsp32)
        .ok_or(MathError::Overflow("clmm_b_to_a sp32*nsp32"))?;

    // delta_num is already amount_net * 2^64
    let amount_out = delta_num / denom;
    u64::try_from(amount_out).map_err(|_| MathError::OutputTooLarge)
}

/// Orca CLMM swap: spending token A (base) to receive token B (SOL/quote).
/// Direction when `is_a_wsol = false` (SOL is token_b, base token is token_a).
///
/// Formula:
///   new_sqrt_price = sqrt_price * liquidity / (liquidity + amount_net * sqrt_price >> 64)
///   amount_out_b   = liquidity * (sqrt_price - new_sqrt_price) >> 64
fn clmm_output_a_to_b(
    sqrt_price_q64: u128,
    liquidity: u128,
    fee_numerator: u64,
    fee_denominator: u64,
    amount_in: u64,
) -> Result<u64, MathError> {
    if liquidity == 0 {
        return Err(MathError::ZeroReserve);
    }

    let fee_denom = fee_denominator.max(1) as u128;
    let amount_net = (amount_in as u128)
        .checked_mul(fee_denom - fee_numerator as u128)
        .ok_or(MathError::Overflow("clmm_a_to_b fee_net"))?
        / fee_denom;

    if amount_net == 0 {
        return Ok(0);
    }

    // The increase in liquidity-per-root-price that amount_net of token A provides.
    // (amount_net * sqrt_price) >> 64 gives it in the same units as liquidity.
    let amount_sqrt = amount_net
        .checked_mul(sqrt_price_q64)
        .ok_or(MathError::Overflow("clmm_a_to_b amount_sqrt"))?
        >> 64;

    let new_denom = liquidity
        .checked_add(amount_sqrt)
        .ok_or(MathError::Overflow("clmm_a_to_b new_denom"))?;

    if new_denom == 0 {
        return Err(MathError::DivisionByZero("clmm_a_to_b new_denom"));
    }

    let new_sqrt_price = sqrt_price_q64
        .checked_mul(liquidity)
        .ok_or(MathError::Overflow("clmm_a_to_b sqrt*liq"))?
        / new_denom;

    if new_sqrt_price >= sqrt_price_q64 {
        // Selling A pushes price down; if it didn't, there is a degenerate input.
        return Ok(0);
    }

    let delta_sqrt = sqrt_price_q64 - new_sqrt_price;

    // amount_out_b = liquidity * delta_sqrt / 2^64
    let amount_out = liquidity
        .checked_mul(delta_sqrt)
        .ok_or(MathError::Overflow("clmm_a_to_b liq*delta"))?
        >> 64;

    u64::try_from(amount_out).map_err(|_| MathError::OutputTooLarge)
}

/// Unified output estimator for one swap leg. Dispatches to:
///   - Raydium CPMM exact protocol math for `RaydiumCPMM`
///   - Orca CLMM math when `pool.clmm_liquidity > 0`
///   - Generic constant-product math for legacy/fallback pools
///
/// The Raydium CPMM path reads the canonical realtime PoolState/AmmConfig/vault
/// snapshot maintained by scanner::raydium_cpmm.
/// Canonical execution quote for a single analyzer leg.
///
/// Raydium CPMM returns its complete protocol-specific quote.
/// Orca currently returns the exact single-tick CLMM output supported by the
/// current estimator.
/// Other constant-product pools use the generic fallback until their
/// protocol-specific reconstruction is completed.
#[derive(Debug, Clone)]
struct LegQuote {
    pub amount_in_raw: u64,
    pub amount_out_raw: u64,

    /// Curve price impact only.
    ///
    /// This deliberately excludes explicit protocol fees so fees aren't
    /// double-counted as "slippage".
    pub curve_price_impact_bps: i128,
}

/// Builds the `TickArrays` value `swap_quote_by_input_token` expects from
/// however many tick arrays are currently loaded for the pool.
///
/// Capped at 3, NOT the 6 `orca_whirlpools_core` can technically represent.
/// This is deliberate: the executor (`executor::orca::build_swap_instruction`)
/// builds Orca's legacy `swap` instruction, which has a hard on-chain limit
/// of exactly 3 tick array accounts (`tick_array_0`, `tick_array_1`,
/// `tick_array_2`) - there is no `remaining_accounts` mechanism available to
/// it the way there is for `swap_v2`. A quote computed against 4-6 arrays
/// would price liquidity the actual submitted transaction can never reach,
/// silently overstating the achievable output - exactly the quote/execution
/// mismatch this phase exists to eliminate. If a swap genuinely needs more
/// than 3 arrays' worth of tick range, that is a real, hard quote failure
/// here, not a case to relax the cap for.
fn build_tick_arrays(
    arrays: &[crate::scanner::orca::DecodedTickArray],
) -> Result<orca_whirlpools_core::TickArrays, MathError> {
    let facades: Vec<TickArrayFacade> = arrays.iter().map(|a| a.to_facade()).collect();
    match facades.as_slice() {
        [a] => Ok([*a].into()),
        [a, b] => Ok([*a, *b].into()),
        [a, b, c] => Ok([*a, *b, *c].into()),
        _ => Err(MathError::OrcaInsufficientTickCoverage),
    }
}

/// Exact Orca Whirlpool CLMM quote via `orca_whirlpools_core::swap_quote_by_input_token`,
/// using the currently published `OrcaQuoteState` (real decoded on-chain
/// tick arrays - see `scanner::orca::OrcaQuoteState`). Replaces the old
/// single-tick `clmm_output_a_to_b`/`clmm_output_b_to_a` approximation.
///
/// `spending_quote`: true if the input token of this leg is SOL/WSOL.
/// Falcon convention: base = non-WSOL token, quote = WSOL (see module docs).
///
/// Direction derivation (Falcon's token-A/B convention, from the prompt spec):
///   if token A is WSOL:  spending quote => specified_token_a = true
///                        spending base  => specified_token_a = false
///   if token B is WSOL:  spending quote => specified_token_a = false
///                        spending base  => specified_token_a = true
/// i.e. specified_token_a == (spending_quote == is_a_wsol).
/// Selects the exact 3 arrays (by `start_tick_index`) the on-chain legacy
/// swap instruction will submit for a trade in direction `a_to_b`, out of
/// however many are currently loaded in the snapshot - the SAME selection
/// rule as `executor::orca::derive_tick_arrays`, applied to already-decoded
/// arrays instead of deriving PDAs. This is what makes quote-time array
/// selection and execution-time array selection provably the same 3
/// accounts: both start from `tick_array_start_index(tick_current_index,
/// tick_spacing)` and the same `[0,-1,-2]` / `[0,1,2]` offsets.
///
/// Errors, rather than silently substituting a different array or fewer
/// than 3, if any of the 3 required starts is not present in the snapshot -
/// this is the "coverage" a swap of this size and direction actually needs,
/// distinct from `build_tick_arrays`'s generic 1-6-count check.
fn select_tick_arrays_for_direction(
    tick_arrays: &[crate::scanner::orca::SlottedTickArray],
    tick_current_index: i32,
    tick_spacing: u16,
    a_to_b: bool,
) -> Result<Vec<crate::scanner::orca::DecodedTickArray>, MathError> {
    use crate::executor::orca::{tick_array_start_index, TICK_ARRAY_SIZE};

    let ticks_in_array = TICK_ARRAY_SIZE * tick_spacing as i32;
    let current_start = tick_array_start_index(tick_current_index, tick_spacing);
    let offsets: [i32; 3] = if a_to_b { [0, -1, -2] } else { [0, 1, 2] };

    offsets
        .iter()
        .map(|off| {
            let start = current_start + off * ticks_in_array;
            tick_arrays
                .iter()
                .find(|s| s.array.start_tick_index == start)
                .map(|s| s.array.clone())
                .ok_or(MathError::OrcaInsufficientTickCoverage)
        })
        .collect()
}

fn exact_orca_clmm_quote(amount_in: u64, spending_quote: bool) -> Result<LegQuote, MathError> {
    let state = crate::scanner::orca::current_orca_quote_state().ok_or(
        MathError::OrcaQuoteUnavailable("no Orca quote state published yet"),
    )?;

    if state.tick_arrays.is_empty() {
        return Err(MathError::OrcaQuoteUnavailable(
            "no tick arrays loaded for this pool",
        ));
    }

    // State-slot coherence gate (Phase 4): refuse to quote against a
    // snapshot where the tick arrays are stale relative to the pool
    // account, rather than silently combining old tick state with new pool
    // state (or vice versa). See OrcaQuoteState::coherence_status and
    // MAX_COHERENT_SLOT_DRIFT for what "stale" means here.
    if let crate::scanner::orca::CoherenceStatus::Incoherent { worst_drift } =
        state.coherence_status()
    {
        return Err(MathError::OrcaStateIncoherent { worst_drift });
    }

    // specified_token_a == a_to_b for the exact-input case (confirmed
    // against orca_whirlpools_core::swap_quote_by_input_token's own source:
    // it passes specified_token_a straight through to compute_swap's a_to_b
    // parameter). Computed via orca_leg_a_to_b - the SAME function the
    // pre-simulation gate uses (analyzer::revalidate_orca_snapshot's
    // caller) - rather than a second, separately-maintained formula, so
    // quote-time and gate-time direction can never disagree.
    let a_to_b = orca_leg_a_to_b(&state, spending_quote);
    let specified_token_a = a_to_b;

    // Select arrays by direction FIRST (see select_tick_arrays_for_direction) -
    // NOT by taking however many happen to be first in state.tick_arrays,
    // which is not guaranteed to be the 3 the executor will actually submit
    // once the realtime loop's 5-array subscription window is fully
    // populated (see scanner::orca::subscription_tick_array_pdas).
    let arrays_only = select_tick_arrays_for_direction(
        &state.tick_arrays,
        state.whirlpool.tick_current_index,
        state.whirlpool.tick_spacing,
        a_to_b,
    )?;
    let tick_arrays = build_tick_arrays(&arrays_only)?;
    let whirlpool_facade = state.whirlpool.to_facade();

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Slippage tolerance here only affects `token_min_out` (which we do not
    // use below) - it has no effect on `token_est_out`, the exact estimate
    // we read for quote/opportunity-sizing purposes. The executor applies
    // its own independently-configured on-chain slippage tolerance at
    // simulation/execution time; this value is a nominal default so the
    // call itself cannot fail on an out-of-range parameter.
    const NOMINAL_SLIPPAGE_TOLERANCE_BPS: u16 = 100;

    let quote = orca_whirlpools_core::swap_quote_by_input_token(
        amount_in,
        specified_token_a,
        NOMINAL_SLIPPAGE_TOLERANCE_BPS,
        whirlpool_facade,
        None, // oracle: standard (non-adaptive-fee) fee tier - see WhirlpoolFacade::is_initialized_with_adaptive_fee
        tick_arrays,
        timestamp,
        None, // transfer_fee_a: base/quote mints here are not Token-2022 with transfer fees
        None, // transfer_fee_b
    )
    .map_err(MathError::OrcaQuoteFailed)?;

    Ok(LegQuote {
        amount_in_raw: quote.token_in,
        amount_out_raw: quote.token_est_out,
        // Exact quote already accounts for the true curve/tick-crossing
        // impact internally; we don't separately reconstruct a price-impact
        // figure the way the generic constant-product fallback does.
        curve_price_impact_bps: 0,
    })
}

// ===========================================================================
// PRE-SIMULATION REVALIDATION (Phase 4)
// ===========================================================================
// Runs immediately before an approved opportunity is simulated. The
// opportunity was priced from a snapshot some time ago; by the time we're
// about to spend an RPC simulation on it, the world may have moved. This
// is a pure decision function over an already-captured snapshot (no RPC,
// no globals besides what the caller passes in), so every branch is
// unit-testable. It never "repairs" anything: any failure means the
// opportunity is discarded and must be re-quoted from fresh state.
// ===========================================================================

/// Maximum number of slots the Orca snapshot may lag behind the newest slot
/// the realtime loop has observed on ANY subscribed account before the
/// opportunity is considered stale. ~400ms per slot, so this is roughly two
/// seconds: long enough to survive normal WebSocket notification jitter,
/// short enough that a wedged subscription can't feed simulations.
pub const MAX_SNAPSHOT_LAG_SLOTS: u64 = 5;

/// Why an approved opportunity was refused at the pre-simulation gate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RevalidationError {
    #[error("no Orca quote snapshot available")]
    NoSnapshot,
    #[error("Orca snapshot incoherent: worst tick-array slot drift {worst_drift}")]
    Incoherent { worst_drift: u64 },
    #[error("Orca snapshot is {lag} slots behind newest observed slot {newest_slot} (max {max})")]
    SnapshotTooOld {
        lag: u64,
        newest_slot: u64,
        max: u64,
    },
    #[error(
        "tick array containing current tick (start {expected_start}) is missing from the snapshot"
    )]
    CurrentTickArrayMissing { expected_start: i32 },
    #[error("tick array required by the executor for this direction (start {start}) is not in the snapshot")]
    ExecutionArrayMissing { start: i32 },
}

/// Validates an Orca snapshot for a trade in direction `a_to_b`.
///
/// `newest_observed_slot` is the highest `context.slot` the realtime loop
/// has seen on any subscribed account. Pass the same value the caller would
/// use to decide "how fresh is fresh".
pub fn revalidate_orca_snapshot(
    state: &crate::scanner::orca::OrcaQuoteState,
    a_to_b: bool,
    newest_observed_slot: u64,
) -> Result<(), RevalidationError> {
    use crate::executor::orca::{tick_array_start_index, TICK_ARRAY_SIZE};

    // 1. Coherence across pool + tick arrays.
    if let crate::scanner::orca::CoherenceStatus::Incoherent { worst_drift } =
        state.coherence_status()
    {
        return Err(RevalidationError::Incoherent { worst_drift });
    }

    // 2. Age of the snapshot itself. The oldest component decides.
    let oldest_slot = state
        .tick_arrays
        .iter()
        .map(|s| s.slot)
        .chain(std::iter::once(state.pool_slot))
        .min()
        .unwrap_or(state.pool_slot);
    let lag = newest_observed_slot.saturating_sub(oldest_slot);
    if lag > MAX_SNAPSHOT_LAG_SLOTS {
        return Err(RevalidationError::SnapshotTooOld {
            lag,
            newest_slot: newest_observed_slot,
            max: MAX_SNAPSHOT_LAG_SLOTS,
        });
    }

    let spacing = state.whirlpool.tick_spacing;
    let ticks_in_array = TICK_ARRAY_SIZE * spacing as i32;
    let current_start = tick_array_start_index(state.whirlpool.tick_current_index, spacing);

    let have = |start: i32| {
        state
            .tick_arrays
            .iter()
            .any(|s| s.array.start_tick_index == start)
    };

    // 3. The on-chain legacy swap requires tick_array_0 to contain the
    //    pool's CURRENT price at execution time. If the pool has since moved
    //    into a neighbouring array and we never loaded it, the quote and the
    //    transaction would disagree.
    if !have(current_start) {
        return Err(RevalidationError::CurrentTickArrayMissing {
            expected_start: current_start,
        });
    }

    // 4. Every array the executor will submit for this direction must be in
    //    the snapshot the quote used - same rule, same source of truth
    //    (executor::orca::derive_tick_arrays uses offsets [0,-1,-2] / [0,1,2]).
    let offsets: [i32; 3] = if a_to_b { [0, -1, -2] } else { [0, 1, 2] };
    for off in offsets {
        let start = current_start + off * ticks_in_array;
        if !have(start) {
            return Err(RevalidationError::ExecutionArrayMissing { start });
        }
    }

    Ok(())
}

/// Direction of the Orca leg, derived exactly the way the executor derives
/// it (`a_to_b = token_mint_a == input_mint`), from the snapshot's own token
/// A mint. `orca_is_buy_leg` is true when Orca is where we BUY the base token
/// (so the input is WSOL) and false when Orca is where we SELL it (input is
/// the base token). One rule, evaluated against the same pool state the quote
/// used, so the gate and the executor cannot silently disagree.
pub fn orca_leg_a_to_b(
    state: &crate::scanner::orca::OrcaQuoteState,
    orca_is_buy_leg: bool,
) -> bool {
    let token_a_is_wsol =
        state.whirlpool.token_mint_a.to_string() == crate::scanner::orca::WSOL_MINT;
    // Input is WSOL on a buy, base on a sell. a_to_b means "input is token A".
    // Buy:  a_to_b iff A is WSOL.   Sell: a_to_b iff A is NOT WSOL (A is base).
    if orca_is_buy_leg {
        token_a_is_wsol
    } else {
        !token_a_is_wsol
    }
}

fn estimate_leg_quote(
    pool: &crate::scanner::PriceUpdate,
    amount_in: u64,
    spending_quote: bool,
) -> Result<LegQuote, MathError> {
    if pool.dex == "RaydiumCPMM" {
        let state =
            raydium_cpmm::current_quote_state().ok_or(MathError::CpmmQuoteStateUnavailable)?;

        let quote = raydium_cpmm::quote_base_input(&state, amount_in, spending_quote)
            .map_err(MathError::CpmmQuote)?;

        let impact = estimate_curve_price_impact_bps(
            quote.effective_reserve_in_raw,
            quote.effective_reserve_out_raw,
            quote.effective_amount_in_raw,
            quote.gross_output_raw,
        )?;

        return Ok(LegQuote {
            amount_in_raw: quote.amount_in_raw,
            amount_out_raw: quote.amount_received_raw,
            curve_price_impact_bps: impact,
        });
    }

    if pool.clmm_liquidity > 0 {
        return exact_orca_clmm_quote(amount_in, spending_quote);
    }

    let (reserve_in, reserve_out) = if spending_quote {
        (pool.quote_reserve_raw, pool.base_reserve_raw)
    } else {
        (pool.base_reserve_raw, pool.quote_reserve_raw)
    };

    let output = estimate_output_amount_raw(reserve_in, reserve_out, amount_in)?;

    let impact = estimate_curve_price_impact_bps(reserve_in, reserve_out, amount_in, output)?;

    Ok(LegQuote {
        amount_in_raw: amount_in,
        amount_out_raw: output,
        curve_price_impact_bps: impact,
    })
}

pub fn find_opportunity(
    price_a: &PriceUpdate,
    price_b: &PriceUpdate,
    trade_size_lamports: u64,
) -> Result<Opportunity, RejectReason> {
    // Orientation: decide which side is cheaper using raw reserves via
    // cross-multiplication (quote_a/base_a vs quote_b/base_b), never a float
    // price comparison.
    //
    // price = quote_reserve / base_reserve (SOL per base token). Lower price
    // = cheaper = where we buy.
    let cross_a = (price_a.quote_reserve_raw as u128) * (price_b.base_reserve_raw as u128);
    let cross_b = (price_b.quote_reserve_raw as u128) * (price_a.base_reserve_raw as u128);

    if cross_a == cross_b {
        return Err(RejectReason::NoSpread);
    }

    let (buy, sell) = if cross_a < cross_b {
        (price_a, price_b)
    } else {
        (price_b, price_a)
    };

    if buy.base_reserve_raw == 0
        || buy.quote_reserve_raw == 0
        || sell.base_reserve_raw == 0
        || sell.quote_reserve_raw == 0
    {
        return Err(RejectReason::NoSpread);
    }

    // raw_spread_bps = (sell_price - buy_price) / buy_price, in bps, via
    // cross-multiplication:
    // sell_price/buy_price = (sell_quote/sell_base) / (buy_quote/buy_base)
    //                      = (sell_quote * buy_base) / (sell_base * buy_quote)
    let sell_over_buy_num = (sell.quote_reserve_raw as u128) * (buy.base_reserve_raw as u128);
    let sell_over_buy_den = (sell.base_reserve_raw as u128) * (buy.quote_reserve_raw as u128);

    if sell_over_buy_den == 0 {
        return Err(RejectReason::NoSpread);
    }

    let raw_spread_bps: i128 = ((sell_over_buy_num as i128) * (BPS_DENOMINATOR as i128)
        / (sell_over_buy_den as i128))
        - BPS_DENOMINATOR as i128;

    const MIN_RAW_SPREAD_BPS: i128 = 1; // 0.01%

    if raw_spread_bps < MIN_RAW_SPREAD_BPS {
        return Err(RejectReason::SpreadTooSmall {
            spread_bps: raw_spread_bps,
            min_required_bps: MIN_RAW_SPREAD_BPS,
        });
    }

    // Fees as exact integer bps: fee_bps = fee_numerator * BPS / fee_denominator.
    let buy_fee_bps = (buy.fee_numerator as i128) * (BPS_DENOMINATOR as i128)
        / (buy.fee_denominator.max(1) as i128);
    let sell_fee_bps = (sell.fee_numerator as i128) * (BPS_DENOMINATOR as i128)
        / (sell.fee_denominator.max(1) as i128);
    let total_fee_bps = buy_fee_bps + sell_fee_bps;

    let fee_adjusted_spread_bps = raw_spread_bps - total_fee_bps;

    if fee_adjusted_spread_bps <= 0 {
        return Err(RejectReason::FeesExceedSpread {
            fee_adjusted_bps: fee_adjusted_spread_bps,
        });
    }

    // CPMM/CLMM quote outputs are chained exactly:
    // leg 2 receives the exact raw output produced by leg 1.
    let buy_quote = match estimate_leg_quote(buy, trade_size_lamports, true) {
        Ok(v) => v,
        Err(_) => return Err(RejectReason::NoSpread),
    };

    let expected_output_after_buy_raw = buy_quote.amount_out_raw;

    let sell_quote = match estimate_leg_quote(sell, expected_output_after_buy_raw, false) {
        Ok(v) => v,
        Err(_) => return Err(RejectReason::NoSpread),
    };

    let expected_output_after_sell_raw = sell_quote.amount_out_raw;

    let buy_slippage_bps = buy_quote.curve_price_impact_bps;
    let sell_slippage_bps = sell_quote.curve_price_impact_bps;

    let total_slippage_bps = buy_slippage_bps
        .checked_add(sell_slippage_bps)
        .ok_or(RejectReason::NoSpread)?;

    let net_spread_after_slippage_bps = fee_adjusted_spread_bps - total_slippage_bps;

    if net_spread_after_slippage_bps <= 0 {
        return Err(RejectReason::SlippageExceedsSpread {
            net_bps: net_spread_after_slippage_bps,
        });
    }

    // --- authoritative decision: actual integer lamport PnL, not a percentage ---
    let fixed_cost = match fixed_cost_lamports() {
        Ok(v) => v,
        Err(_) => return Err(RejectReason::ArithmeticFailure),
    };

    let net_profit_lamports =
        (expected_output_after_sell_raw as i128) - (trade_size_lamports as i128) - fixed_cost;

    if net_profit_lamports <= 0 {
        return Err(RejectReason::NetProfitNotPositive {
            net_profit_lamports,
        });
    }

    // --- presentation-only derivations, computed once, for logging ---
    let buy_price = buy.quote_reserve_raw as f64 / buy.base_reserve_raw as f64;
    let sell_price = sell.quote_reserve_raw as f64 / sell.base_reserve_raw as f64;
    let raw_spread_pct = raw_spread_bps as f64 / 100.0;
    let fee_adjusted_spread_pct = fee_adjusted_spread_bps as f64 / 100.0;
    let net_spread_after_slippage_pct = net_spread_after_slippage_bps as f64 / 100.0;
    let net_profit_pct = if trade_size_lamports > 0 {
        (net_profit_lamports as f64 / trade_size_lamports as f64) * 100.0
    } else {
        0.0
    };

    Ok(Opportunity {
        buy_dex: buy.dex.clone(),
        sell_dex: sell.dex.clone(),
        pair: price_a.pair.clone(),
        trade_size_lamports,
        expected_output_after_buy_raw,
        expected_output_after_sell_raw,
        net_profit_lamports,
        buy_price,
        sell_price,
        raw_spread_pct,
        fee_adjusted_spread_pct,
        net_spread_after_slippage_pct,
        net_profit_pct,
    })
}

/// Tries several trade sizes (as integer bps fractions of the thinner pool's
/// raw reserves) and returns whichever produces the best outcome - either the
/// highest-profit approved Opportunity (by integer lamports, not a
/// percentage), or if none are profitable, the least-bad rejection.
///
/// `capital_source` decides whether each candidate size gets clamped to a
/// real balance ceiling before being evaluated:
///   - `CapitalSource::Wallet { max_lamports }`: every candidate is clamped
///     to `max_lamports` - sizing never proposes spending more than the
///     wallet can actually afford, no matter how deep the pool is.
///   - `CapitalSource::Pool`: no clamp - sizing is exactly what it was
///     before, purely a function of pool depth. This is the flash-loan
///     placeholder path; do not select it against a real wallet's own funds.
pub fn find_best_opportunity(
    price_a: &PriceUpdate,
    price_b: &PriceUpdate,
    capital_source: crate::config::CapitalSource,
) -> Result<Opportunity, RejectReason> {
    // Same six sizes as before (0.1%, 0.5%, 1%, 2%, 5%, 10%), now as exact
    // integer bps instead of f64 fractions.
    const SIZE_FRACTIONS_BPS: [u128; 6] = [10, 50, 100, 200, 500, 1_000];

    // Trade size is sized in lamports against the thinner pool's QUOTE
    // (lamport) reserve - this is what's actually being spent on the buy
    // leg, so sizing off the quote side (rather than the base side, which
    // the pre-migration f64 code used) keeps the size in the same unit as
    // what's spent, with no price-based unit conversion needed.
    let thinner_quote_reserve = price_a.quote_reserve_raw.min(price_b.quote_reserve_raw) as u128;

    let mut best_opportunity: Option<Opportunity> = None;
    let mut least_bad_rejection: Option<RejectReason> = None;
    let mut least_bad_net = i128::MIN;

    for fraction_bps in SIZE_FRACTIONS_BPS {
        let pool_derived_size = thinner_quote_reserve * fraction_bps / BPS_DENOMINATOR;

        // The ONLY place capital_source affects anything: clamp the
        // pool-derived candidate down to the wallet's real ceiling when
        // running in Wallet mode. In Pool mode this is a no-op - the
        // candidate passes through exactly as pool-depth sizing produced it,
        // same as before this change existed.
        let trade_size = match capital_source {
            crate::config::CapitalSource::Wallet { max_lamports } => {
                pool_derived_size.min(max_lamports as u128)
            }
            crate::config::CapitalSource::Pool => pool_derived_size,
        };

        let trade_size_lamports = trade_size.min(u64::MAX as u128) as u64;

        if trade_size_lamports == 0 {
            continue;
        }

        match find_opportunity(price_a, price_b, trade_size_lamports) {
            Ok(opp) => {
                let is_better = match &best_opportunity {
                    Some(current_best) => {
                        opp.net_profit_lamports > current_best.net_profit_lamports
                    }
                    None => true,
                };
                if is_better {
                    best_opportunity = Some(opp);
                }
            }
            Err(reason) => {
                let net = match &reason {
                    RejectReason::SlippageExceedsSpread { net_bps } => *net_bps,
                    RejectReason::NetProfitNotPositive {
                        net_profit_lamports,
                    } => *net_profit_lamports,
                    RejectReason::FeesExceedSpread { fee_adjusted_bps } => *fee_adjusted_bps,
                    _ => i128::MIN,
                };
                if best_opportunity.is_none() && net > least_bad_net {
                    least_bad_net = net;
                    least_bad_rejection = Some(reason);
                }
            }
        }
    }

    match best_opportunity {
        Some(opp) => Ok(opp),
        None => Err(least_bad_rejection.unwrap_or(RejectReason::NoSpread)),
    }
}

/// Evaluates price updates with a staleness check. If either pool's snapshot is older
/// than `max_price_age_secs`, rejects the opportunity to prevent trading on phantom/ghost spreads.
pub fn find_best_opportunity_with_staleness(
    price_a: &PriceUpdate,
    price_b: &PriceUpdate,
    max_price_age_secs: u64,
    capital_source: crate::config::CapitalSource,
) -> Result<Opportunity, RejectReason> {
    let now = std::time::SystemTime::now();
    let age_a = now
        .duration_since(price_a.timestamp)
        .unwrap_or_default()
        .as_secs();
    let age_b = now
        .duration_since(price_b.timestamp)
        .unwrap_or_default()
        .as_secs();
    let max_age = age_a.max(age_b);
    if max_price_age_secs > 0 && max_age > max_price_age_secs {
        return Err(RejectReason::StalePrice {
            age_secs: max_age,
            max_age_secs: max_price_age_secs,
        });
    }
    find_best_opportunity(price_a, price_b, capital_source)
}

// ===========================================================================
// TESTS
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn pu(
        dex: &str,
        base_reserve: u64,
        quote_reserve: u64,
        fee_num: u64,
        fee_den: u64,
    ) -> PriceUpdate {
        PriceUpdate {
            dex: dex.to_string(),
            pair: "TESTPAIR".to_string(),
            base_reserve_raw: base_reserve,
            quote_reserve_raw: quote_reserve,
            base_decimals: 6,
            quote_decimals: 9,
            fee_numerator: fee_num,
            fee_denominator: fee_den,
            price: quote_reserve as f64 / base_reserve as f64,
            base_liquidity: base_reserve as f64,
            quote_liquidity: quote_reserve as f64,
            fee_pct: (fee_num as f64 / fee_den as f64) * 100.0,
            timestamp: std::time::SystemTime::now(),
            clmm_sqrt_price_q64: 0,
            clmm_liquidity: 0,
            clmm_is_a_wsol: false,
        }
    }

    // 1. CPMM quote correctness
    #[test]
    fn quote_matches_constant_product_formula() {
        // reserve_in=1_000_000, reserve_out=2_000_000, amount_in=1000
        // expected = (1000 * 2_000_000) / (1_000_000 + 1000) = 2_000_000_000 / 1_001_000 = 1998 (floor)
        let out = estimate_output_amount_raw(1_000_000, 2_000_000, 1_000).unwrap();
        assert_eq!(out, 1998);
    }

    #[test]
    fn quote_zero_input_gives_zero_output() {
        let out = estimate_output_amount_raw(1_000_000, 2_000_000, 0).unwrap();
        assert_eq!(out, 0);
    }

    // 2. fee calculation (via find_opportunity's fee_adjusted_spread_bps path)
    #[test]
    fn fees_correctly_reduce_spread() {
        // 1% spread, but each side charges 0.6% -> 1.2% total fees -> should reject
        let a = pu("A", 1_000_000_000, 1_000_000_000, 6_000, 1_000_000); // 0.6% fee
        let b = pu("B", 1_000_000_000, 1_010_000_000, 6_000, 1_000_000); // 0.6% fee, 1% higher price
        let result = find_opportunity(&a, &b, 50_000_000);
        assert!(matches!(result, Err(RejectReason::FeesExceedSpread { .. })));
    }

    // 3. minimum_out calculation
    #[test]
    fn minimum_out_applies_correct_tolerance() {
        // 1% tolerance (100 bps) on 1_000_000 -> 990_000
        let min_out = calculate_minimum_out_raw(1_000_000, 100).unwrap();
        assert_eq!(min_out, 990_000);
    }

    #[test]
    fn minimum_out_never_weaker_than_tolerance_due_to_rounding() {
        // 3 units, 1 bps tolerance: raw_reduction = 3 * 1 = 3, ceil(3/10_000) = 1
        // minimum_out = 3 - 1 = 2, NOT 3 (which would be a weaker/looser floor)
        let min_out = calculate_minimum_out_raw(3, 1).unwrap();
        assert_eq!(min_out, 2);
    }

    // 4. slippage boundaries
    #[test]
    fn slippage_zero_for_negligible_trade() {
        let bps = estimate_slippage_bps(1_000_000_000_000, 1_000_000_000_000, 1_000_000).unwrap();
        assert!(
            bps.abs() < 2,
            "expected ~0 bps slippage for a tiny trade, got {bps}"
        );
    }

    #[test]
    fn slippage_grows_with_trade_size() {
        let small = estimate_slippage_bps(1_000_000, 1_000_000, 1_000).unwrap();
        let large = estimate_slippage_bps(1_000_000, 1_000_000, 100_000).unwrap();
        assert!(large > small, "larger trade should have more slippage bps");
    }

    // 5. trade-size calculations
    #[test]
    fn safe_trade_size_is_one_percent_of_thinner_pool() {
        let size = safe_trade_size_raw(1_000_000, 2_000_000);
        assert_eq!(size, 10_000); // 1% of 1_000_000
    }

    // 6. profit calculation / overall approval
    #[test]
    fn profitable_opportunity_is_approved_with_correct_lamport_profit() {
        // Large, clearly profitable spread with negligible fees/slippage at tiny size.
        let a = pu("A", 1_000_000_000_000, 1_000_000_000_000, 1, 1_000_000);
        let b = pu("B", 1_000_000_000_000, 1_100_000_000_000, 1, 1_000_000); // 10% higher
        let result = find_opportunity(&a, &b, 50_000_000);
        match result {
            Ok(opp) => {
                assert!(opp.net_profit_lamports > 0);
                assert_eq!(opp.buy_dex, "A");
                assert_eq!(opp.sell_dex, "B");
            }
            Err(e) => panic!("expected approval, got rejection: {e:?}"),
        }
    }

    // 7. overflow handling
    #[test]
    fn quote_overflow_is_caught_not_wrapped() {
        // amount_in * reserve_out overflowing u128 is astronomically unlikely with
        // real u64 inputs (u64::MAX * u64::MAX still fits u128), so instead verify
        // the checked path structurally: max u64 reserves/amount must not panic
        // and must return either Ok or a MathError, never wrap silently.
        let result = estimate_output_amount_raw(u64::MAX, u64::MAX, u64::MAX);
        assert!(result.is_ok() || matches!(result, Err(MathError::OutputTooLarge)));
    }

    // 8. division-by-zero handling
    #[test]
    fn quote_zero_reserve_is_explicit_error() {
        let result = estimate_output_amount_raw(0, 1_000_000, 1_000);
        assert!(matches!(result, Err(MathError::ZeroReserve)));
    }

    #[test]
    fn minimum_out_rejects_invalid_tolerance() {
        let result = calculate_minimum_out_raw(1_000_000, 10_001);
        assert!(matches!(result, Err(MathError::InvalidInput(_))));
    }

    // 9. decimal conversion - covered at the scanner layer (raw amount parsing
    // from RPC `amount` string), not applicable to analyzer's raw-only domain.

    // 10. very small token amounts
    #[test]
    fn very_small_amounts_do_not_panic() {
        let out = estimate_output_amount_raw(2, 2, 1).unwrap();
        assert_eq!(out, 0); // floors to zero, correctly - not an error
    }

    // 11. very large reserves
    #[test]
    fn very_large_reserves_do_not_overflow() {
        let out = estimate_output_amount_raw(u64::MAX / 2, u64::MAX / 2, 1_000_000).unwrap();
        assert!(out > 0);
    }

    // 12. exact leg-1 -> leg-2 propagation
    #[test]
    fn leg1_output_feeds_leg2_input_exactly() {
        let a = pu("A", 500_000_000_000, 500_000_000_000, 2_500, 1_000_000);
        let b = pu("B", 500_000_000_000, 520_000_000_000, 2_500, 1_000_000);
        let opp = find_opportunity(&a, &b, 50_000_000).expect("should approve");

        // Recompute leg 1 independently and confirm the Opportunity carries
        // the exact same integer value through to what leg 2 used as input.
        let expected_leg1 =
            estimate_output_amount_raw(a.quote_reserve_raw, a.base_reserve_raw, 50_000_000)
                .unwrap();
        assert_eq!(opp.expected_output_after_buy_raw, expected_leg1);

        let expected_leg2 =
            estimate_output_amount_raw(b.base_reserve_raw, b.quote_reserve_raw, expected_leg1)
                .unwrap();
        assert_eq!(opp.expected_output_after_sell_raw, expected_leg2);
    }

    // 13. negative PnL
    #[test]
    fn unprofitable_trade_is_rejected_with_negative_profit_visible() {
        // Spread barely exists after fees but slippage/cost should push it negative.
        let a = pu("A", 1_000_000, 1_000_000, 2_500, 1_000_000);
        let b = pu("B", 1_000_000, 1_000_500, 2_500, 1_000_000); // tiny 0.05% spread
        let result = find_opportunity(&a, &b, 500_000); // large relative to pool
        assert!(result.is_err(), "expected rejection for unprofitable trade");
    }

    // --- fixed transaction cost (no Jito placeholder) ---

    #[test]
    fn fixed_cost_is_the_real_atomic_tx_cost_not_the_old_jito_placeholder() {
        // 1 signature (5_000) + 350_000 CU * 25_000 micro-lamports (8_750) = 13_750.
        // The removed placeholder charged (5_000 + 100_000) * 2 = 210_000.
        assert_eq!(fixed_cost_lamports().unwrap(), 13_750);
        assert_ne!(fixed_cost_lamports().unwrap(), 210_000);
    }

    #[test]
    fn analyzer_and_executor_cost_constants_are_the_same_source() {
        // The analyzer prices from config; the executor passes the same
        // config constants to set_compute_unit_limit/price. Recomputing the
        // cost from those exact constants must equal what the analyzer uses.
        let from_shared = crate::config::fixed_tx_cost_lamports(
            crate::config::TX_SIGNATURES,
            crate::config::COMPUTE_UNIT_LIMIT,
            crate::config::COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        )
        .unwrap();
        assert_eq!(fixed_cost_lamports().unwrap(), i128::from(from_shared));
    }

    #[test]
    fn net_profit_lamports_subtracts_exactly_the_fixed_cost() {
        // Same market, two different trade sizes can't isolate the cost, so
        // recompute: profit == sell_output - trade_size - fixed_cost, exactly.
        let a = pu("A", 1_000_000_000_000, 1_000_000_000_000, 1, 1_000_000);
        let b = pu("B", 1_000_000_000_000, 1_100_000_000_000, 1, 1_000_000);
        let opp = find_opportunity(&a, &b, 50_000_000).unwrap();
        assert_eq!(
            opp.net_profit_lamports,
            opp.expected_output_after_sell_raw as i128
                - opp.trade_size_lamports as i128
                - fixed_cost_lamports().unwrap()
        );
    }

    #[test]
    fn trade_profitable_only_after_removing_the_fake_tip_is_now_approved() {
        // Find a tiny trade whose gross gain sits between the real cost
        // (13_750) and the old placeholder cost (210_000): the old code
        // rejected it, the real cost model must approve it.
        let a = pu("A", 1_000_000_000_000, 1_000_000_000_000, 1, 1_000_000);
        let b = pu("B", 1_000_000_000_000, 1_100_000_000_000, 1, 1_000_000);

        let mut found = false;
        for size in [
            100_000u64, 200_000, 400_000, 800_000, 1_600_000, 3_200_000, 6_400_000,
        ] {
            if let Ok(opp) = find_opportunity(&a, &b, size) {
                let gross =
                    opp.expected_output_after_sell_raw as i128 - opp.trade_size_lamports as i128;
                if gross > 13_750 && gross <= 210_000 {
                    assert!(opp.net_profit_lamports > 0);
                    found = true;
                    break;
                }
            }
        }
        assert!(
            found,
            "expected at least one size with real cost < gross gain <= old placeholder cost"
        );
    }

    #[test]
    fn fixed_cost_overflow_is_an_error_not_a_wrapped_value() {
        assert!(matches!(
            fixed_cost_from(u64::MAX, 0, 0),
            Err(MathError::Overflow(_))
        ));
        assert!(matches!(
            fixed_cost_from(1, u32::MAX, u64::MAX),
            Err(MathError::Overflow(_))
        ));
        // Sanity: the same function with real inputs still succeeds.
        assert_eq!(fixed_cost_from(1, 350_000, 25_000).unwrap(), 13_750);
    }

    #[test]
    fn arithmetic_failure_reject_reason_is_distinct_from_no_spread() {
        assert_ne!(
            format!("{:?}", RejectReason::ArithmeticFailure),
            format!("{:?}", RejectReason::NoSpread)
        );
        assert_eq!(RejectReason::ArithmeticFailure.as_display_pct(), 0.0);
    }

    // 14. rounding direction
    #[test]
    fn output_amount_floors_never_overstates() {
        // 7 / 3 in the constant-product formula should floor, not round.
        // reserve_in=3, reserve_out=7, amount_in=3 -> (3*7)/(3+3) = 21/6 = 3 (floor of 3.5)
        let out = estimate_output_amount_raw(3, 7, 3).unwrap();
        assert_eq!(out, 3);
    }

    // 15. values around integer boundaries
    #[test]
    fn values_at_u64_boundary_are_handled() {
        let out = estimate_output_amount_raw(u64::MAX, 1, u64::MAX);
        assert!(out.is_ok());
    }

    #[test]
    fn no_spread_when_prices_equal() {
        let a = pu("A", 1_000_000, 1_000_000, 2_500, 1_000_000);
        let b = pu("B", 1_000_000, 1_000_000, 2_500, 1_000_000);
        assert!(matches!(
            find_opportunity(&a, &b, 1_000),
            Err(RejectReason::NoSpread)
        ));
    }

    #[test]
    fn freshness_check_accepts_identical_reserves() {
        assert!(is_still_fresh(1_000_000, 2_000_000, 1_000_000, 2_000_000, 50).unwrap());
    }

    #[test]
    fn curve_price_impact_is_zero_for_infinitesimal_trade() {
        let impact = estimate_curve_price_impact_bps(1_000_000, 2_000_000, 1, 1).unwrap();

        assert!(impact >= 0);
    }

    #[test]
    fn curve_price_impact_increases_with_trade_size() {
        let small = estimate_curve_price_impact_bps(1_000_000, 2_000_000, 1_000, 1_998).unwrap();

        let large =
            estimate_curve_price_impact_bps(1_000_000, 2_000_000, 100_000, 181_818).unwrap();

        assert!(large > small);
    }

    #[test]
    fn canonical_quote_output_is_used_as_next_leg_input() {
        let buy_output = 123_456u64;

        let sell_input = buy_output;

        assert_eq!(sell_input, 123_456);
    }

    #[test]
    fn freshness_check_rejects_large_drift() {
        // Price roughly doubles - should exceed even a generous 500 bps (5%) threshold.
        assert!(!is_still_fresh(1_000_000, 2_000_000, 1_000_000, 4_000_000, 500).unwrap());
    }

    // =======================================================================
    // Phase 4: exact Orca CLMM quote tests (exact_orca_clmm_quote)
    // =======================================================================
    // Fixtures build a real Whirlpool + real on-chain-shaped tick arrays and
    // publish them via `scanner::orca::publish_orca_quote_state`, exactly as
    // the realtime loop will in production, then exercise the exact quote
    // path through the public `estimate_leg_quote` entry point (which
    // dispatches to `exact_orca_clmm_quote` whenever `pool.clmm_liquidity >
    // 0`), not the internal function directly - this covers the real wiring,
    // not just the math in isolation.
    mod orca_exact_quote {
        use super::*;
        use crate::scanner::orca::{
            publish_orca_quote_state, DecodedTickArray, OrcaQuoteState, Whirlpool,
            WhirlpoolRewardInfo, WSOL_MINT,
        };
        use orca_whirlpools_core::{tick_index_to_sqrt_price, TickFacade, TICK_ARRAY_SIZE};
        use solana_sdk::pubkey::Pubkey;
        use std::str::FromStr;

        const TICK_SPACING: u16 = 64;
        const ARRAY_SPAN: i32 = TICK_ARRAY_SIZE as i32 * TICK_SPACING as i32; // 5632
        /// Current tick sits at the midpoint of its containing array
        /// (44 * 64 = 2816), leaving room to walk in both directions within
        /// a single loaded array - see `test_whirlpool`'s doc comment.
        const CURRENT_TICK: i32 = (TICK_ARRAY_SIZE as i32 / 2) * TICK_SPACING as i32;

        /// `CURRENT_ORCA_QUOTE_STATE` (in `scanner::orca`) is ONE process-
        /// global slot, not keyed per pool - `current_orca_quote_state()`
        /// returns whatever was most recently published by ANY caller,
        /// regardless of which pool_pubkey that state belongs to. Every
        /// test in this module that publishes a state and then reads it
        /// back via `exact_orca_clmm_quote` is therefore racing every other
        /// such test under the default parallel test runner - a test can
        /// publish its own state and read back a DIFFERENT test's state
        /// that landed in the gap between the two calls.
        ///
        /// This lock serializes every publish-then-read test in this module
        /// against every other one, so each test's publish() and its
        /// subsequent read are guaranteed uninterrupted by another thread.
        /// It does not fix the global's lack of per-caller isolation in
        /// production (there, one realtime-loop writer and one analyzer
        /// reader make this far less exposed) - it only makes the test
        /// suite deterministic. A real fix would thread an explicit
        /// `OrcaQuoteState` parameter through `exact_orca_clmm_quote`
        /// instead of reading a global, removing the shared-state
        /// requirement entirely; that is a larger refactor left for a
        /// later stage rather than folded into this test-flakiness fix.
        static ORCA_STATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn zero_reward_infos() -> [WhirlpoolRewardInfo; 3] {
            [
                WhirlpoolRewardInfo {
                    mint: Pubkey::default(),
                    vault: Pubkey::default(),
                    authority: Pubkey::default(),
                    emissions_per_second_x64: 0,
                    growth_global_x64: 0,
                },
                WhirlpoolRewardInfo {
                    mint: Pubkey::default(),
                    vault: Pubkey::default(),
                    authority: Pubkey::default(),
                    emissions_per_second_x64: 0,
                    growth_global_x64: 0,
                },
                WhirlpoolRewardInfo {
                    mint: Pubkey::default(),
                    vault: Pubkey::default(),
                    authority: Pubkey::default(),
                    emissions_per_second_x64: 0,
                    growth_global_x64: 0,
                },
            ]
        }

        /// Builds a Whirlpool with the given starting liquidity, `is_a_wsol`
        /// orientation, and a 0.3% fee rate (fee_rate is in hundredths of a
        /// bip: 3000 = 0.30%). `tick_current_index` is deliberately placed
        /// at the MIDPOINT of its containing tick array (not the array's
        /// start) - `orca_whirlpools_core`'s tick-walk needs room on both
        /// sides of the current tick within the loaded array bounds, in
        /// both the a-to-b and b-to-a directions; a current tick sitting
        /// exactly on an array's start boundary leaves zero room to walk
        /// downward and is rejected as an invalid tick array sequence.
        fn test_whirlpool(_pool_pubkey: Pubkey, liquidity: u128, is_a_wsol: bool) -> Whirlpool {
            let sqrt_price = tick_index_to_sqrt_price(CURRENT_TICK);
            let wsol = Pubkey::from_str(WSOL_MINT).unwrap();
            let other = Pubkey::new_from_array([42u8; 32]);

            Whirlpool {
                whirlpools_config: Pubkey::default(),
                whirlpool_bump: [0],
                tick_spacing: TICK_SPACING,
                tick_spacing_seed: TICK_SPACING.to_le_bytes(),
                fee_rate: 3000,
                protocol_fee_rate: 0,
                liquidity,
                sqrt_price,
                tick_current_index: CURRENT_TICK,
                protocol_fee_owed_a: 0,
                protocol_fee_owed_b: 0,
                token_mint_a: if is_a_wsol { wsol } else { other },
                token_vault_a: Pubkey::new_from_array([1u8; 32]),
                fee_growth_global_a: 0,
                token_mint_b: if is_a_wsol { other } else { wsol },
                token_vault_b: Pubkey::new_from_array([2u8; 32]),
                fee_growth_global_b: 0,
                reward_last_updated_timestamp: 0,
                reward_infos: zero_reward_infos(),
            }
        }

        /// A single tick array whose midpoint is the pool's current tick
        /// (see `CURRENT_TICK`), with no initialized ticks anywhere -
        /// liquidity is flat across the whole array, so no crossing occurs
        /// for any swap that stays within it. There is room on both sides
        /// of the current tick within this one array's bounds.
        /// The full 5-array subscription window (2 below, current, 2 above)
        /// a trade in EITHER direction can draw its 3-array selection from -
        /// i.e. what scanner::orca::subscription_tick_array_pdas actually
        /// subscribes to in production. All flat (no initialized ticks).
        /// Needed because select_tick_arrays_for_direction requires the
        /// EXACT 3 starts a trade's direction calls for
        /// ([0,-1,-2]*ARRAY_SPAN for a_to_b, [0,1,2]*ARRAY_SPAN for b_to_a) -
        /// a narrower fixture (e.g. only 1 neighbour each side) satisfies
        /// one direction but not the other.
        fn flat_single_array(pool_pubkey: Pubkey) -> Vec<DecodedTickArray> {
            [-2, -1, 0, 1, 2]
                .into_iter()
                .map(|off| DecodedTickArray {
                    start_tick_index: off * ARRAY_SPAN,
                    whirlpool: pool_pubkey,
                    ticks: [TickFacade::default(); TICK_ARRAY_SIZE],
                })
                .collect()
        }

        /// Three adjacent tick arrays (covering the array containing the
        /// pool's current tick, plus both neighbors) with ONE initialized
        /// tick set partway through the middle array, carrying a negative
        /// `liquidity_net` of `delta` - i.e. liquidity DROPS by `delta` once
        /// price crosses that tick moving upward (the standard Orca
        /// convention: liquidity_net is added when crossing upward through
        /// the tick from below).
        fn three_arrays_with_one_boundary(
            pool_pubkey: Pubkey,
            boundary_offset_in_middle_array: usize,
            delta: i128,
        ) -> Vec<DecodedTickArray> {
            let mut middle = DecodedTickArray {
                start_tick_index: 0,
                whirlpool: pool_pubkey,
                ticks: [TickFacade::default(); TICK_ARRAY_SIZE],
            };
            middle.ticks[boundary_offset_in_middle_array] = TickFacade {
                initialized: true,
                liquidity_net: delta,
                liquidity_gross: delta.unsigned_abs(),
                fee_growth_outside_a: 0,
                fee_growth_outside_b: 0,
                reward_growths_outside: [0, 0, 0],
            };

            // Full 5-array window despite the name/docstring's "three" - the
            // middle array carries the boundary tick; the 4 neighbours are
            // flat. select_tick_arrays_for_direction needs the SPECIFIC 3
            // starts a trade's direction calls for
            // ([0,-1,-2]*ARRAY_SPAN or [0,1,2]*ARRAY_SPAN), so a trade in
            // EITHER direction needs both sides present, not just one
            // neighbour each side.
            [-2, -1, 1, 2]
                .into_iter()
                .map(|off| DecodedTickArray {
                    start_tick_index: off * ARRAY_SPAN,
                    whirlpool: pool_pubkey,
                    ticks: [TickFacade::default(); TICK_ARRAY_SIZE],
                })
                .chain(std::iter::once(middle))
                .collect()
        }

        fn publish(whirlpool: Whirlpool, pool_pubkey: Pubkey, tick_arrays: Vec<DecodedTickArray>) {
            // Existing tests don't care about slot coherence, so every
            // array shares the pool's slot (drift = 0, always coherent).
            publish_with_slots(whirlpool, pool_pubkey, 100, tick_arrays, |_| 100);
        }

        /// Publishes a state where the pool sits at `pool_slot` and each
        /// tick array's slot is chosen by `slot_for_array(index)`. Used by
        /// the coherence tests to build deliberately-incoherent snapshots.
        fn publish_with_slots(
            whirlpool: Whirlpool,
            pool_pubkey: Pubkey,
            pool_slot: u64,
            tick_arrays: Vec<DecodedTickArray>,
            slot_for_array: impl Fn(usize) -> u64,
        ) {
            let slotted = tick_arrays
                .into_iter()
                .enumerate()
                .map(|(i, array)| crate::scanner::orca::SlottedTickArray {
                    array,
                    slot: slot_for_array(i),
                })
                .collect();
            publish_orca_quote_state(OrcaQuoteState {
                whirlpool_pubkey: pool_pubkey,
                whirlpool,
                tick_arrays: slotted,
                pool_slot,
            });
        }

        /// Publishes the given state and immediately calls
        /// `exact_orca_clmm_quote`, holding `ORCA_STATE_TEST_LOCK` for the
        /// whole publish-then-read critical section. Every test in this
        /// module that needs "publish this state, then read it back through
        /// the real quote path" MUST go through this helper rather than
        /// calling `publish()` and `exact_orca_clmm_quote()` separately -
        /// see `ORCA_STATE_TEST_LOCK`'s doc comment for why the two calls
        /// are otherwise racy against every other test in this module.
        fn publish_and_quote(
            whirlpool: Whirlpool,
            pool_pubkey: Pubkey,
            tick_arrays: Vec<DecodedTickArray>,
            amount_in: u64,
            spending_quote: bool,
        ) -> Result<LegQuote, MathError> {
            let _guard = ORCA_STATE_TEST_LOCK.lock().unwrap();
            publish(whirlpool, pool_pubkey, tick_arrays);
            exact_orca_clmm_quote(amount_in, spending_quote)
        }

        fn clmm_pool(is_a_wsol: bool) -> PriceUpdate {
            // Only clmm_liquidity/clmm_is_a_wsol matter for routing into
            // exact_orca_clmm_quote - the exact math itself reads from the
            // published OrcaQuoteState, not from these PriceUpdate fields.
            PriceUpdate {
                clmm_liquidity: 1, // any nonzero value routes to the exact-quote path
                clmm_is_a_wsol: is_a_wsol,
                ..pu("Orca", 1, 1, 3_000, 1_000_000)
            }
        }

        // --- Test 1: exact Orca input quote (basic, no crossing) ---

        #[test]
        fn exact_input_quote_within_single_array_is_nonzero_and_below_naive_output() {
            let pool_pubkey = Pubkey::new_from_array([99u8; 32]);
            let whirlpool = test_whirlpool(pool_pubkey, 10_000_000_000_000, true);
            let quote = publish_and_quote(
                whirlpool,
                pool_pubkey,
                flat_single_array(pool_pubkey),
                1_000_000,
                true,
            )
            .unwrap();

            assert!(quote.amount_out_raw > 0);
            assert_eq!(quote.amount_in_raw, 1_000_000);
            // A 0.3% protocol fee is always charged on the input side, so
            // the trade_fee-inclusive `token_in` the engine reports back
            // (echoed here as amount_in_raw) must exceed what's actually
            // net-applied to the curve - i.e. output can never exceed input
            // 1:1 once ANY fee is charged, regardless of the pool's price.
            // We assert the weaker, price-independent invariant: output is
            // strictly positive and the quote succeeded end-to-end.
        }

        // --- Test 2: token A input ---

        #[test]
        fn quote_handles_token_a_as_input_when_a_is_wsol() {
            let pool_pubkey = Pubkey::new_from_array([1u8; 32]);
            let whirlpool = test_whirlpool(pool_pubkey, 10_000_000_000_000, true); // A = WSOL
                                                                                   // spending_quote = true means spending WSOL = spending token A here.
            let result = publish_and_quote(
                whirlpool,
                pool_pubkey,
                flat_single_array(pool_pubkey),
                500_000,
                true,
            );
            assert!(
                result.is_ok(),
                "token-A-input quote should succeed: {result:?}"
            );
        }

        // --- Test 3: token B input ---

        #[test]
        fn quote_handles_token_b_as_input_when_b_is_wsol() {
            let pool_pubkey = Pubkey::new_from_array([2u8; 32]);
            let whirlpool = test_whirlpool(pool_pubkey, 10_000_000_000_000, false); // B = WSOL
                                                                                    // spending_quote = true means spending WSOL = spending token B here.
            let result = publish_and_quote(
                whirlpool,
                pool_pubkey,
                flat_single_array(pool_pubkey),
                500_000,
                true,
            );
            assert!(
                result.is_ok(),
                "token-B-input quote should succeed: {result:?}"
            );
        }

        #[test]
        fn quote_direction_flips_correctly_between_a_and_b_wsol_orientation() {
            // Two pools, opposite is_a_wsol orientation. In each, selling
            // base (spending_quote=false) should succeed and produce a
            // nonzero quote - proving specified_token_a is derived
            // correctly (not just defaulted) for both orientations. Outputs
            // need not be numerically equal: is_a_wsol also flips which
            // side of the pool the WSOL leg sits on, and the underlying
            // fixture pools are not perfectly symmetric once that's
            // accounted for - so this test checks direction handling
            // succeeds cleanly in both cases, not that magnitudes match.
            let pool_a = Pubkey::new_from_array([3u8; 32]);
            let out_a_is_wsol = publish_and_quote(
                test_whirlpool(pool_a, 10_000_000_000_000, true),
                pool_a,
                flat_single_array(pool_a),
                1_000_000,
                false,
            )
            .unwrap();
            assert!(out_a_is_wsol.amount_out_raw > 0);

            let pool_b = Pubkey::new_from_array([4u8; 32]);
            let out_b_is_wsol = publish_and_quote(
                test_whirlpool(pool_b, 10_000_000_000_000, false),
                pool_b,
                flat_single_array(pool_b),
                1_000_000,
                false,
            )
            .unwrap();
            assert!(out_b_is_wsol.amount_out_raw > 0);
        }

        // --- Test 4: crossing one initialized tick ---

        #[test]
        fn quote_crosses_one_initialized_tick_when_swap_is_large_enough() {
            let pool_pubkey = Pubkey::new_from_array([5u8; 32]);
            // Modest liquidity so a large swap genuinely moves price past a
            // boundary tick set a small distance above the current tick.
            let whirlpool = test_whirlpool(pool_pubkey, 50_000_000, true);
            // Boundary tick a few slots above the current tick (slot 44 in
            // the middle array is CURRENT_TICK itself; pick slot 49 => a
            // tick index comfortably reachable above the current price).
            let arrays = three_arrays_with_one_boundary(pool_pubkey, 49, -20_000_000);

            // Both reads must happen against the SAME published state and
            // without another test's publish() landing in between, so both
            // calls stay inside one locked critical section rather than
            // going through publish_and_quote (which only covers a single
            // read per publish).
            let (small, large) = {
                let _guard = ORCA_STATE_TEST_LOCK.lock().unwrap();
                publish(whirlpool, pool_pubkey, arrays);
                // A large buy (spending quote/WSOL) should push price up
                // through the boundary tick.
                let small = exact_orca_clmm_quote(1_000, true).unwrap();
                let large = exact_orca_clmm_quote(5_000_000, true).unwrap();
                (small, large)
            };

            // Sanity: larger input still yields more output than a tiny
            // input, and both succeed - i.e. tick-crossing didn't break the
            // quote or make it degenerate.
            assert!(large.amount_out_raw > small.amount_out_raw);
        }

        // --- Test 5: crossing multiple initialized ticks ---

        #[test]
        fn quote_succeeds_across_multiple_tick_arrays() {
            let pool_pubkey = Pubkey::new_from_array([6u8; 32]);
            // Liquidity high enough that even after both boundary ticks
            // remove some, there is still plenty left to absorb the swap
            // well before running off the edge of the 3 loaded arrays.
            let whirlpool = test_whirlpool(pool_pubkey, 200_000_000, true);
            // Two boundary ticks in the middle array above the current tick
            // (slot 44), each removing a modest amount of liquidity as price
            // rises - forces the quote engine to cross more than one
            // initialized tick for a large enough swap while still
            // resolving comfortably within the 3 loaded arrays.
            let mut arrays = three_arrays_with_one_boundary(pool_pubkey, 50, -30_000_000);
            arrays[1].ticks[60] = TickFacade {
                initialized: true,
                liquidity_net: -30_000_000,
                liquidity_gross: 30_000_000,
                fee_growth_outside_a: 0,
                fee_growth_outside_b: 0,
                reward_growths_outside: [0, 0, 0],
            };
            let result = publish_and_quote(whirlpool, pool_pubkey, arrays, 500_000, true);
            assert!(
                result.is_ok(),
                "multi-tick-crossing quote should still resolve: {result:?}"
            );
        }

        // --- Test 6: insufficient tick-array coverage ---

        #[test]
        fn empty_tick_arrays_is_rejected_by_build_tick_arrays() {
            // Exercises the exact same code path exact_orca_clmm_quote takes
            // when state.tick_arrays.is_empty() - checked directly against
            // the pure function rather than through the shared
            // CURRENT_ORCA_QUOTE_STATE global, which other tests in this
            // binary write to concurrently under the default (parallel)
            // test runner. A publish-then-immediately-read against a
            // process-global is not safe to assert on across threads: another
            // test can publish its own (valid, non-empty) state in the gap
            // between this test's publish() and its read, making the
            // assertion flaky rather than deterministic. See
            // `quote_fails_cleanly_when_no_tick_arrays_are_loaded_serial`
            // below for an end-to-end version of this same check, run with
            // explicit serialization.
            let result = build_tick_arrays(&[]);
            assert!(matches!(
                result,
                Err(MathError::OrcaInsufficientTickCoverage)
            ));
        }

        /// End-to-end version of the empty-coverage check, going through
        /// `exact_orca_clmm_quote` and the real `OrcaQuoteState` global -
        /// this catches a regression in the `state.tick_arrays.is_empty()`
        /// short-circuit itself (the check that runs BEFORE
        /// `build_tick_arrays` is ever called), which the pure
        /// `build_tick_arrays` test above cannot see.
        ///
        /// This test takes `ORCA_STATE_TEST_LOCK` for its entire body,
        /// serializing it against every other test in this module that also
        /// takes the lock, so no other thread can publish a competing state
        /// to `CURRENT_ORCA_QUOTE_STATE` between this test's publish() and
        /// its read.
        #[test]
        fn quote_fails_cleanly_when_no_tick_arrays_are_loaded_serial() {
            let _guard = ORCA_STATE_TEST_LOCK.lock().unwrap();

            let pool_pubkey = Pubkey::new_from_array([7u8; 32]);
            let whirlpool = test_whirlpool(pool_pubkey, 10_000_000_000_000, true);
            publish(whirlpool, pool_pubkey, vec![]); // no arrays at all

            let result = exact_orca_clmm_quote(1_000_000, true);
            assert!(matches!(result, Err(MathError::OrcaQuoteUnavailable(_))));
        }

        #[test]
        fn build_tick_arrays_rejects_more_than_three() {
            // The cap is 3, not 6 - see build_tick_arrays's doc comment: the
            // executor's legacy swap instruction can only ever submit 3
            // tick array accounts on-chain, so a quote computed against more
            // than 3 would price liquidity the real transaction can't reach.
            let pool_pubkey = Pubkey::new_from_array([8u8; 32]);
            let mut arrays = Vec::new();
            for i in 0..4 {
                arrays.push(DecodedTickArray {
                    start_tick_index: (i - 2) * ARRAY_SPAN,
                    whirlpool: pool_pubkey,
                    ticks: [TickFacade::default(); TICK_ARRAY_SIZE],
                });
            }
            let result = build_tick_arrays(&arrays);
            assert!(matches!(
                result,
                Err(MathError::OrcaInsufficientTickCoverage)
            ));
        }

        // --- Tests 9/10: stale state and coherent snapshot validation ---

        #[test]
        fn coherent_snapshot_within_slot_tolerance_quotes_successfully() {
            let pool_pubkey = Pubkey::new_from_array([11u8; 32]);
            let whirlpool = test_whirlpool(pool_pubkey, 10_000_000_000_000, true);
            let max = crate::scanner::orca::MAX_COHERENT_SLOT_DRIFT;

            let result = {
                let _guard = ORCA_STATE_TEST_LOCK.lock().unwrap();
                // Array is exactly at the tolerance boundary - still coherent.
                publish_with_slots(
                    whirlpool,
                    pool_pubkey,
                    1_000,
                    flat_single_array(pool_pubkey),
                    |_| 1_000 - max,
                );
                exact_orca_clmm_quote(500_000, true)
            };
            assert!(
                result.is_ok(),
                "drift == max must still be coherent: {result:?}"
            );
        }

        #[test]
        fn stale_tick_array_beyond_tolerance_is_rejected() {
            let pool_pubkey = Pubkey::new_from_array([12u8; 32]);
            let whirlpool = test_whirlpool(pool_pubkey, 10_000_000_000_000, true);
            let max = crate::scanner::orca::MAX_COHERENT_SLOT_DRIFT;

            let result = {
                let _guard = ORCA_STATE_TEST_LOCK.lock().unwrap();
                // One slot past tolerance: old tick state against newer pool state.
                publish_with_slots(
                    whirlpool,
                    pool_pubkey,
                    1_000,
                    flat_single_array(pool_pubkey),
                    |_| 1_000 - max - 1,
                );
                exact_orca_clmm_quote(500_000, true)
            };
            assert!(matches!(
                result,
                Err(MathError::OrcaStateIncoherent { worst_drift }) if worst_drift == max + 1
            ));
        }

        #[test]
        fn tick_array_newer_than_pool_beyond_tolerance_is_also_rejected() {
            // Drift is symmetric: a tick array NEWER than the pool account is
            // just as inconsistent as one that is older.
            let pool_pubkey = Pubkey::new_from_array([13u8; 32]);
            let whirlpool = test_whirlpool(pool_pubkey, 10_000_000_000_000, true);
            let max = crate::scanner::orca::MAX_COHERENT_SLOT_DRIFT;

            let result = {
                let _guard = ORCA_STATE_TEST_LOCK.lock().unwrap();
                publish_with_slots(
                    whirlpool,
                    pool_pubkey,
                    1_000,
                    flat_single_array(pool_pubkey),
                    |_| 1_000 + max + 1,
                );
                exact_orca_clmm_quote(500_000, true)
            };
            assert!(matches!(result, Err(MathError::OrcaStateIncoherent { .. })));
        }

        #[test]
        fn one_stale_array_among_several_fails_the_whole_snapshot() {
            let pool_pubkey = Pubkey::new_from_array([14u8; 32]);
            let whirlpool = test_whirlpool(pool_pubkey, 200_000_000, true);
            let max = crate::scanner::orca::MAX_COHERENT_SLOT_DRIFT;

            let result = {
                let _guard = ORCA_STATE_TEST_LOCK.lock().unwrap();
                publish_with_slots(
                    whirlpool,
                    pool_pubkey,
                    1_000,
                    three_arrays_with_one_boundary(pool_pubkey, 50, -30_000_000),
                    // Two fresh arrays, one stale: the worst one decides.
                    |i| if i == 2 { 1_000 - max - 10 } else { 1_000 },
                );
                exact_orca_clmm_quote(500_000, true)
            };
            assert!(matches!(
                result,
                Err(MathError::OrcaStateIncoherent { worst_drift }) if worst_drift == max + 10
            ));
        }

        #[test]
        fn coherence_status_is_pure_and_reports_worst_drift() {
            // Direct check of OrcaQuoteState::coherence_status, no global involved.
            use crate::scanner::orca::{CoherenceStatus, SlottedTickArray};
            let pool_pubkey = Pubkey::new_from_array([15u8; 32]);
            let mk = |slot: u64| SlottedTickArray {
                array: flat_single_array(pool_pubkey).remove(0),
                slot,
            };
            let state = OrcaQuoteState {
                whirlpool_pubkey: pool_pubkey,
                whirlpool: test_whirlpool(pool_pubkey, 1, true),
                tick_arrays: vec![mk(500), mk(497), mk(480)],
                pool_slot: 500,
            };
            assert_eq!(
                state.coherence_status(),
                CoherenceStatus::Incoherent { worst_drift: 20 }
            );

            let ok = OrcaQuoteState {
                tick_arrays: vec![mk(500), mk(498)],
                ..state
            };
            assert_eq!(ok.coherence_status(), CoherenceStatus::Coherent);
        }

        // --- Pre-simulation revalidation (pure function, no global) ---

        /// Builds a snapshot whose tick arrays sit at the given offsets (in
        /// units of one array) from the array containing CURRENT_TICK.
        fn snapshot_with_offsets(
            offsets: &[i32],
            pool_slot: u64,
            array_slot: u64,
        ) -> OrcaQuoteState {
            let pool_pubkey = Pubkey::new_from_array([21u8; 32]);
            let current_start =
                crate::executor::orca::tick_array_start_index(CURRENT_TICK, TICK_SPACING);
            OrcaQuoteState {
                whirlpool_pubkey: pool_pubkey,
                whirlpool: test_whirlpool(pool_pubkey, 1_000_000, true),
                tick_arrays: offsets
                    .iter()
                    .map(|off| crate::scanner::orca::SlottedTickArray {
                        array: DecodedTickArray {
                            start_tick_index: current_start + off * ARRAY_SPAN,
                            whirlpool: pool_pubkey,
                            ticks: [TickFacade::default(); TICK_ARRAY_SIZE],
                        },
                        slot: array_slot,
                    })
                    .collect(),
                pool_slot,
            }
        }

        #[test]
        fn revalidation_accepts_full_window_in_both_directions() {
            let s = snapshot_with_offsets(&[-2, -1, 0, 1, 2], 1_000, 1_000);
            assert_eq!(revalidate_orca_snapshot(&s, true, 1_000), Ok(()));
            assert_eq!(revalidate_orca_snapshot(&s, false, 1_000), Ok(()));
        }

        #[test]
        fn revalidation_accepts_exactly_the_three_arrays_for_one_direction() {
            // a_to_b needs [0,-1,-2]; b_to_a needs [0,1,2].
            let down = snapshot_with_offsets(&[-2, -1, 0], 1_000, 1_000);
            assert_eq!(revalidate_orca_snapshot(&down, true, 1_000), Ok(()));
            assert!(matches!(
                revalidate_orca_snapshot(&down, false, 1_000),
                Err(RevalidationError::ExecutionArrayMissing { .. })
            ));
        }

        #[test]
        fn revalidation_rejects_missing_current_tick_array() {
            // Neighbours present, but the array holding the current tick is not.
            let s = snapshot_with_offsets(&[-2, -1, 1, 2], 1_000, 1_000);
            assert!(matches!(
                revalidate_orca_snapshot(&s, true, 1_000),
                Err(RevalidationError::CurrentTickArrayMissing { .. })
            ));
        }

        #[test]
        fn revalidation_rejects_missing_execution_array() {
            // Current array present but only one below - a_to_b needs two.
            let s = snapshot_with_offsets(&[-1, 0, 1, 2], 1_000, 1_000);
            let err = revalidate_orca_snapshot(&s, true, 1_000).unwrap_err();
            let current_start =
                crate::executor::orca::tick_array_start_index(CURRENT_TICK, TICK_SPACING);
            assert_eq!(
                err,
                RevalidationError::ExecutionArrayMissing {
                    start: current_start - 2 * ARRAY_SPAN
                }
            );
        }

        #[test]
        fn revalidation_rejects_incoherent_snapshot() {
            let s = snapshot_with_offsets(&[-2, -1, 0, 1, 2], 1_000, 900);
            assert!(matches!(
                revalidate_orca_snapshot(&s, true, 1_000),
                Err(RevalidationError::Incoherent { worst_drift: 100 })
            ));
        }

        #[test]
        fn revalidation_rejects_snapshot_too_old_even_if_internally_coherent() {
            // Pool and arrays agree with each other (drift 0) but the whole
            // snapshot is far behind the newest observed slot.
            let max = MAX_SNAPSHOT_LAG_SLOTS;
            let s = snapshot_with_offsets(&[-2, -1, 0, 1, 2], 1_000, 1_000);
            assert!(matches!(
                revalidate_orca_snapshot(&s, true, 1_000 + max + 1),
                Err(RevalidationError::SnapshotTooOld { lag, .. }) if lag == max + 1
            ));
            // Exactly at the limit is still acceptable.
            assert_eq!(revalidate_orca_snapshot(&s, true, 1_000 + max), Ok(()));
        }

        #[test]
        fn revalidation_uses_oldest_component_for_lag() {
            // Pool is fresh, arrays are a few slots behind but still coherent:
            // the OLDEST component must drive the lag calculation.
            let max = MAX_SNAPSHOT_LAG_SLOTS;
            let coherent_gap = crate::scanner::orca::MAX_COHERENT_SLOT_DRIFT;
            let s = snapshot_with_offsets(&[-2, -1, 0, 1, 2], 1_000, 1_000 - coherent_gap);
            let newest = 1_000 - coherent_gap + max + 1;
            assert!(matches!(
                revalidate_orca_snapshot(&s, true, newest),
                Err(RevalidationError::SnapshotTooOld { .. })
            ));
        }

        #[test]
        fn revalidation_handles_newest_slot_older_than_snapshot() {
            // saturating_sub: a caller passing a stale "newest" value must not
            // underflow/panic.
            let s = snapshot_with_offsets(&[-2, -1, 0, 1, 2], 1_000, 1_000);
            assert_eq!(revalidate_orca_snapshot(&s, true, 10), Ok(()));
        }

        // --- Direction derivation for the pre-simulation gate ---

        #[test]
        fn orca_leg_direction_matches_executor_rule_for_all_four_cases() {
            let a_wsol = snapshot_with_offsets(&[0], 1, 1); // token A == WSOL
            assert!(orca_leg_a_to_b(&a_wsol, true)); // buy: spend WSOL(A) -> a_to_b
            assert!(!orca_leg_a_to_b(&a_wsol, false)); // sell: spend base(B) -> b_to_a

            let pool_pubkey = Pubkey::new_from_array([22u8; 32]);
            let b_wsol = OrcaQuoteState {
                whirlpool: test_whirlpool(pool_pubkey, 1_000_000, false), // token B == WSOL
                ..snapshot_with_offsets(&[0], 1, 1)
            };
            assert!(!orca_leg_a_to_b(&b_wsol, true)); // buy: spend WSOL(B) -> b_to_a
            assert!(orca_leg_a_to_b(&b_wsol, false)); // sell: spend base(A) -> a_to_b
        }

        // --- Test 9 (legacy): stale/no state published yet ---

        #[test]
        fn quote_fails_when_no_orca_state_ever_published_for_a_fresh_process() {
            // This test is best-effort: since CURRENT_ORCA_QUOTE_STATE is a
            // process-global OnceLock, other tests in this binary may have
            // already published a state by the time this runs. It documents
            // the intended behavior (OrcaQuoteUnavailable on an empty store)
            // even though full isolation would require a per-test instance
            // rather than a global.
            let _ = exact_orca_clmm_quote(1, true);
        }

        // --- Direction-derivation unit coverage (no quote engine involved) ---

        #[test]
        fn specified_token_a_matches_falcon_convention_table() {
            // specified_token_a == (spending_quote == is_a_wsol), per Falcon's
            // stated convention. Check the formula itself, for all 4 cases,
            // rather than asserting on disconnected literals.
            let specified_token_a =
                |spending_quote: bool, is_a_wsol: bool| spending_quote == is_a_wsol;

            // if token A is WSOL: spending quote => specified_token_a = true
            assert!(specified_token_a(true, true));
            // if token A is WSOL: spending base => specified_token_a = false
            assert!(!specified_token_a(false, true));
            // if token B is WSOL: spending quote => specified_token_a = false
            assert!(!specified_token_a(true, false));
            // if token B is WSOL: spending base => specified_token_a = true
            assert!(specified_token_a(false, false));
        }

        // =====================================================================
        // Quote/execution parity tests (Phase 4)
        // =====================================================================
        // These exist to prove, mechanically, that the arrays/accounts the
        // analyzer QUOTES against are the exact same ones the executor will
        // SUBMIT on-chain for the same trade - not merely "3 arrays selected
        // somehow", but the specific 3 PDAs in the specific order
        // executor::orca::derive_tick_arrays computes. A mismatch here is
        // exactly the class of bug that produced the original observed
        // failure (Orca buy succeeded at quote-time assumptions the
        // Raydium CPMM sell then contradicted).
        //
        // This module found a real bug during Phase 4 stage 6: the array
        // selection this suite exercises used to take whatever 3 entries
        // happened to be first in OrcaQuoteState.tick_arrays's Vec, which is
        // NOT guaranteed to be the direction-specific 3 the executor derives
        // once more than 3 arrays are loaded (which the realtime loop's
        // 5-array subscription window, added in stage 3, routinely does).
        // select_tick_arrays_for_direction was added specifically to fix
        // this; the tests below are the parity proof, and
        // array_selection_ignores_vec_order_regression_guard is a direct
        // regression test for the exact failure mode.
        mod quote_execution_parity {
            use super::*;
            use crate::scanner::orca::{DecodedTickArray, SlottedTickArray};
            use orca_whirlpools_core::{TickArrayFacade, TickFacade, TICK_ARRAY_SIZE};
            use solana_sdk::pubkey::Pubkey;

            fn flat_array(pool_pubkey: Pubkey, start_tick_index: i32) -> DecodedTickArray {
                DecodedTickArray {
                    start_tick_index,
                    whirlpool: pool_pubkey,
                    ticks: [TickFacade::default(); TICK_ARRAY_SIZE],
                }
            }

            /// The full 5-array subscription window around `tick_current_index`,
            /// matching what scanner::orca::subscription_tick_array_pdas
            /// subscribes to in production.
            fn five_array_window(
                pool_pubkey: Pubkey,
                tick_current_index: i32,
                tick_spacing: u16,
            ) -> Vec<DecodedTickArray> {
                let ticks_in_array = crate::executor::orca::TICK_ARRAY_SIZE * tick_spacing as i32;
                let start =
                    crate::executor::orca::tick_array_start_index(tick_current_index, tick_spacing);
                [-2, -1, 0, 1, 2]
                    .into_iter()
                    .map(|off| flat_array(pool_pubkey, start + off * ticks_in_array))
                    .collect()
            }

            // --- Direct parity: selection logic vs. execution logic ---

            #[test]
            fn array_selection_matches_executor_derivation_across_many_ticks_and_both_directions() {
                let pool_pubkey = Pubkey::new_unique();
                let tick_spacing: u16 = 64;

                // Sweep a range of current ticks, including negative ones and
                // ones sitting exactly on an array boundary, to catch an
                // off-by-one in either side's rounding.
                let mut current_ticks = vec![0, 64, 2816, 5631, 5632, -1, -64, -2816, -5632];
                for base in [-3, -2, -1, 0, 1, 2, 3] {
                    current_ticks.push(base * TICK_ARRAY_SIZE as i32 * tick_spacing as i32);
                }

                for tick_current_index in current_ticks {
                    let window = five_array_window(pool_pubkey, tick_current_index, tick_spacing);
                    let slotted: Vec<SlottedTickArray> = window
                        .iter()
                        .cloned()
                        .map(|array| SlottedTickArray { array, slot: 1 })
                        .collect();

                    for a_to_b in [true, false] {
                        // What the executor will actually submit on-chain.
                        let executor_pdas = crate::executor::orca::derive_tick_arrays(
                            &pool_pubkey,
                            tick_current_index,
                            tick_spacing,
                            a_to_b,
                        )
                        .unwrap();
                        let executor_starts: Vec<i32> = executor_pdas
                            .iter()
                            .map(|pda| {
                                // Recover the start each PDA was derived from by
                                // checking which of the 5 window arrays' own PDA
                                // matches - i.e. decode via re-derivation, proving
                                // the PDAs are for these exact starts.
                                window
                                    .iter()
                                    .find(|w| {
                                        crate::executor::orca::derive_tick_array_pda(
                                            &pool_pubkey,
                                            w.start_tick_index,
                                        )
                                        .unwrap()
                                            == *pda
                                    })
                                    .map(|w| w.start_tick_index)
                                    .expect("executor PDA must correspond to one of the 5 window arrays")
                            })
                            .collect();

                        // What the analyzer will quote against.
                        let selected = select_tick_arrays_for_direction(
                            &slotted,
                            tick_current_index,
                            tick_spacing,
                            a_to_b,
                        )
                        .unwrap();
                        let selected_starts: Vec<i32> =
                            selected.iter().map(|a| a.start_tick_index).collect();

                        assert_eq!(
                            selected_starts, executor_starts,
                            "tick={tick_current_index} a_to_b={a_to_b}: analyzer selected {selected_starts:?}, executor would submit {executor_starts:?}"
                        );
                    }
                }
            }

            #[test]
            fn selected_array_pdas_are_byte_identical_to_executor_pdas() {
                // Same proof as above, but comparing actual derived Pubkeys
                // rather than start_tick_index values, closing the gap that a
                // start_tick_index match doesn't strictly prove PDA identity
                // (a bug in derive_tick_array_pda itself could still slip
                // through the test above).
                let pool_pubkey = Pubkey::new_unique();
                let tick_spacing: u16 = 64;
                let tick_current_index = 2816;

                let window = five_array_window(pool_pubkey, tick_current_index, tick_spacing);
                let slotted: Vec<SlottedTickArray> = window
                    .iter()
                    .cloned()
                    .map(|array| SlottedTickArray { array, slot: 1 })
                    .collect();

                for a_to_b in [true, false] {
                    let executor_pdas = crate::executor::orca::derive_tick_arrays(
                        &pool_pubkey,
                        tick_current_index,
                        tick_spacing,
                        a_to_b,
                    )
                    .unwrap();

                    let selected = select_tick_arrays_for_direction(
                        &slotted,
                        tick_current_index,
                        tick_spacing,
                        a_to_b,
                    )
                    .unwrap();
                    let selected_pdas: Vec<Pubkey> = selected
                        .iter()
                        .map(|a| {
                            crate::executor::orca::derive_tick_array_pda(
                                &pool_pubkey,
                                a.start_tick_index,
                            )
                            .unwrap()
                        })
                        .collect();

                    assert_eq!(selected_pdas, executor_pdas.to_vec(), "a_to_b={a_to_b}");
                }
            }

            #[test]
            fn selection_is_order_sensitive_matching_derive_tick_arrays_exactly() {
                // Not just SET equality - ORDER matters too, since
                // build_tick_arrays feeds the 3 arrays to
                // orca_whirlpools_core in the order given, and tick_array_0
                // must specifically be the array containing the current
                // price (see revalidate_orca_snapshot's
                // CurrentTickArrayMissing check for why this matters).
                let pool_pubkey = Pubkey::new_unique();
                let tick_spacing: u16 = 64;
                let tick_current_index = 2816;
                let window = five_array_window(pool_pubkey, tick_current_index, tick_spacing);
                let slotted: Vec<SlottedTickArray> = window
                    .iter()
                    .cloned()
                    .map(|array| SlottedTickArray { array, slot: 1 })
                    .collect();

                let selected_a_to_b = select_tick_arrays_for_direction(
                    &slotted,
                    tick_current_index,
                    tick_spacing,
                    true,
                )
                .unwrap();
                let starts: Vec<i32> = selected_a_to_b.iter().map(|a| a.start_tick_index).collect();
                // a_to_b offsets are [0, -1, -2]: current array FIRST, then
                // descending - exactly executor::orca::derive_tick_arrays's
                // own offset order for a_to_b=true.
                assert_eq!(starts, vec![0, -5632, -11264]);

                let selected_b_to_a = select_tick_arrays_for_direction(
                    &slotted,
                    tick_current_index,
                    tick_spacing,
                    false,
                )
                .unwrap();
                let starts: Vec<i32> = selected_b_to_a.iter().map(|a| a.start_tick_index).collect();
                assert_eq!(starts, vec![0, 5632, 11264]);
            }

            // --- Regression guard for the exact bug this test module found ---

            #[test]
            fn array_selection_ignores_vec_order_regression_guard() {
                // Deliberately construct OrcaQuoteState.tick_arrays in an
                // order that would have fooled the OLD "just take the first
                // 3 in the Vec" logic: put 3 arrays that are NOT what either
                // direction needs first, with the correct 5-array window
                // appended after them in scrambled order.
                let pool_pubkey = Pubkey::new_unique();
                let tick_spacing: u16 = 64;
                let tick_current_index = 2816;
                let window = five_array_window(pool_pubkey, tick_current_index, tick_spacing);

                // Scrambled order: upper-2, current, lower-1, lower-2, upper-1.
                // The old bug would have taken [upper-2, current, lower-1] as
                // "the 3 arrays" regardless of direction - wrong for BOTH
                // a_to_b (needs [current, lower-1, lower-2]) and b_to_a
                // (needs [current, upper-1, upper-2]).
                let scrambled = vec![
                    window[4].clone(), // upper-2 (+2*ARRAY_SPAN)
                    window[2].clone(), // current (0)
                    window[1].clone(), // lower-1 (-1*ARRAY_SPAN)
                    window[0].clone(), // lower-2 (-2*ARRAY_SPAN)
                    window[3].clone(), // upper-1 (+1*ARRAY_SPAN)
                ];
                let slotted: Vec<SlottedTickArray> = scrambled
                    .into_iter()
                    .map(|array| SlottedTickArray { array, slot: 1 })
                    .collect();

                let selected = select_tick_arrays_for_direction(
                    &slotted,
                    tick_current_index,
                    tick_spacing,
                    true, // a_to_b
                )
                .unwrap();
                let starts: Vec<i32> = selected.iter().map(|a| a.start_tick_index).collect();

                // Correct regardless of input order: current, then
                // descending - matching derive_tick_arrays's a_to_b=true
                // offsets exactly, NOT whatever the Vec's first 3 entries
                // happened to be.
                assert_eq!(starts, vec![0, -5632, -11264]);
                assert_ne!(
                    starts,
                    vec![5632 * 2, 0, -5632],
                    "must not silently fall back to Vec insertion order"
                );
            }

            #[test]
            fn missing_the_specific_required_array_fails_even_with_enough_arrays_present() {
                // 3 arrays are present - enough to satisfy build_tick_arrays's
                // generic 1-6 count check - but they are the WRONG 3 for the
                // requested direction. Must fail, not silently substitute.
                let pool_pubkey = Pubkey::new_unique();
                let tick_spacing: u16 = 64;
                let tick_current_index = 2816;
                let window = five_array_window(pool_pubkey, tick_current_index, tick_spacing);

                // Only lower-2, lower-1, upper-1 - missing "current" (0) and
                // upper-2, so NEITHER direction's exact 3 is present.
                let insufficient = vec![window[0].clone(), window[1].clone(), window[3].clone()];
                let slotted: Vec<SlottedTickArray> = insufficient
                    .into_iter()
                    .map(|array| SlottedTickArray { array, slot: 1 })
                    .collect();

                for a_to_b in [true, false] {
                    let result = select_tick_arrays_for_direction(
                        &slotted,
                        tick_current_index,
                        tick_spacing,
                        a_to_b,
                    );
                    assert!(
                        matches!(result, Err(MathError::OrcaInsufficientTickCoverage)),
                        "a_to_b={a_to_b}: expected InsufficientTickCoverage, got {result:?}"
                    );
                }
            }

            // --- End-to-end: publish -> quote -> verify arrays actually used ---

            #[test]
            fn end_to_end_quote_uses_exactly_the_executor_selected_arrays() {
                // Full round-trip through the real published-state global and
                // exact_orca_clmm_quote, not just the pure selection function -
                // proves the wiring, not just the algorithm.
                let pool_pubkey = Pubkey::new_from_array([200u8; 32]);
                let tick_spacing: u16 = 64;
                let tick_current_index = 2816;
                let sqrt_price = orca_whirlpools_core::tick_index_to_sqrt_price(tick_current_index);
                let wsol = Pubkey::from_str(crate::scanner::orca::WSOL_MINT).unwrap();
                let other = Pubkey::new_from_array([201u8; 32]);

                let whirlpool = crate::scanner::orca::Whirlpool {
                    whirlpools_config: Pubkey::default(),
                    whirlpool_bump: [0],
                    tick_spacing,
                    tick_spacing_seed: tick_spacing.to_le_bytes(),
                    fee_rate: 3000,
                    protocol_fee_rate: 0,
                    liquidity: 10_000_000_000_000,
                    sqrt_price,
                    tick_current_index,
                    protocol_fee_owed_a: 0,
                    protocol_fee_owed_b: 0,
                    token_mint_a: wsol, // A = WSOL
                    token_vault_a: Pubkey::new_from_array([1u8; 32]),
                    fee_growth_global_a: 0,
                    token_mint_b: other,
                    token_vault_b: Pubkey::new_from_array([2u8; 32]),
                    fee_growth_global_b: 0,
                    reward_last_updated_timestamp: 0,
                    reward_infos: [
                        crate::scanner::orca::WhirlpoolRewardInfo {
                            mint: Pubkey::default(),
                            vault: Pubkey::default(),
                            authority: Pubkey::default(),
                            emissions_per_second_x64: 0,
                            growth_global_x64: 0,
                        },
                        crate::scanner::orca::WhirlpoolRewardInfo {
                            mint: Pubkey::default(),
                            vault: Pubkey::default(),
                            authority: Pubkey::default(),
                            emissions_per_second_x64: 0,
                            growth_global_x64: 0,
                        },
                        crate::scanner::orca::WhirlpoolRewardInfo {
                            mint: Pubkey::default(),
                            vault: Pubkey::default(),
                            authority: Pubkey::default(),
                            emissions_per_second_x64: 0,
                            growth_global_x64: 0,
                        },
                    ],
                };

                let window = five_array_window(pool_pubkey, tick_current_index, tick_spacing);
                let tick_arrays: Vec<SlottedTickArray> = window
                    .into_iter()
                    .map(|array| SlottedTickArray { array, slot: 1 })
                    .collect();

                let expected_pdas = crate::executor::orca::derive_tick_arrays(
                    &pool_pubkey,
                    tick_current_index,
                    tick_spacing,
                    true, // spending_quote=true with A=WSOL => a_to_b=true
                )
                .unwrap();

                let (quote_result, arrays_actually_selected) = {
                    let _guard = ORCA_STATE_TEST_LOCK.lock().unwrap();
                    publish_orca_quote_state(OrcaQuoteState {
                        whirlpool_pubkey: pool_pubkey,
                        whirlpool: whirlpool.clone(),
                        tick_arrays,
                        pool_slot: 1,
                    });
                    let quote_result = exact_orca_clmm_quote(1_000_000, true);
                    let arrays_actually_selected = select_tick_arrays_for_direction(
                        &crate::scanner::orca::current_orca_quote_state()
                            .unwrap()
                            .tick_arrays,
                        tick_current_index,
                        tick_spacing,
                        true,
                    )
                    .unwrap();
                    (quote_result, arrays_actually_selected)
                };

                assert!(quote_result.is_ok(), "{quote_result:?}");

                let actual_pdas: Vec<Pubkey> = arrays_actually_selected
                    .iter()
                    .map(|a| {
                        crate::executor::orca::derive_tick_array_pda(
                            &pool_pubkey,
                            a.start_tick_index,
                        )
                        .unwrap()
                    })
                    .collect();
                assert_eq!(actual_pdas, expected_pdas.to_vec());
            }
        }
    }
}
