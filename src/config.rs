use anyhow::{Context, Result};
use solana_sdk::signature::{Keypair, Signer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderType {
    Cpmm,
    Amm,
}

/// Where trade size limits come from - this is the foundation for the
/// flash-loan integration. Two modes today, a third to come:
///
/// - `Wallet`: trade size is capped to what the wallet can actually afford
///   (a real lamport ceiling), regardless of how deep the pool is. This is
///   the mode to run in BEFORE flash loans are wired up - the wallet is
///   real capital, so sizing must respect its real balance.
/// - `Pool`: trade size is NOT capped by wallet balance at all - sizing
///   stays purely a function of pool depth, exactly as `find_best_opportunity`
///   already computes it. This is the flash-loan placeholder: once a flash
///   loan instruction is wired into the executor, this variant is where that
///   capital is assumed to come from instead of the wallet, so pool-depth
///   sizing becomes correct rather than dangerous.
///
/// Selected via `MTS` in .env (`WALLET` or `POOL`). Nothing about
/// `find_best_opportunity`'s sizing logic changes based on this - it always
/// proposes the same pool-depth-based candidate sizes; this only decides
/// whether those candidates get clamped against a real balance before being
/// evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapitalSource {
    Wallet { max_lamports: u64 },
    Pool,
}

pub struct Config {
    pub helius_rpc_url: String,
    pub helius_ws_url: String,
    pub jito_block_engine_url: String,
    pub keypair: Keypair,
    pub pair: String,
    pub raydium_pool_id: String,
    pub orca_pool_id: String,
    pub decoder: DecoderType,
    pub priority_fee_micro_lamports: u64,
    pub max_price_age_secs: u64,
    pub capital_source: CapitalSource,
}

impl Config {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let helius_rpc_url =
            std::env::var("HELIUS_RPC_URL").context("HELIUS_RPC_URL not set in environment")?;

        let helius_ws_url =
            std::env::var("HELIUS_WS_URL").context("HELIUS_WS_URL not set in environment")?;

        let jito_block_engine_url = std::env::var("JITO_BLOCK_ENGINE_URL")
            .context("JITO_BLOCK_ENGINE_URL not set in environment")?;

        let private_key_str = std::env::var("WALLET_PRIVATE_KEY")
            .context("WALLET_PRIVATE_KEY not set in environment")?;

        let secret_bytes = bs58::decode(&private_key_str)
            .into_vec()
            .context("WALLET_PRIVATE_KEY is not valid base58")?;

        let keypair = Keypair::try_from(secret_bytes.as_slice())
            .map_err(|e| anyhow::anyhow!("Failed to construct Keypair: {e}"))?;

        let pair = std::env::var("PAIR").context("PAIR not set in environment")?;

        let raydium_pool_id =
            std::env::var("RAYDIUM_POOL_ID").context("RAYDIUM_POOL_ID not set in environment")?;

        let orca_pool_id =
            std::env::var("ORCA_POOL_ID").context("ORCA_POOL_ID not set in environment")?;

        let decoder_str = std::env::var("DECODER").unwrap_or_else(|_| "CPMM".to_string());
        let decoder = match decoder_str.trim().to_uppercase().as_str() {
            "AMM" | "AMMV4" | "LEGACY" => DecoderType::Amm,
            "CPMM" => DecoderType::Cpmm,
            other => anyhow::bail!("Invalid DECODER '{other}' in .env: must be CPMM or AMM"),
        };

        let priority_fee_micro_lamports: u64 = std::env::var("PRIORITY_FEE_MICRO_LAMPORTS")
            .unwrap_or_else(|_| "25000".to_string())
            .parse()
            .unwrap_or(25_000);

        let max_price_age_secs: u64 = std::env::var("MAX_PRICE_AGE_SECS")
            .unwrap_or_else(|_| "0".to_string())
            .parse()
            .unwrap_or(0);

        // MTS (Max Trade Size mode): WALLET or POOL. Defaults to WALLET -
        // the safe default for a real, non-flash-loan-funded wallet. Switch
        // to POOL only once a flash loan is actually wired into the
        // executor and providing the capital for the buy leg.
        let mts_str = std::env::var("MTS").unwrap_or_else(|_| "WALLET".to_string());
        let capital_source = match mts_str.trim().to_uppercase().as_str() {
            "POOL" => CapitalSource::Pool,
            "WALLET" => {
                // MAX_TRADE_SIZE_LAMPORTS: an explicit ceiling, set by you
                // based on real wallet balance minus a fee/rent buffer. Not
                // auto-derived from an RPC balance check, so it stays a
                // deliberate, explicit number you control - same spirit as
                // every other execution-critical value in this codebase
                // being explicit rather than inferred.
                let max_lamports: u64 = std::env::var("MAX_TRADE_SIZE_LAMPORTS")
                    .context(
                        "MAX_TRADE_SIZE_LAMPORTS not set in environment - required when MTS=WALLET \
                         (set it to comfortably under your wallet's real lamport balance)",
                    )?
                    .parse()
                    .context("MAX_TRADE_SIZE_LAMPORTS must be a valid u64")?;
                CapitalSource::Wallet { max_lamports }
            }
            other => anyhow::bail!("Invalid MTS '{other}' in .env: must be WALLET or POOL"),
        };

        tracing::info!("Config loaded. Wallet pubkey: {}", keypair.pubkey());
        tracing::info!(
            "Pair: {}, Raydium pool: {}, Orca pool: {}, Decoder: {:?}",
            pair,
            raydium_pool_id,
            orca_pool_id,
            decoder,
        );
        match capital_source {
            CapitalSource::Wallet { max_lamports } => {
                tracing::info!(
                    "Capital source: WALLET (trade size capped at {} lamports)",
                    max_lamports
                );
            }
            CapitalSource::Pool => {
                tracing::warn!(
                    "Capital source: POOL (trade size NOT capped to wallet balance - \
                     this mode assumes flash-loan-funded capital; do not run this against \
                     a real wallet's own funds)"
                );
            }
        }

        Ok(Self {
            helius_rpc_url,
            helius_ws_url,
            jito_block_engine_url,
            keypair,
            pair,
            raydium_pool_id,
            orca_pool_id,
            decoder,
            priority_fee_micro_lamports,
            max_price_age_secs,
            capital_source,
        })
    }
}