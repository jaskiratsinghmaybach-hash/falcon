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

/// Subscribes to a single account over WebSocket and streams raw decoded bytes
/// to `tx` as they arrive. Blocking - intended to be run on its own OS thread
/// (see scanner/mod.rs, which spawns one of these per subscribed account).
pub fn subscribe_to_account(
    ws_url: &str,
    account_id: &str,
    tx: mpsc::Sender<AccountUpdate>,
) -> Result<()> {
    let pubkey = Pubkey::from_str(account_id).context("Invalid account pubkey")?;

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

/// Decodes an SPL Token account's raw bytes into its `amount` field (u64,
/// smallest units) with zero RPC calls and zero allocation beyond the copy.
/// Layout per the SPL Token program (fixed 165-byte account):
///   0..32   mint (Pubkey)
///   32..64  owner (Pubkey)
///   64..72  amount (u64, little-endian)
///   ... (rest not needed here)
/// This is the same `amount` the JSON-RPC `get_token_account_balance` call
/// would return - just read directly from the bytes the WebSocket already
/// pushed us, instead of making a second network round-trip to ask for it.
pub fn decode_token_account_balance(data: &[u8]) -> Result<u64> {
    if data.len() < 72 {
        anyhow::bail!(
            "Token account data too short to contain balance: {} bytes (need >= 72)",
            data.len()
        );
    }
    let amount_bytes: [u8; 8] = data[64..72]
        .try_into()
        .context("Failed to slice token account amount bytes")?;
    Ok(u64::from_le_bytes(amount_bytes))
}