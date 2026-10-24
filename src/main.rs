mod analyzer;
mod config;
mod executor;
mod failure;
mod ledger;
mod logger;
mod risk;
mod scanner;

use solana_client::rpc_client::RpcClient;
use solana_sdk::signer::Signer;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    logger::ensure_header()?;

    tracing::info!("Falcon starting up");

    let config = config::Config::load()?;

    // Execution mode + risk limits. Defaults to PAPER (simulate only, never
    // broadcast). LIVE refuses to start without every guard in place.
    let mode = risk::mode_from_env()?;
    let wallet_capital = matches!(config.capital_source, config::CapitalSource::Wallet { .. });
    risk::validate_live_prerequisites(mode, wallet_capital, risk::read_live_ack()?.as_deref())?;
    let max_trade = match config.capital_source {
        config::CapitalSource::Wallet { max_lamports } => max_lamports,
        config::CapitalSource::Pool => u64::MAX,
    };
    let limits = risk::RiskLimits::from_env(max_trade)?;
    tracing::warn!("EXECUTION MODE: {} | risk limits: {:?}", mode, limits);

    let blockhash_cache = executor::spawn_blockhash_poller(config.helius_rpc_url.clone(), 1500)?;
    tracing::info!("Background blockhash cache started (1500ms interval)");

    let exec_client = RpcClient::new(config.helius_rpc_url.clone());
    let raydium_exec_ctx =
        executor::RaydiumPoolContext::load(&exec_client, &config.raydium_pool_id, config.decoder)
            .expect("Failed to load Raydium pool context for executor");
    let orca_exec_ctx = executor::OrcaPoolContext::load(&exec_client, &config.orca_pool_id)
        .expect("Failed to load Orca pool context for executor");

    let wsol_mint: solana_sdk::pubkey::Pubkey = executor::WSOL_MINT.parse()?;
    let other_token_mint = if orca_exec_ctx.info.token_mint_a == wsol_mint {
        orca_exec_ctx.info.token_mint_b
    } else {
        orca_exec_ctx.info.token_mint_a
    };

    // Creating ATAs sends a real transaction and spends rent, so it is LIVE
    // only. In OBSERVE/PAPER missing ATAs are just included in the simulated
    // transaction instead.
    if mode == risk::ExecutionMode::Live {
        tracing::warn!(
            "LIVE MODE: real funds from wallet {}. Starting in 10s - Ctrl+C now to abort.",
            config.keypair.pubkey()
        );
        std::thread::sleep(std::time::Duration::from_secs(10));
        executor::ensure_wallet_atas(&exec_client, &config.keypair, &other_token_mint)
            .expect("Failed to ensure wallet ATAs exist");
    }

    let ata_cache =
        executor::AtaCache::load(&exec_client, &config.keypair.pubkey(), &other_token_mint)
            .expect("Failed to load ATA cache");
    tracing::info!(
        "ATA cache loaded: WSOL ATA {} (exists: {}), other-token ATA {} (exists: {})",
        ata_cache.wsol_ata,
        ata_cache.wsol_exists,
        ata_cache.other_ata,
        ata_cache.other_exists
    );

    tracing::info!("Falcon initialized and ready - starting real-time engine");

    scanner::run_realtime_loop(
        &config.ws_endpoints,
        &config.helius_rpc_url,
        &config.pair,
        &config.raydium_pool_id,
        &config.orca_pool_id,
        config.decoder,
        50_000_000.0,
        config.max_price_age_secs,
        config.capital_source,
        scanner::ExecutionContext {
            client: exec_client,
            payer: config.keypair,
            raydium_ctx: raydium_exec_ctx,
            orca_ctx: orca_exec_ctx,
            other_token_mint,
            blockhash_cache,
            ata_cache,
            mode,
            risk: risk::RiskManager::new(mode, limits),
            ledger: ledger::Ledger::open(match mode {
                risk::ExecutionMode::Live => "live_trades.csv",
                _ => "paper_trades.csv",
            })?,
        },
    )
    .await?;

    Ok(())
}
