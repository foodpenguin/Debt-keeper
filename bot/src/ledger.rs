//! The day's money in live mode, kept across restarts: the failed-gas budget (block path), the
//! Atlas bond as it stood at the start of the UTC day, and the block path's realized PnL. A restart
//! must not reset what was already spent today.
//!
//! - Block path PnL per settled tx: what the liquidator paid PROFIT_RECEIVER (its receipt's ERC20
//!   `Transfer`s, in USD by the loan-token price model) minus the gas and L1 fee.
//! - Atlas: a won op pays its gas in ETH out of its proceeds (`gasPayWei`); Atlas then credits the
//!   bond with what it paid beyond the real charge or takes the shortfall, and a failed op's gas is
//!   taken from the bond. Kept apart (`AtlasDay`): the gas won ops paid, the bond's credits and debits
//!   between readings (a debit in an interval with failed ops of ours and no win counts as failed-op
//!   gas), and what the bot itself unbonded (moved out, not lost). Atlas income (the op's profit,
//!   paid to PROFIT_RECEIVER in the loan token) is not counted: the day's PnL is a lower bound, so the
//!   loss stop (net bond debits) can only fire early, never late.
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
    /// The last bond reading (decimal wei): the next reading's change is attributed against it.
    #[serde(default)]
    bond_last_wei: Option<String>,
    #[serde(default)]
    atlas: AtlasDay,
    /// ETH withdrawn from the bond and not yet sent on to PROFIT_RECEIVER (decimal wei).
    #[serde(default)]
    bond_send_pending_wei: Option<String>,
    /// The unbonding atlETH at the last reading (decimal wei).
    #[serde(default)]
    unbonding_last_wei: Option<String>,
    /// A bond tx sent and not yet seen settled.
    #[serde(default)]
    bond_tx: Option<BondTx>,
}

/// The Atlas side of a UTC day, in ETH.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtlasDay {
    /// Our ops that won (ran and paid their bid) / failed (ran or were rejected without winning).
    pub won: u32,
    pub failed: u32,
    /// Gas won ops paid in ETH out of their proceeds (their gasPayWei).
    pub gas_paid_eth: f64,
    /// Bond increases in intervals with a won op: Atlas crediting what won ops overpaid.
    pub bond_credit_eth: f64,
    /// Bond decreases between readings (not counting atlETH moved to unbonding).
    pub bond_debit_eth: f64,
    /// The part of the debits from intervals with failed ops and no win: failed ops' gas.
    pub failed_gas_eth: f64,
    /// atlETH moved from bonded to unbonding (by the bot or an operator): moved out, not lost.
    pub unbonded_eth: f64,
    /// Bond increases in intervals without a won op: a top-up, not income (it does not offset the
    /// day's debits in the loss stop).
    #[serde(default)]
    pub deposit_eth: f64,
}

