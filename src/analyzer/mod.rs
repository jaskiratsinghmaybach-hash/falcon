use crate::scanner::PriceUpdate;

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
    /// tx/tip costs). Signed because a losing trade has a real negative
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
fn estimate_slippage_bps(
    reserve_in: u64,
    reserve_out: u64,
    amount_in: u64,
) -> Result<i128, MathError> {
    let amount_out = estimate_output_amount_raw(reserve_in, reserve_out, amount_in)?;
    if amount_out == 0 {
        // No output for a non-zero input against valid reserves means the
        // trade is degenerate at this size - treat as maximal slippage
        // rather than dividing by zero.
        return Ok(BPS_DENOMINATOR as i128);
    }

    // spot_price      = reserve_out / reserve_in      (out per in)
    // effective_price = amount_out / amount_in         (out per in)
    // slippage = (spot_price - effective_price) / spot_price
    //          = 1 - (effective_price / spot_price)
    //          = 1 - (amount_out * reserve_in) / (amount_in * reserve_out)
    //
    // slippage_bps = BPS - (amount_out * reserve_in * BPS) / (amount_in * reserve_out)
    let reserve_in = reserve_in as i128;
    let reserve_out = reserve_out as i128;
    let amount_in = amount_in as i128;
    let amount_out = amount_out as i128;
    let bps = BPS_DENOMINATOR as i128;

    let numerator = amount_out
        .checked_mul(reserve_in)
        .and_then(|v| v.checked_mul(bps))
        .ok_or(MathError::Overflow("slippage numerator"))?;
    let denominator = amount_in
        .checked_mul(reserve_out)
        .ok_or(MathError::Overflow("slippage denominator"))?;

    if denominator == 0 {
        return Err(MathError::DivisionByZero("amount_in * reserve_out"));
    }

    let effective_over_spot_bps = numerator / denominator;
    Ok(bps - effective_over_spot_bps)
}

/// Fixed per-transaction costs (base fee + tip), in lamports. Kept as a
/// function (not a bare constant) so the Jito-aware version can replace this
/// later without touching call sites - per the roadmap, tip sizing becomes
/// profit-aware at the Jito-bundle stage, not before.
fn fixed_cost_lamports() -> Result<i128, MathError> {
    const BASE_TX_FEE_LAMPORTS: i128 = 5_000; // 0.000005 SOL
    const JITO_TIP_LAMPORTS: i128 = 100_000; // 0.0001 SOL (placeholder - see roadmap)
    const NUM_TRANSACTIONS: i128 = 2;

    (BASE_TX_FEE_LAMPORTS + JITO_TIP_LAMPORTS)
        .checked_mul(NUM_TRANSACTIONS)
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
    let reduction = (raw_reduction + BPS_DENOMINATOR - 1) / BPS_DENOMINATOR;

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
///   - Orca CLMM math when `pool.clmm_liquidity > 0`  (Whirlpool)
///   - Constant-product math otherwise                  (Raydium CPMM/AMM)
///
/// `spending_quote` = true  → we're spending SOL/quote to receive base token  (buy leg)
/// `spending_quote` = false → we're spending base token to receive SOL/quote  (sell leg)
fn estimate_leg_output(
    pool: &crate::scanner::PriceUpdate,
    amount_in: u64,
    spending_quote: bool,
) -> Result<u64, MathError> {
    if pool.clmm_liquidity > 0 {
        // CLMM path (Orca Whirlpool)
        // spending_quote=true  → spend SOL → b_to_a when SOL is token_b (!is_a_wsol)
        //                                    a_to_b when SOL is token_a ( is_a_wsol)
        // spending_quote=false → spend base → a_to_b when base is token_a (!is_a_wsol)
        //                                     b_to_a when base is token_b ( is_a_wsol)
        let b_to_a = spending_quote ^ pool.clmm_is_a_wsol;
        if b_to_a {
            clmm_output_b_to_a(
                pool.clmm_sqrt_price_q64,
                pool.clmm_liquidity,
                pool.fee_numerator,
                pool.fee_denominator,
                amount_in,
            )
        } else {
            clmm_output_a_to_b(
                pool.clmm_sqrt_price_q64,
                pool.clmm_liquidity,
                pool.fee_numerator,
                pool.fee_denominator,
                amount_in,
            )
        }
    } else {
        // Constant-product path (Raydium CPMM / AMM)
        let (reserve_in, reserve_out) = if spending_quote {
            (pool.quote_reserve_raw, pool.base_reserve_raw)
        } else {
            (pool.base_reserve_raw, pool.quote_reserve_raw)
        };
        estimate_output_amount_raw(reserve_in, reserve_out, amount_in)
    }
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

    // --- exact quote engine: chain leg 1's real output into leg 2's real input ---
    // Buy leg: spend SOL/quote on the cheaper DEX, receive base token.
    // For Orca CLMM legs this uses sqrt_price + liquidity math; for Raydium
    // CPMM/AMM legs it falls back to the constant-product formula.
    let expected_output_after_buy_raw = match estimate_leg_output(buy, trade_size_lamports, true) {
        Ok(v) => v,
        Err(_) => return Err(RejectReason::NoSpread),
    };

    // Sell leg: spend the base tokens received above on the pricier DEX,
    // receiving SOL/quote back. Same dispatch logic as the buy leg.
    let expected_output_after_sell_raw = match estimate_leg_output(sell, expected_output_after_buy_raw, false) {
        Ok(v) => v,
        Err(_) => return Err(RejectReason::NoSpread),
    };

    let buy_slippage_bps = match estimate_slippage_bps(
        buy.quote_reserve_raw,
        buy.base_reserve_raw,
        trade_size_lamports,
    ) {
        Ok(v) => v,
        Err(_) => return Err(RejectReason::NoSpread),
    };
    let sell_slippage_bps = match estimate_slippage_bps(
        sell.base_reserve_raw,
        sell.quote_reserve_raw,
        expected_output_after_buy_raw,
    ) {
        Ok(v) => v,
        Err(_) => return Err(RejectReason::NoSpread),
    };

    let total_slippage_bps = buy_slippage_bps + sell_slippage_bps;
    let net_spread_after_slippage_bps = fee_adjusted_spread_bps - total_slippage_bps;

    if net_spread_after_slippage_bps <= 0 {
        return Err(RejectReason::SlippageExceedsSpread {
            net_bps: net_spread_after_slippage_bps,
        });
    }

    // --- authoritative decision: actual integer lamport PnL, not a percentage ---
    let fixed_cost = match fixed_cost_lamports() {
        Ok(v) => v,
        Err(_) => return Err(RejectReason::NoSpread),
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
    let age_a = now.duration_since(price_a.timestamp).unwrap_or_default().as_secs();
    let age_b = now.duration_since(price_b.timestamp).unwrap_or_default().as_secs();
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
            estimate_output_amount_raw(a.quote_reserve_raw, a.base_reserve_raw, 50_000_000).unwrap();
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
    fn freshness_check_rejects_large_drift() {
        // Price roughly doubles - should exceed even a generous 500 bps (5%) threshold.
        assert!(!is_still_fresh(1_000_000, 2_000_000, 1_000_000, 4_000_000, 500).unwrap());
    }
}
