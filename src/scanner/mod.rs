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
            tx,
        );
    }

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

    for event in rx {
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
                            tracing::error!("Failed to simulate opportunity: {:?}", e);
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