/// A bond tx (unbond / withdraw / ETH sent on) sent and not yet seen settled, kept across restarts:
/// its effect is applied from its receipt even when this process never sees the executor settle it
/// (a restart, or a tx reported dropped that still landed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BondTx {
    /// `atlas_bond::TAG_*`.
    pub tag: u8,
    pub amount: U256,
    pub hash: alloy::primitives::B256,
    pub sent_block: u64,
    pub deadline_block: u64,
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
    unbonding_now: Option<U256>,
    block_pnl_usd: f64,
    pub atlas: AtlasDay,
    bond_send_pending: Option<U256>,
    bond_tx: Option<BondTx>,
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
        let wei = |v: Option<String>, what: &str| v.map(|w| w.parse::<U256>()).transpose().with_context(|| format!("ledger {}: {what}", path.display()));
        let bond_day_start = wei(s.bond_day_start_wei, "bond_day_start_wei")?;
        // A ledger from before v0.4.0 has no last reading: start from the day's first one.
        let bond_now = wei(s.bond_last_wei, "bond_last_wei")?.or(bond_day_start);
        let bond_send_pending = wei(s.bond_send_pending_wei, "bond_send_pending_wei")?;
        let unbonding_now = wei(s.unbonding_last_wei, "unbonding_last_wei")?;
        Ok(Self { path: Some(path), day: s.day, failed: s.failed, bond_day_start, bond_now, unbonding_now, block_pnl_usd: s.block_pnl_usd, atlas: s.atlas, bond_send_pending, bond_tx: s.bond_tx })
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
            self.atlas = AtlasDay::default();
            // The new day starts from the bond as it is now (read again at the next check).
            self.bond_day_start = self.bond_now;
        }
    }

    /// A settled block-path tx: `income_usd` paid to the profit receiver, `cost_usd` its gas.
    pub fn record_block_tx(&mut self, unix_secs: u64, income_usd: f64, cost_usd: f64) {
        self.roll(unix_secs);
        self.block_pnl_usd += income_usd - cost_usd;
    }

    /// A reading of the signer's bonded and unbonding atlETH, with how many of our ops won and failed
    /// since the previous reading (Atlas's results). The change is attributed from the chain state
    /// alone (not from which of our txs this process saw settle): atlETH that moved from bonded to
    /// unbonding (an unbond, the bot's or an operator's) is not a debit; what is left of a drop is a
    /// debit, failed-op gas if the interval had failed ops and no win; a rise is a credit (a won op
    /// overpaid) only in an interval with a win, else a deposit (a top-up does not offset losses).
    /// Unbonding that leaves (a withdrawal) is not the bond's business. Atlas taking a charge out of
    /// unbonding (only once bonded is exhausted, far below bond_min_eth) is not counted.
    pub fn observe_bond(&mut self, unix_secs: u64, bonded_wei: U256, unbonding_wei: U256, won: u32, failed: u32) {
        self.roll(unix_secs);
        if self.bond_day_start.is_none() {
            self.bond_day_start = Some(bonded_wei);
        }
        let eth = |w: U256| crate::morpho::math::units(w, 18);
        if let Some(prev) = self.bond_now {
            let unbonded = unbonding_wei.saturating_sub(self.unbonding_now.unwrap_or(unbonding_wei));
            self.atlas.unbonded_eth += eth(unbonded);
            let after = bonded_wei + unbonded;
            if after > prev {
                let d = eth(after - prev);
                if won > 0 {
                    self.atlas.bond_credit_eth += d;
                } else {
                    self.atlas.deposit_eth += d;
                }
            } else if after < prev {
                let d = eth(prev - after);
                self.atlas.bond_debit_eth += d;
                if failed > 0 && won == 0 {
                    self.atlas.failed_gas_eth += d;
                }
            }
        }
        self.atlas.won += won;
        self.atlas.failed += failed;
        self.bond_now = Some(bonded_wei);
        self.unbonding_now = Some(unbonding_wei);
    }

    /// A won op paid `gas_pay_wei` of its gas in ETH.
    pub fn record_gas_paid(&mut self, unix_secs: u64, gas_pay_wei: U256) {
        self.roll(unix_secs);
        self.atlas.gas_paid_eth += crate::morpho::math::units(gas_pay_wei, 18);
    }

    /// The bond tx sent and not yet seen settled (None = none).
    pub fn bond_tx(&self) -> Option<BondTx> {
        self.bond_tx
    }

    pub fn set_bond_tx(&mut self, tx: Option<BondTx>) {
        self.bond_tx = tx;
    }

    /// ETH withdrawn from the bond and not yet sent on to PROFIT_RECEIVER (survives restarts).
    pub fn bond_send_pending(&self) -> Option<U256> {
        self.bond_send_pending
    }

    pub fn set_bond_send_pending(&mut self, wei: Option<U256>) {
        self.bond_send_pending = wei;
    }

    /// The last bond reading (None = not read yet).
    pub fn bond_wei(&self) -> Option<U256> {
        self.bond_now
    }

    /// What the bond lost today on its own account, in USD: debits net of credits (failed ops' gas,
    /// won ops that paid less than charged). Unbonding is not a loss, and a top-up (a rise without a
    /// win) is a deposit, not a credit: it does not offset the day's losses.
    pub fn atlas_loss_usd(&self, eth_usd: f64) -> f64 {
        (self.atlas.bond_debit_eth - self.atlas.bond_credit_eth).max(0.0) * eth_usd
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
        let s = Stored {
            day: self.day,
            failed: self.failed.clone(),
            bond_day_start_wei: self.bond_day_start.map(|w| w.to_string()),
            block_pnl_usd: self.block_pnl_usd,
            bond_last_wei: self.bond_now.map(|w| w.to_string()),
            atlas: self.atlas.clone(),
            bond_send_pending_wei: self.bond_send_pending.map(|w| w.to_string()),
            unbonding_last_wei: self.unbonding_now.map(|w| w.to_string()),
            bond_tx: self.bond_tx,
        };
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
        l.observe_bond(now, U256::from(10u64).pow(U256::from(16u64)), U256::ZERO, 0, 0); // 0.01 ETH
        l.observe_bond(now + 1, U256::from(9u64) * U256::from(10u64).pow(U256::from(15u64)), U256::ZERO, 0, 1); // 0.009
        l.save().unwrap();
        let mut back = Ledger::load(path.clone()).unwrap();
        assert_eq!(back.failed.spent_usd(now + 2), 1.5, "failed gas survives a restart");
        // The start-of-day bond survives; the current one is read again after the restart.
        back.observe_bond(now + 2, U256::from(9u64) * U256::from(10u64).pow(U256::from(15u64)), U256::ZERO, 0, 0);
        assert!((back.atlas_loss_usd(2000.0) - 2.0).abs() < 1e-9, "0.001 ETH shrinkage at $2000");
        assert!((back.pnl_usd(now + 3, 2000.0) - (2.75 - 2.0)).abs() < 1e-9);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_new_day_starts_from_the_current_bond() {
        let mut l = Ledger::in_memory();
        let day = 86_400 * 7;
        l.observe_bond(day, U256::from(100u64), U256::ZERO, 0, 0);
        l.observe_bond(day + 10, U256::from(60u64), U256::ZERO, 0, 0);
        l.record_block_tx(day + 20, 0.0, 1.0);
        assert!(l.pnl_usd(day + 30, 1e18) < 0.0);
        // Next UTC day: the PnL restarts at zero and the bond baseline is the last reading.
        assert_eq!(l.pnl_usd(day + 86_400, 1e18), 0.0);
        l.observe_bond(day + 86_401, U256::from(60u64), U256::ZERO, 0, 0);
        assert_eq!(l.atlas_loss_usd(1e18), 0.0);
    }

    fn milli(m: u64) -> U256 {
        U256::from(m) * U256::from(10u64).pow(U256::from(15u64))
    }

    #[test]
    fn the_bond_side_is_kept_apart() {
        let mut l = Ledger::in_memory();
        let t = 86_400 * 9;
        l.observe_bond(t, milli(100), U256::ZERO, 0, 0);
        // A win that overpaid: credited. Its gas, paid in ETH, is recorded on its own.
        l.record_gas_paid(t + 1, milli(10));
        l.observe_bond(t + 2, milli(102), U256::ZERO, 1, 0);
        // A failed op and no win: the drop is failed-op gas.
        l.observe_bond(t + 3, milli(99), U256::ZERO, 0, 1);
        // A win that underpaid: a debit, but not failed-op gas.
        l.observe_bond(t + 4, milli(98), U256::ZERO, 1, 0);
        // A win and a failure in one interval: the drop cannot be told apart, so it is a debit but
        // not counted as failed-op gas.
        l.observe_bond(t + 5, milli(97), U256::ZERO, 1, 1);
        let a = &l.atlas;
        assert_eq!((a.won, a.failed), (3, 2));
        assert!((a.gas_paid_eth - 0.010).abs() < 1e-12);
        assert!((a.bond_credit_eth - 0.002).abs() < 1e-12);
        assert!((a.bond_debit_eth - 0.005).abs() < 1e-12);
        assert!((a.failed_gas_eth - 0.003).abs() < 1e-12, "only the interval with a failure and no win");
        // Net loss: 0.005 debited - 0.002 credited.
        assert!((l.atlas_loss_usd(1000.0) - 3.0).abs() < 1e-9);
    }

    /// An unbond — the bot's, seen before its tx settled, after a restart, or an operator's by hand —
    /// moves bonded to unbonding: read from the chain, it is never a loss (review A#1/B#3), and the
    /// withdrawal that empties the unbonding later is not one either.
    #[test]
    fn unbonding_is_never_a_loss_whoever_did_it() {
        let mut l = Ledger::in_memory();
        let t = 86_400 * 10;
        l.observe_bond(t, milli(120), U256::ZERO, 0, 0);
        l.observe_bond(t + 1, milli(100), milli(20), 0, 0);
        assert_eq!((l.atlas.bond_debit_eth, l.atlas_loss_usd(1000.0)), (0.0, 0.0));
        assert!((l.atlas.unbonded_eth - 0.020).abs() < 1e-12);
        // Withdrawn: the unbonding leaves; still no loss.
        l.observe_bond(t + 2, milli(100), U256::ZERO, 0, 0);
        assert_eq!(l.atlas_loss_usd(1000.0), 0.0);
        // An unbond in the same interval as a failed op's charge: only the charge is a debit.
        l.observe_bond(t + 3, milli(79), milli(20), 0, 1);
        assert!((l.atlas.bond_debit_eth - 0.001).abs() < 1e-12 && (l.atlas.failed_gas_eth - 0.001).abs() < 1e-12);
    }

    /// A top-up is a deposit, not income: it cannot hide the day's failed-op losses from the loss
    /// stop (review B#7). Only a rise in an interval with a won op is a credit.
    #[test]
    fn a_top_up_does_not_offset_the_days_losses() {
        let mut l = Ledger::in_memory();
        let t = 86_400 * 11;
        l.observe_bond(t, milli(100), U256::ZERO, 0, 0);
        l.observe_bond(t + 1, milli(95), U256::ZERO, 0, 2); // failed ops: 0.005
        l.observe_bond(t + 2, milli(145), U256::ZERO, 0, 0); // operator adds 0.05
        assert!((l.atlas.deposit_eth - 0.050).abs() < 1e-12);
        assert_eq!(l.atlas.bond_credit_eth, 0.0);
        assert!((l.atlas_loss_usd(1000.0) - 5.0).abs() < 1e-9, "the 0.005 loss still counts");
    }

    #[test]
    fn a_withdrawal_waiting_to_be_sent_survives_a_restart() {
        let path = tmp("pending");
        let _ = std::fs::remove_file(&path);
        let mut l = Ledger::load(path.clone()).unwrap();
        l.observe_bond(86_400 * 3, milli(90), milli(4), 0, 0);
        l.set_bond_send_pending(Some(milli(25)));
        let tx = BondTx { tag: 2, amount: milli(25), hash: alloy::primitives::B256::repeat_byte(7), sent_block: 100, deadline_block: 150 };
        l.set_bond_tx(Some(tx));
        l.save().unwrap();
        let mut back = Ledger::load(path.clone()).unwrap();
        assert_eq!(back.bond_send_pending(), Some(milli(25)));
        assert_eq!(back.bond_tx(), Some(tx), "the bond tx in flight survives, to be reconciled from its receipt");
        assert_eq!(back.bond_wei(), Some(milli(90)), "the last reading is kept, not only the day's first");
        // The unbonding baseline survives too: no false unbond on the first reading after a restart.
        back.observe_bond(86_400 * 3 + 5, milli(90), milli(4), 0, 0);
        assert_eq!(back.atlas.unbonded_eth, 0.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_ledger_from_before_the_bond_band_still_loads() {
        let path = tmp("old");
        std::fs::write(&path, "v1\n{\"day\":5,\"failed\":{\"day\":5,\"spent_usd\":0.0},\"bond_day_start_wei\":\"1000\",\"block_pnl_usd\":1.5}\n").unwrap();
        let l = Ledger::load(path.clone()).unwrap();
        assert_eq!((l.bond_wei(), l.atlas.clone(), l.bond_send_pending()), (Some(U256::from(1000u64)), AtlasDay::default(), None));
        let _ = std::fs::remove_file(&path);
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
