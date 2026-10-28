//! Risk gate: decides whether a simulated plan would be sent, and at what priority fee.
//! In shadow mode the decision is only journaled; the future executor must go through it.

use serde::Serialize;

use crate::{config::ExecConfig, planner::Plan};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Decision {
    Send { priority_fee_wei: u128, est_cost_usd: f64 },
    Skip { reason: String },
}

/// Gas burned by failed (reverted) txs today (resets at UTC midnight). Successful sends are not
/// charged: they paid for their own gas, and charging them would block profitable liquidations
/// on a busy day once a few had landed.
#[derive(Debug, Default)]
pub struct DailyBudget {
    day: u64,
    spent_usd: f64,
}

impl DailyBudget {
    fn roll(&mut self, unix_secs: u64) {
        let day = unix_secs / 86_400;
        if day != self.day {
            self.day = day;
            self.spent_usd = 0.0;
        }
    }

    /// Call with the actual cost of a tx that reverted on-chain.
    pub fn charge(&mut self, unix_secs: u64, usd: f64) {
        self.roll(unix_secs);
        self.spent_usd += usd;
    }
}

/// Inputs: the simulated plan, ETH price and base fee for cost math, and the kill-switch state.
pub fn decide(cfg: &ExecConfig, budget: &mut DailyBudget, plan: &Plan, eth_usd: f64, base_fee_wei: u128, now_unix: u64, kill_switch: bool) -> Decision {
    let skip = |r: &str| Decision::Skip { reason: r.to_string() };
    if kill_switch {
        return skip("kill switch file present");
    }
    if let Some(e) = &plan.error {
        return skip(&format!("plan error: {e}"));
    }
    let (Some(net), Some(gas), Some(profit)) = (plan.net_usd, plan.gas_used, plan.sim_profit) else {
        return skip("not simulated");
    };
    if net < cfg.min_net_usd {
        return skip(&format!("net {net:.4} < min {}", cfg.min_net_usd));
    }
    // Bid a share of the net profit as priority fee, capped; never below the configured floor.
    let wei_per_usd = 1e18 / eth_usd.max(1.0);
    let share_wei = (net * cfg.priority_fee_share * wei_per_usd / gas as f64) as u128;
    let priority_fee_wei = share_wei.clamp(cfg.priority_fee_wei, cfg.max_priority_fee_wei.max(cfg.priority_fee_wei));
    let est_cost_usd = gas as f64 * (base_fee_wei + priority_fee_wei) as f64 / wei_per_usd;
    if est_cost_usd >= profit {
        return skip(&format!("cost {est_cost_usd:.4} >= profit {profit:.4}"));
    }
    budget.roll(now_unix);
    if budget.spent_usd + est_cost_usd > cfg.daily_gas_budget_usd {
        return skip(&format!("daily gas budget: spent {:.4} + {est_cost_usd:.4} > {}", budget.spent_usd, cfg.daily_gas_budget_usd));
    }
    Decision::Send { priority_fee_wei, est_cost_usd }
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
        Plan { sim_profit: Some(net + 0.01), net_usd: Some(net), gas_used: Some(gas), ..Default::default() }
    }

    #[test]
    fn kill_switch_and_errors_win() {
        let c = cfg();
        let mut b = DailyBudget::default();
        assert!(matches!(decide(&c, &mut b, &plan(100.0, 500_000), 2400.0, 5_000_000, 0, true), Decision::Skip { .. }));
        let mut p = plan(100.0, 500_000);
        p.error = Some("route too gas-heavy".into());
        assert!(matches!(decide(&c, &mut b, &p, 2400.0, 5_000_000, 0, false), Decision::Skip { .. }));
    }

    #[test]
    fn small_profit_skipped() {
        let mut b = DailyBudget::default();
        assert!(matches!(decide(&cfg(), &mut b, &plan(0.2, 500_000), 2400.0, 5_000_000, 0, false), Decision::Skip { .. }));
    }

    #[test]
    fn priority_fee_capped_and_floored() {
        let c = cfg();
        let mut b = DailyBudget::default();
        // Huge profit: share would be enormous -> capped at max_priority_fee_wei.
        match decide(&c, &mut b, &plan(10_000.0, 500_000), 2400.0, 5_000_000, 0, false) {
            Decision::Send { priority_fee_wei, .. } => assert_eq!(priority_fee_wei, c.max_priority_fee_wei),
            d => panic!("{d:?}"),
        }
        // Share of profit below the floor -> floor (share 0 to force it).
        let mut c0 = cfg();
        c0.priority_fee_share = 0.0;
        match decide(&c0, &mut b, &plan(0.6, 5_000_000), 2400.0, 5_000_000, 0, false) {
            Decision::Send { priority_fee_wei, .. } => assert_eq!(priority_fee_wei, c0.priority_fee_wei),
            d => panic!("{d:?}"),
        }
        // In between: 0.6 USD net, 5M gas, 30% share = 0.18 USD / 5M gas = 0.015 gwei.
        match decide(&c, &mut b, &plan(0.6, 5_000_000), 2400.0, 5_000_000, 0, false) {
            Decision::Send { priority_fee_wei, .. } => assert_eq!(priority_fee_wei, 15_000_000),
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn daily_budget_blocks_and_resets() {
        let c = cfg();
        let mut b = DailyBudget::default();
        b.charge(0, 4.99);
        // At the cap price (1 gwei * 1M gas ~= $2.4) this exceeds the remaining $0.01 of the $5 budget.
        assert!(matches!(decide(&c, &mut b, &plan(10_000.0, 1_000_000), 2400.0, 5_000_000, 10, false), Decision::Skip { .. }));
        // Next UTC day the budget is fresh again.
        let d = decide(&c, &mut b, &plan(10.0, 300_000), 2400.0, 5_000_000, 86_400 + 10, false);
        assert!(matches!(d, Decision::Send { .. }), "{d:?}");
    }
}
