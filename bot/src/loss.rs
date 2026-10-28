//! Loss analysis: for every real liquidation in our markets, how did we compare?
//! Classifies each one as seen first / below our threshold / same-block / untracked.

use std::collections::{HashMap, HashSet};

use alloy::primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

use crate::{
    morpho::PositionView,
    morpho::{
        events::LiquidationEvent,
        math::{self, ORACLE_PRICE_SCALE},
        onchain::{MarketMeta, MarketSnapshot},
    },
};

type Key = (usize, Address);

/// Positions within this HF of liquidation keep their last HF, to explain same-block losses.
const PREV_HF_KEEP_BELOW: f64 = 1.25;

struct Detection {
    first_block: u64,
    first_hf: f64,
    /// First block where it also passed our min-bonus filter (i.e. we would have acted).
    candidate_block: Option<u64>,
}

#[derive(Default)]
pub struct LossTracker {
    detections: HashMap<Key, Detection>,
    prev_hf: HashMap<Key, f64>,
    plans: HashMap<Key, (u64, Option<f64>, Option<String>)>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LiquidationRow {
    pub kind: String,
    pub block: u64,
    pub tx: B256,
    pub tx_index: u64,
    pub market: String,
    pub borrower: Address,
    /// Liquidator contract (Morpho `caller`).
    pub liquidator: Address,
    /// EOA that sent the transaction.
    pub eoa: Option<Address>,
    pub priority_fee_gwei: Option<f64>,
    pub fee_usd: Option<f64>,
    /// Loan-token units.
    pub repaid: f64,
    /// Seized collateral valued at the oracle price of the block we processed it in.
    pub seized_value: f64,
    pub gross_bonus: f64,
    pub bad_debt: f64,
    /// seen_first | below_min_bonus | interest_edge | same_block | untracked
    pub outcome: String,
    pub our_first_block: Option<u64>,
    pub our_candidate_block: Option<u64>,
    /// Blocks between our first detection and the liquidation.
    pub delay_blocks: Option<u64>,
    pub first_hf: Option<f64>,
    /// Our HF for the position at the last block we evaluated before the liquidation.
    pub prev_block_hf: Option<f64>,
    pub our_plan_block: Option<u64>,
    pub our_plan_net_usd: Option<f64>,
    pub our_plan_error: Option<String>,
}

pub struct TxInfo {
    pub eoa: Address,
    pub priority_fee_wei: u128,
    pub fee_wei: U256,
}

impl LiquidationRow {
    /// Fills the sender / fee fields once the tx lookup is done (it runs off the hot path).
    pub fn apply_tx(&mut self, tx: TxInfo, eth_usd: f64) {
        self.eoa = Some(tx.eoa);
        self.priority_fee_gwei = Some(tx.priority_fee_wei as f64 / 1e9);
        self.fee_usd = Some(math::units(tx.fee_wei, 18) * eth_usd);
    }
}

impl LossTracker {
    /// Call once per evaluated block, after processing that block's liquidations.
    pub fn on_views(&mut self, block: u64, views: &[PositionView], min_bonus: f64) {
        self.prev_hf.clear();
        let mut liquidatable = HashSet::new();
        for v in views {
            if v.hf >= PREV_HF_KEEP_BELOW {
                break; // views are sorted by HF
            }
            let key = (v.market, v.user);
            self.prev_hf.insert(key, v.hf);
            if v.health.is_liquidatable() && v.max_seize > U256::ZERO {
                liquidatable.insert(key);
                let d = self.detections.entry(key).or_insert(Detection { first_block: block, first_hf: v.hf, candidate_block: None });
                if d.candidate_block.is_none() && v.bonus >= min_bonus {
                    d.candidate_block = Some(block);
                }
            }
        }
        // Positions that recovered (or were liquidated) start a fresh episode next time.
        self.detections.retain(|k, _| liquidatable.contains(k));
        self.plans.retain(|k, _| liquidatable.contains(k));
    }

