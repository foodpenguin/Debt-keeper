//! The day's money in live mode, kept across restarts: the failed-gas budget (block path), the
//! Atlas bond as it stood at the start of the UTC day, and the block path's realized PnL. A restart
//! must not reset what was already spent today.
//!
//! - Block path PnL per settled tx: what the liquidator paid PROFIT_RECEIVER (its receipt's ERC20
//!   `Transfer`s, in USD by the loan-token price model) minus the gas and L1 fee.
//! - Atlas: what an op cost is not seen per op (it runs inside someone else's metacall); the bond's
//!   shrinkage over the day is (`bonded(day start) - bonded(now)`). Atlas income is not counted
//!   until reconciliation exists: the day's PnL is a lower bound, so the loss stop can only fire
//!   early, never late.
//!
//! File: line 1 `v1`, line 2 JSON; written to `<path>.tmp` then renamed. A file this version cannot
//! read refuses live mode (see `load`). Reservations of txs in flight are not persisted: a crash
//! with one in flight forgets at most one tx's worst case (`[exec] max_tx_cost_eth`).

use std::path::{Path, PathBuf};

use alloy::primitives::{Address, U256};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::gates::DailyBudget;

const FORMAT: &str = "v1";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Stored {
    day: u64,
    failed: DailyBudget,
    /// atlETH bonded at the day's first reading (decimal wei).
    bond_day_start_wei: Option<String>,
    block_pnl_usd: f64,
}

#[derive(Debug, Default)]
pub struct Ledger {
    /// None = in memory only (shadow mode).
    path: Option<PathBuf>,
    day: u64,
    /// Failed-gas spend and the reservations of txs in flight (`[exec] daily_gas_budget_usd`).
    pub failed: DailyBudget,
    bond_day_start: Option<U256>,
    bond_now: Option<U256>,
    block_pnl_usd: f64,
}

impl Ledger {
    /// The ledger at `path`; a missing file is a fresh day. An unreadable one is an error: live
    /// mode must not start from a spend it cannot see.
    pub fn load(path: PathBuf) -> Result<Self> {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self { path: Some(path), ..Default::default() }),
            Err(e) => return Err(e).with_context(|| format!("reading ledger {}", path.display())),
        };
        let (version, json) = text.split_once('\n').unwrap_or((text.as_str(), ""));
        anyhow::ensure!(version.trim() == FORMAT, "ledger {} has format {:?}, this bot reads {FORMAT:?} (move the file away to start a fresh day)", path.display(), version.trim());
        let s: Stored = serde_json::from_str(json.trim()).with_context(|| format!("parsing ledger {}", path.display()))?;
        let bond_day_start = s.bond_day_start_wei.map(|w| w.parse::<U256>()).transpose().with_context(|| format!("ledger {}: bond_day_start_wei", path.display()))?;
        Ok(Self { path: Some(path), day: s.day, failed: s.failed, bond_day_start, bond_now: bond_day_start, block_pnl_usd: s.block_pnl_usd })
    }

    /// An in-memory ledger (shadow mode, tests).
    pub fn in_memory() -> Self {
        Self::default()
    }

    fn roll(&mut self, unix_secs: u64) {
        let day = unix_secs / 86_400;
        if day != self.day {
            self.day = day;
            self.block_pnl_usd = 0.0;
            // The new day starts from the bond as it is now (read again at the next check).
            self.bond_day_start = self.bond_now;
        }
    }

    /// A settled block-path tx: `income_usd` paid to the profit receiver, `cost_usd` its gas.
    pub fn record_block_tx(&mut self, unix_secs: u64, income_usd: f64, cost_usd: f64) {
        self.roll(unix_secs);
        self.block_pnl_usd += income_usd - cost_usd;
    }

    /// A reading of the atlETH bonded by the solver signer.
    pub fn observe_bond(&mut self, unix_secs: u64, bonded_wei: U256) {
        self.roll(unix_secs);
        if self.bond_day_start.is_none() {
            self.bond_day_start = Some(bonded_wei);
        }
        self.bond_now = Some(bonded_wei);
    }

    /// The last bond reading (None = not read yet).
    pub fn bond_wei(&self) -> Option<U256> {
        self.bond_now
    }

    /// How much the bond shrank today, in USD (a deposit counts as no loss).
    pub fn atlas_loss_usd(&self, eth_usd: f64) -> f64 {
        match (self.bond_day_start, self.bond_now) {
            (Some(s), Some(n)) => crate::morpho::math::units(s.saturating_sub(n), 18) * eth_usd,
            _ => 0.0,
        }
    }

    /// Today's realized PnL so far: block path receipts minus the bond's shrinkage.
    pub fn pnl_usd(&mut self, unix_secs: u64, eth_usd: f64) -> f64 {
        self.roll(unix_secs);
        self.block_pnl_usd - self.atlas_loss_usd(eth_usd)
    }

    /// Writes the ledger (no-op in memory).
    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let s = Stored { day: self.day, failed: self.failed.clone(), bond_day_start_wei: self.bond_day_start.map(|w| w.to_string()), block_pnl_usd: self.block_pnl_usd };
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, format!("{FORMAT}\n{}\n", serde_json::to_string(&s)?)).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// `save`, logging (not failing) on error: a full disk must not stop the loop mid-block.
    pub fn save_or_warn(&self) {
        if let Err(e) = self.save() {
            warn!(error = format!("{e:#}"), "ledger write failed");
        }
    }
}

