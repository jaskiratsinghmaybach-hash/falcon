use anyhow::{Context, Result};
use futures_util::StreamExt;
use solana_account_decoder::UiAccountEncoding;
use solana_client::rpc_config::RpcAccountInfoConfig;
use solana_pubsub_client::pubsub_client::PubsubClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::str::FromStr;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Raw account bytes pushed directly from the WebSocket notification - no extra
/// RPC round-trip needed to get the actual data.
pub struct AccountUpdate {
    pub data: Vec<u8>,
    pub owner: Pubkey,
    /// The RPC/account slot this push's `context.slot` reported - i.e. the
    /// slot this account's state was current as of. Used for state-slot
    /// coherence checks (Phase 4): combining data from two pushes with
    /// different slots without detecting the gap is exactly the "old pool
    /// state + new vault state" inconsistency the phase spec warns against.
    pub slot: u64,
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
                let slot = response.context.slot;
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
                    slot,
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

/// A cancellable, bounded-lifetime account subscription. The existing blocking
/// client is appropriate for subscriptions that live for the whole process,
/// but its own API documents that shutdown can block indefinitely. Tick-array
/// windows move, so they use the async client and explicitly await its
/// unsubscribe closure instead.
pub struct CancellableAccountSubscription {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl CancellableAccountSubscription {
    pub fn cancel_and_join(mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Starts one cancellable WebSocket account subscription. Its owner must call
/// cancel_and_join before replacing it; this sends accountUnsubscribe, closes
/// the client, and waits for the worker to end before a new generation begins.
pub fn spawn_cancellable_account_subscription(
    ws_url: String,
    account_id: String,
    tx: mpsc::Sender<AccountUpdate>,
) -> CancellableAccountSubscription {
    let (cancel, mut cancelled) = tokio::sync::oneshot::channel();

    let join = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!("Failed to create tick-array subscription runtime: {error}");
                return;
            }
        };

        runtime.block_on(async move {
            use solana_pubsub_client::nonblocking::pubsub_client::PubsubClient as AsyncPubsubClient;

            let pubkey = match Pubkey::from_str(&account_id) {
                Ok(pubkey) => pubkey,
                Err(error) => {
                    tracing::error!("Invalid tick-array account pubkey {account_id}: {error}");
                    return;
                }
            };

            let config = RpcAccountInfoConfig {
                commitment: Some(CommitmentConfig::confirmed()),
                encoding: Some(UiAccountEncoding::Base64),
                data_slice: None,
                min_context_slot: None,
            };

            let client = tokio::select! {
                _ = &mut cancelled => return,
                result = AsyncPubsubClient::new(&ws_url) => match result {
                    Ok(client) => client,
                    Err(error) => {
                        tracing::error!("Failed to connect tick-array subscription {account_id}: {error}");
                        return;
                    }
                },
            };

            let (mut notifications, unsubscribe) = tokio::select! {
                _ = &mut cancelled => {
                    return;
                }
                result = client.account_subscribe(&pubkey, Some(config)) => match result {
                    Ok(subscription) => subscription,
                    Err(error) => {
                        tracing::error!("Failed to subscribe tick array {account_id}: {error}");
                        return;
                    }
                },
            };

            loop {
                tokio::select! {
                    _ = &mut cancelled => break,
                    response = notifications.next() => {
                        let Some(response) = response else { break; };
                        let data = match response.value.data.decode() {
                            Some(data) => data,
                            None => {
                                tracing::warn!("Failed to decode tick-array account data from WebSocket notification");
                                continue;
                            }
                        };
                        let owner = Pubkey::from_str(&response.value.owner).unwrap_or_default();
                        if tx.send(AccountUpdate { data, owner, slot: response.context.slot }).is_err() {
                            break;
                        }
                    }
                }
            }

            drop(notifications);
            unsubscribe().await;
            let _ = tokio::time::timeout(Duration::from_secs(5), client.shutdown()).await;
        });
    });

    CancellableAccountSubscription {
        cancel: Some(cancel),
        join: Some(join),
    }
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
