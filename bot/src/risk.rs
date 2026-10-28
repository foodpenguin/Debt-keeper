//! Risk gate: decides whether a simulated plan would be sent, and at what priority fee.
//! In shadow mode the decision is only journaled; the executor sends only what it approves.
//!
//! The priority fee comes from `onchain_core::bid::BidCurve`: just above what the winners of past
//! liquidations paid per USD of gross (when enough of them were observed), never more than a share
//! of the net that grows with the size of the opportunity, never below the base priority.

pub use onchain_core::gates::DailyBudget;
use onchain_core::bid::BidCurve;
use serde::Serialize;

use crate::{config::ExecConfig, planner::Plan};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Decision {
    Send {
        priority_fee_wei: u128,
        est_cost_usd: f64,
        /// The predicted competitor priority fee the bid is set against (None = no model yet).
        competitor_wei: Option<u128>,
    },
    Skip {
        reason: String,
    },
}

impl ExecConfig {
    /// The bid curve these settings describe.
    pub fn bid_curve(&self) -> BidCurve {
        BidCurve {
            base_wei: self.priority_fee_wei,
            max_wei: self.max_priority_fee_wei,
            share: self.priority_fee_share,
            share_large: self.bid_share_large.unwrap_or(self.priority_fee_share),
            large_net_usd: self.bid_large_net_usd,
            quantile: self.bid_quantile,
            outbid_bps: self.bid_outbid_bps,
            min_samples: self.bid_min_samples,
        }
    }
}

