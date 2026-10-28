//! Turns a liquidatable position into a concrete, simulated liquidation plan, for any
//! `LendingProtocol` executed by any `LiquidationVenue`.
//! Two exit routers: the self-built direct V3-pool router (fast, exact, cheap in gas) and the
//! KyberSwap aggregator (wider venue coverage). In `best` mode both run in parallel and the one
//! with the higher simulated net wins; the loser's result is kept for comparison.

use std::time::Instant;

use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes, U256},
    providers::Provider,
    rpc::types::state::StateOverride,
};
use anyhow::Result;
use futures::future::join_all;
use serde::Serialize;

use crate::{
    config::ExecConfig,
    dex::{Dex, ExitRoute},
    engine::PositionView,
    num,
    protocol::LendingProtocol,
    v3router::{self, V3Router},
    venue::{LiquidationVenue, SimAt, SimFee, SimResult, SimReverted},
};

/// Fractions of the max seizable collateral the aggregator path tries.
const SEIZE_FRACTIONS_BPS: [u64; 3] = [10_000, 5_000, 2_500];
/// Gas a second, third... direct path adds beyond its own swap (pool call + callback + transfer).
const GAS_PER_EXTRA_PATH: u64 = 60_000;
/// Bundle valuation: gas per liquidated borrower inside one solver op. Calibrated on an observed
/// Base SVR metacall: 32 liquidations + Aave supply/borrow + oracle update = 3.73M gas.
pub const BUNDLE_GAS_PER_POSITION: u64 = 110_000;
/// Bundle valuation: gas of a solver op besides liquidations and the swap (oracle update inside
/// the metacall, Atlas bookkeeping, paying the bid).
pub const BUNDLE_OVERHEAD_GAS: u64 = 600_000;
/// Least gas worth leaving a bundle's swap (a 1-2 path exit of a big bundle: ~0.5-1M).
const MIN_BUNDLE_SWAP_GAS: u64 = 1_000_000;

/// How many positions one solver op of `solver_gas_limit` can liquidate while leaving
/// MIN_BUNDLE_SWAP_GAS for the swap (6M -> 40).
pub fn bundle_capacity(solver_gas_limit: u64) -> usize {
    (solver_gas_limit.saturating_sub(BUNDLE_OVERHEAD_GAS + MIN_BUNDLE_SWAP_GAS) / BUNDLE_GAS_PER_POSITION) as usize
}

/// Gas a bundle of `n` positions leaves its swap inside one solver op.
pub fn bundle_swap_budget(solver_gas_limit: u64, n: usize) -> u64 {
    solver_gas_limit.saturating_sub(BUNDLE_OVERHEAD_GAS + BUNDLE_GAS_PER_POSITION * n as u64)
}

/// Gas of one liquidation outside its swap (protocol liquidate, callback, checks, transfers): what a
/// single-position plan reserves under max_gas before sizing the Kyber route.
const LIQUIDATION_OVERHEAD_GAS: u64 = 400_000;

/// One Atlas-style bundle: every liquidatable position of a market liquidated together and the
/// collateral sold as one exit (see `Planner::quote_bundle`).
#[derive(Debug, Serialize, Default, Clone)]
pub struct BundleQuote {
    pub positions: usize,
    /// Exit that won: "direct" (own V3 router) or the aggregator's `Dex::name()` ("kyber").
    pub router: String,
    /// Share of the bundle's total seizable collateral the best exit sells.
    pub fraction_bps: u64,
    /// Exit output minus repaid debt, in USD (`LendingProtocol::loan_usd`), before gas.
    pub gross_usd: f64,
    pub gas_usd: f64,
    /// Gas of the whole solver op as estimated (liquidations + swap + overhead).
    pub gas_units: u64,
    pub net_usd: Option<f64>,
    /// Gas the swap may use inside the solver op, and what the winning exit is estimated at.
    pub swap_gas_budget: u64,
    pub swap_gas: u64,
    pub direct_net_usd: Option<f64>,
    pub kyber_net_usd: Option<f64>,
    pub error: Option<String>,
    pub quote_ms: u64,
}

