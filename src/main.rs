mod analyzer;
mod config;
mod executor;
mod logger;
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

    executor::ensure_wallet_atas(&exec_client, &config.keypair, &other_token_mint)
        .expect("Failed to ensure wallet ATAs exist");

    let ata_cache = executor::AtaCache::load(&exec_client, &config.keypair.pubkey(), &other_token_mint)
        .expect("Failed to load ATA cache");
    tracing::info!(
        "ATA cache loaded: WSOL ATA {} (exists: {}), other-token ATA {} (exists: {})",
        ata_cache.wsol_ata, ata_cache.wsol_exists, ata_cache.other_ata, ata_cache.other_exists
    );

    tracing::info!("Falcon initialized and ready - starting real-time engine");

    scanner::run_realtime_loop(
        &config.helius_ws_url,
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
        },
    )
    .await?;

    Ok(())
}