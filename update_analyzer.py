path = /home/jaskirat_singh/falcon/src/analyzer/mod.rs
content = open(path).read()

old_code = "pub fn find_opportunity(
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
        - BPS_DENOMINATOR as i128;"

new_code = "/// Returns the exact spot price ratio  for a pool.
///
/// For CPMM / AMM pools (Raydium), spot price = quote_reserve_raw / base_reserve_raw
/// (lamports of SOL per raw base token).
///
/// For Orca Whirlpool (CLMM), spot price is derived from 
/// (Q64.64 fixed point) where .
/// - If  is true: token A is WSOL (quote), token B is base token.
///   Then raw_quote / raw_base = raw_a / raw_b = 2^128 / (sqrt_price^2).
/// - If  is false: token A is base token, token B is WSOL (quote).
///   Then raw_quote / raw_base = raw_b / raw_a = (sqrt_price^2) / 2^128.
///
/// Both pools trade the exact same pair (with identical token mints and decimals),
/// so  shares the exact same unit (raw lamports per raw base token).
pub fn spot_price_ratio(price: &PriceUpdate) -> Result<(ethnum::U256, ethnum::U256), RejectReason> {
    if price.clmm_sqrt_price_q64 > 0 {
        let sp = ethnum::U256::from(price.clmm_sqrt_price_q64);
        let sp_sq = sp * sp;
        let two_128 = ethnum::U256::from(1u128) << 128;
        if sp_sq == 0 {
            return Err(RejectReason::NoSpread);
        }
        if price.clmm_is_a_wsol {
            Ok((two_128, sp_sq))
        } else {
            Ok((sp_sq, two_128))
        }
    } else {
        if price.base_reserve_raw == 0 || price.quote_reserve_raw == 0 {
            return Err(RejectReason::NoSpread);
        }
        Ok((
            ethnum::U256::from(price.quote_reserve_raw),
            ethnum::U256::from(price.base_reserve_raw),
        ))
    }
}

pub fn find_opportunity(
    price_a: &PriceUpdate,
    price_b: &PriceUpdate,
    trade_size_lamports: u64,
) -> Result<Opportunity, RejectReason> {
    // Orientation: decide which side is cheaper using spot price components via
    // exact cross-multiplication, never a naive vault balance ratio for CLMM.
    let (quote_a, base_a) = spot_price_ratio(price_a)?;
    let (quote_b, base_b) = spot_price_ratio(price_b)?;

    let cross_a = quote_a * base_b;
    let cross_b = quote_b * base_a;

    if cross_a == cross_b {
        return Err(RejectReason::NoSpread);
    }

    let (buy, sell, buy_quote, buy_base, sell_quote, sell_base) = if cross_a < cross_b {
        (price_a, price_b, quote_a, base_a, quote_b, base_b)
    } else {
        (price_b, price_a, quote_b, base_b, quote_a, base_a)
    };

    // raw_spread_bps = (sell_price - buy_price) / buy_price, in bps
    let sell_over_buy_num = sell_quote * buy_base;
    let sell_over_buy_den = sell_base * buy_quote;

    if sell_over_buy_den == 0 {
        return Err(RejectReason::NoSpread);
    }

    let raw_spread_bps: i128 = ((sell_over_buy_num * ethnum::U256::from(BPS_DENOMINATOR)
        / sell_over_buy_den)
        - ethnum::U256::from(BPS_DENOMINATOR))
    .as_i128();"

assert old_code in content, old_code
