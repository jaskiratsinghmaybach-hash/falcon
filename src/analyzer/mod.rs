use crate::scanner::PriceUpdate;

#[derive(Debug, Clone)]
pub struct Opportunity {
    pub buy_dex: String,
    pub sell_dex: String,
    pub buy_price: f64,
    pub sell_price: f64,
    pub raw_spread_pct: f64,
    pub fee_adjusted_spread_pct: f64,
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
}

pub fn find_opportunity(
    price_a: &PriceUpdate,
    price_b: &PriceUpdate,
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

    // Subtract both DEXs' swap fees from the raw spread - you pay a fee to buy AND to sell.
    let total_fee_pct = buy.fee_pct + sell.fee_pct;
    let fee_adjusted_spread_pct = raw_spread_pct - total_fee_pct;

    if fee_adjusted_spread_pct <= 0.0 {
        return Err(RejectReason::FeesExceedSpread {
            fee_adjusted_pct: fee_adjusted_spread_pct,
        });
    }

    Ok(Opportunity {
        buy_dex: buy.dex.clone(),
        sell_dex: sell.dex.clone(),
        buy_price: buy.price,
        sell_price: sell.price,
        raw_spread_pct,
        fee_adjusted_spread_pct,
        pair: price_a.pair.clone(),
    })
}
