mod analyzer;
mod config;
mod executor;
mod failure;
mod logger;
mod scanner;

use anyhow::Context;
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

    // The Orca legacy `swap` and Raydium AMM v4 instructions only support the
    // classic SPL Token program, and the executor creates ATAs with it. A
    // Token-2022 mint would fail every trade on-chain, so refuse to start.
    let other_mint_owner = exec_client
        .get_account(&other_token_mint)
        .context("Failed to fetch the non-SOL token mint account")?
        .owner;
    if other_mint_owner.to_string() != executor::TOKEN_PROGRAM_ID {
        anyhow::bail!(
            "Token mint {} is owned by {} (not the SPL Token program); Token-2022 pairs are not supported by the Orca legacy swap path",
            other_token_mint,
            other_mint_owner
        );
    }

    executor::ensure_wallet_atas(&exec_client, &config.keypair, &other_token_mint)
        .expect("Failed to ensure wallet ATAs exist");

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

    let flash = match config.kamino_reserve {
        Some(reserve_id) => {
            let reserve = executor::kamino::KaminoReserve::load(&exec_client, &reserve_id)
                .context("Failed to load Kamino flash-loan reserve")?;
            if reserve.liquidity_mint.to_string() != executor::WSOL_MINT
                || reserve.token_program.to_string() != executor::TOKEN_PROGRAM_ID
            {
                anyhow::bail!(
                    "Kamino reserve {} lends {} (token program {}); POOL mode needs a WSOL / SPL Token reserve",
                    reserve_id,
                    reserve.liquidity_mint,
                    reserve.token_program
                );
            }
            config::set_flash_loan_fee_sf(reserve.flash_loan_fee_sf);
            tracing::info!(
                "Kamino flash loans: reserve {}, fee {} lamports per SOL, {} lamports available",
                reserve_id,
                reserve.flash_fee(1_000_000_000).unwrap_or(0),
                reserve.available_amount
            );
            Some(reserve)
        }
        None => None,
    };
    let mut lookup_tables = Vec::new();
    for key in &config.lookup_tables {
        let account = exec_client
            .get_account(key)
            .with_context(|| format!("Failed to fetch lookup table {key}"))?;
        let table =
            solana_sdk::address_lookup_table::state::AddressLookupTable::deserialize(&account.data)
                .map_err(|e| {
                    anyhow::anyhow!("Account {key} is not an address lookup table: {e}")
                })?;
        tracing::info!(
            "Lookup table {} loaded ({} addresses)",
            key,
            table.addresses.len()
        );
        lookup_tables.push(
            solana_sdk::address_lookup_table::AddressLookupTableAccount {
                key: *key,
                addresses: table.addresses.to_vec(),
            },
        );
    }

    let jito = config
        .jito_block_engine_url
        .as_deref()
        .map(executor::jito::JitoSender::new);

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
            live_trading: config.live_trading,
            exec_cooldown_ms: config.exec_cooldown_ms,
            extras: executor::ExecExtras {
                flash,
                jito_tip_lamports: config::jito_tip_lamports(),
                lookup_tables,
            },
            jito,
        },
    )
    .await?;

    Ok(())
}
