//! Jito block-engine submission. The arb transaction carries its own tip
//! transfer (last instruction, so it is only paid if everything else
//! succeeds) and is sent with `bundleOnly=true`: Jito lands it as a
//! single-transaction bundle with revert protection, so a reverting arb is
//! dropped instead of landing and burning the fee.

use anyhow::{Context, Result};
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    instruction::Instruction, pubkey, pubkey::Pubkey, signature::Signature, system_instruction,
    transaction::VersionedTransaction,
};

pub const DEFAULT_BLOCK_ENGINE_URL: &str = "https://mainnet.block-engine.jito.wtf";

/// Jito's mainnet tip accounts (`getTipAccounts`).
pub const TIP_ACCOUNTS: [Pubkey; 8] = [
    pubkey!("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"),
    pubkey!("HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe"),
    pubkey!("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY"),
    pubkey!("ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49"),
    pubkey!("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
    pubkey!("ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt"),
    pubkey!("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL"),
    pubkey!("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"),
];

/// Jito's minimum accepted bundle tip.
pub const MIN_TIP_LAMPORTS: u64 = 1_000;

/// Picks a tip account pseudo-randomly to spread write-lock contention.
pub fn pick_tip_account() -> Pubkey {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    TIP_ACCOUNTS[nanos as usize % TIP_ACCOUNTS.len()]
}

pub fn tip_ix(payer: &Pubkey, tip_account: &Pubkey, lamports: u64) -> Instruction {
    system_instruction::transfer(payer, tip_account, lamports)
}

pub struct JitoSender {
    client: RpcClient,
}

impl JitoSender {
    /// `block_engine_url` is the base URL, e.g. `DEFAULT_BLOCK_ENGINE_URL`.
    pub fn new(block_engine_url: &str) -> Self {
        let url = format!(
            "{}/api/v1/transactions?bundleOnly=true",
            block_engine_url.trim_end_matches('/')
        );
        Self {
            client: RpcClient::new(url),
        }
    }

    /// Jito's `/transactions` endpoint speaks JSON-RPC `sendTransaction`.
    pub fn send(&self, tx: &VersionedTransaction) -> Result<Signature> {
        self.client
            .send_transaction_with_config(
                tx,
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            )
            .context("Jito block engine rejected the transaction")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tip_account_is_a_known_one() {
        assert!(TIP_ACCOUNTS.contains(&pick_tip_account()));
    }
}
