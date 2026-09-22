use anyhow::{Context, Result};
use solana_account_decoder::UiAccountEncoding;
use solana_client::rpc_config::RpcAccountInfoConfig;
use solana_pubsub_client::pubsub_client::PubsubClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::str::FromStr;
use std::sync::mpsc;

/// Raw account bytes pushed directly from the WebSocket notification - no extra
/// RPC round-trip needed to get the actual data.
pub struct AccountUpdate {
    pub data: Vec<u8>,
    pub owner: Pubkey,
}

pub fn subscribe_to_account(
    ws_url: &str,
    pool_id: &str,
    tx: mpsc::Sender<AccountUpdate>,
) -> Result<()> {
    let pubkey = Pubkey::from_str(pool_id).context("Invalid pool pubkey")?;

    let config = RpcAccountInfoConfig {
        commitment: Some(CommitmentConfig::confirmed()),
        encoding: Some(UiAccountEncoding::Base64),
        data_slice: None,
        min_context_slot: None,
    };

    let (_subscription, receiver) = PubsubClient::account_subscribe(ws_url, &pubkey, Some(config))
        .context("Failed to subscribe to account")?;

    loop {
        match receiver.recv() {
            Ok(response) => {
                let ui_account = response.value;

                let data_bytes = match ui_account.data.decode() {
                    Some(bytes) => bytes,
                    None => {
                        tracing::warn!("Failed to decode account data from WebSocket notification");
                        continue;
                    }
                };

                let owner = Pubkey::from_str(&ui_account.owner).unwrap_or_default();

                let update = AccountUpdate {
                    data: data_bytes,
                    owner,
                };

                if tx.send(update).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    Ok(())
}
