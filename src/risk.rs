//! Execution modes and pre-trade risk controls.
//!
//! Everything here is deliberately pure (no RPC, no clock reads except the
//! `Instant` passed in) so every rule can be unit-tested.
//!
//! The three modes, from safest to most dangerous:
//!
//! - `Observe`: log opportunities only. Nothing is built or simulated.
//! - `Paper`  (default): build and simulate the real atomic transaction
//!   against mainnet state, record the result in the paper ledger, never send.
//! - `Live`: simulate, then broadcast. Requires an explicit acknowledgement
//!   phrase in the environment (see `LIVE_ACK_PHRASE`) AND a wallet-funded
//!   capital source with a hard trade-size ceiling.
//!
//! The circuit breakers (consecutive failures, session loss, trade-count cap,
//! kill-switch file) only ever bind in `Live`; in `Paper` a failed simulation
//! is just a data point, not a reason to stop collecting data.

use anyhow::{bail, Context, Result};
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::analyzer::Opportunity;

/// Must be present in `LIVE_TRADING_ACK` for `EXECUTION_MODE=LIVE` to start.
/// Typing it is the "yes, this moves real money" step.
pub const LIVE_ACK_PHRASE: &str = "I_ACCEPT_REAL_FUNDS_RISK";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Observe,
    Paper,
    Live,
}

impl ExecutionMode {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_uppercase().as_str() {
            "OBSERVE" => Ok(Self::Observe),
            "PAPER" | "" => Ok(Self::Paper),
            "LIVE" => Ok(Self::Live),
            other => bail!("Invalid EXECUTION_MODE '{other}': must be OBSERVE, PAPER or LIVE"),
        }
    }
}

impl fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Observe => write!(f, "OBSERVE"),
            Self::Paper => write!(f, "PAPER"),
            Self::Live => write!(f, "LIVE"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RiskLimits {
    /// Reject opportunities whose expected net profit (after fixed tx costs)
    /// is below this. The analyzer approves anything > 0 lamports, which is
    /// within the noise of quote error; this is the margin of safety.
    pub min_profit_lamports: i128,
    /// Hard ceiling on the SOL-side size of a single trade.
    pub max_trade_size_lamports: u64,
    /// The same opportunity (identical sizes and quotes) re-arrives many times
    /// per second from duplicate WebSocket/poll events. Skip repeats inside
    /// this window so one spread is not simulated or sent dozens of times.
    pub dedupe_window: Duration,
    /// Live only: halt after this many failed/losing live trades in a row.
    pub max_consecutive_live_failures: u32,
    /// Live only: halt once cumulative realized loss reaches this many lamports.
    pub max_session_loss_lamports: i128,
    /// Live only: halt after this many live trade attempts in one session.
    /// Keeps the first real runs small and supervised.
    pub max_live_trades: u32,
    /// Live only: if this file exists, nothing is sent. `touch HALT` is the
    /// emergency stop that needs no code change and no restart.
    pub kill_switch_path: String,
    /// Live only: SOL that must stay in the wallet on top of trade size and
    /// fee (rent-exempt minimum, future ATA/rent, safety cushion).
    pub wallet_reserve_lamports: u64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            min_profit_lamports: 50_000,
            max_trade_size_lamports: u64::MAX,
            dedupe_window: Duration::from_millis(1_000),
            max_consecutive_live_failures: 3,
            max_session_loss_lamports: 10_000_000,
            max_live_trades: 10,
            kill_switch_path: "HALT".to_string(),
            wallet_reserve_lamports: 10_000_000,
        }
    }
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: fmt::Display,
{
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("{name}='{v}' is invalid: {e}")),
        _ => Ok(default),
    }
}

impl RiskLimits {
    /// Reads overrides from the environment; anything unset keeps its
    /// conservative default. An unparsable value is a startup error, never a
    /// silent fallback - a typo in a safety limit must not quietly loosen it.
    pub fn from_env(max_trade_size_lamports: u64) -> Result<Self> {
        let d = Self::default();
        Ok(Self {
            min_profit_lamports: env_parse("MIN_PROFIT_LAMPORTS", d.min_profit_lamports)?,
            max_trade_size_lamports,
            dedupe_window: Duration::from_millis(env_parse(
                "DEDUPE_WINDOW_MS",
                d.dedupe_window.as_millis() as u64,
            )?),
            max_consecutive_live_failures: env_parse(
                "MAX_CONSECUTIVE_LIVE_FAILURES",
                d.max_consecutive_live_failures,
            )?,
            max_session_loss_lamports: env_parse(
                "MAX_SESSION_LOSS_LAMPORTS",
                d.max_session_loss_lamports,
            )?,
            max_live_trades: env_parse("MAX_LIVE_TRADES", d.max_live_trades)?,
            kill_switch_path: std::env::var("KILL_SWITCH_FILE").unwrap_or(d.kill_switch_path),
            wallet_reserve_lamports: env_parse(
                "WALLET_RESERVE_LAMPORTS",
                d.wallet_reserve_lamports,
            )?,
        })
    }
}

