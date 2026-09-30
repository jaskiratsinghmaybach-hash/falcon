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
    /// its Pubkey and subscription generation.
    OrcaTickArray {
        pda: Pubkey,
        generation: u64,
    },
}

struct RealtimeEvent {
    role: AccountRole,
    update: realtime::AccountUpdate,
    /// True when this event came from the RPC snapshot poller rather than a
    /// WebSocket push.
    via_poll: bool,
}

/// Sentinel generation for polled tick-array events: "tag me with whatever
/// the window's generation is at the moment I'm processed", so a window
/// shift triggered by the pool event just before it cannot make the whole
/// polled batch look stale.
const POLLED_GENERATION: u64 = u64::MAX;

/// How often the Orca snapshot is force-refreshed over RPC. The pool and all
/// window tick arrays are fetched in ONE getMultipleAccounts call, so they
/// share a single context slot (perfectly coherent). Keep this well under
/// ~2s: analyzer::MAX_SNAPSHOT_LAG_SLOTS / MAX_COHERENT_SLOT_DRIFT are 5 slots.
fn orca_poll_interval() -> std::time::Duration {
    let ms = std::env::var("ORCA_POLL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(750)
        .clamp(200, 1500);
    std::time::Duration::from_millis(ms)
}

/// Fetches pool + both vaults + every tick array in the current window in one
/// RPC call and turns them into synthetic events, so they flow through the
/// exact same handling code as WebSocket pushes.
fn poll_orca_events(
    client: &RpcClient,
    pool_pubkey: Pubkey,
    ctx: &orca::OrcaStaticContext,
    window: &orca::DynamicTickWindow,
) -> Result<Vec<RealtimeEvent>> {
    use solana_sdk::commitment_config::CommitmentConfig;

    let mut keys: Vec<Pubkey> = vec![pool_pubkey, ctx.token_vault_a, ctx.token_vault_b];
    keys.extend(window.window_pdas.iter().copied());

    let response =
        client.get_multiple_accounts_with_commitment(&keys, CommitmentConfig::confirmed())?;
    let slot = response.context.slot;

    let mut events = Vec::with_capacity(keys.len());
    for (i, maybe_account) in response.value.into_iter().enumerate() {
        let Some(account) = maybe_account else {
            // A tick array that does not exist on-chain is legitimate (the
            // range is uninitialised); the pool / vaults must exist.
            if i < 3 {
                anyhow::bail!("Orca poll: account {} not found", keys[i]);
            }
            continue;
        };
        let role = match i {
            0 => AccountRole::OrcaPoolState,
            1 => AccountRole::OrcaVaultA,
            2 => AccountRole::OrcaVaultB,
            _ => AccountRole::OrcaTickArray {
                pda: keys[i],
                generation: POLLED_GENERATION,
            },
        };
        events.push(RealtimeEvent {
            role,
            update: realtime::AccountUpdate {
                data: account.data,
                owner: account.owner,
                slot,
            },
            via_poll: true,
        });
    }
    Ok(events)
}

fn spawn_cancellable_tick_subscriber(
    ws_url: String,
    account_id: String,
    pda: Pubkey,
    generation: u64,
    tx: mpsc::Sender<RealtimeEvent>,
) -> realtime::CancellableAccountSubscription {
    let (inner_tx, inner_rx) = mpsc::channel();
    let sub = realtime::spawn_cancellable_account_subscription(ws_url, account_id, inner_tx);
    std::thread::spawn(move || {
        let role = AccountRole::OrcaTickArray { pda, generation };
        for update in inner_rx {
            if tx.send(RealtimeEvent { role, update, via_poll: false }).is_err() {
                break;
            }
        }
    });
    sub
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
            if tx.send(RealtimeEvent { role, update, via_poll: false }).is_err() {
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

/// FORCE_SIM=1 dry-run: when the market offers no profitable opportunity
/// (the normal case), build the cheaper->dearer round trip anyway at a tiny
/// size and run it through `simulateTransaction`. This proves the whole
/// transaction path against mainnet state without spending anything, and
/// without pretending the trade is profitable. Nothing here ever broadcasts.
fn run_forced_simulation(
    r: &PriceUpdate,
    o: &PriceUpdate,
    capital_source: CapitalSource,
    exec_ctx: &ExecutionContext,
    newest_observed_slot: u64,
) {
    let requested: u64 = std::env::var("FORCE_SIM_LAMPORTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let size = match capital_source {
        CapitalSource::Wallet { max_lamports } => requested.min(max_lamports),
        CapitalSource::Pool => requested,
    };

    let opp = match analyzer::build_forced_opportunity(r, o, size) {
        Ok(opp) => opp,
        Err(reason) => {
            tracing::warn!("[FORCE_SIM] cannot build dry-run opportunity: {:?}", reason);
            return;
        }
    };

    let state = orca::current_orca_quote_state();
    let orca_is_buy_leg = opp.buy_dex == "Orca";
    let revalidation = match &state {
        Some(st) => analyzer::revalidate_orca_snapshot(
            st,
            analyzer::orca_leg_a_to_b(st, orca_is_buy_leg),
            newest_observed_slot,
        ),
        None => Err(analyzer::RevalidationError::NoSnapshot),
    };
    if let Err(reason) = revalidation {
        tracing::warn!("[FORCE_SIM] skipped - Orca snapshot not usable: {}", reason);
        return;
    }

    tracing::warn!(
        "[FORCE_SIM] DRY RUN (nothing is sent): buy on {} with {} lamports, sell on {}; expected net {} lamports (unprofitable is expected)",
        opp.buy_dex,
        opp.trade_size_lamports,
        opp.sell_dex,
        opp.net_profit_lamports
    );

    let cached_hash = exec_ctx.blockhash_cache.read().ok().map(|guard| *guard);
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
        state.as_ref(),
        true,
    ) {
        Ok(()) => tracing::warn!("[FORCE_SIM] SIMULATION SUCCESS on mainnet state - no funds moved"),
        Err(e) => {
            let class = crate::failure::classify_error_chain(&e);
            tracing::error!(failure_class = %class, "[FORCE_SIM] simulation failed: {:?}", e);
        }
    }
}

/// Which WebSocket key slot (0-based, into `ws_endpoints`) a given
/// tick-array PDA should use. Deterministic by PDA bytes rather than by
/// loop position, so a PDA keeps the same key across a window shift (its
/// subscription is cancelled and a fresh one opened, but always against
/// the same key) instead of key assignment depending on which iteration
/// of which loop happened to (re)subscribe it - which could otherwise pile
/// more than 2-3 tick arrays onto one key over time and blow past its
/// connection cap. Spreads across TICK_ARRAY_KEY_SLOTS below.
fn tick_array_key_slot(pda: &Pubkey) -> usize {
    TICK_ARRAY_KEY_SLOTS[(pda.to_bytes()[0] as usize) % TICK_ARRAY_KEY_SLOTS.len()]
}

/// Target key slot for each of the 5 window tick arrays, matching the
/// 3-key Free-tier split documented on `ws_endpoint_for`: 3 land on key 1
/// (alongside the 2 Orca vaults, 5 total), 2 land on key 2. Assignment is
/// probabilistic (by PDA byte, not a guaranteed round-robin over the 5
/// actual window PDAs), so with only 5 PDAs the real split could occasionally
/// skew - acceptable since even a 4/1 skew (4 on key 1, 1 on key 2) still
/// fits every key's 5-connection cap alongside what else is assigned to it.
const TICK_ARRAY_KEY_SLOTS: [usize; 5] = [1, 1, 1, 2, 2];

/// Which of `ws_endpoints` a given account role's WebSocket subscription
/// should use, as a fixed index (wrapped with `% ws_endpoints.len()` at the
/// call site so this still works, just over that key's connection cap
/// instead of erroring, if fewer endpoints are configured than the ideal
/// 3-key split below assumes).
///
/// Split chosen to fit Helius Free's 5-concurrent-connection-per-key cap
/// with 12 total subscriptions (4 Raydium + 3 Orca top-level + 5 Orca tick
/// arrays) across 3 keys:
///   key 0 (5): Raydium pool, AMM config, vault0, vault1, Orca pool state
///   key 1 (5): Orca vaultA, vaultB, + ~3 tick arrays (see tick_array_key_slot)
///   key 2 (~2): remaining ~2 tick arrays
fn ws_endpoint_for<'a>(ws_endpoints: &'a [String], slot: usize) -> &'a str {
    debug_assert!(!ws_endpoints.is_empty(), "ws_endpoints must not be empty");
    &ws_endpoints[slot % ws_endpoints.len()]
}

