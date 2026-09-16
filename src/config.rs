use anyhow::{Context, Result};
use solana_sdk::signature::{Keypair, Signer};

pub struct Config {
    pub helius_rpc_url: String,
    pub jito_block_engine_url: String,
    pub keypair: Keypair,
}

impl Config {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let helius_rpc_url =
            std::env::var("HELIUS_RPC_URL").context("HELIUS_RPC_URL not set in environment")?;

        let jito_block_engine_url = std::env::var("JITO_BLOCK_ENGINE_URL")
            .context("JITO_BLOCK_ENGINE_URL not set in environment")?;

        let private_key_str = std::env::var("WALLET_PRIVATE_KEY")
            .context("WALLET_PRIVATE_KEY not set in environment")?;

        let secret_bytes = bs58::decode(&private_key_str)
            .into_vec()
            .context("WALLET_PRIVATE_KEY is not valid base58")?;

        let keypair = Keypair::try_from(secret_bytes.as_slice()).map_err(|e| {
            anyhow::anyhow!("Failed to construct Keypair from decoded private key bytes: {e}")
        })?;

        tracing::info!("Config loaded. Wallet pubkey: {}", keypair.pubkey());

        Ok(Self {
            helius_rpc_url,
            jito_block_engine_url,
            keypair,
        })
    }
}