    pub fn on_plan(&mut self, key: Key, block: u64, net_usd: Option<f64>, error: Option<String>) {
        self.plans.entry(key).or_insert((block, net_usd, error));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn classify(
        &self,
        mi: usize,
        ev: &LiquidationEvent,
        meta: &MarketMeta,
        snap: Option<&MarketSnapshot>,
        ever_tracked: bool,
        tx: Option<TxInfo>,
        eth_usd: f64,
    ) -> LiquidationRow {
        let key = (mi, ev.borrower);
        let det = self.detections.get(&key);
        let prev_hf = self.prev_hf.get(&key).copied();
        let outcome = match det {
            Some(d) if d.candidate_block.is_some_and(|b| b < ev.block) => "seen_first",
            Some(d) if d.first_block < ev.block => "below_min_bonus",
            // Sat right at HF 1 and went under from interest alone: predictable one block ahead.
            _ if prev_hf.is_some_and(|h| h < 1.0005) => "interest_edge",
            _ if ever_tracked => "same_block",
            _ => "untracked",
        };
        let repaid = math::units(ev.repaid_assets, meta.loan_decimals);
        let seized_value = snap
            .map(|s| math::units(math::mul_div_down(ev.seized_assets, s.price, ORACLE_PRICE_SCALE), meta.loan_decimals))
            .unwrap_or(0.0);
        let plan = self.plans.get(&key);
        LiquidationRow {
            kind: "liquidation".into(),
            block: ev.block,
            tx: ev.tx,
            tx_index: ev.tx_index,
            market: meta.name.clone(),
            borrower: ev.borrower,
            liquidator: ev.liquidator,
            eoa: tx.as_ref().map(|t| t.eoa),
            priority_fee_gwei: tx.as_ref().map(|t| t.priority_fee_wei as f64 / 1e9),
            fee_usd: tx.as_ref().map(|t| math::units(t.fee_wei, 18) * eth_usd),
            repaid,
            seized_value,
            gross_bonus: seized_value - repaid,
            bad_debt: math::units(ev.bad_debt_assets, meta.loan_decimals),
            outcome: outcome.into(),
            our_first_block: det.map(|d| d.first_block),
            our_candidate_block: det.and_then(|d| d.candidate_block),
            delay_blocks: det.map(|d| ev.block.saturating_sub(d.first_block)),
            first_hf: det.map(|d| d.first_hf),
            prev_block_hf: prev_hf,
            our_plan_block: plan.map(|p| p.0),
            our_plan_net_usd: plan.and_then(|p| p.1),
            our_plan_error: plan.and_then(|p| p.2.clone()),
        }
    }
}

/// Summary over all `liquidation` rows in a journal.
pub fn report(rows: &[LiquidationRow]) {
    if rows.is_empty() {
        println!("no liquidations recorded yet");
        return;
    }
    let n = rows.len();
    let gross: f64 = rows.iter().map(|r| r.gross_bonus).sum();
    let first = rows.iter().map(|r| r.block).min().unwrap_or(0);
    let last = rows.iter().map(|r| r.block).max().unwrap_or(0);
    println!("liquidations: {n} | blocks {first}..{last} | gross bonus (oracle price) {gross:.2}");

    println!("\nby outcome:");
    let mut by: HashMap<&str, (usize, f64)> = HashMap::new();
    for r in rows {
        let e = by.entry(r.outcome.as_str()).or_default();
        e.0 += 1;
        e.1 += r.gross_bonus;
    }
    let mut v: Vec<_> = by.into_iter().collect();
    v.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    for (k, (c, g)) in v {
        let gp = if gross != 0.0 { 100.0 * g / gross } else { 0.0 };
        println!("  {k:<16} {c:>6} ({:>5.1}%)  gross {g:>12.2} ({gp:>5.1}%)", 100.0 * c as f64 / n as f64);
    }

    let mut delays: Vec<u64> = rows.iter().filter(|r| r.outcome == "seen_first").filter_map(|r| r.delay_blocks).collect();
    if !delays.is_empty() {
        delays.sort();
        let q = |p: usize| delays[(delays.len() - 1) * p / 100];
        println!("\nseen_first: blocks from our first detection to their liquidation p10 {} p50 {} p90 {} max {}", q(10), q(50), q(90), q(100));
        let planned: Vec<&LiquidationRow> = rows.iter().filter(|r| r.outcome == "seen_first" && r.our_plan_net_usd.is_some()).collect();
        let net: f64 = planned.iter().filter_map(|r| r.our_plan_net_usd).filter(|x| *x > 0.0).sum();
        println!("  simulated plans for {} of them; sum of positive simulated net {net:.2} USD", planned.len());
    }

    println!("\nby market:");
    let mut bm: HashMap<&str, (usize, f64, usize)> = HashMap::new();
    for r in rows {
        let e = bm.entry(r.market.as_str()).or_default();
        e.0 += 1;
        e.1 += r.gross_bonus;
        if r.outcome == "seen_first" {
            e.2 += 1;
        }
    }
    let mut v: Vec<_> = bm.into_iter().collect();
    v.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    for (m, (c, g, s)) in v {
        println!("  {m:<14} {c:>6} liquidations  gross {g:>12.2}  seen_first {s}");
    }

    println!("\ntop liquidators (by gross):");
    let mut bl: HashMap<(Address, Option<Address>), (usize, f64, f64, Vec<u64>)> = HashMap::new();
    for r in rows {
        let e = bl.entry((r.liquidator, r.eoa)).or_default();
        e.0 += 1;
        e.1 += r.gross_bonus;
        e.2 += r.priority_fee_gwei.unwrap_or(0.0);
        e.3.push(r.tx_index);
    }
    let mut v: Vec<_> = bl.into_iter().collect();
    v.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    for ((c, e), (cnt, g, prio, mut idx)) in v.into_iter().take(10) {
        idx.sort();
        println!(
            "  contract {c:#x} eoa {} | {cnt:>5} liq | gross {g:>10.2} | avg prio {:.4} gwei | median tx index {}",
            e.map(|a| format!("{a:#x}")).unwrap_or("-".into()),
            prio / cnt as f64,
            idx[idx.len() / 2]
        );
    }
}
