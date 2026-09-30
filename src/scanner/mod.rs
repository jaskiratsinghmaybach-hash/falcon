pub mod orca;
pub mod raydium;
pub mod raydium_cpmm;
pub mod realtime;

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::sync::mpsc;

use crate::analyzer;
use crate::config::{CapitalSource, DecoderType};
use crate::executor;

#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub dex: String,
    pub pair: String,
    pub base_reserve_raw: u64,
    pub quote_reserve_raw: u64,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub fee_numerator: u64,
    pub fee_denominator: u64,
    pub price: f64,
    pub base_liquidity: f64,
    pub quote_liquidity: f64,
    pub fee_pct: f64,
    pub timestamp: std::time::SystemTime,

    /// Orca Whirlpool CLMM fields.
    ///
    /// `clmm_liquidity == 0` means constant-product pool.
    pub clmm_sqrt_price_q64: u128,
    pub clmm_liquidity: u128,
    pub clmm_is_a_wsol: bool,
}

pub struct ExecutionContext {
    pub client: RpcClient,
    pub payer: solana_sdk::signature::Keypair,
    pub raydium_ctx: executor::RaydiumPoolContext,
    pub orca_ctx: executor::OrcaPoolContext,
    pub other_token_mint: Pubkey,
    pub blockhash_cache: executor::BlockhashCache,
    pub ata_cache: executor::AtaCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountRole {
    RaydiumPoolState,
    RaydiumAmmConfig,
    RaydiumVault0,
    RaydiumVault1,
    OrcaPoolState,
    OrcaVaultA,
    OrcaVaultB,
    /// One of the 5 tick-array accounts in the current subscription
    /// window (see `orca::subscription_tick_array_pdas`), identified by
    /// its position in that fixed-order array (0..5), NOT by its on-chain
    /// start_tick_index - the window shifts as price moves, so the same
    /// index can refer to a different account over the life of the
    /// process. `role_generation` disambiguates a push against a since-
    /// superseded window from one that matches the current window.
    OrcaTickArray {
        index: usize,
        generation: u64,
    },
}

struct RealtimeEvent {
    role: AccountRole,
    update: realtime::AccountUpdate,
}

fn spawn_subscriber(
    ws_url: String,
    account_id: String,
    role: AccountRole,
    tx: mpsc::Sender<RealtimeEvent>,
) {
    std::thread::spawn(move || {
        let (inner_tx, inner_rx) = mpsc::channel();

        let ws_url_clone = ws_url.clone();
        let account_clone = account_id.clone();

        std::thread::spawn(move || {
            if let Err(e) = realtime::subscribe_to_account(&ws_url_clone, &account_clone, inner_tx)
            {
                tracing::error!("Subscription for {:?} ended: {}", role, e);
            }
        });

        for update in inner_rx {
            if tx.send(RealtimeEvent { role, update }).is_err() {
                break;
            }
        }
    });
}

fn publish_cpmm_snapshot(
    pool_id: Pubkey,
    pool_state: &Option<raydium_cpmm::PoolState>,
    amm_config: &Option<raydium_cpmm::AmmConfig>,
    vault_0_raw: u64,
    vault_1_raw: u64,
) {
    if let (Some(pool), Some(config)) = (pool_state.as_ref(), amm_config.as_ref()) {
        let state = raydium_cpmm::quote_state_from_accounts(
            pool_id,
            pool,
            config,
            vault_0_raw,
            vault_1_raw,
        );

        raydium_cpmm::publish_quote_state(state);

        tracing::debug!(
            "CPMM quote snapshot published: vault0={}, vault1={}, protocol0={}, protocol1={}, fund0={}, fund1={}, creator0={}, creator1={}",
            vault_0_raw,
            vault_1_raw,
            pool.protocol_fees_token_0,
            pool.protocol_fees_token_1,
            pool.fund_fees_token_0,
            pool.fund_fees_token_1,
            pool.creator_fees_token_0,
            pool.creator_fees_token_1
        );
    }
}

/// Publishes the exact-quote OrcaQuoteState (Phase 4 stage 3) whenever
/// either the pool state or any tick array in the subscription window has
/// been decoded. Mirrors publish_cpmm_snapshot's "publish whatever we have,
/// as soon as we have the pool state" shape, but tick_arrays is built from
/// whichever of the 5 subscribed slots have actually been decoded so far -
/// this can be fewer than 5 (or even 0) early in the process's life, before
/// every subscription has pushed its first update. exact_orca_clmm_quote
/// (analyzer::mod.rs) is responsible for treating too few arrays as a hard
/// quote failure, not this function - this function publishes whatever is
/// currently known, honestly, without waiting for full coverage.
///
/// `pool_slot` and each tick array's own slot come from the WebSocket
/// notification's real `context.slot` (threaded through via
/// `realtime::AccountUpdate::slot`) - not a placeholder - so
/// `OrcaQuoteState::coherence_status` downstream is checking real drift,
/// not a fiction.
fn publish_orca_exact_snapshot(
    whirlpool_pubkey: Option<Pubkey>,
    whirlpool: &Option<(orca::Whirlpool, u64)>,
    tick_arrays: &[Option<(orca::DecodedTickArray, u64)>; 5],
) {
    let (Some(pool_pubkey), Some((wp, pool_slot))) = (whirlpool_pubkey, whirlpool.as_ref()) else {
        return;
    };

    let loaded_arrays: Vec<orca::SlottedTickArray> = tick_arrays
        .iter()
        .filter_map(|a| a.clone())
        .map(|(array, slot)| orca::SlottedTickArray { array, slot })
        .collect();

    orca::publish_orca_quote_state(orca::OrcaQuoteState {
        whirlpool_pubkey: pool_pubkey,
        whirlpool: wp.clone(),
        tick_arrays: loaded_arrays.clone(),
        pool_slot: *pool_slot,
    });

    tracing::debug!(
        "Orca exact-quote snapshot published: tick_current_index={}, tick_spacing={}, tick_arrays_loaded={}/5, pool_slot={}",
        wp.tick_current_index,
        wp.tick_spacing,
        loaded_arrays.len(),
        pool_slot
    );
}

#[allow(clippy::too_many_arguments)]
pub async fn run_realtime_loop(
    ws_url: &str,
    rpc_url: &str,
    pair: &str,
    raydium_pool_id: &str,
    orca_pool_id: &str,
    decoder: DecoderType,
    _trade_size_hint: f64,
    max_price_age_secs: u64,
    capital_source: CapitalSource,
    exec_ctx: ExecutionContext,
) -> Result<()> {
    let client = RpcClient::new(rpc_url.to_string());
    let (tx, rx) = mpsc::channel::<RealtimeEvent>();

    let raydium_pool_pubkey: Pubkey = raydium_pool_id
        .parse()
        .expect("Invalid Raydium CPMM pool ID");

    let cpmm_ctx = match decoder {
        DecoderType::Cpmm => {
            match raydium_cpmm::CpmmStaticContext::load(&client, raydium_pool_id) {
                Ok(ctx) => {
                    tracing::info!("Pre-cached Raydium CPMM static pool metadata");
                    Some(ctx)
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to pre-cache CPMM metadata, falling back to dynamic fetch: {}",
                        e
                    );
                    None
                }
            }
        }
        DecoderType::Amm => None,
    };

