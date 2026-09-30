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

// ===========================================================================
// TRANSACTION COST MODEL (single source of truth)
// ===========================================================================
// The analyzer prices an opportunity's fixed costs and the executor builds the
// transaction that actually pays them. Both MUST read the same numbers, or the
// analyzer approves/rejects trades based on a cost the chain never charges
// (that is exactly what the old hardcoded 100_000-lamport "Jito tip" did: it
// charged ~15x the real cost and rejected profitable trades).
//
// Falcon submits ONE atomic transaction (buy leg + sell leg), signed by ONE
// key, with a compute-unit limit and compute-unit price. There is no Jito
// bundle and no tip. Real cost of that transaction:
//
//   base fee     = LAMPORTS_PER_SIGNATURE * signatures
//   priority fee = ceil(compute_unit_limit * compute_unit_price_micro / 1_000_000)
//
// Jito tips and flash loans are deferred future optimizations; nothing here
// models them.
// ===========================================================================

/// Solana's fixed base fee per transaction signature.
pub const LAMPORTS_PER_SIGNATURE: u64 = 5_000;

/// Signatures on Falcon's atomic transaction (fee payer only).
pub const TX_SIGNATURES: u64 = 1;

/// Compute-unit limit requested by the executor's atomic transaction.
pub const COMPUTE_UNIT_LIMIT: u32 = 350_000;

/// Compute-unit price (micro-lamports per CU) requested by the executor.
/// Must equal what the executor passes to `set_compute_unit_price`.
pub const COMPUTE_UNIT_PRICE_MICRO_LAMPORTS: u64 = 25_000;

/// Priority fee in lamports, rounded UP (the runtime rounds up, so rounding
/// down here would under-price the transaction). `None` on overflow.
pub fn priority_fee_lamports(
    compute_unit_limit: u32,
    compute_unit_price_micro_lamports: u64,
) -> Option<u64> {
    let product =
        (compute_unit_limit as u128).checked_mul(compute_unit_price_micro_lamports as u128)?;
    let lamports = product.checked_add(999_999)? / 1_000_000;
    u64::try_from(lamports).ok()
}

/// Total fixed lamport cost of one Falcon atomic transaction: base signature
/// fee plus the priority fee. `None` on overflow.
pub fn fixed_tx_cost_lamports(
    signatures: u64,
    compute_unit_limit: u32,
    compute_unit_price_micro_lamports: u64,
) -> Option<u64> {
    let base = LAMPORTS_PER_SIGNATURE.checked_mul(signatures)?;
    let priority = priority_fee_lamports(compute_unit_limit, compute_unit_price_micro_lamports)?;
    base.checked_add(priority)
}

pub struct Config {
    pub helius_rpc_url: String,
    pub helius_ws_url: String,
    /// Every WebSocket endpoint available for account subscriptions, in
    /// order. Index 0 is always `helius_ws_url` (HELIUS_WS_URL) for
    /// backward compatibility; HELIUS_WS_URL_2, HELIUS_WS_URL_3, ... add
    /// more. Each distinct Helius API key has its own concurrent-
    /// WebSocket-connection cap (5 on Free as of writing), so splitting
    /// subscriptions across several keys is how a process that needs more
    /// than one key's worth of live sockets stays under each individual
    /// cap. See scanner::mod::ws_endpoint_for, which assigns each
    /// subscription a fixed slot into this list.
    pub ws_endpoints: Vec<String>,
    pub keypair: Keypair,
    pub pair: String,
    pub raydium_pool_id: String,
    pub orca_pool_id: String,
    pub decoder: DecoderType,
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