/// Validates the combination of mode and capital source at startup.
/// `Live` is refused unless every guard is in place.
pub fn validate_live_prerequisites(
    mode: ExecutionMode,
    capital_is_wallet: bool,
    ack: Option<&str>,
) -> Result<()> {
    if mode != ExecutionMode::Live {
        return Ok(());
    }
    if ack.map(str::trim) != Some(LIVE_ACK_PHRASE) {
        bail!(
            "EXECUTION_MODE=LIVE moves real funds. Set LIVE_TRADING_ACK={LIVE_ACK_PHRASE} to confirm. \
             Run PAPER mode first and review paper_trades.csv."
        );
    }
    if !capital_is_wallet {
        bail!(
            "EXECUTION_MODE=LIVE requires MTS=WALLET with MAX_TRADE_SIZE_LAMPORTS set: POOL mode \
             assumes flash-loan capital, which is not implemented, so sizing would be unbounded."
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    BelowMinProfit { have: i128, need: i128 },
    ExceedsMaxTradeSize { size: u64, max: u64 },
    Duplicate,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BelowMinProfit { have, need } => {
                write!(f, "net profit {have} lamports below minimum {need}")
            }
            Self::ExceedsMaxTradeSize { size, max } => {
                write!(f, "trade size {size} lamports exceeds ceiling {max}")
            }
            Self::Duplicate => write!(f, "duplicate of a recently handled opportunity"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaltReason {
    KillSwitch(String),
    ConsecutiveFailures(u32),
    SessionLoss { loss: i128, limit: i128 },
    LiveTradeCap(u32),
}

impl fmt::Display for HaltReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KillSwitch(p) => write!(f, "kill-switch file '{p}' exists"),
            Self::ConsecutiveFailures(n) => write!(f, "{n} consecutive failed/losing live trades"),
            Self::SessionLoss { loss, limit } => {
                write!(f, "session loss {loss} lamports reached limit {limit}")
            }
            Self::LiveTradeCap(n) => write!(f, "live trade cap of {n} reached"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Skip(SkipReason),
    /// Stop trading entirely. Sticky: once halted, always halted.
    Halt(HaltReason),
}

/// Result of one live trade attempt, fed back to the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveOutcome {
    /// Landed. `pnl_lamports` is the measured change in wallet SOL + WSOL.
    Confirmed { pnl_lamports: i128 },
    /// Did not land / reverted. `cost_lamports` is the known fee paid (0 if
    /// it was rejected before landing).
    Failed { cost_lamports: i128 },
}

type Fingerprint = (String, String, u64, u64, u64);

pub struct RiskManager {
    mode: ExecutionMode,
    limits: RiskLimits,
    last_seen: Option<(Fingerprint, Instant)>,
    consecutive_failures: u32,
    session_pnl_lamports: i128,
    live_trades: u32,
    halted: Option<HaltReason>,
}

impl RiskManager {
    pub fn new(mode: ExecutionMode, limits: RiskLimits) -> Self {
        Self {
            mode,
            limits,
            last_seen: None,
            consecutive_failures: 0,
            session_pnl_lamports: 0,
            live_trades: 0,
            halted: None,
        }
    }

    pub fn min_profit_lamports(&self) -> i128 {
        self.limits.min_profit_lamports
    }

    pub fn wallet_reserve_lamports(&self) -> u64 {
        self.limits.wallet_reserve_lamports
    }

    pub fn session_pnl_lamports(&self) -> i128 {
        self.session_pnl_lamports
    }

    pub fn live_trades(&self) -> u32 {
        self.live_trades
    }

    fn fingerprint(opp: &Opportunity) -> Fingerprint {
        (
            opp.buy_dex.clone(),
            opp.sell_dex.clone(),
            opp.trade_size_lamports,
            opp.expected_output_after_buy_raw,
            opp.expected_output_after_sell_raw,
        )
    }

    fn halt(&mut self, reason: HaltReason) -> Decision {
        self.halted = Some(reason.clone());
        Decision::Halt(reason)
    }

    /// Decide whether `opp` may proceed to simulation / execution.
    pub fn check(&mut self, opp: &Opportunity, now: Instant) -> Decision {
        if let Some(reason) = &self.halted {
            return Decision::Halt(reason.clone());
        }

        if self.mode == ExecutionMode::Live {
            if Path::new(&self.limits.kill_switch_path).exists() {
                return self.halt(HaltReason::KillSwitch(self.limits.kill_switch_path.clone()));
            }
            if self.live_trades >= self.limits.max_live_trades {
                return self.halt(HaltReason::LiveTradeCap(self.limits.max_live_trades));
            }
        }

        if opp.net_profit_lamports < self.limits.min_profit_lamports {
            return Decision::Skip(SkipReason::BelowMinProfit {
                have: opp.net_profit_lamports,
                need: self.limits.min_profit_lamports,
            });
        }

        if opp.trade_size_lamports > self.limits.max_trade_size_lamports {
            return Decision::Skip(SkipReason::ExceedsMaxTradeSize {
                size: opp.trade_size_lamports,
                max: self.limits.max_trade_size_lamports,
            });
        }

        let fp = Self::fingerprint(opp);
        if let Some((last_fp, at)) = &self.last_seen {
            if *last_fp == fp && now.saturating_duration_since(*at) < self.limits.dedupe_window {
                return Decision::Skip(SkipReason::Duplicate);
            }
        }
        self.last_seen = Some((fp, now));

        Decision::Allow
    }

    /// Feed back the result of a live trade attempt. May trip a breaker; the
    /// next `check` then returns `Halt`.
    pub fn record_live_outcome(&mut self, outcome: LiveOutcome) {
        self.live_trades += 1;

        let (pnl, is_failure) = match outcome {
            LiveOutcome::Confirmed { pnl_lamports } => (pnl_lamports, pnl_lamports < 0),
            LiveOutcome::Failed { cost_lamports } => (-cost_lamports.abs(), true),
        };

        self.session_pnl_lamports += pnl;

        if is_failure {
            self.consecutive_failures += 1;
        } else {
            self.consecutive_failures = 0;
        }

        if self.consecutive_failures >= self.limits.max_consecutive_live_failures {
            self.halted = Some(HaltReason::ConsecutiveFailures(self.consecutive_failures));
        } else if -self.session_pnl_lamports >= self.limits.max_session_loss_lamports {
            self.halted = Some(HaltReason::SessionLoss {
                loss: -self.session_pnl_lamports,
                limit: self.limits.max_session_loss_lamports,
            });
        }
    }
}

/// Minimum SOL the sell leg must return for the whole transaction to be
/// non-losing: what was put in, plus the fixed transaction cost, plus the
/// required profit margin. Passed to the sell leg as `minimum_amount_out`, so
/// the chain itself reverts the transaction (costing only the fee) rather than
/// completing a trade that loses money.
///
/// The previous floor was `amount_in + 1_000`, which ignored the ~13_750
/// lamport fee and so allowed a "passing" trade to lose money.
pub fn sell_leg_floor_lamports(
    amount_in_lamports: u64,
    fixed_cost_lamports: u64,
    min_profit_lamports: i128,
) -> Option<u64> {
    let margin = u64::try_from(min_profit_lamports.max(0)).ok()?;
    amount_in_lamports
        .checked_add(fixed_cost_lamports)?
        .checked_add(margin)
}

/// Refuses a send the wallet cannot comfortably fund: the trade wraps
/// `trade_size` SOL, pays `fixed_cost`, and must leave `reserve` behind.
pub fn check_wallet_funds(
    sol_balance: u64,
    trade_size: u64,
    fixed_cost: u64,
    reserve: u64,
) -> Result<()> {
    let need = trade_size
        .checked_add(fixed_cost)
        .and_then(|v| v.checked_add(reserve))
        .context("wallet requirement overflow")?;
    if sol_balance < need {
        bail!(
            "Refusing to send: wallet has {sol_balance} lamports but trade needs {need} \
             (size {trade_size} + fee {fixed_cost} + reserve {reserve})"
        );
    }
    Ok(())
}

pub fn read_live_ack() -> Result<Option<String>> {
    Ok(std::env::var("LIVE_TRADING_ACK").ok())
}

/// Convenience for `main`: parse mode from the environment.
pub fn mode_from_env() -> Result<ExecutionMode> {
    let raw = std::env::var("EXECUTION_MODE").unwrap_or_else(|_| "PAPER".to_string());
    ExecutionMode::parse(&raw).context("reading EXECUTION_MODE")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opp(net_profit: i128, size: u64) -> Opportunity {
        Opportunity {
            buy_dex: "Orca".into(),
            sell_dex: "RaydiumCPMM".into(),
            pair: "TEST".into(),
            trade_size_lamports: size,
            expected_output_after_buy_raw: 1_000_000,
            expected_output_after_sell_raw: size + net_profit.max(0) as u64,
            net_profit_lamports: net_profit,
            buy_price: 1.0,
            sell_price: 1.0,
            raw_spread_pct: 0.0,
            fee_adjusted_spread_pct: 0.0,
            net_spread_after_slippage_pct: 0.0,
            net_profit_pct: 0.0,
        }
    }

    fn limits() -> RiskLimits {
        RiskLimits {
            min_profit_lamports: 50_000,
            max_trade_size_lamports: 100_000_000,
            dedupe_window: Duration::from_millis(1_000),
            max_consecutive_live_failures: 3,
            max_session_loss_lamports: 1_000_000,
            max_live_trades: 5,
            kill_switch_path: "definitely/not/a/real/path/HALT".into(),
            wallet_reserve_lamports: 10_000_000,
        }
    }

    #[test]
    fn parses_modes_and_defaults_to_paper() {
        assert_eq!(ExecutionMode::parse("observe").unwrap(), ExecutionMode::Observe);
        assert_eq!(ExecutionMode::parse(" Paper ").unwrap(), ExecutionMode::Paper);
        assert_eq!(ExecutionMode::parse("LIVE").unwrap(), ExecutionMode::Live);
        assert_eq!(ExecutionMode::parse("").unwrap(), ExecutionMode::Paper);
        assert!(ExecutionMode::parse("yolo").is_err());
    }

    #[test]
    fn live_requires_ack_and_wallet_capital() {
        assert!(validate_live_prerequisites(ExecutionMode::Paper, false, None).is_ok());
        assert!(validate_live_prerequisites(ExecutionMode::Live, true, None).is_err());
        assert!(validate_live_prerequisites(ExecutionMode::Live, true, Some("yes")).is_err());
        assert!(
            validate_live_prerequisites(ExecutionMode::Live, false, Some(LIVE_ACK_PHRASE)).is_err()
        );
        assert!(
            validate_live_prerequisites(ExecutionMode::Live, true, Some(LIVE_ACK_PHRASE)).is_ok()
        );
    }

    #[test]
    fn skips_below_min_profit_and_above_size_ceiling() {
        let mut rm = RiskManager::new(ExecutionMode::Paper, limits());
        let now = Instant::now();
        assert_eq!(
            rm.check(&opp(49_999, 1_000_000), now),
            Decision::Skip(SkipReason::BelowMinProfit { have: 49_999, need: 50_000 })
        );
        assert_eq!(
            rm.check(&opp(60_000, 200_000_000), now),
            Decision::Skip(SkipReason::ExceedsMaxTradeSize { size: 200_000_000, max: 100_000_000 })
        );
        assert_eq!(rm.check(&opp(50_000, 1_000_000), now), Decision::Allow);
    }

    #[test]
    fn duplicate_inside_window_is_skipped_then_allowed_after() {
        let mut rm = RiskManager::new(ExecutionMode::Paper, limits());
        let t0 = Instant::now();
        let o = opp(60_000, 1_000_000);
        assert_eq!(rm.check(&o, t0), Decision::Allow);
        assert_eq!(
            rm.check(&o, t0 + Duration::from_millis(500)),
            Decision::Skip(SkipReason::Duplicate)
        );
        assert_eq!(rm.check(&o, t0 + Duration::from_millis(1_500)), Decision::Allow);
    }

    #[test]
    fn different_opportunity_is_not_a_duplicate() {
        let mut rm = RiskManager::new(ExecutionMode::Paper, limits());
        let t0 = Instant::now();
        assert_eq!(rm.check(&opp(60_000, 1_000_000), t0), Decision::Allow);
        assert_eq!(rm.check(&opp(70_000, 2_000_000), t0), Decision::Allow);
    }

    #[test]
    fn consecutive_live_failures_halt_and_stay_halted() {
        let mut rm = RiskManager::new(ExecutionMode::Live, limits());
        let now = Instant::now();
        for _ in 0..3 {
            rm.record_live_outcome(LiveOutcome::Failed { cost_lamports: 5_000 });
        }
        assert_eq!(
            rm.check(&opp(60_000, 1_000_000), now),
            Decision::Halt(HaltReason::ConsecutiveFailures(3))
        );
        // Sticky even for an otherwise perfect opportunity later.
        assert!(matches!(
            rm.check(&opp(9_000_000, 1_000_000), now + Duration::from_secs(60)),
            Decision::Halt(_)
        ));
    }

    #[test]
    fn a_confirmed_profit_resets_the_failure_streak() {
        // Five outcomes are recorded here, so lift the trade cap above 5.
        let mut l = limits();
        l.max_live_trades = 10;
        let mut rm = RiskManager::new(ExecutionMode::Live, l);
        rm.record_live_outcome(LiveOutcome::Failed { cost_lamports: 5_000 });
        rm.record_live_outcome(LiveOutcome::Failed { cost_lamports: 5_000 });
        rm.record_live_outcome(LiveOutcome::Confirmed { pnl_lamports: 80_000 });
        rm.record_live_outcome(LiveOutcome::Failed { cost_lamports: 5_000 });
        rm.record_live_outcome(LiveOutcome::Failed { cost_lamports: 5_000 });
        assert_eq!(
            rm.check(&opp(60_000, 1_000_000), Instant::now()),
            Decision::Allow
        );
    }

    #[test]
    fn a_confirmed_but_losing_trade_counts_as_a_failure() {
        let mut rm = RiskManager::new(ExecutionMode::Live, limits());
        for _ in 0..3 {
            rm.record_live_outcome(LiveOutcome::Confirmed { pnl_lamports: -1_000 });
        }
        assert!(matches!(
            rm.check(&opp(60_000, 1_000_000), Instant::now()),
            Decision::Halt(HaltReason::ConsecutiveFailures(3))
        ));
    }

    #[test]
    fn session_loss_limit_halts() {
        let mut rm = RiskManager::new(ExecutionMode::Live, limits());
        rm.record_live_outcome(LiveOutcome::Confirmed { pnl_lamports: -600_000 });
        rm.record_live_outcome(LiveOutcome::Confirmed { pnl_lamports: 100_000 });
        rm.record_live_outcome(LiveOutcome::Confirmed { pnl_lamports: -600_000 });
        assert_eq!(rm.session_pnl_lamports(), -1_100_000);
        assert!(matches!(
            rm.check(&opp(60_000, 1_000_000), Instant::now()),
            Decision::Halt(HaltReason::SessionLoss { .. })
        ));
    }

    #[test]
    fn live_trade_cap_halts() {
        let mut rm = RiskManager::new(ExecutionMode::Live, limits());
        for _ in 0..5 {
            rm.record_live_outcome(LiveOutcome::Confirmed { pnl_lamports: 60_000 });
        }
        assert_eq!(
            rm.check(&opp(60_000, 1_000_000), Instant::now()),
            Decision::Halt(HaltReason::LiveTradeCap(5))
        );
    }

    #[test]
    fn breakers_never_bind_in_paper_mode() {
        let mut rm = RiskManager::new(ExecutionMode::Paper, limits());
        let path = std::env::temp_dir().join(format!("falcon_halt_paper_{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        rm.limits.kill_switch_path = path.to_string_lossy().into_owned();
        // Kill switch file exists, but Paper mode ignores it.
        assert_eq!(rm.check(&opp(60_000, 1_000_000), Instant::now()), Decision::Allow);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kill_switch_file_halts_live_mode() {
        let path = std::env::temp_dir().join(format!("falcon_halt_live_{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let mut l = limits();
        l.kill_switch_path = path.to_string_lossy().into_owned();
        let mut rm = RiskManager::new(ExecutionMode::Live, l);
        assert!(matches!(
            rm.check(&opp(60_000, 1_000_000), Instant::now()),
            Decision::Halt(HaltReason::KillSwitch(_))
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn wallet_funds_check_requires_size_fee_and_reserve() {
        assert!(check_wallet_funds(1_000, 600, 100, 300).is_ok());
        assert!(check_wallet_funds(999, 600, 100, 300).is_err());
        assert!(check_wallet_funds(u64::MAX, u64::MAX, 1, 0).is_err());
    }

    #[test]
    fn sell_floor_includes_fixed_cost_and_margin() {
        // 1 SOL in, 13_750 fee, 50_000 margin.
        assert_eq!(
            sell_leg_floor_lamports(1_000_000_000, 13_750, 50_000),
            Some(1_000_063_750)
        );
        // A negative margin is treated as zero - the floor never drops
        // below break-even-after-fees.
        assert_eq!(
            sell_leg_floor_lamports(1_000_000_000, 13_750, -5),
            Some(1_000_013_750)
        );
        assert_eq!(sell_leg_floor_lamports(u64::MAX, 1, 0), None);
    }
}
