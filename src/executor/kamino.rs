//! Kamino Lend (klend) flash loans: `flash_borrow_reserve_liquidity` /
//! `flash_repay_reserve_liquidity`. The borrow must be a top-level
//! instruction and the matching repay (same accounts, same amount, pointing
//! back at the borrow's index) must appear later in the SAME transaction,
//! so the loan can never outlive the arbitrage.

use anyhow::{bail, Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey,
    pubkey::Pubkey,
    sysvar,
};

pub const KLEND_PROGRAM_ID: Pubkey = pubkey!("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD");
/// SOL reserve of the Kamino main market.
pub const DEFAULT_SOL_RESERVE: &str = "d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q";

/// Kamino's address lookup table for the main market (contains the market,
/// SOL reserve, its vaults, the instructions sysvar and the token program).
pub const MAIN_MARKET_LOOKUP_TABLE: &str = "284iwGtA9X9aLy3KsyV8uT2pXLARhYbiSi5SiM2g47M2";

const FLASH_BORROW_DISCRIMINATOR: [u8; 8] = [135, 231, 52, 167, 7, 52, 212, 193];
const FLASH_REPAY_DISCRIMINATOR: [u8; 8] = [185, 117, 0, 203, 96, 245, 180, 186];
const RESERVE_DISCRIMINATOR: [u8; 8] = [43, 242, 204, 202, 26, 247, 59, 127];

// Byte offsets in the zero-copy `Reserve` account (incl. 8-byte discriminator).
const OFF_LENDING_MARKET: usize = 32;
const OFF_LIQUIDITY_MINT: usize = 128;
const OFF_SUPPLY_VAULT: usize = 160;
const OFF_FEE_VAULT: usize = 192;
const OFF_AVAILABLE_AMOUNT: usize = 224;
const OFF_LIQUIDITY_TOKEN_PROGRAM: usize = 408;
const OFF_FLASH_LOAN_FEE_SF: usize = 4904;
const RESERVE_MIN_LEN: usize = OFF_FLASH_LOAN_FEE_SF + 8;

/// klend `Fraction` is U68F60 fixed point.
const FRACTION_ONE_BITS: u128 = 1 << 60;

#[derive(Debug, Clone)]
pub struct KaminoReserve {
    pub reserve: Pubkey,
    pub lending_market: Pubkey,
    pub lending_market_authority: Pubkey,
    pub liquidity_mint: Pubkey,
    pub supply_vault: Pubkey,
    pub fee_vault: Pubkey,
    pub token_program: Pubkey,
    pub flash_loan_fee_sf: u64,
    pub available_amount: u64,
}

impl KaminoReserve {
    pub fn load(client: &RpcClient, reserve: &Pubkey) -> Result<Self> {
        let account = client
            .get_account(reserve)
            .with_context(|| format!("Failed to fetch Kamino reserve {reserve}"))?;
        if account.owner != KLEND_PROGRAM_ID {
            bail!("Account {reserve} is not owned by Kamino Lend");
        }
        Self::decode(*reserve, &account.data)
    }

    pub fn decode(reserve: Pubkey, data: &[u8]) -> Result<Self> {
        if data.len() < RESERVE_MIN_LEN || data[..8] != RESERVE_DISCRIMINATOR {
            bail!("Account {reserve} is not a Kamino Reserve");
        }
        let pk = |off: usize| Pubkey::try_from(&data[off..off + 32]).expect("32 bytes");
        let u64_at = |off: usize| u64::from_le_bytes(data[off..off + 8].try_into().unwrap());

        let lending_market = pk(OFF_LENDING_MARKET);
        let flash_loan_fee_sf = u64_at(OFF_FLASH_LOAN_FEE_SF);
        if flash_loan_fee_sf == u64::MAX {
            bail!("Flash loans are disabled on Kamino reserve {reserve}");
        }
        Ok(Self {
            reserve,
            lending_market,
            lending_market_authority: lending_market_authority(&lending_market),
            liquidity_mint: pk(OFF_LIQUIDITY_MINT),
            supply_vault: pk(OFF_SUPPLY_VAULT),
            fee_vault: pk(OFF_FEE_VAULT),
            token_program: pk(OFF_LIQUIDITY_TOKEN_PROGRAM),
            flash_loan_fee_sf,
            available_amount: u64_at(OFF_AVAILABLE_AMOUNT),
        })
    }

    pub fn flash_fee(&self, amount: u64) -> Option<u64> {
        flash_fee_lamports(self.flash_loan_fee_sf, amount)
    }

