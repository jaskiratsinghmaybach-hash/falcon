use super::PriceUpdate;
use anyhow::{bail, Result};
use solana_client::rpc_client::RpcClient;

pub fn fetch_price(_client: &RpcClient) -> Result<PriceUpdate> {
    bail!("Orca not yet implemented")
}