    let orca_ctx = match orca::OrcaStaticContext::load(&client, orca_pool_id) {
        Ok(ctx) => {
            tracing::info!("Pre-cached Orca Whirlpool static pool metadata");
            Some(ctx)
        }
        Err(e) => {
            tracing::warn!(
                "Failed to pre-cache Orca metadata, falling back to dynamic fetch: {}",
                e
            );
            None
        }
    };

    spawn_subscriber(
        ws_url.to_string(),
        raydium_pool_id.to_string(),
        AccountRole::RaydiumPoolState,
        tx.clone(),
    );

    if let Some(ctx) = &cpmm_ctx {
        spawn_subscriber(
            ws_url.to_string(),
            ctx.amm_config.to_string(),
            AccountRole::RaydiumAmmConfig,
            tx.clone(),
        );

        spawn_subscriber(
            ws_url.to_string(),
            ctx.token_0_vault.to_string(),
            AccountRole::RaydiumVault0,
            tx.clone(),
        );

        spawn_subscriber(
            ws_url.to_string(),
            ctx.token_1_vault.to_string(),
            AccountRole::RaydiumVault1,
            tx.clone(),
        );
    }

    spawn_subscriber(
        ws_url.to_string(),
        orca_pool_id.to_string(),
        AccountRole::OrcaPoolState,
        tx.clone(),
    );