    /// Accounts shared by borrow and repay; klend requires them identical.
    fn accounts(&self, user: Pubkey, user_token_account: Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new_readonly(user, true),
            AccountMeta::new_readonly(self.lending_market_authority, false),
            AccountMeta::new_readonly(self.lending_market, false),
            AccountMeta::new(self.reserve, false),
            AccountMeta::new_readonly(self.liquidity_mint, false),
            AccountMeta::new(self.supply_vault, false),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new(self.fee_vault, false),
            // Optional referrer_token_state / referrer_account: None is
            // encoded as the program id.
            AccountMeta::new_readonly(KLEND_PROGRAM_ID, false),
            AccountMeta::new_readonly(KLEND_PROGRAM_ID, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            AccountMeta::new_readonly(self.token_program, false),
        ]
    }

    pub fn borrow_ix(&self, user: Pubkey, user_token_account: Pubkey, amount: u64) -> Instruction {
        let mut data = FLASH_BORROW_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&amount.to_le_bytes());
        Instruction {
            program_id: KLEND_PROGRAM_ID,
            accounts: self.accounts(user, user_token_account),
            data,
        }
    }

    /// `borrow_ix_index` is the borrow's position in the transaction.
    pub fn repay_ix(
        &self,
        user: Pubkey,
        user_token_account: Pubkey,
        amount: u64,
        borrow_ix_index: u8,
    ) -> Instruction {
        let mut data = FLASH_REPAY_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(borrow_ix_index);
        Instruction {
            program_id: KLEND_PROGRAM_ID,
            accounts: self.accounts(user, user_token_account),
            data,
        }
    }
}

/// Refreshes the reserve's fee and available liquidity into `config` every
/// `interval_ms`, so POOL sizing and the profit floor track the chain.
pub fn spawn_reserve_poller(rpc_url: String, reserve: Pubkey, interval_ms: u64) {
    std::thread::spawn(move || {
        let client = RpcClient::new(rpc_url);
        let mut last_fee_sf = None;
        loop {
            match KaminoReserve::load(&client, &reserve) {
                Ok(r) => {
                    if last_fee_sf.is_some_and(|f| f != r.flash_loan_fee_sf) {
                        tracing::warn!(
                            "Kamino flash-loan fee changed: {:?} -> {}",
                            last_fee_sf,
                            r.flash_loan_fee_sf
                        );
                    }
                    last_fee_sf = Some(r.flash_loan_fee_sf);
                    crate::config::update_flash_reserve(r.flash_loan_fee_sf, r.available_amount);
                }
                Err(e) => tracing::warn!("Kamino reserve refresh failed: {e:#}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        }
    });
}

pub fn lending_market_authority(lending_market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"lma", lending_market.as_ref()], &KLEND_PROGRAM_ID).0
}

/// Upper bound on klend's flash-loan fee: `amount * rate` rounded UP, at
/// least 1 (klend rounds to nearest, so this never under-estimates).
/// `None` on overflow.
pub fn flash_fee_lamports(flash_loan_fee_sf: u64, amount: u64) -> Option<u64> {
    if flash_loan_fee_sf == 0 || amount == 0 {
        return Some(0);
    }
    let product = (amount as u128).checked_mul(flash_loan_fee_sf as u128)?;
    let fee = product.div_ceil(FRACTION_ONE_BITS).max(1);
    u64::try_from(fee).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 0.001% as stored on the mainnet SOL reserve.
    const MAINNET_SOL_FEE_SF: u64 = 11_529_215_046_068;

    #[test]
    fn fee_matches_mainnet_rate() {
        assert_eq!(
            flash_fee_lamports(MAINNET_SOL_FEE_SF, 1_000_000_000),
            Some(10_000)
        );
        assert_eq!(
            flash_fee_lamports(MAINNET_SOL_FEE_SF, 10_000_000),
            Some(100)
        );
        // Minimum fee of 1.
        assert_eq!(flash_fee_lamports(MAINNET_SOL_FEE_SF, 1), Some(1));
        assert_eq!(flash_fee_lamports(0, 1_000_000), Some(0));
    }

    #[test]
    fn borrow_and_repay_share_accounts_and_amount() {
        let r = KaminoReserve {
            reserve: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            lending_market_authority: Pubkey::new_unique(),
            liquidity_mint: Pubkey::new_unique(),
            supply_vault: Pubkey::new_unique(),
            fee_vault: Pubkey::new_unique(),
            token_program: Pubkey::new_unique(),
            flash_loan_fee_sf: MAINNET_SOL_FEE_SF,
            available_amount: 0,
        };
        let user = Pubkey::new_unique();
        let ata = Pubkey::new_unique();
        let b = r.borrow_ix(user, ata, 42);
        let p = r.repay_ix(user, ata, 42, 4);
        assert_eq!(b.accounts, p.accounts);
        assert_eq!(b.accounts[3].pubkey, r.reserve);
        assert_eq!(b.data[8..16], p.data[8..16]);
        assert_eq!(p.data[16], 4);
    }
}