#[allow(clippy::too_many_arguments)]
pub async fn run_realtime_loop(
    ws_endpoints: &[String],
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

    if ws_endpoints.len() < 3 {
        tracing::warn!(
            "Only {} WebSocket endpoint(s) configured (HELIUS_WS_URL + HELIUS_WS_URL_2/_3/...); \
             Falcon opens 12 account subscriptions total, and each Helius key is capped at 5 \
             concurrent WebSocket connections (Free tier) - fewer than 3 keys means some \
             subscriptions will share a key past its cap and may fail to connect, as seen in \
             earlier runs. Set HELIUS_WS_URL_2 and HELIUS_WS_URL_3 for a clean 5/5/2 split.",
            ws_endpoints.len()
        );
    }
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
        ws_endpoint_for(ws_endpoints, 0).to_string(),
        raydium_pool_id.to_string(),
        AccountRole::RaydiumPoolState,
        tx.clone(),
    );

    if let Some(ctx) = &cpmm_ctx {
        spawn_subscriber(
            ws_endpoint_for(ws_endpoints, 0).to_string(),
            ctx.amm_config.to_string(),
            AccountRole::RaydiumAmmConfig,
            tx.clone(),
        );

        spawn_subscriber(
            ws_endpoint_for(ws_endpoints, 0).to_string(),
            ctx.token_0_vault.to_string(),
            AccountRole::RaydiumVault0,
            tx.clone(),
        );

        spawn_subscriber(
            ws_endpoint_for(ws_endpoints, 0).to_string(),
            ctx.token_1_vault.to_string(),
            AccountRole::RaydiumVault1,
            tx.clone(),
        );
    }

    spawn_subscriber(
        ws_endpoint_for(ws_endpoints, 0).to_string(),
        orca_pool_id.to_string(),
        AccountRole::OrcaPoolState,
        tx.clone(),
    );

    if let Some(ctx) = &orca_ctx {
        spawn_subscriber(
            ws_endpoint_for(ws_endpoints, 1).to_string(),
            ctx.token_vault_a.to_string(),
            AccountRole::OrcaVaultA,
            tx.clone(),
        );

        spawn_subscriber(
            ws_endpoint_for(ws_endpoints, 1).to_string(),
            ctx.token_vault_b.to_string(),
            AccountRole::OrcaVaultB,
            tx.clone(),
        );
    }

    // Dynamic tick-array subscription window (Phase 4 Area 3):
    // Managed dynamically by orca::DynamicTickWindow with cancellable subscriptions.
    let mut dynamic_window: Option<orca::DynamicTickWindow> = None;
    let mut tick_sub_handles: std::collections::HashMap<Pubkey, realtime::CancellableAccountSubscription> = std::collections::HashMap::new();

    if let Ok(pool_pubkey) = orca_pool_id.parse::<Pubkey>() {
        if let Ok(account) = client.get_account(&pool_pubkey) {
            if let Ok(wp) = orca::decode_whirlpool(&account.data) {
                match orca::DynamicTickWindow::new(pool_pubkey, wp.tick_spacing, wp.tick_current_index) {
                    Ok(dw) => {
                        tracing::info!(
                            "Orca tick-array dynamic window initialized at tick {} (spacing {}): {:?}",
                            wp.tick_current_index,
                            wp.tick_spacing,
                            dw.window_pdas
                        );
                        for pda in &dw.window_pdas {
                            let sub = spawn_cancellable_tick_subscriber(
                                ws_endpoint_for(ws_endpoints, tick_array_key_slot(pda)).to_string(),
                                pda.to_string(),
                                *pda,
                                dw.generation,
                                tx.clone(),
                            );
                            tick_sub_handles.insert(*pda, sub);
                        }
                        dynamic_window = Some(dw);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to initialize Orca dynamic tick window: {e}");
                    }
                }
            }
        }
    }

    let loop_tx = tx.clone();
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

    // Orca snapshot poller state. The WebSocket path alone is not enough for
    // Orca: a quiet pool pushes nothing for long stretches (so the quote
    // state was never published and every Orca leg failed to quote), and the
    // 5 extra tick-array sockets can be refused by the RPC provider. Polling
    // seeds the snapshot immediately and keeps it inside the freshness
    // window; WebSocket pushes still apply on top, and never roll back a
    // newer polled state (see the slot guards below).
    let poll_interval = orca_poll_interval();
    let mut last_orca_poll = std::time::Instant::now() - poll_interval;
    let mut pending: std::collections::VecDeque<RealtimeEvent> =
        std::collections::VecDeque::new();
    let mut last_forced_sim = std::time::Instant::now() - std::time::Duration::from_secs(3600);
    let force_sim_enabled = std::env::var("FORCE_SIM").map(|v| v == "1").unwrap_or(false);
    if force_sim_enabled {
        tracing::warn!("FORCE_SIM=1: dry-run simulations will be attempted even without a profitable spread (simulate-only, never broadcast)");
    }

    loop {
        if let (Some(ctx), Some(pool_pk)) = (&orca_ctx, orca_pool_pubkey) {
            if last_orca_poll.elapsed() >= poll_interval {
                last_orca_poll = std::time::Instant::now();
                if let Some(dw) = dynamic_window.as_ref() {
                    match poll_orca_events(&client, pool_pk, ctx, dw) {
                        Ok(events) => pending.extend(events),
                        Err(e) => tracing::warn!("Orca RPC snapshot poll failed: {e:#}"),
                    }
                }
            }
        }

        let event = match pending.pop_front() {
            Some(e) => e,
            None => match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(e) => e,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };

        newest_observed_slot = newest_observed_slot.max(event.update.slot);

        if event.via_poll {
            tracing::debug!(
                "RPC poll event: {:?} ({} bytes, slot {})",
                event.role,
                event.update.data.len(),
                event.update.slot
            );
        } else {
            tracing::info!(
                "WebSocket event received: {:?} ({} bytes)",
                event.role,
                event.update.data.len()
            );
        }

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
                Ok(_)
                    if orca_whirlpool_decoded
                        .as_ref()
                        .map(|(_, s)| event.update.slot < *s)
                        .unwrap_or(false) =>
                {
                    // Older than what we already hold (e.g. a late WebSocket
                    // push after a newer RPC poll): never roll state back.
                }
                Ok(whirlpool) => {
                    orca_sqrt_price = whirlpool.sqrt_price;
                    orca_liquidity = whirlpool.liquidity;
                    orca_fee_rate = whirlpool.fee_rate;

                    // Dynamic tick-array window management (Phase 4 Area 3)
                    if let Some(dw) = dynamic_window.as_mut() {
                        match dw.update_whirlpool_tick(whirlpool.tick_current_index) {
                            Ok(orca::WindowTransition::Shift {
                                old_generation,
                                new_generation,
                                obsolete_pdas,
                                new_pdas,
                                retained_pdas,
                            }) => {
                                tracing::info!(
                                    "Orca tick window shifted from gen {} to gen {} at tick {}. Obsolete: {}, New: {}, Retained: {}",
                                    old_generation,
                                    new_generation,
                                    whirlpool.tick_current_index,
                                    obsolete_pdas.len(),
                                    new_pdas.len(),
                                    retained_pdas.len(),
                                );
                                // Cancel and unsubscribe obsolete subscriptions
                                for obs in obsolete_pdas {
                                    if let Some(handle) = tick_sub_handles.remove(&obs) {
                                        handle.cancel_and_join();
                                    }
                                }
                                // Start subscriptions for newly required PDAs
                                for new_pda in new_pdas {
                                    let sub = spawn_cancellable_tick_subscriber(
                                        ws_endpoint_for(ws_endpoints, tick_array_key_slot(&new_pda)).to_string(),
                                        new_pda.to_string(),
                                        new_pda,
                                        new_generation,
                                        loop_tx.clone(),
                                    );
                                    tick_sub_handles.insert(new_pda, sub);
                                }
                            }
                            Ok(orca::WindowTransition::NoOp) => {}
                            Err(e) => {
                                tracing::warn!("Failed to update dynamic tick window: {e}");
                            }
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

                    if let Some(dw) = dynamic_window.as_ref() {
                        orca::publish_orca_quote_state(
                            dw.current_quote_state(whirlpool, event.update.slot),
                        );
                    }
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

            AccountRole::OrcaTickArray { pda, generation } => {
                let Some(dw) = dynamic_window.as_mut() else {
                    continue;
                };

                let generation = if generation == POLLED_GENERATION {
                    dw.generation
                } else {
                    generation
                };

                if generation != dw.generation {
                    tracing::warn!(
                        "Ignoring Orca tick-array push for stale subscription generation {} (current: {}) for PDA {}",
                        generation,
                        dw.generation,
                        pda
                    );
                    continue;
                }

                match orca::decode_tick_array(&event.update.data) {
                    Ok(decoded) => {
                        let expected_whirlpool = orca_pool_pubkey;
                        let whirlpool_ok = expected_whirlpool
                            .map(|p| p == decoded.whirlpool)
                            .unwrap_or(false);

                        if !whirlpool_ok {
                            tracing::warn!(
                                "Orca tick-array push for PDA {} belongs to whirlpool {}, expected {:?} - discarding",
                                pda,
                                decoded.whirlpool,
                                expected_whirlpool
                            );
                        } else if let Some((wp, pool_slot)) = &orca_whirlpool_decoded {
                            if let Err(e) = orca::validate_tick_array(
                                &decoded,
                                &decoded.whirlpool,
                                wp.tick_spacing,
                            ) {
                                tracing::warn!(
                                    "Orca tick-array push for PDA {} failed validation: {}",
                                    pda,
                                    e
                                );
                            } else if let Ok(true) = dw.handle_tick_array_update(
                                &pda,
                                generation,
                                decoded,
                                event.update.slot,
                            ) {
                                orca::publish_orca_quote_state(
                                    dw.current_quote_state(wp.clone(), *pool_slot),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to decode Orca tick-array push for PDA {}: {}",
                            pda,
                            e
                        );
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

                    let live_orca_state_for_exec = orca::current_orca_quote_state();
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
                        live_orca_state_for_exec.as_ref(),
                        false,
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

                    if force_sim_enabled
                        && last_forced_sim.elapsed() >= std::time::Duration::from_secs(20)
                    {
                        last_forced_sim = std::time::Instant::now();
                        run_forced_simulation(r, o, capital_source, &exec_ctx, newest_observed_slot);
                    }
                }
            }
        }
    }

    Ok(())
}


#[cfg(test)]
mod key_split_tests {
    use super::*;

    #[test]
    fn ws_endpoint_for_wraps_when_fewer_keys_than_slots() {
        let endpoints = vec!["key0".to_string()];
        // With only 1 key configured, every slot must resolve to it rather
        // than panicking or silently dropping the subscription.
        assert_eq!(ws_endpoint_for(&endpoints, 0), "key0");
        assert_eq!(ws_endpoint_for(&endpoints, 1), "key0");
        assert_eq!(ws_endpoint_for(&endpoints, 2), "key0");
    }

    #[test]
    fn ws_endpoint_for_selects_correct_key_with_three_configured() {
        let endpoints = vec!["key0".to_string(), "key1".to_string(), "key2".to_string()];
        assert_eq!(ws_endpoint_for(&endpoints, 0), "key0");
        assert_eq!(ws_endpoint_for(&endpoints, 1), "key1");
        assert_eq!(ws_endpoint_for(&endpoints, 2), "key2");
    }

    #[test]
    fn tick_array_key_slot_is_deterministic_for_the_same_pda() {
        let pda = Pubkey::new_unique();
        let a = tick_array_key_slot(&pda);
        let b = tick_array_key_slot(&pda);
        assert_eq!(a, b, "the same PDA must always land on the same key slot");
    }

    #[test]
    fn tick_array_key_slot_only_ever_returns_key_1_or_key_2() {
        // TICK_ARRAY_KEY_SLOTS reserves key 0 for Raydium + Orca pool
        // state (already at its 5-connection cap); tick arrays must never
        // be assigned there.
        for _ in 0..64 {
            let pda = Pubkey::new_unique();
            let slot = tick_array_key_slot(&pda);
            assert!(slot == 1 || slot == 2, "unexpected key slot {slot} for tick array");
        }
    }

    #[test]
    fn full_twelve_subscription_split_respects_five_connection_cap_per_key() {
        // Simulates the real assignment: 5 fixed top-level subscriptions
        // (3 on key 0, 2 on key 1) plus 5 tick-array PDAs distributed by
        // tick_array_key_slot, and checks every key's total against
        // Helius Free's cap of 5 concurrent connections.
        let mut counts = std::collections::HashMap::new();
        // Raydium: pool, amm_config, vault0, vault1 -> key 0 (slot 0)
        for _ in 0..4 {
            *counts.entry(0usize).or_insert(0) += 1;
        }
        // Orca pool state -> key 0 (slot 0)
        *counts.entry(0usize).or_insert(0) += 1;
        // Orca vaultA, vaultB -> key 1 (slot 1)
        for _ in 0..2 {
            *counts.entry(1usize).or_insert(0) += 1;
        }
        // 5 tick arrays, worst case all land on the same key (adversarial
        // PDA bytes) - even then, key 0 (already at 5) must not receive
        // any, since tick_array_key_slot never returns 0.
        for _ in 0..5 {
            let pda = Pubkey::new_unique();
            *counts.entry(tick_array_key_slot(&pda)).or_insert(0) += 1;
        }
        assert_eq!(*counts.get(&0).unwrap(), 5, "key 0 must be exactly the 5 fixed subscriptions");
        assert!(counts.get(&0).copied().unwrap_or(0) <= 5);
        // Keys 1 and 2 together hold exactly: 2 fixed (Orca vaults) + 5 tick arrays = 7
        let k1 = counts.get(&1).copied().unwrap_or(0);
        let k2 = counts.get(&2).copied().unwrap_or(0);
        assert_eq!(k1 + k2, 7, "Orca vaults + all 5 tick arrays must total 7 across keys 1 and 2");
    }
}