    if let Some(ctx) = &orca_ctx {
        spawn_subscriber(
            ws_url.to_string(),
            ctx.token_vault_a.to_string(),
            AccountRole::OrcaVaultA,
            tx.clone(),
        );

        spawn_subscriber(
            ws_url.to_string(),
            ctx.token_vault_b.to_string(),
            AccountRole::OrcaVaultB,
            tx.clone(),
        );
    }

    // Tick-array subscription window (Phase 4 stage 3): fetch the pool's
    // CURRENT tick_current_index/tick_spacing once up front, derive the
    // 5-array subscription window around it (orca::subscription_tick_array_pdas),
    // and subscribe to those 5 accounts.
    //
    // KNOWN LIMITATION, stated plainly rather than silently: this window is
    // fixed for the life of the process. It is NOT re-derived if price
    // later moves far enough to leave it (a Whirlpool position boundary
    // crossing into a 4th/5th array beyond what's subscribed). When that
    // happens, exact_orca_clmm_quote will correctly fail closed with
    // OrcaInsufficientTickCoverage or return quotes anchored to stale tick
    // data - Falcon will emit warnings (see the staleness check in the main
    // loop below) but will NOT automatically re-subscribe to a new window.
    // Live re-subscription requires an unsubscribe/cleanup path that
    // realtime::subscribe_to_account does not currently have (each
    // subscription thread runs for the life of the process with no
    // cancellation signal) - building that safely is separate follow-up
    // work, not folded into this stage.
    let mut orca_tick_window_bounds: Option<(i32, i32)> = None;
    let orca_tick_array_generation: u64 = 0;

    if let Ok(pool_pubkey) = orca_pool_id.parse::<Pubkey>() {
        if let Ok(account) = client.get_account(&pool_pubkey) {
            if let Ok(wp) = orca::decode_whirlpool(&account.data) {
                match orca::subscription_tick_array_pdas(
                    &pool_pubkey,
                    wp.tick_current_index,
                    wp.tick_spacing,
                ) {
                    Ok(window) => {
                        tracing::info!(
                            "Orca tick-array subscription window derived at tick {} (spacing {}): {:?}",
                            wp.tick_current_index,
                            wp.tick_spacing,
                            window
                        );
                        for (i, pda) in window.iter().enumerate() {
                            spawn_subscriber(
                                ws_url.to_string(),
                                pda.to_string(),
                                AccountRole::OrcaTickArray {
                                    index: i,
                                    generation: orca_tick_array_generation,
                                },
                                tx.clone(),
                            );
                        }

                        // Bounds of the ticks the 5-array window actually
                        // covers, used by the staleness check below to warn
                        // (not re-subscribe - see the KNOWN LIMITATION note
                        // above) once live price drifts outside what's
                        // subscribed.
                        let ticks_in_array =
                            crate::executor::orca::TICK_ARRAY_SIZE * wp.tick_spacing as i32;
                        let window_start = crate::executor::orca::tick_array_start_index(
                            wp.tick_current_index,
                            wp.tick_spacing,
                        );
                        orca_tick_window_bounds = Some((
                            window_start - 2 * ticks_in_array,
                            window_start + 3 * ticks_in_array - 1,
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to derive Orca tick-array subscription window: {} - exact Orca CLMM quoting will be unavailable",
                            e
                        );
                    }
                }
            }
        }
    }

    drop(tx);

    tracing::info!(
        "Real-time WebSocket subscriptions started for {} (Raydium mode: {:?})",
        pair,
        decoder
    );

    let mut last_raydium: Option<PriceUpdate> = None;
    let mut last_orca: Option<PriceUpdate> = None;

    let mut raydium_vault_0_raw: u64 = 0;
    let mut raydium_vault_1_raw: u64 = 0;

    let mut raydium_pool_state: Option<raydium_cpmm::PoolState> = None;
    let mut raydium_amm_config: Option<raydium_cpmm::AmmConfig> = None;

    let mut orca_vault_a_raw: u64 = 0;
    let mut orca_vault_b_raw: u64 = 0;
    let mut orca_sqrt_price: u128 = 0;
    let mut orca_liquidity: u128 = 0;
    let mut orca_fee_rate: u16 = 0;
    let mut orca_whirlpool_decoded: Option<(orca::Whirlpool, u64)> = None;
    let mut orca_tick_arrays: [Option<(orca::DecodedTickArray, u64)>; 5] = Default::default();
    let orca_pool_pubkey: Option<Pubkey> = orca_pool_id.parse().ok();

    match decoder {
        DecoderType::Cpmm => {
            if let Some(ctx) = &cpmm_ctx {
                let pool_pubkey: Pubkey = ctx.pool_id;

                if let Ok(account) = client.get_account(&pool_pubkey) {
                    match raydium_cpmm::decode_pool_state(&account.data) {
                        Ok(pool) => {
                            raydium_pool_state = Some(pool);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to decode initial Raydium CPMM PoolState: {}",
                                e
                            );
                        }
                    }
                }

                if let Ok(account) = client.get_account(&ctx.amm_config) {
                    match raydium_cpmm::decode_amm_config(&account.data) {
                        Ok(config) => {
                            raydium_amm_config = Some(config);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to decode initial Raydium CPMM AmmConfig: {}",
                                e
                            );
                        }
                    }
                }

                if let (Ok(b0), Ok(b1)) = (
                    client.get_token_account_balance(&ctx.token_0_vault),
                    client.get_token_account_balance(&ctx.token_1_vault),
                ) {
                    raydium_vault_0_raw = b0.amount.parse().unwrap_or(0);
                    raydium_vault_1_raw = b1.amount.parse().unwrap_or(0);

                    last_raydium = Some(ctx.price_from_raw_reserves(
                        raydium_vault_0_raw,
                        raydium_vault_1_raw,
                        pair,
                    ));

                    publish_cpmm_snapshot(
                        raydium_pool_pubkey,
                        &raydium_pool_state,
                        &raydium_amm_config,
                        raydium_vault_0_raw,
                        raydium_vault_1_raw,
                    );
                }
            } else if let Ok(p) = raydium_cpmm::fetch_price(&client, raydium_pool_id, pair) {
                last_raydium = Some(p);
            }
        }

