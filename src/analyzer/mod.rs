use crate::scanner::PriceUpdate;

#[derive(Debug, Clone)]
pub struct Opportunity {
    pub buy_dex: String,
    pub sell_dex: String,
    pub buy_price: f64,
    pub sell_price: f64,
    pub raw_spread_pct: f64,
    pub fee_adjusted_spread_pct: f64,
    pub net_spread_after_slippage_pct: f64,
    pub net_profit_pct: f64,
    pub trade_size_base: f64,
    pub expected_output_after_buy: f64,
    pub pair: String,
}

#[derive(Debug)]
pub enum RejectReason {
    NoSpread,
    SpreadTooSmall {
        spread_pct: f64,
        min_required_pct: f64,
    },
    FeesExceedSpread {
        fee_adjusted_pct: f64,
    },
    SlippageExceedsSpread {
        net_pct: f64,
    },
    NetProfitNotPositive {
        net_profit_pct: f64,
    },
}

/// How many base tokens you actually receive for spending `input_quote` into a
/// constant-product pool with the given reserves.
pub fn estimate_output_amount(base_reserve: f64, quote_reserve: f64, input_quote: f64) -> f64 {
    let k = base_reserve * quote_reserve;
    let new_quote_reserve = quote_reserve + input_quote;
    let new_base_reserve = k / new_quote_reserve;
    base_reserve - new_base_reserve
}

fn estimate_slippage_pct(base_reserve: f64, quote_reserve: f64, trade_size_quote: f64) -> f64 {
    let base_received = estimate_output_amount(base_reserve, quote_reserve, trade_size_quote);
    let spot_price = quote_reserve / base_reserve;
    let effective_price = trade_size_quote / base_received;
    ((effective_price - spot_price) / spot_price) * 100.0
}

fn estimate_cost_pct(trade_size_base: f64, buy_price_sol: f64) -> f64 {
    const BASE_TX_FEE_SOL: f64 = 0.000005;
    const JITO_TIP_SOL: f64 = 0.0001;
    const NUM_TRANSACTIONS: f64 = 2.0;

    let total_fixed_cost_sol = (BASE_TX_FEE_SOL + JITO_TIP_SOL) * NUM_TRANSACTIONS;
    let trade_value_sol = trade_size_base * buy_price_sol;

    (total_fixed_cost_sol / trade_value_sol) * 100.0
}

/// Picks a trade size that's reasonable relative to the thinner pool's depth,
/// so we're testing a realistic trade rather than an impossible one.
/// This is NOT the flashloan optimal-sizing logic (that's a separate, more
/// rigorous calculation for later) - just a sane default for pre-flashloan testing.
pub fn safe_trade_size(pool_a_base_liquidity: f64, pool_b_base_liquidity: f64) -> f64 {
    const SAFETY_FRACTION: f64 = 0.01; // 1% of the thinner pool's reserves

    let thinner_pool_liquidity = pool_a_base_liquidity.min(pool_b_base_liquidity);
    thinner_pool_liquidity * SAFETY_FRACTION
}

pub fn find_opportunity(
    price_a: &PriceUpdate,
    price_b: &PriceUpdate,
    trade_size_base: f64,
) -> Result<Opportunity, RejectReason> {
    if price_a.price == price_b.price {
        return Err(RejectReason::NoSpread);
    }

    let (buy, sell) = if price_a.price < price_b.price {
        (price_a, price_b)
    } else {
        (price_b, price_a)
    };

    let raw_spread_pct = ((sell.price - buy.price) / buy.price) * 100.0;

    const MIN_RAW_SPREAD_PCT: f64 = 0.01;

    if raw_spread_pct < MIN_RAW_SPREAD_PCT {
        return Err(RejectReason::SpreadTooSmall {
            spread_pct: raw_spread_pct,
            min_required_pct: MIN_RAW_SPREAD_PCT,
        });
    }

    let total_fee_pct = buy.fee_pct + sell.fee_pct;
    let fee_adjusted_spread_pct = raw_spread_pct - total_fee_pct;

    if fee_adjusted_spread_pct <= 0.0 {
        return Err(RejectReason::FeesExceedSpread {
            fee_adjusted_pct: fee_adjusted_spread_pct,
        });
    }

    let buy_trade_size_quote = trade_size_base * buy.price;
    let sell_trade_size_quote = trade_size_base * sell.price;

    let expected_output_after_buy = estimate_output_amount(
        buy.base_liquidity,
        buy.quote_liquidity,
        buy_trade_size_quote,
    );

    let buy_slippage_pct = estimate_slippage_pct(
        buy.base_liquidity,
        buy.quote_liquidity,
        buy_trade_size_quote,
    );
    let sell_slippage_pct = estimate_slippage_pct(
        sell.base_liquidity,
        sell.quote_liquidity,
        sell_trade_size_quote,
    );

    let total_slippage_pct = buy_slippage_pct + sell_slippage_pct;
    let net_spread_after_slippage_pct = fee_adjusted_spread_pct - total_slippage_pct;

    if net_spread_after_slippage_pct <= 0.0 {
        return Err(RejectReason::SlippageExceedsSpread {
            net_pct: net_spread_after_slippage_pct,
        });
    }

    let cost_pct = estimate_cost_pct(trade_size_base, buy.price);
    let net_profit_pct = net_spread_after_slippage_pct - cost_pct;

    if net_profit_pct <= 0.0 {
        return Err(RejectReason::NetProfitNotPositive { net_profit_pct });
    }

    Ok(Opportunity {
        buy_dex: buy.dex.clone(),
        sell_dex: sell.dex.clone(),
        buy_price: buy.price,
        sell_price: sell.price,
        raw_spread_pct,
        fee_adjusted_spread_pct,
        net_spread_after_slippage_pct,
        net_profit_pct,
        trade_size_base,
        expected_output_after_buy,
        pair: price_a.pair.clone(),
    })
}

/// Tries several trade sizes (as fractions of the thinner pool's reserves) and
/// returns whichever produces the best outcome - either the highest-profit
/// approved Opportunity, or if none are profitable, the least-bad rejection
/// (so we can still see how close we got).
pub fn find_best_opportunity(
    price_a: &PriceUpdate,
    price_b: &PriceUpdate,
) -> Result<Opportunity, RejectReason> {
    const SIZE_FRACTIONS: [f64; 6] = [0.001, 0.005, 0.01, 0.02, 0.05, 0.10];

    let thinner_pool_liquidity = price_a.base_liquidity.min(price_b.base_liquidity);

    let mut best_opportunity: Option<Opportunity> = None;
    let mut least_bad_rejection: Option<RejectReason> = None;
    let mut least_bad_net_pct = f64::NEG_INFINITY;

    for fraction in SIZE_FRACTIONS {
        let trade_size = thinner_pool_liquidity * fraction;

        match find_opportunity(price_a, price_b, trade_size) {
            Ok(opp) => {
                let is_better = match &best_opportunity {
                    Some(current_best) => opp.net_profit_pct > current_best.net_profit_pct,
                    None => true,
                };
                if is_better {
                    best_opportunity = Some(opp);
                }
            }
            Err(reason) => {
                let net_pct = match &reason {
                    RejectReason::SlippageExceedsSpread { net_pct } => *net_pct,
                    RejectReason::NetProfitNotPositive { net_profit_pct } => *net_profit_pct,
                    RejectReason::FeesExceedSpread { fee_adjusted_pct } => *fee_adjusted_pct,
                    _ => f64::NEG_INFINITY,
                };
                if best_opportunity.is_none() && net_pct > least_bad_net_pct {
                    least_bad_net_pct = net_pct;
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