/// One candidate exit for a bundle: (router, fraction_bps, gross, swap gas).
type BundleExit = (&'static str, u64, f64, u64);

/// Total collateral a bundle seizes and the debt it repays (each position at its max seize).
fn bundle_totals<L: LendingProtocol>(protocol: &L, market: &L::Market, snap: &L::Snapshot, views: &[PositionView<L>]) -> (U256, U256) {
    views.iter().fold((U256::ZERO, U256::ZERO), |(s, r), v| (s + v.max_seize, r + protocol.repay_for_seize(market, snap, v.max_seize)))
}

/// Debt repaid when a bundle seizes `s` of its `total_seize`: linear at a fixed price, rounded up.
fn bundle_repay(s: U256, total_seize: U256, total_repaid: U256) -> U256 {
    num::mul_div_up(s, total_repaid, total_seize)
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct Plan {
    /// "direct" (own V3 router) or "kyber"
    pub router: String,
    pub fraction_bps: u64,
    pub seize: String,
    pub repaid: String,
    /// Exit quote minus repaid debt (loan-token units), before gas.
    pub quoted_gross: f64,
    /// Profit the simulated contract call returned (loan-token units).
    pub sim_profit: Option<f64>,
    /// The same in USD (`LendingProtocol::loan_usd`).
    pub profit_usd: Option<f64>,
    pub gas_used: Option<u64>,
    /// L2 gas at the planning priority plus the L1 data fee, in USD.
    pub gas_cost_usd: Option<f64>,
    /// The L1 data fee part of gas_cost_usd (paid whatever the priority fee).
    pub l1_fee_usd: Option<f64>,
    pub net_usd: Option<f64>,
    pub error: Option<String>,
    pub quote_ms: u64,
    pub sim_ms: u64,
    /// Direct router: number of pool hops / paths used.
    pub hops: usize,
    pub paths_used: usize,
    /// Direct router: swap gas predicted from the quoter (before simulation).
    pub est_swap_gas: Option<u64>,
    /// Block the node simulated (pending plans: tells a fresh in-progress-block state from a stale one).
    pub sim_block: Option<u64>,
    /// The price probe (PlanCtx::price_probe) saw the old price: this state predates the update.
    pub stale_price: bool,
    /// The other router's outcome in `best` mode (for comparison in the journal).
    pub alt: Option<Box<Plan>>,
    /// What to send if the risk gate approves (minProfit is patched in at send time).
    #[serde(skip)]
    pub payload: Option<crate::executor::Payload>,
}

impl Plan {
    fn score(&self) -> f64 {
        if self.error.is_some() { f64::NEG_INFINITY } else { self.net_usd.unwrap_or(f64::NEG_INFINITY) }
    }
}

pub struct Planner<'a, L: LendingProtocol, V: LiquidationVenue<L>, A: Dex> {
    pub protocol: &'a L,
    /// External swap aggregator for the aggregator exit (KyberSwap in production).
    pub aggregator: &'a A,
    pub venue: &'a V,
    pub cfg: &'a ExecConfig,
    pub v3: Option<&'a V3Router>,
    /// "best" | "direct" | "aggregator" (alias "kyber")
    pub mode: &'a str,
}

pub struct PlanCtx<'a, L: LendingProtocol> {
    pub meta: &'a L::Market,
    pub snap: &'a L::Snapshot,
    pub view: &'a PositionView<L>,
    pub block: u64,
    pub base_fee_wei: u128,
    pub eth_usd: f64,
    /// Extra state overrides for the simulation (e.g. a mocked oracle in what-if runs).
    pub overrides: StateOverride,
    /// Last block the tx may be included in (from deadline_secs and the observed block time).
    pub deadline_block: u64,
    /// Timestamp the simulation runs at: the next block's (= the time interest was accrued to
    /// when the candidate was flagged). The simulated block number is `block + 1`.
    pub sim_time: u64,
    /// Quote and simulate on the "pending" block as the sequencer is building it (it already is
    /// block + 1 with its own time, so no block override). CLI only (`plan --pending`): measured
    /// 2026-10-19, alchemy/ankr/preconf often run "pending" calls 1-5 blocks behind latest, where
    /// the snapshot the candidate came from no longer matches. The live paths pin `block`.
    pub pending: bool,
    /// Read this oracle in the simulated state and require its price to be (closer to) `new` than
    /// `old`: on an SVR secondary, proof that the node's pending state already holds the metacall.
    pub price_probe: Option<PriceProbe>,
}

#[derive(Debug, Clone, Copy)]
pub struct PriceProbe {
    pub oracle: Address,
    pub old: U256,
    pub new: U256,
}

impl PriceProbe {
    /// Is `seen` the new price rather than the old one? (Closer to new: another feed of the same
    /// oracle may have moved too.) Unknown = no.
    pub fn is_new(&self, seen: Option<U256>) -> bool {
        seen.is_some_and(|s| s.abs_diff(self.new) < s.abs_diff(self.old))
    }
}

impl<L: LendingProtocol> PlanCtx<'_, L> {
    pub fn block_id(&self) -> BlockId {
        if self.pending { BlockId::pending() } else { BlockId::number(self.block) }
    }

    pub fn sim_at(&self) -> Option<SimAt> {
        (!self.pending).then_some(SimAt { number: self.block + 1, time: self.sim_time })
    }

    /// For a "pending" plan: did the node really simulate on top of the in-progress block
    /// (block + 1), i.e. as block + 2 or later? Unknown (no sim ran, or no number) = not fresh.
    pub fn is_fresh(&self, plan: &Plan) -> bool {
        is_fresh_block(plan.sim_block, self.block + 2) && !plan.stale_price
    }
}

fn is_fresh_block(sim_block: Option<u64>, floor: u64) -> bool {
    sim_block.is_some_and(|n| n >= floor)
}

/// Why a "pending" simulation cannot be trusted, if it cannot: the node said it simulated an
/// older block than `floor`, or it succeeded without saying which block. A sim that failed
/// for another reason (transport, revert on an unknown block) keeps its own error.
fn pending_staleness(sim_block: Option<u64>, sim_ok: bool, floor: u64) -> Option<String> {
    match sim_block {
        Some(n) if n < floor => Some(format!("stale pending state: node simulated block {n} (want >= {floor})")),
        None if sim_ok => Some("stale pending state: node did not say which block it simulated".into()),
        _ => None,
    }
}