/// Where the ledger of `wallet` lives: `[exec] ledger_file` (Config::load makes a relative one
/// relative to the config file), else next to the shared nonce files.
pub fn ledger_path(ledger_file: &str, nonce_path: &Path, wallet: Address) -> PathBuf {
    if ledger_file.is_empty() {
        return nonce_path.parent().unwrap_or(Path::new(".")).join(format!("ledger-{wallet:#x}"));
    }
    PathBuf::from(ledger_file)
}

/// Writes the kill switch file with why and when: every send is refused until a person removes it.
pub fn write_kill_switch(path: &str, reason: &str) -> Result<()> {
    anyhow::ensure!(!path.is_empty(), "no exec.kill_switch_file configured");
    if let Some(dir) = Path::new(path).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(path, format!("{reason}\nwritten at unix {}\n", crate::gates::unix_secs())).with_context(|| format!("writing kill switch {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ledger-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("ledger")
    }

    #[test]
    fn a_restart_keeps_the_days_spend_and_bond() {
        let path = tmp("restart");
        let _ = std::fs::remove_file(&path);
        let now = 86_400 * 100 + 50;
        let mut l = Ledger::load(path.clone()).unwrap();
        l.failed.charge(now, 1.5);
        l.record_block_tx(now, 3.0, 0.25);
        l.observe_bond(now, U256::from(10u64).pow(U256::from(16u64))); // 0.01 ETH
        l.observe_bond(now + 1, U256::from(9u64) * U256::from(10u64).pow(U256::from(15u64))); // 0.009
        l.save().unwrap();
        let mut back = Ledger::load(path.clone()).unwrap();
        assert_eq!(back.failed.spent_usd(now + 2), 1.5, "failed gas survives a restart");
        // The start-of-day bond survives; the current one is read again after the restart.
        back.observe_bond(now + 2, U256::from(9u64) * U256::from(10u64).pow(U256::from(15u64)));
        assert!((back.atlas_loss_usd(2000.0) - 2.0).abs() < 1e-9, "0.001 ETH shrinkage at $2000");
        assert!((back.pnl_usd(now + 3, 2000.0) - (2.75 - 2.0)).abs() < 1e-9);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_new_day_starts_from_the_current_bond() {
        let mut l = Ledger::in_memory();
        let day = 86_400 * 7;
        l.observe_bond(day, U256::from(100u64));
        l.observe_bond(day + 10, U256::from(60u64));
        l.record_block_tx(day + 20, 0.0, 1.0);
        assert!(l.pnl_usd(day + 30, 1e18) < 0.0);
        // Next UTC day: the PnL restarts at zero and the bond baseline is the last reading.
        assert_eq!(l.pnl_usd(day + 86_400, 1e18), 0.0);
        l.observe_bond(day + 86_401, U256::from(60u64));
        assert_eq!(l.atlas_loss_usd(1e18), 0.0);
    }

    #[test]
    fn an_unreadable_ledger_refuses_to_load() {
        let path = tmp("bad");
        std::fs::write(&path, "v9\n{}\n").unwrap();
        let err = Ledger::load(path.clone()).unwrap_err().to_string();
        assert!(err.contains("format"), "{err}");
        std::fs::write(&path, "v1\nnot json\n").unwrap();
        assert!(Ledger::load(path.clone()).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_ledger_path_follows_the_config() {
        let nonce = Path::new("/state/onchain-bots/nonce-0xabc");
        let w = Address::repeat_byte(0xab);
        assert_eq!(ledger_path("", nonce, w), PathBuf::from(format!("/state/onchain-bots/ledger-{w:#x}")));
        assert_eq!(ledger_path("/abs/l", nonce, w), PathBuf::from("/abs/l"));
    }
}