/// Inputs: the simulated plan, ETH price and base fee for cost math, the kill-switch state, and
/// the competition (`WinningBids::ratio_quantile` for this market; None = no model).
#[allow(clippy::too_many_arguments)]
pub fn decide(cfg: &ExecConfig, budget: &mut DailyBudget, plan: &Plan, eth_usd: f64, base_fee_wei: u128, now_unix: u64, kill_switch: bool, competition: Option<f64>) -> Decision {
    let skip = |r: &str| Decision::Skip { reason: r.to_string() };
    if kill_switch {
        return skip("kill switch file present");
    }
    if let Some(e) = &plan.error {
        return skip(&format!("plan error: {e}"));
    }
    let (Some(net), Some(gas), Some(profit)) = (plan.net_usd, plan.gas_used, plan.profit_usd) else {
        return skip("not simulated");
    };
    if net < cfg.min_net_usd {
        return skip(&format!("net {net:.4} < min {}", cfg.min_net_usd));
    }
    let bid = cfg.bid_curve().priority(competition, profit, net, 0.0, gas, eth_usd);
    let priority_fee_wei = bid.priority_wei;
    // L2 gas at the bid plus the L1 data fee (paid whatever the priority).
    let wei_per_usd = 1e18 / eth_usd.max(1.0);
    let est_cost_usd = gas as f64 * (base_fee_wei + priority_fee_wei) as f64 / wei_per_usd + plan.l1_fee_usd.unwrap_or(0.0);
    if est_cost_usd >= profit {
        return skip(&format!("cost {est_cost_usd:.4} >= profit {profit:.4}"));
    }
    if !budget.allows(now_unix, est_cost_usd, cfg.daily_gas_budget_usd) {
        return skip(&format!("daily gas budget: spent {:.4} + {est_cost_usd:.4} > {}", budget.spent_usd(now_unix), cfg.daily_gas_budget_usd));
    }
    Decision::Send { priority_fee_wei, est_cost_usd, competitor_wei: bid.competitor_wei }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ExecConfig {
        toml::from_str(
            r#"
            executor = "0x000000000000000000000000000000000000dEaD"
            liquidator_artifact = "x"
            kyber_api = "x"
            kyber_client_id = "x"
            slippage_bps = 50
            priority_fee_wei = 1000000
            deadline_blocks = 5
            replan_blocks = 5
            max_plans_per_block = 3
            eth_usd_fallback = 2400.0
            min_net_usd = 0.5
            max_priority_fee_wei = 1000000000
            priority_fee_share = 0.3
            daily_gas_budget_usd = 5.0
            "#,
        )
        .unwrap()
    }

    fn plan(net: f64, gas: u64) -> Plan {
        Plan { sim_profit: Some(net + 0.01), profit_usd: Some(net + 0.01), net_usd: Some(net), gas_used: Some(gas), ..Default::default() }
    }

    #[test]
    fn the_l1_fee_counts_towards_the_cost() {
        // Gas alone fits under the profit; with the L1 fee it does not.
        let mut c = cfg();
        c.min_net_usd = 0.0;
        let mut p = plan(0.01, 1_000);
        p.profit_usd = Some(0.02);
        assert!(matches!(decide(&c, &mut DailyBudget::default(), &p, 2400.0, 1, 0, false, None), Decision::Send { .. }), "tiny gas: sendable");
        p.l1_fee_usd = Some(0.05);
        assert!(matches!(decide(&c, &mut DailyBudget::default(), &p, 2400.0, 1, 0, false, None), Decision::Skip { .. }), "the L1 fee makes it cost more than it earns");
    }

    #[test]
    fn kill_switch_and_errors_win() {
        let c = cfg();
        let mut b = DailyBudget::default();
        assert!(matches!(decide(&c, &mut b, &plan(100.0, 500_000), 2400.0, 5_000_000, 0, true, None), Decision::Skip { .. }));
        let mut p = plan(100.0, 500_000);
        p.error = Some("route too gas-heavy".into());
        assert!(matches!(decide(&c, &mut b, &p, 2400.0, 5_000_000, 0, false, None), Decision::Skip { .. }));
    }

    #[test]
    fn small_profit_skipped() {
        let mut b = DailyBudget::default();
        assert!(matches!(decide(&cfg(), &mut b, &plan(0.2, 500_000), 2400.0, 5_000_000, 0, false, None), Decision::Skip { .. }));
    }

    #[test]
    fn priority_fee_capped_and_floored() {
        let c = cfg();
        let mut b = DailyBudget::default();
        // Huge profit: share would be enormous -> capped at max_priority_fee_wei.
        match decide(&c, &mut b, &plan(10_000.0, 500_000), 2400.0, 5_000_000, 0, false, None) {
            Decision::Send { priority_fee_wei, .. } => assert_eq!(priority_fee_wei, c.max_priority_fee_wei),
            d => panic!("{d:?}"),
        }
        // Share of profit below the floor -> floor (share 0 to force it).
        let mut c0 = cfg();
        c0.priority_fee_share = 0.0;
        match decide(&c0, &mut b, &plan(0.6, 5_000_000), 2400.0, 5_000_000, 0, false, None) {
            Decision::Send { priority_fee_wei, .. } => assert_eq!(priority_fee_wei, c0.priority_fee_wei),
            d => panic!("{d:?}"),
        }
        // In between: 0.6 USD net, 5M gas, 30% share = 0.18 USD / 5M gas = 0.015 gwei on top of the base.
        match decide(&c, &mut b, &plan(0.6, 5_000_000), 2400.0, 5_000_000, 0, false, None) {
            Decision::Send { priority_fee_wei, .. } => assert_eq!(priority_fee_wei, c.priority_fee_wei + 15_000_000),
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn the_bid_follows_the_competition_up_to_what_the_profit_affords() {
        let c = cfg();
        let send = |competition| match decide(&c, &mut DailyBudget::default(), &plan(100.0, 500_000), 2400.0, 5_000_000, 0, false, competition) {
            Decision::Send { priority_fee_wei, competitor_wei, .. } => (priority_fee_wei, competitor_wei),
            d => panic!("{d:?}"),
        };
        // Without a model: 30% of the net, on top of the base priority (capped at 1 gwei).
        let (share_bid, none) = send(None);
        assert_eq!(none, None);
        // Winners paid 1e5 wei per gas per USD of gross: ~1e7 for our $100.01, bid 5% above it.
        let (bid, competitor) = send(Some(1e5));
        assert_eq!(competitor, Some(10_001_000));
        assert_eq!(bid, 10_001_000 + 500_050 + 1);
        assert!(bid < share_bid, "winning needs less than the share would pay");
        // A competition this profit cannot beat: it bids what it affords (the share), no more.
        assert_eq!(send(Some(1e9)).0, share_bid);
    }

    #[test]
    fn daily_budget_blocks_and_resets() {
        let c = cfg();
        let mut b = DailyBudget::default();
        b.charge(0, 4.99);
        // At the cap price (1 gwei * 1M gas ~= $2.4) this exceeds the remaining $0.01 of the $5 budget.
        assert!(matches!(decide(&c, &mut b, &plan(10_000.0, 1_000_000), 2400.0, 5_000_000, 10, false, None), Decision::Skip { .. }));
        // Next UTC day the budget is fresh again.
        let d = decide(&c, &mut b, &plan(10.0, 300_000), 2400.0, 5_000_000, 86_400 + 10, false, None);
        assert!(matches!(d, Decision::Send { .. }), "{d:?}");
    }
}