impl<L: LendingProtocol, V: LiquidationVenue<L>, A: Dex> Planner<'_, L, V, A> {
    pub async fn plan<P: Provider>(&self, p: &P, c: &PlanCtx<'_, L>) -> Result<Plan> {
        let pools = self.v3.is_some_and(|v| v.routes.get(&c.view.market).is_some_and(|r| !r.is_empty()));
        // The direct router's route is packed: only a venue with a packed entry can take it.
        let direct_ok = pools && self.venue.packed_routes();
        match self.mode {
            "aggregator" | "kyber" => self.plan_aggregator(p, c).await,
            "direct" if direct_ok => self.plan_direct(p, c).await,
            "direct" => {
                let why = if pools { "this venue has no packed-route entry" } else { "no direct pools for this market" };
                Ok(Plan { router: "direct".into(), error: Some(why.into()), ..Default::default() })
            }
            _ => {
                if !direct_ok {
                    return self.plan_aggregator(p, c).await;
                }
                let (d, k) = tokio::join!(self.plan_direct(p, c), self.plan_aggregator(p, c));
                let (d, k) = (d?, k?);
                let (mut win, lose) = if d.score() >= k.score() { (d, k) } else { (k, d) };
                win.alt = Some(Box::new(lose));
                Ok(win)
            }
        }
    }

    /// The fee fields every plan's simulation carries: what a non-bidding send would pay (max fee
    /// = 2 x base fee + the base priority fee). A bid is checked again at its own priority before
    /// it is sent (see the send path).
    pub fn sim_fee(&self, c: &PlanCtx<'_, L>) -> SimFee {
        SimFee { max_fee_per_gas: c.base_fee_wei * 2 + self.cfg.priority_fee_wei, max_priority_fee_per_gas: self.cfg.priority_fee_wei }
    }

    /// USD per loan token for this plan, or the error that refuses the market.
    fn loan_usd(&self, meta: &L::Market, snap: &L::Snapshot, eth_usd: f64) -> Result<f64, String> {
        self.protocol.loan_usd(meta, snap, eth_usd).filter(|u| *u > 0.0).ok_or_else(|| "loan token has no USD price model (market refused)".to_string())
    }

    /// Fills sim-derived fields and applies the sanity checks shared by both routers.
    fn score_sim(&self, plan: &mut Plan, res: Result<SimResult>, c: &PlanCtx<'_, L>) {
        plan.sim_block = match &res {
            Ok(r) => r.block_number,
            Err(e) => e.downcast_ref::<SimReverted>().and_then(|s| s.block_number),
        };
        if let Some(q) = c.price_probe {
            let seen = match &res {
                Ok(r) => r.probe_price,
                Err(e) => e.downcast_ref::<SimReverted>().and_then(|s| s.probe_price),
            };
            if !q.is_new(seen) {
                plan.stale_price = true;
                plan.error = Some(format!("oracle update not in this state yet: price {} (old {}, new {})", seen.map_or("?".into(), |s| s.to_string()), q.old, q.new));
                return;
            }
        }
        if c.pending {
            if let Some(why) = pending_staleness(plan.sim_block, res.is_ok(), c.block + 2) {
                plan.error = Some(why);
                return;
            }
        }
        match res {
            Ok(r) => {
                let loan_usd = match self.loan_usd(c.meta, c.snap, c.eth_usd) {
                    Ok(u) => u,
                    Err(e) => {
                        plan.error = Some(e);
                        return;
                    }
                };
                let profit = num::units(r.profit, self.protocol.loan_decimals(c.meta));
                let gas_wei = U256::from(r.gas_used) * U256::from(c.base_fee_wei + self.cfg.priority_fee_wei) + r.l1_fee_wei;
                let gas_usd = num::units(gas_wei, 18) * c.eth_usd;
                plan.sim_profit = Some(profit);
                plan.profit_usd = Some(profit * loan_usd);
                plan.gas_used = Some(r.gas_used);
                plan.gas_cost_usd = Some(gas_usd);
                plan.l1_fee_usd = Some(num::units(r.l1_fee_wei, 18) * c.eth_usd);
                plan.net_usd = Some(profit * loan_usd - gas_usd);
                let gap = (profit - plan.quoted_gross).abs() / plan.quoted_gross.max(1e-9);
                if gap > self.cfg.max_sim_quote_gap {
                    // Quote and execution disagree: fee-on-transfer, manipulated pool or stale quote.
                    plan.error = Some(format!("sim/quote mismatch: sim {profit:.4} vs quote {:.4}", plan.quoted_gross));
                }
                if r.gas_used > self.cfg.max_gas {
                    // Too close to the per-tx gas cap (16.7M on Base) to send safely.
                    plan.error = Some(format!("route too gas-heavy: {} > {}", r.gas_used, self.cfg.max_gas));
                }
            }
            Err(e) => plan.error = Some(e.to_string()),
        }
    }

    // ---------------------------------------------------------------- direct V3 router

    /// Gas price direct routes are priced at: under competition every extra leg's gas lowers the
    /// priority fee per gas we can afford, so a leg must earn more than that.
    fn route_gas_price(&self, base_fee_wei: u128) -> f64 {
        (base_fee_wei + self.cfg.priority_fee_wei.max(self.cfg.planning_priority_fee_wei)) as f64
    }

    /// Best direct-router exit for selling up to `max_seize` collateral of `market` at `block`:
    /// one grid quote, then the allocation that maximises out - repay(seize) - extra-leg gas.
    /// Err = the quote failed, Ok(None) = no profitable allocation.
    #[allow(clippy::too_many_arguments)]
    async fn direct_exit<P: Provider>(
        &self,
        p: &P,
        meta: &L::Market,
        market: usize,
        snap: &L::Snapshot,
        max_seize: U256,
        eth_usd: f64,
        base_fee_wei: u128,
        block: BlockId,
        overrides: &StateOverride,
        repay: impl Fn(U256) -> U256,
        gas_budget: Option<u64>,
    ) -> Result<Option<(v3router::Grid, v3router::Allocation)>, String> {
        let v3 = self.v3.expect("checked by caller");
        let loan_dec = self.protocol.loan_decimals(meta);
        // Connector grid range: what all seizable collateral is worth in the connector (WETH, 18
        // decimals) at the oracle price, with 25% headroom for price differences.
        let seize_value = num::units(self.protocol.collateral_value(meta, snap, max_seize), loan_dec);
        let max_conn = U256::from((seize_value / eth_usd.max(1.0) * 1.25 * 1e18) as u128);
        let loan_usd = self.loan_usd(meta, snap, eth_usd)?;
        // Quotes priced like the send: pools that read tx.gasprice answer what the tx would get.
        let fee = SimFee { max_fee_per_gas: base_fee_wei * 2 + self.cfg.priority_fee_wei, max_priority_fee_per_gas: self.cfg.priority_fee_wei };
        let grid = v3.grid(p, market, max_seize, max_conn, block, overrides, Some(fee)).await.map_err(|e| format!("grid quote failed: {e}"))?;
        let gas_price = self.route_gas_price(base_fee_wei);
        let n_direct = v3.routes[&market].direct.len();
        // The extra-path gas cost is weighed against output in loan-token units.
        match v3router::allocate_within(&grid, n_direct, repay, GAS_PER_EXTRA_PATH, gas_price / 1e18 * eth_usd / loan_usd, |x| num::units(x, loan_dec), gas_budget) {
            Ok(a) => Ok(a.map(|a| (grid, a))),
            Err(g) => Err(format!("direct route {g} gas > budget {}", gas_budget.unwrap_or(0))),
        }
    }

    /// Solver valuation: all `views` (liquidatable positions of one market at `snap`) liquidated
    /// in one bundle, their collateral sold as ONE exit, so price impact accumulates as it would
    /// for an Atlas solver op (planning each position alone assumes each gets the whole pool).
    /// Repayment is linear in the seized amount at a fixed price, so the bundle sizes like one
    /// big position. Both exits are tried in parallel and must fit `swap_gas_budget` (what the
    /// solver op leaves for the swap): the direct router (exact at `block`) and Kyber (live market,
    /// prop AMMs / RFQ makers: on a large alt-coin sale during a price drop the aggregator cost
    /// 0.2-0.4% against ~2% for the direct router). Gas is an estimate: the contract liquidates
    /// one borrower per call, so the bundle itself cannot be simulated.
    #[allow(clippy::too_many_arguments)]
    pub async fn quote_bundle<P: Provider>(
        &self,
        p: &P,
        meta: &L::Market,
        snap: &L::Snapshot,
        views: &[PositionView<L>],
        block: u64,
        overrides: &StateOverride,
        base_fee_wei: u128,
        eth_usd: f64,
        swap_gas_budget: u64,
        use_kyber: bool,
    ) -> BundleQuote {
        let t0 = Instant::now();
        let (collateral, loan) = self.protocol.exit_tokens(meta);
        let loan_dec = self.protocol.loan_decimals(meta);
        let mut q = BundleQuote { positions: views.len(), swap_gas_budget, ..Default::default() };
        let loan_usd = match self.loan_usd(meta, snap, eth_usd) {
            Ok(u) => u,
            Err(e) => {
                q.error = Some(e);
                return q;
            }
        };
        let (total_seize, total_repaid) = bundle_totals(self.protocol, meta, snap, views);
        if total_seize.is_zero() {
            q.error = Some("nothing to seize".into());
            return q;
        }
        let repay = |s: U256| bundle_repay(s, total_seize, total_repaid);
        let gross = |out: U256, s: U256| (num::units(out, loan_dec) - num::units(repay(s), loan_dec)) * loan_usd;
        let direct = async {
            let market = views[0].market;
            if !self.v3.is_some_and(|v| v.routes.get(&market).is_some_and(|r| !r.is_empty())) {
                return Err("no direct pools for this market".to_string());
            }
            match self.direct_exit(p, meta, market, snap, total_seize, eth_usd, base_fee_wei, BlockId::number(block), overrides, repay, Some(swap_gas_budget)).await? {
                None => Err("no profitable direct allocation".into()),
                Some((_, a)) => Ok(("direct", (num::u256_f64(a.seize) / num::u256_f64(total_seize) * 1e4) as u64, gross(a.expected_out, a.seize), a.est_swap_gas)),
            }
        };
        let agg_name = self.aggregator.name();
        let kyber = async {
            if !use_kyber {
                return Err(format!("off ({agg_name} quotes the current market only)"));
            }
            let sizes: Vec<(u64, U256)> = [10_000u64, 5_000].iter().map(|f| (*f, total_seize * U256::from(*f) / U256::from(10_000u64))).collect();
            let quotes = join_all(sizes.iter().map(|(_, s)| self.aggregator.quote_within(collateral, loan, *s, swap_gas_budget))).await;
            let mut best: Option<BundleExit> = None;
            let mut last_err = format!("{agg_name}: no quote");
            for ((f, s), r) in sizes.iter().zip(quotes) {
                match r {
                    Ok(k) => {
                        let g = gross(self.aggregator.amount_out(&k), *s);
                        if best.is_none_or(|b| g > b.2) {
                            best = Some((agg_name, *f, g, self.aggregator.swap_gas(&k).unwrap_or(swap_gas_budget)));
                        }
                    }
                    Err(e) => last_err = e.to_string(),
                }
            }
            best.ok_or(last_err)
        };
        let (direct, kyber) = tokio::join!(direct, kyber);
        q.quote_ms = t0.elapsed().as_millis() as u64;
        let gas_price = self.route_gas_price(base_fee_wei);
        let gas_units = |e: &BundleExit| e.3 + BUNDLE_GAS_PER_POSITION * views.len() as u64 + BUNDLE_OVERHEAD_GAS;
        let net = |e: &BundleExit| e.2 - gas_units(e) as f64 * gas_price / 1e18 * eth_usd;
        q.direct_net_usd = direct.as_ref().ok().map(net);
        q.kyber_net_usd = kyber.as_ref().ok().map(net);
        let best = [&direct, &kyber].into_iter().filter_map(|r| r.as_ref().ok()).max_by(|a, b| net(a).total_cmp(&net(b)));
        match best {
            Some(e) => {
                (q.router, q.fraction_bps, q.gross_usd, q.swap_gas, q.gas_units) = (e.0.to_string(), e.1, e.2, e.3, gas_units(e));
                q.net_usd = Some(net(e));
                q.gas_usd = q.gross_usd - net(e);
            }
            None => q.error = Some(format!("direct: {}; {agg_name}: {}", direct.err().unwrap_or_default(), kyber.err().unwrap_or_default())),
        }
        q
    }

    async fn plan_direct<P: Provider>(&self, p: &P, c: &PlanCtx<'_, L>) -> Result<Plan> {
        let v3 = self.v3.expect("checked by caller");
        let t0 = Instant::now();
        let mut plan = Plan { router: "direct".into(), ..Default::default() };
        let repay = |s| self.protocol.repay_for_seize(c.meta, c.snap, s);
        let budget = Some(self.cfg.max_gas.saturating_sub(LIQUIDATION_OVERHEAD_GAS));
        let exit = self.direct_exit(p, c.meta, c.view.market, c.snap, c.view.max_seize, c.eth_usd, c.base_fee_wei, c.block_id(), &c.overrides, repay, budget).await;
        plan.quote_ms = t0.elapsed().as_millis() as u64;
        let (grid, alloc) = match exit {
            Ok(Some(x)) => x,
            Ok(None) => {
                plan.error = Some("no profitable direct allocation".into());
                return Ok(plan);
            }
            Err(e) => {
                plan.error = Some(e);
                return Ok(plan);
            }
        };
        let loan_dec = self.protocol.loan_decimals(c.meta);
        let repaid = repay(alloc.seize);
        plan.seize = alloc.seize.to_string();
        plan.repaid = repaid.to_string();
        plan.fraction_bps = alloc.source_units.iter().sum::<u64>() * 10_000 / v3.grid_points.max(1);
        plan.quoted_gross = num::units(alloc.expected_out, loan_dec) - num::units(repaid, loan_dec);
        plan.paths_used = alloc.source_units.iter().filter(|u| **u > 0).count();
        plan.hops = alloc.legs_used();
        plan.est_swap_gas = Some(alloc.est_swap_gas);

        let route: Bytes = v3.encode(c.view.market, &grid, &alloc);
        let t1 = Instant::now();
        // Simulate exactly what would be sent: the venue's encoding of the packed route.
        let payload = self.venue.encode(c.meta, c.view.user, alloc.seize, repaid, &ExitRoute::Packed(route), U256::ZERO, c.deadline_block);
        let calldata = payload.calldata.clone();
        plan.payload = Some(payload);
        let res = self.venue.simulate(p, calldata, c.block_id(), &c.overrides, c.sim_at(), Some(self.sim_fee(c)), c.price_probe.map(|q| q.oracle)).await;
        plan.sim_ms = t1.elapsed().as_millis() as u64;
        self.score_sim(&mut plan, res, c);
        Ok(plan)
    }

    // ---------------------------------------------------------------- aggregator

    /// Plans the best size; while the route is too gas-heavy, retries with only smaller sizes.
    async fn plan_aggregator<P: Provider>(&self, p: &P, c: &PlanCtx<'_, L>) -> Result<Plan> {
        let mut plan = self.plan_aggregator_sizes(p, c, &SEIZE_FRACTIONS_BPS).await?;
        while plan.error.as_deref().is_some_and(|e| e.starts_with("route too gas-heavy")) {
            let smaller: Vec<u64> = SEIZE_FRACTIONS_BPS.iter().copied().filter(|f| *f < plan.fraction_bps).collect();
            if smaller.is_empty() {
                break;
            }
            plan = self.plan_aggregator_sizes(p, c, &smaller).await?;
        }
        Ok(plan)
    }

    async fn plan_aggregator_sizes<P: Provider>(&self, p: &P, c: &PlanCtx<'_, L>, fractions: &[u64]) -> Result<Plan> {
        let (collateral, loan) = self.protocol.exit_tokens(c.meta);
        let loan_dec = self.protocol.loan_decimals(c.meta);
        let t0 = Instant::now();

        // 1. Quote every candidate size in parallel; keep the best gross margin.
        let sizes: Vec<(u64, U256)> = fractions
            .iter()
            .map(|&f| (f, c.view.max_seize * U256::from(f) / U256::from(10_000u64)))
            .filter(|(_, s)| !s.is_zero())
            .collect();
        // Each size: the best route whose swap leaves room for the liquidation itself under max_gas.
        let budget = self.cfg.max_gas.saturating_sub(LIQUIDATION_OVERHEAD_GAS);
        let quotes = join_all(sizes.iter().map(|(_, s)| self.aggregator.quote_within(collateral, loan, *s, budget))).await;
        let mut best: Option<(u64, U256, U256, A::Quote, f64)> = None;
        let mut last_err = None;
        for ((f, seize), q) in sizes.iter().zip(quotes) {
            match q {
                Ok(q) => {
                    let repaid = self.protocol.repay_for_seize(c.meta, c.snap, *seize);
                    let gross = num::units(self.aggregator.amount_out(&q), loan_dec) - num::units(repaid, loan_dec);
                    if best.as_ref().is_none_or(|b| gross > b.4) {
                        best = Some((*f, *seize, repaid, q, gross));
                    }
                }
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        let quote_ms = t0.elapsed().as_millis() as u64;
        let Some((fraction_bps, seize, repaid, quote, quoted_gross)) = best else {
            return Ok(Plan { router: self.aggregator.name().into(), error: Some(format!("no exit quote: {}", last_err.unwrap_or_default())), quote_ms, ..Default::default() });
        };
        let mut plan = Plan { router: self.aggregator.name().into(), fraction_bps, seize: seize.to_string(), repaid: repaid.to_string(), quoted_gross, quote_ms, ..Default::default() };
        if quoted_gross <= 0.0 {
            plan.error = Some("exit does not cover repayment".into());
            return Ok(plan);
        }

        // 2. Build the swap for the chosen size and simulate the whole liquidation.
        let t1 = Instant::now();
        let holder = self.venue.swap_recipient();
        let built = match self.aggregator.build(&quote, holder, holder, self.cfg.slippage_bps).await {
            Ok(b) => b,
            Err(e) => {
                plan.error = Some(e.to_string());
                return Ok(plan);
            }
        };
        // Never trust the aggregator's router address blindly (poisoned API / MITM).
        if !self.cfg.allowed_targets.contains(&built.target) {
            plan.error = Some(format!("router {} not allowlisted", built.target));
            return Ok(plan);
        }
        let route = ExitRoute::Call { target: built.target, calldata: built.calldata };
        let payload = self.venue.encode(c.meta, c.view.user, seize, repaid, &route, U256::ZERO, c.deadline_block);
        let calldata = payload.calldata.clone();
        plan.payload = Some(payload);
        if std::env::var_os("BOT_GAS_PROBE").is_some() {
            let probe = self.venue.probe_gas(p, calldata.clone(), c.block_id(), &c.overrides, &[500_000, 1_000_000, 2_000_000, 4_000_000, 8_000_000, 12_000_000]).await;
            tracing::info!(?probe, fraction_bps, "gas probe (limit, ok)");
        }
        let res = self.venue.simulate(p, calldata, c.block_id(), &c.overrides, c.sim_at(), Some(self.sim_fee(c)), c.price_probe.map(|q| q.oracle)).await;
        plan.sim_ms = t1.elapsed().as_millis() as u64;
        self.score_sim(&mut plan, res, c);
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::morpho::{
        Morpho, PositionView,
        math::{self, BorrowTotals, Health, VIRTUAL_SHARES},
        onchain::{MarketMeta, MarketParams, MarketSnapshot},
    };
    use alloy::primitives::{Address, B256};

    fn view(max_seize: u64) -> PositionView {
        PositionView {
            market: 0,
            user: Address::ZERO,
            health: Health { borrowed: U256::from(2u8), max_borrow: U256::from(1u8) },
            hf: 0.99,
            debt: 0.0,
            max_seize: U256::from(max_seize),
            quote: None,
            bonus: 0.0,
        }
    }

    fn meta(lltv: U256) -> MarketMeta {
        MarketMeta {
            id: B256::ZERO,
            name: "t".into(),
            params: MarketParams { loanToken: Address::ZERO, collateralToken: Address::ZERO, oracle: Address::ZERO, irm: Address::ZERO, lltv },
            loan_decimals: 6,
            collateral_decimals: 6,
            loan_symbol: "USDC".into(),
            collateral_symbol: "X".into(),
        }
    }

    #[test]
    fn bundle_repays_the_sum_of_its_positions() {
        // 1 collateral unit = 2 loan units (price 2e36), LLTV 86%.
        let snap = MarketSnapshot {
            price: U256::from(2u8) * math::ORACLE_PRICE_SCALE,
            totals: BorrowTotals { total_borrow_assets: U256::from(10_000_000_000u64), total_borrow_shares: U256::from(10_000_000_000u64) * VIRTUAL_SHARES },
            raw: Default::default(),
        };
        let lltv = U256::from(860_000_000_000_000_000u64);
        let (morpho, m) = (Morpho::for_tests(), meta(lltv));
        let views = [view(1_000_000), view(3_000_000)];
        let (seize, repaid) = bundle_totals(&morpho, &m, &snap, &views);
        assert_eq!(seize, U256::from(4_000_000u64));
        let alone: U256 = views.iter().map(|v| math::quote_by_seized(v.max_seize, snap.price, lltv, snap.totals).repaid_assets).fold(U256::ZERO, |a, b| a + b);
        assert_eq!(repaid, alone);
        // Seizing everything repays everything; half repays half (rounded up, never under).
        assert_eq!(bundle_repay(seize, seize, repaid), repaid);
        let half = bundle_repay(seize / U256::from(2u8), seize, repaid);
        assert!(half >= repaid / U256::from(2u8) && half <= repaid / U256::from(2u8) + U256::from(1u8));
        // Repaid ~ seized value / LIF (1.0438 at 86% LLTV): 8 loan units of value -> ~7.66.
        assert!((math::u256_f64(repaid) / 8_000_000.0 - 1.0 / 1.043841336).abs() < 1e-4);
    }
}

#[cfg(test)]
mod pending_tests {
    use super::*;

    #[test]
    fn solver_op_capacity() {
        // 6M solverGasLimit: 600k overhead + 1M swap floor leave room for 40 liquidations.
        assert_eq!(bundle_capacity(6_000_000), 40);
        assert_eq!(bundle_swap_budget(6_000_000, 40), 1_000_000);
        // A 31-position auction leaves 1.99M for the swap.
        assert_eq!(bundle_swap_budget(6_000_000, 31), 1_990_000);
        assert_eq!(bundle_capacity(1_000_000), 0);
    }

    #[test]
    fn pending_plan_is_trusted_only_on_the_in_progress_block() {
        // Snapshot at 100, oracle update in block 101 (in progress): the sim must be block >= 102.
        let floor = 102;
        assert!(is_fresh_block(Some(102), floor));
        assert!(is_fresh_block(Some(103), floor), "a later base is newer, still fine");
        assert!(!is_fresh_block(Some(99), floor), "2026-10-19: alchemy simulated 1-5 blocks behind");
        assert!(!is_fresh_block(None, floor));
        // Stale or unverifiable successes are rejected...
        assert!(pending_staleness(Some(99), true, floor).unwrap().contains("block 99"));
        assert!(pending_staleness(None, true, floor).is_some());
        assert!(pending_staleness(Some(99), false, floor).is_some(), "a revert on a stale base says nothing");
        // ...but a failure with no block (transport, 429) keeps its own error, and a fresh result passes.
        assert!(pending_staleness(None, false, floor).is_none());
        assert!(pending_staleness(Some(102), true, floor).is_none());
        assert!(pending_staleness(Some(102), false, floor).is_none(), "a revert on the fresh base is real information");
    }

    #[test]
    fn price_probe_tells_whether_the_update_is_in_the_state() {
        let q = PriceProbe { oracle: Address::ZERO, old: U256::from(1_000u64), new: U256::from(900u64) };
        assert!(q.is_new(Some(U256::from(900u64))));
        assert!(q.is_new(Some(U256::from(905u64))), "another feed of the oracle moved a little too");
        assert!(!q.is_new(Some(U256::from(1_000u64))), "the node has not applied the metacall yet");
        assert!(!q.is_new(None), "probe failed: unknown counts as not applied");
    }
}

#[cfg(test)]
mod generic_tests {
    //! The planner instantiated on a protocol and a venue that are not Morpho / MorphoLiquidator.
    use alloy::providers::ProviderBuilder;

    use super::*;
    use crate::{
        engine::view,
        testkit::{MockDex, MockVenue, Mk, Toy, cfg, key, user},
    };

    #[tokio::test]
    async fn planner_runs_on_a_non_morpho_protocol_and_venue() {
        // Never contacted: no exit is available, so neither the RPC nor the aggregator is queried.
        let provider = ProviderBuilder::new().connect_http("http://127.0.0.1:1".parse().unwrap());
        let (toy, venue, cfg) = (Toy::default(), MockVenue, cfg());
        let m = Mk { key: key(1), name: "alpha".into() };
        // hf 0.9: liquidatable, may seize all 90 collateral.
        let v = view(&toy, 0, user(1), &m, &1.0, &(100, 90));
        assert_eq!(v.max_seize, U256::from(90u64));

        let planner = Planner { protocol: &toy, aggregator: &MockDex, venue: &venue, cfg: &cfg.exec, v3: None, mode: "direct" };
        let q = planner.quote_bundle(&provider, &m, &1.0, std::slice::from_ref(&v), 1, &StateOverride::default(), 0, 2000.0, 1_000_000, false).await;
        assert_eq!(q.positions, 1);
        let err = q.error.unwrap();
        assert!(err.contains("no direct pools") && err.contains("mockdex: off (mockdex quotes"), "{err}");

        let ctx = PlanCtx { meta: &m, snap: &1.0, view: &v, block: 1, base_fee_wei: 0, eth_usd: 2000.0, overrides: StateOverride::default(), deadline_block: 3, sim_time: 0, pending: false, price_probe: None };
        let plan = planner.plan(&provider, &ctx).await.unwrap();
        assert_eq!((plan.router.as_str(), plan.error.as_deref()), ("direct", Some("no direct pools for this market")));

        // The venue decides the encoding: a packed route and an external call differ.
        let packed = venue.encode(&m, user(1), U256::from(90u64), U256::from(90u64), &ExitRoute::Packed(Bytes::new()), U256::ZERO, 3);
        let call = venue.encode(&m, user(1), U256::from(90u64), U256::from(90u64), &ExitRoute::Call { target: Address::ZERO, calldata: Bytes::new() }, U256::ZERO, 3);
        assert_eq!((packed.to, packed.calldata[0], call.calldata[0]), (venue.target(), 1, 2));
    }

    #[tokio::test]
    async fn aggregator_exit_is_dex_agnostic_and_allowlist_checked() {
        let provider = ProviderBuilder::new().connect_http("http://127.0.0.1:1".parse().unwrap());
        let (toy, venue, mut cfg) = (Toy::default(), MockVenue, cfg());
        let m = Mk { key: key(1), name: "alpha".into() };
        let v = view(&toy, 0, user(1), &m, &1.0, &(100, 90));
        let ctx = PlanCtx { meta: &m, snap: &1.0, view: &v, block: 1, base_fee_wei: 0, eth_usd: 2000.0, overrides: StateOverride::default(), deadline_block: 3, sim_time: 0, pending: false, price_probe: None };

        // The aggregator's router is not allowlisted: the plan stops before encoding.
        let plan = Planner { protocol: &toy, aggregator: &MockDex, venue: &venue, cfg: &cfg.exec, v3: None, mode: "aggregator" }.plan(&provider, &ctx).await.unwrap();
        assert_eq!(plan.router, "mockdex");
        assert_eq!(plan.seize, "90", "full seize: 2 loan per collateral beats every smaller size");
        assert!(plan.quoted_gross > 0.0 && plan.payload.is_none());
        assert_eq!(plan.error.as_deref(), Some(format!("router {} not allowlisted", Address::repeat_byte(0x22)).as_str()));

        // Allowlisted: the venue encodes the external call and is asked to simulate it.
        cfg.exec.allowed_targets.push(Address::repeat_byte(0x22));
        let plan = Planner { protocol: &toy, aggregator: &MockDex, venue: &venue, cfg: &cfg.exec, v3: None, mode: "kyber" }.plan(&provider, &ctx).await.unwrap();
        let payload = plan.payload.as_ref().expect("encoded");
        assert_eq!((payload.to, payload.calldata[0]), (venue.target(), 2));
        assert!(plan.error.as_deref().is_some_and(|e| e.contains("mock venue: no simulation")), "{:?}", plan.error);
    }

    #[tokio::test]
    async fn a_flash_funded_venue_borrows_the_planned_repayment_and_takes_only_aggregator_exits() {
        use crate::{
            flash_venue::{FlashLiquidatorVenue, IFlashLiquidator},
            funding::UniswapV4,
        };
        use alloy::sol_types::SolCall;
        let provider = ProviderBuilder::new().connect_http("http://127.0.0.1:1".parse().unwrap());
        let (toy, mut cfg) = (Toy::default(), cfg());
        cfg.exec.allowed_targets.push(Address::repeat_byte(0x22));
        let venue = FlashLiquidatorVenue::new(Toy::default(), Some(Address::repeat_byte(0xf1)), Address::repeat_byte(0xee), Box::new(UniswapV4 { pool_manager: Address::repeat_byte(4) }), 0, None, vec![], vec![]);
        let m = Mk { key: key(1), name: "alpha".into() };
        let v = view(&toy, 0, user(1), &m, &1.0, &(100, 90));
        let ctx = PlanCtx { meta: &m, snap: &1.0, view: &v, block: 1, base_fee_wei: 0, eth_usd: 2000.0, overrides: StateOverride::default(), deadline_block: 3, sim_time: 0, pending: false, price_probe: None };
        let planner = Planner { protocol: &toy, aggregator: &MockDex, venue: &venue, cfg: &cfg.exec, v3: None, mode: "aggregator" };
        let plan = planner.plan(&provider, &ctx).await.unwrap();
        let call = IFlashLiquidator::liquidateCall::abi_decode(&plan.payload.as_ref().expect("encoded").calldata).unwrap();
        // Toy repays 1 per collateral unit: seize 90 -> borrow 90 + 1 wei (no headroom here).
        assert_eq!((call.funding, call.debtAmount, call.swaps[0].target), (2, U256::from(91u64), Address::repeat_byte(0x22)));
        assert!(plan.error.as_deref().is_some_and(|e| e.contains("simulation unavailable")), "simulated against the venue: {:?}", plan.error);
    }
}
