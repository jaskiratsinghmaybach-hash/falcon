//! Append-only trade ledger (`paper_trades.csv` in PAPER mode,
//! `live_trades.csv` in LIVE). One row per simulated or broadcast attempt, so
//! a paper run can be reviewed afterwards: how many opportunities simulated
//! cleanly, how they failed, and what profit the analyzer expected.

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const HEADER: &str = "timestamp,mode,pair,buy_dex,sell_dex,size_lamports,expected_net_lamports,outcome,units_consumed,signature,realized_pnl_lamports";

pub struct Ledger {
    path: PathBuf,
}

pub struct LedgerRow<'a> {
    pub mode: &'a str,
    pub pair: &'a str,
    pub buy_dex: &'a str,
    pub sell_dex: &'a str,
    pub size_lamports: u64,
    pub expected_net_lamports: i128,
    /// e.g. SIM_OK, SIM_FAILED[class], LIVE_CONFIRMED, LIVE_REVERTED
    pub outcome: &'a str,
    pub units_consumed: Option<u64>,
    pub signature: Option<String>,
    pub realized_pnl_lamports: Option<i128>,
}

/// Keeps a free-text field from breaking the CSV (commas, quotes, newlines).
pub fn sanitize_field(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ',' | '\n' | '\r' | '"' => ' ',
            c => c,
        })
        .collect()
}

impl Ledger {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("Failed to create ledger {}", path.display()))?;
            writeln!(f, "{HEADER}")?;
        }
        Ok(Self { path })
    }

    pub fn append(&self, row: &LedgerRow) -> Result<()> {
        let mut f = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .with_context(|| format!("Failed to open ledger {}", self.path.display()))?;
        writeln!(
            f,
            "{},{},{},{},{},{},{},{},{},{},{}",
            chrono::Utc::now().to_rfc3339(),
            row.mode,
            sanitize_field(row.pair),
            row.buy_dex,
            row.sell_dex,
            row.size_lamports,
            row.expected_net_lamports,
            sanitize_field(row.outcome),
            row.units_consumed.map(|u| u.to_string()).unwrap_or_default(),
            row.signature.clone().unwrap_or_default(),
            row.realized_pnl_lamports
                .map(|p| p.to_string())
                .unwrap_or_default(),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_csv_breakers() {
        assert_eq!(sanitize_field("a,b\n\"c\""), "a b  c ");
    }

    #[test]
    fn ledger_writes_header_once_and_one_line_per_row() {
        let path = std::env::temp_dir().join(format!("falcon_ledger_{}.csv", std::process::id()));
        std::fs::remove_file(&path).ok();
        {
            let l = Ledger::open(&path).unwrap();
            let row = LedgerRow {
                mode: "PAPER",
                pair: "X",
                buy_dex: "Orca",
                sell_dex: "RaydiumCPMM",
                size_lamports: 1,
                expected_net_lamports: 2,
                outcome: "SIM_FAILED[a,b]",
                units_consumed: Some(3),
                signature: None,
                realized_pnl_lamports: None,
            };
            l.append(&row).unwrap();
            l.append(&row).unwrap();
        }
        // Reopening must not write a second header.
        Ledger::open(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], HEADER);
        for l in &lines[1..] {
            assert_eq!(l.split(',').count(), HEADER.split(',').count());
        }
    }
}