        DecoderType::Amm => {
            if let Ok(p) = raydium::fetch_price(&client, raydium_pool_id, pair) {
                last_raydium = Some(p);
            }
        }
    }

    if let Some(ctx) = &orca_ctx {
        if let Ok(p) = ctx.fetch_price_with_context(&client, pair) {
            last_orca = Some(p);
        }

        if let (Ok(b_a), Ok(b_b)) = (
            client.get_token_account_balance(&ctx.token_vault_a),
            client.get_token_account_balance(&ctx.token_vault_b),
        ) {
            orca_vault_a_raw = b_a.amount.parse().unwrap_or(0);
            orca_vault_b_raw = b_b.amount.parse().unwrap_or(0);
        }

        if let Ok(pool_pubkey) = orca_pool_id.parse::<Pubkey>() {
            if let Ok(account) = client.get_account(&pool_pubkey) {
                if let Ok(wp) = orca::decode_whirlpool(&account.data) {
                    orca_sqrt_price = wp.sqrt_price;
                    orca_liquidity = wp.liquidity;
                    orca_fee_rate = wp.fee_rate;
                }
            }
        }
    } else if let Ok(p) = orca::fetch_price(&client, orca_pool_id, pair) {
        last_orca = Some(p);
    }

    // Highest `context.slot` seen on ANY subscribed account so far. Used by
    // the pre-simulation gate to judge how far behind the Orca snapshot is.
    let mut newest_observed_slot: u64 = 0;

    for event in rx {
        newest_observed_slot = newest_observed_slot.max(event.update.slot);

        tracing::info!(
            "WebSocket event received: {:?} ({} bytes)",
            event.role,
            event.update.data.len()
        );

        match event.role {
            AccountRole::RaydiumPoolState => {
                if decoder == DecoderType::Cpmm {
                    match raydium_cpmm::decode_pool_state(&event.update.data) {
                        Ok(pool) => {
                            raydium_pool_state = Some(pool);

                            publish_cpmm_snapshot(
                                raydium_pool_pubkey,
                                &raydium_pool_state,
                                &raydium_amm_config,
                                raydium_vault_0_raw,
                                raydium_vault_1_raw,
                            );
                        }

                        Err(e) => {
                            tracing::warn!("Failed to decode Raydium CPMM PoolState push: {}", e);
                        }
                    }
                }
            }

            AccountRole::RaydiumAmmConfig => {
                if decoder == DecoderType::Cpmm {
                    match raydium_cpmm::decode_amm_config(&event.update.data) {
                        Ok(config) => {
                            raydium_amm_config = Some(config);

                            publish_cpmm_snapshot(
                                raydium_pool_pubkey,
                                &raydium_pool_state,
                                &raydium_amm_config,
                                raydium_vault_0_raw,
                                raydium_vault_1_raw,
                            );
                        }

                        Err(e) => {
                            tracing::warn!("Failed to decode Raydium CPMM AmmConfig push: {}", e);
                        }
                    }
                }
            }

            AccountRole::RaydiumVault0 => {
                match realtime::decode_token_account_balance(&event.update.data) {
                    Ok(balance) => {
                        raydium_vault_0_raw = balance;

                        if let Some(ctx) = &cpmm_ctx {
                            last_raydium = Some(ctx.price_from_raw_reserves(
                                raydium_vault_0_raw,
                                raydium_vault_1_raw,
                                pair,
                            ));
                        }

                        publish_cpmm_snapshot(
                            raydium_pool_pubkey,
                            &raydium_pool_state,
                            &raydium_amm_config,
                            raydium_vault_0_raw,
                            raydium_vault_1_raw,
                        );
                    }

                    Err(e) => {
                        tracing::warn!("Failed to decode Raydium vault 0 push: {}", e);
                    }
                }
            }

            AccountRole::RaydiumVault1 => {
                match realtime::decode_token_account_balance(&event.update.data) {
                    Ok(balance) => {
                        raydium_vault_1_raw = balance;

                        if let Some(ctx) = &cpmm_ctx {
                            last_raydium = Some(ctx.price_from_raw_reserves(
                                raydium_vault_0_raw,
                                raydium_vault_1_raw,
                                pair,
                            ));
                        }

                        publish_cpmm_snapshot(
                            raydium_pool_pubkey,
                            &raydium_pool_state,
                            &raydium_amm_config,
                            raydium_vault_0_raw,
                            raydium_vault_1_raw,
                        );
                    }

                    Err(e) => {
                        tracing::warn!("Failed to decode Raydium vault 1 push: {}", e);
                    }
                }
            }

            AccountRole::OrcaPoolState => match orca::decode_whirlpool(&event.update.data) {
                Ok(whirlpool) => {
                    orca_sqrt_price = whirlpool.sqrt_price;
                    orca_liquidity = whirlpool.liquidity;
                    orca_fee_rate = whirlpool.fee_rate;

                    // Staleness check for the KNOWN LIMITATION documented
                    // where orca_tick_window_bounds is derived: the 5-array
                    // subscription window is fixed for the process's life
                    // and is never re-derived. If live price has drifted
                    // outside the bounds it covered at startup,
                    // exact_orca_clmm_quote will start failing closed
                    // (OrcaInsufficientTickCoverage) or quoting against
                    // stale tick data for any array still nominally in
                    // range but no longer adjacent to the live tick. Warn
                    // loudly rather than let that happen silently.
                    if let Some((lo, hi)) = orca_tick_window_bounds {
                        if whirlpool.tick_current_index < lo || whirlpool.tick_current_index > hi {
                            tracing::warn!(
                                "Orca current tick {} has drifted outside the subscribed tick-array window [{}, {}] - exact CLMM quoting for this pool is now unreliable (stale or insufficient tick-array coverage). Restart the process to re-derive the subscription window at the current price.",
                                whirlpool.tick_current_index,
                                lo,
                                hi
                            );
                        }
                    }

                    orca_whirlpool_decoded = Some((whirlpool.clone(), event.update.slot));

                    if let Some(ctx) = &orca_ctx {
                        last_orca = Some(ctx.price_from_whirlpool_and_reserves(
                            orca_sqrt_price,
                            orca_liquidity,
                            orca_fee_rate,
                            orca_vault_a_raw,
                            orca_vault_b_raw,
                            pair,
                        ));
                    }

                    publish_orca_exact_snapshot(
                        orca_pool_pubkey,
                        &orca_whirlpool_decoded,
                        &orca_tick_arrays,
                    );
                }

                Err(e) => {
                    tracing::warn!("Failed to decode Orca whirlpool push: {}", e);
                }
            },

            AccountRole::OrcaVaultA => {
                match realtime::decode_token_account_balance(&event.update.data) {
                    Ok(balance) => {
                        orca_vault_a_raw = balance;

                        if let Some(ctx) = &orca_ctx {
                            last_orca = Some(ctx.price_from_raw_reserves(
                                orca_sqrt_price,
                                orca_liquidity,
                                orca_fee_rate,
                                orca_vault_a_raw,
                                orca_vault_b_raw,
                                pair,
                            ));
                        }
                    }

                    Err(e) => {
                        tracing::warn!("Failed to decode Orca vault A push: {}", e);
                    }
                }
            }

            AccountRole::OrcaVaultB => {
                match realtime::decode_token_account_balance(&event.update.data) {
                    Ok(balance) => {
                        orca_vault_b_raw = balance;

                        if let Some(ctx) = &orca_ctx {
                            last_orca = Some(ctx.price_from_raw_reserves(
                                orca_sqrt_price,
                                orca_liquidity,
                                orca_fee_rate,
                                orca_vault_a_raw,
                                orca_vault_b_raw,
                                pair,
                            ));
                        }
                    }

                    Err(e) => {
                        tracing::warn!("Failed to decode Orca vault B push: {}", e);
                    }
                }
            }

            AccountRole::OrcaTickArray { index, generation } => {
                if generation != orca_tick_array_generation {
                    // Push arrived for a subscription window that has
                    // since been superseded (see the KNOWN LIMITATION note
                    // where the window is derived: today generation never
                    // actually advances past 0, so this branch is
                    // unreachable in practice, but the check is kept as a
                    // hard guard against ever silently blending two
                    // different windows' arrays into one OrcaQuoteState if
                    // dynamic re-subscription is added later without
                    // updating this check).
                    tracing::warn!(
                        "Ignoring Orca tick-array push for stale subscription generation {} (current: {})",
                        generation,
                        orca_tick_array_generation
                    );
                } else {
                    match orca::decode_tick_array(&event.update.data) {
                        Ok(decoded) => {
                            let expected_whirlpool = orca_pool_pubkey;
                            let whirlpool_ok = expected_whirlpool
                                .map(|p| p == decoded.whirlpool)
                                .unwrap_or(false);

                            if !whirlpool_ok {
                                tracing::warn!(
                                    "Orca tick-array push at window slot {} belongs to whirlpool {}, expected {:?} - discarding",
                                    index,
                                    decoded.whirlpool,
                                    expected_whirlpool
                                );
                            } else if let Some((wp, _pool_slot)) = &orca_whirlpool_decoded {
                                if let Err(e) = orca::validate_tick_array(
                                    &decoded,
                                    &decoded.whirlpool,
                                    wp.tick_spacing,
                                ) {
                                    tracing::warn!(
                                        "Orca tick-array push at window slot {} failed validation: {}",
                                        index,
                                        e
                                    );
                                } else {
                                    orca_tick_arrays[index] = Some((decoded, event.update.slot));
                                    publish_orca_exact_snapshot(
                                        orca_pool_pubkey,
                                        &orca_whirlpool_decoded,
                                        &orca_tick_arrays,
                                    );
                                }
                            } else {
                                // Pool state not decoded yet (e.g. a tick
                                // array push arrived before the first
                                // whirlpool push) - store it unvalidated-
                                // against-tick-spacing for now; it will be
                                // covered by publish_orca_exact_snapshot's
                                // next call once the whirlpool push does
                                // arrive, and by then any genuinely wrong
                                // array would already have been caught by
                                // the whirlpool-ownership check above.
                                orca_tick_arrays[index] = Some((decoded, event.update.slot));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to decode Orca tick-array push at window slot {}: {}",
                                index,
                                e
                            );
                        }
                    }
                }
            }
        }

        if let (Some(r), Some(o)) = (&last_raydium, &last_orca) {
            match analyzer::find_best_opportunity_with_staleness(
                r,
                o,
                max_price_age_secs,
                capital_source,
            ) {
                Ok(opp) => {
                    tracing::warn!(
                        "REAL-TIME OPPORTUNITY: buy on {} @ {:.10}, sell on {} @ {:.10}, NET PROFIT: {:.4}%",
                        opp.buy_dex,
                        opp.buy_price,
                        opp.sell_dex,
                        opp.sell_price,
                        opp.net_profit_pct
                    );

                    let _ = crate::logger::log_row(
                        pair,
                        r.price,
                        o.price,
                        r.base_liquidity,
                        r.quote_liquidity,
                        o.base_liquidity,
                        o.quote_liquidity,
                        &opp.buy_dex,
                        &opp.sell_dex,
                        opp.raw_spread_pct,
                        opp.fee_adjusted_spread_pct,
                        opp.net_spread_after_slippage_pct,
                        opp.net_profit_pct,
                        "APPROVED_REALTIME",
                    );

                    let cached_hash = exec_ctx.blockhash_cache.read().ok().map(|guard| *guard);

                    // Pre-simulation revalidation (Phase 4). The opportunity
                    // was priced from a snapshot; before spending a
                    // simulation on it, confirm that snapshot is still
                    // coherent, fresh, and covers exactly the tick arrays
                    // the executor will submit. Any failure discards the
                    // opportunity - it is never "repaired" here, it must be
                    // re-quoted from fresh state on a later push.
                    let orca_is_buy_leg = opp.buy_dex == "Orca";
                    let revalidation = match orca::current_orca_quote_state() {
                        Some(state) => analyzer::revalidate_orca_snapshot(
                            &state,
                            analyzer::orca_leg_a_to_b(&state, orca_is_buy_leg),
                            newest_observed_slot,
                        ),
                        None => Err(analyzer::RevalidationError::NoSnapshot),
                    };

                    if let Err(reason) = revalidation {
                        let class = crate::failure::Classify::failure_class(&reason);
                        tracing::warn!(
                            failure_class = %class,
                            "Discarding opportunity before simulation - pre-simulation revalidation failed: {}",
                            reason
                        );
                        let _ = crate::logger::log_row(
                            pair,
                            r.price,
                            o.price,
                            r.base_liquidity,
                            r.quote_liquidity,
                            o.base_liquidity,
                            o.quote_liquidity,
                            &opp.buy_dex,
                            &opp.sell_dex,
                            opp.raw_spread_pct,
                            opp.fee_adjusted_spread_pct,
                            opp.net_spread_after_slippage_pct,
                            opp.net_profit_pct,
                            &format!("DISCARDED_REVALIDATION: {}", reason),
                        );
                        continue;
                    }

                    match executor::simulate_opportunity(
                        &exec_ctx.client,
                        &exec_ctx.payer,
                        &opp,
                        &exec_ctx.raydium_ctx,
                        &exec_ctx.orca_ctx,
                        &exec_ctx.other_token_mint,
                        0,
                        cached_hash,
                        &exec_ctx.ata_cache,
                    ) {
                        Ok(()) => {
                            tracing::warn!(
                                "Opportunity simulated successfully - see logs above for sim result."
                            );
                        }

                        Err(e) => {
                            let class = crate::failure::classify_error_chain(&e);
                            tracing::error!(
                                failure_class = %class,
                                "Failed to simulate opportunity: {:?}",
                                e
                            );
                            let _ = crate::logger::log_row(
                                pair,
                                r.price,
                                o.price,
                                r.base_liquidity,
                                r.quote_liquidity,
                                o.base_liquidity,
                                o.quote_liquidity,
                                &opp.buy_dex,
                                &opp.sell_dex,
                                opp.raw_spread_pct,
                                opp.fee_adjusted_spread_pct,
                                opp.net_spread_after_slippage_pct,
                                opp.net_profit_pct,
                                &format!("SIMULATION_FAILED[{}]: {}", class, e),
                            );
                        }
                    }
                }

                Err(reason) => {
                    tracing::info!(
                        "[{}] Opportunity check: {:?} | {} price: {:.10}, {} price: {:.10}",
                        pair,
                        reason,
                        r.dex,
                        r.price,
                        o.dex,
                        o.price
                    );

                    let (buy_dex, sell_dex) = if r.price < o.price {
                        (r.dex.as_str(), o.dex.as_str())
                    } else {
                        (o.dex.as_str(), r.dex.as_str())
                    };

                    let raw_spread_pct = ((r.price.max(o.price) - r.price.min(o.price))
                        / r.price.min(o.price))
                        * 100.0;

                    let _ = crate::logger::log_row(
                        pair,
                        r.price,
                        o.price,
                        r.base_liquidity,
                        r.quote_liquidity,
                        o.base_liquidity,
                        o.quote_liquidity,
                        buy_dex,
                        sell_dex,
                        raw_spread_pct,
                        0.0,
                        0.0,
                        reason.as_display_pct(),
                        &format!("{:?}", reason),
                    );
                }
            }
        }
    }

    Ok(())
}
