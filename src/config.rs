use anyhow::{Context, Result};
use solana_sdk::signature::{Keypair, Signer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderType {
    Cpmm,
    Amm,
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

        tracing::info!("Config loaded. Wallet pubkey: {}", keypair.pubkey());
        tracing::info!(
            "Pair: {}, Raydium pool: {}, Orca pool: {}, Decoder: {:?}",
            pair,
            raydium_pool_id,
            orca_pool_id,
            decoder,
        );

        Ok(Self {
            helius_rpc_url,
            helius_ws_url,
            jito_block_engine_url,
            keypair,
            pair,
            raydium_pool_id,
            orca_pool_id,
            decoder,
        })
    }
}