        // Additional WebSocket endpoints (separate Helius API keys, or any
        // other RPC provider's WS endpoint) for spreading account
        // subscriptions across more than one connection-count cap.
        // HELIUS_WS_URL_2, HELIUS_WS_URL_3, ... - numbering starts at 2
        // because HELIUS_WS_URL itself is endpoint 1. Stops at the first
        // gap (e.g. HELIUS_WS_URL_2 set but HELIUS_WS_URL_3 unset ends the
        // scan at 2 endpoints total) so a typo'd higher number is never
        // silently skipped over.
        let mut ws_endpoints = vec![helius_ws_url.clone()];
        let mut n = 2u32;
        loop {
            match std::env::var(format!("HELIUS_WS_URL_{n}")) {
                Ok(url) if !url.trim().is_empty() => {
                    ws_endpoints.push(url);
                    n += 1;
                }
                _ => break,
            }
        }
        if ws_endpoints.len() > 1 {
            tracing::info!(
                "{} WebSocket endpoint(s) configured for account subscriptions",
                ws_endpoints.len()
            );
        }

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

        // The executor prices its transaction from the compile-time cost model
        // (COMPUTE_UNIT_PRICE_MICRO_LAMPORTS) so the analyzer and executor can
        // never disagree about cost - there is no Config field for it. An env
        // override that silently did nothing would be worse than none, so if
        // one is present and differs from the model, say so loudly instead of
        // pretending it applies.
        if let Ok(v) = std::env::var("PRIORITY_FEE_MICRO_LAMPORTS") {
            if v.trim().parse::<u64>().ok() != Some(COMPUTE_UNIT_PRICE_MICRO_LAMPORTS) {
                tracing::warn!(
                    "PRIORITY_FEE_MICRO_LAMPORTS={} in .env is IGNORED: the priority fee is fixed at {} micro-lamports/CU by config::COMPUTE_UNIT_PRICE_MICRO_LAMPORTS so analyzer cost and executor cost stay identical.",
                    v,
                    COMPUTE_UNIT_PRICE_MICRO_LAMPORTS
                );
            }
        }

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
            ws_endpoints,
            keypair,
            pair,
            raydium_pool_id,
            orca_pool_id,
            decoder,
            max_price_age_secs,
            capital_source,
        })
    }
}

#[cfg(test)]
mod cost_model_tests {
    use super::*;

    #[test]
    fn priority_fee_matches_hand_computation() {
        // 350_000 CU * 25_000 micro-lamports/CU = 8_750_000_000 micro-lamports
        // = 8_750 lamports exactly.
        assert_eq!(priority_fee_lamports(350_000, 25_000), Some(8_750));
    }

    #[test]
    fn priority_fee_rounds_up_never_down() {
        // 1 CU * 1 micro-lamport = 0.000001 lamport -> must round UP to 1.
        assert_eq!(priority_fee_lamports(1, 1), Some(1));
        // 1_000_001 micro-lamports -> 2 lamports, not 1.
        assert_eq!(priority_fee_lamports(1_000_001, 1), Some(2));
        // Exactly divisible does not gain an extra lamport.
        assert_eq!(priority_fee_lamports(1_000_000, 1), Some(1));
    }

    #[test]
    fn zero_priority_price_costs_nothing_extra() {
        assert_eq!(priority_fee_lamports(350_000, 0), Some(0));
        assert_eq!(fixed_tx_cost_lamports(1, 350_000, 0), Some(5_000));
    }

    #[test]
    fn fixed_cost_is_base_plus_priority_with_no_tip() {
        assert_eq!(
            fixed_tx_cost_lamports(
                TX_SIGNATURES,
                COMPUTE_UNIT_LIMIT,
                COMPUTE_UNIT_PRICE_MICRO_LAMPORTS
            ),
            Some(13_750)
        );
    }

    #[test]
    fn fixed_cost_scales_with_signature_count() {
        assert_eq!(fixed_tx_cost_lamports(2, 350_000, 25_000), Some(18_750));
    }

    #[test]
    fn cost_model_overflow_is_none_not_wraparound() {
        assert_eq!(priority_fee_lamports(u32::MAX, u64::MAX), None);
        assert_eq!(fixed_tx_cost_lamports(u64::MAX, 0, 0), None);
    }
}
