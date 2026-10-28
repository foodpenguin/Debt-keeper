//! Self-built exit router over V3-style pools (Uniswap V3, PancakeSwap V3, Aerodrome Slipstream).
//!
//! Model (per market): collateral C is sold to the loan token L through
//!   - direct legs    C -> L (one pool each), and/or
//!   - connector legs C -> K (e.g. WETH), whose combined K output is then sold through the
//!     sink legs K -> L. All connector legs share the sinks, so sinks are modelled as ONE concave
//!     sell curve V(k) and each connector unit is valued at its marginal value on V (quoting each
//!     two-hop path separately would double-count the shared sink liquidity).
//!
//! 1. Discovery: factories are asked for C/L, C/K and K/L pools.
//! 2. Grid quote: GridQuoter (injected via state override) prices every leg at K evenly spaced
//!    sizes, one eth_call per leg, all in parallel (one round trip; spreads over the RPC pool).
//! 3. Allocation: greedy by marginal value (optimal for concave curves), compared against each
//!    source leg used alone because every extra leg costs fixed gas.
//! 4. Encoding: direct and connector legs carry exact amounts; sink legs split the pooled
//!    connector balance by bps share (hop flag bit2), so nothing is left behind.

use std::collections::HashMap;

use alloy::{
    eips::BlockId,
    network::TransactionBuilder,
    primitives::{Address, Bytes, U256, address},
    providers::Provider,
    rpc::types::{
        TransactionRequest,
        state::{AccountOverride, StateOverride},
    },
    sol,
    sol_types::SolCall,
};
use anyhow::{Context, Result};
use futures::future::join_all;
use serde::Deserialize;

use crate::morpho::onchain::multicall;

/// Placeholder address the quoter is injected at (never deployed).
pub const SIM_QUOTER: Address = address!("00000000000000000000000000000000000b0772");
/// Extra gas a balance-share sink leg costs (token0/token1 + balanceOf lookups).
const GAS_PER_SHARE_LEG: u64 = 8_000;

sol! {
    interface IV3FactoryFee { function getPool(address a, address b, uint24 fee) external view returns (address); }
    interface IV3FactorySpacing { function getPool(address a, address b, int24 tickSpacing) external view returns (address); }
    interface IV3PoolView { function liquidity() external view returns (uint128); }
    interface IERC20Bal { function balanceOf(address owner) external view returns (uint256); }
    struct QPath { address[] pools; bool[] zeroForOne; }
    interface IGridQuoter {
        function quote(QPath[] paths, uint256[] amounts) external returns (uint256[][] outs, uint256[][] gasUsed);
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct FactoryEntry {
    pub name: String,
    pub address: Address,
    /// "fee" (Uniswap/Pancake: getPool(a,b,uint24 fee)) or "tick_spacing" (Slipstream: int24).
    pub kind: String,
    pub params: Vec<i64>,
}

#[derive(Debug, Clone)]
pub struct Leg {
    pub pool: Address,
    pub zero_for_one: bool,
    pub label: String,
}

#[derive(Debug, Clone, Default)]
pub struct MarketRoutes {
    /// C -> L
    pub direct: Vec<Leg>,
    /// C -> K
    pub to_conn: Vec<Leg>,
    /// K -> L
    pub sinks: Vec<Leg>,
}

impl MarketRoutes {
    pub fn is_empty(&self) -> bool {
        self.direct.is_empty() && (self.to_conn.is_empty() || self.sinks.is_empty())
    }
    pub fn labels(&self) -> Vec<String> {
        let mut v: Vec<String> = self.direct.iter().map(|l| format!("direct {}", l.label)).collect();
        v.extend(self.to_conn.iter().map(|l| format!("to-conn {}", l.label)));
        v.extend(self.sinks.iter().map(|l| format!("sink {}", l.label)));
        v
    }
}

/// Output (and gas) of one leg at sizes k * unit, k = 1..=K.
#[derive(Debug, Clone, Default)]
pub struct Curve {
    pub out: Vec<U256>,
    pub gas: Vec<u64>,
}

impl Curve {
    /// Output for j units (0 for j = 0).
    fn at(&self, j: u64) -> U256 {
        if j == 0 { U256::ZERO } else { self.out[(j - 1) as usize] }
    }
    fn gas_at(&self, j: u64) -> u64 {
        if j == 0 { 0 } else { self.gas.get((j - 1) as usize).copied().unwrap_or(0) }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Grid {
    /// Collateral unit; source legs are quoted at k * unit.
    pub unit: U256,
    /// direct legs then connector legs (same order as MarketRoutes::direct ++ to_conn)
    pub sources: Vec<Curve>,
    /// Connector unit; sink legs are quoted at k * conn_unit.
    pub conn_unit: U256,
    pub sinks: Vec<Curve>,
}

/// Combined sink sell curve: value[j] = best total L output for j connector units split over the
/// sinks, with the per-sink split that achieves it (greedy, optimal for concave curves).
struct SinkCurve {
    value: Vec<U256>,
    split: Vec<Vec<u64>>,
    gas: Vec<u64>,
}

impl SinkCurve {
    fn build(grid: &Grid) -> Self {
        let n = grid.sinks.len();
        let k_max = grid.sinks.first().map(|c| c.out.len() as u64).unwrap_or(0);
        let mut units = vec![0u64; n];
        let mut value = vec![U256::ZERO];
        let mut split = vec![units.clone()];
        let mut gas = vec![0u64];
        let mut total = U256::ZERO;
        for _ in 1..=k_max {
            let mut pick = None;
            let mut best = U256::ZERO;
            for (s, &u) in units.iter().enumerate() {
                if u >= k_max {
                    continue;
                }
                let g = grid.sinks[s].at(u + 1).saturating_sub(grid.sinks[s].at(u));
                if g > best {
                    best = g;
                    pick = Some(s);
                }
            }
            if let Some(s) = pick {
                units[s] += 1;
                total += best;
            }
            value.push(total);
            split.push(units.clone());
            let used = units.iter().filter(|u| **u > 0).count() as u64;
            gas.push(units.iter().enumerate().map(|(s, &u)| grid.sinks[s].gas_at(u)).sum::<u64>() + GAS_PER_SHARE_LEG * used);
        }
        Self { value, split, gas }
    }

    /// V(w) by linear interpolation between grid points; clamped (conservatively) at the top.
    fn value_at(&self, w: U256, conn_unit: U256) -> U256 {
        if conn_unit.is_zero() || self.value.len() < 2 {
            return U256::ZERO;
        }
        let k_max = (self.value.len() - 1) as u64;
        let j = (w / conn_unit).to::<u64>();
        if j >= k_max {
            return self.value[k_max as usize];
        }
        let rem = w - conn_unit * U256::from(j);
        let lo = self.value[j as usize];
        let hi = self.value[(j + 1) as usize];
        lo + (hi.saturating_sub(lo)) * rem / conn_unit
    }

    /// Nearest grid point for w (used for encoding shares and estimating gas).
    fn index_for(&self, w: U256, conn_unit: U256) -> usize {
        if conn_unit.is_zero() || self.value.len() < 2 {
            return 0;
        }
        let k_max = self.value.len() - 1;
        (((w + conn_unit / U256::from(2)) / conn_unit).to::<u64>() as usize).clamp(1, k_max)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Allocation {
    /// units per source leg (direct ++ to_conn)
    pub source_units: Vec<u64>,
    /// sink units split for the pooled connector amount (empty when no connector leg is used)
    pub sink_units: Vec<u64>,
    pub seize: U256,
    pub expected_out: U256,
    pub est_swap_gas: u64,
}

impl Allocation {
    pub fn legs_used(&self) -> usize {
        self.source_units.iter().chain(&self.sink_units).filter(|u| **u > 0).count()
    }
}

pub struct V3Router {
    pub factories: Vec<FactoryEntry>,
    pub connectors: Vec<Address>,
    quoter_code: Bytes,
    /// market index -> legs
    pub routes: HashMap<usize, MarketRoutes>,
    pub max_legs: usize,
    pub grid_points: u64,
}

impl V3Router {
    pub fn new(factories: Vec<FactoryEntry>, connectors: Vec<Address>, quoter_artifact: &str, max_legs: usize, grid_points: u64) -> Result<Self> {
        let text = std::fs::read_to_string(quoter_artifact).with_context(|| format!("reading {quoter_artifact} (run `forge build` in contracts/)"))?;
        let a: serde_json::Value = serde_json::from_str(&text)?;
        let quoter_code: Bytes = a["deployedBytecode"]["object"].as_str().context("quoter: no deployedBytecode")?.parse()?;
        Ok(Self { factories, connectors, quoter_code, routes: HashMap::new(), max_legs, grid_points })
    }

    /// Pools for a token pair across factories, liquidity > 0, deepest first (by `rank_token` held).
    async fn pools_for<P: Provider>(&self, p: &P, a: Address, b: Address, rank_token: Address) -> Result<Vec<(Address, String)>> {
        let mut calls = Vec::new();
        let mut labels = Vec::new();
        for f in &self.factories {
            for &param in &f.params {
                let data: Bytes = if f.kind == "tick_spacing" {
                    IV3FactorySpacing::getPoolCall { a, b, tickSpacing: alloy::primitives::aliases::I24::try_from(param)? }.abi_encode().into()
                } else {
                    IV3FactoryFee::getPoolCall { a, b, fee: alloy::primitives::aliases::U24::from(param as u32) }.abi_encode().into()
                };
                calls.push((f.address, data));
                labels.push(format!("{}:{}", f.name, param));
            }
        }
        let res = multicall(p, &calls, BlockId::latest()).await?;
        let found: Vec<(Address, String)> = res
            .iter()
            .zip(labels)
            .filter_map(|(r, l)| {
                let addr = IV3FactoryFee::getPoolCall::abi_decode_returns(r.as_ref()?).ok()?;
                (addr != Address::ZERO).then_some((addr, l))
            })
            .collect();
        let mut calls = Vec::new();
        for (addr, _) in &found {
            calls.push((*addr, IV3PoolView::liquidityCall {}.abi_encode().into()));
            calls.push((rank_token, IERC20Bal::balanceOfCall { owner: *addr }.abi_encode().into()));
        }
        let res = multicall(p, &calls, BlockId::latest()).await?;
        let mut out: Vec<(Address, String, U256)> = Vec::new();
        for (i, (addr, label)) in found.into_iter().enumerate() {
            let liq = res[2 * i].as_ref().and_then(|b| IV3PoolView::liquidityCall::abi_decode_returns(b).ok()).unwrap_or(0);
            let depth = res[2 * i + 1].as_ref().and_then(|b| IERC20Bal::balanceOfCall::abi_decode_returns(b).ok()).unwrap_or_default();
            if liq > 0 && !depth.is_zero() {
                out.push((addr, label, depth));
            }
        }
        out.sort_by(|x, y| y.2.cmp(&x.2));
        Ok(out.into_iter().map(|(a, l, _)| (a, l)).collect())
    }

    /// Discovers direct, connector and sink legs for one market (first connector with pools wins).
    pub async fn discover<P: Provider>(&mut self, p: &P, market: usize, collateral: Address, loan: Address) -> Result<&MarketRoutes> {
        let leg = |pool: Address, label: String, input: Address, output: Address| Leg { pool, zero_for_one: input < output, label };
        let mut r = MarketRoutes::default();
        for (pool, label) in self.pools_for(p, collateral, loan, loan).await?.into_iter().take(self.max_legs) {
            r.direct.push(leg(pool, label, collateral, loan));
        }
        for &k in &self.connectors {
            if k == collateral || k == loan {
                continue;
            }
            let to_conn = self.pools_for(p, collateral, k, k).await?;
            let sinks = self.pools_for(p, k, loan, loan).await?;
            if to_conn.is_empty() || sinks.is_empty() {
                continue;
            }
            r.to_conn = to_conn.into_iter().take(self.max_legs).map(|(pool, l)| leg(pool, l, collateral, k)).collect();
            r.sinks = sinks.into_iter().take(3).map(|(pool, l)| leg(pool, l, k, loan)).collect();
            break;
        }
        self.routes.insert(market, r);
        Ok(&self.routes[&market])
    }

    async fn quote_legs<P: Provider>(&self, p: &P, legs: &[&Leg], unit: U256, block: BlockId, ov: &StateOverride) -> Vec<Curve> {
        let k = self.grid_points.max(1);
        let amounts: Vec<U256> = (1..=k).map(|i| unit * U256::from(i)).collect();
        let calls = legs.iter().map(|leg| {
            let data: Bytes = IGridQuoter::quoteCall {
                paths: vec![QPath { pools: vec![leg.pool], zeroForOne: vec![leg.zero_for_one] }],
                amounts: amounts.clone(),
            }
            .abi_encode()
            .into();
            let tx = TransactionRequest::default().with_to(SIM_QUOTER).with_input(data).with_gas_limit(60_000_000);
            async move {
                let out = p.call(tx).block(block).overrides(ov.clone()).await?;
                let r = IGridQuoter::quoteCall::abi_decode_returns(&out)?;
                anyhow::Ok(Curve {
                    out: r.outs.into_iter().next().unwrap_or_default(),
                    gas: r.gasUsed.into_iter().next().unwrap_or_default().into_iter().map(|g| g.to::<u64>()).collect(),
                })
            }
        });
        join_all(calls)
            .await
            .into_iter()
            .map(|r| r.unwrap_or_else(|_| Curve { out: vec![U256::ZERO; k as usize], gas: vec![0; k as usize] }))
            .collect()
    }

    /// Quotes every leg in parallel. `max_conn` bounds the connector grid (an estimate of the
    /// connector amount all collateral would buy, e.g. from oracle prices, with headroom).
    pub async fn grid<P: Provider>(&self, p: &P, market: usize, max_seize: U256, max_conn: U256, block: BlockId, extra: &StateOverride) -> Result<Grid> {
        let r = self.routes.get(&market).context("no routes discovered for market")?;
        let k = self.grid_points.max(1);
        let unit = max_seize / U256::from(k);
        let conn_unit = max_conn / U256::from(k);
        let mut ov = extra.clone();
        ov.insert(SIM_QUOTER, AccountOverride { code: Some(self.quoter_code.clone()), ..Default::default() });
        let sources: Vec<&Leg> = r.direct.iter().chain(&r.to_conn).collect();
        let sinks: Vec<&Leg> = if r.to_conn.is_empty() { vec![] } else { r.sinks.iter().collect() };
        let (src, snk) = tokio::join!(self.quote_legs(p, &sources, unit, block, &ov), self.quote_legs(p, &sinks, conn_unit, block, &ov));
        Ok(Grid { unit, sources: src, conn_unit, sinks: snk })
    }

    /// Route bytes for an allocation: direct and connector legs with exact amounts, then the sink
    /// legs splitting the pooled connector balance by bps share (last sink takes the rest).
    pub fn encode(&self, market: usize, grid: &Grid, alloc: &Allocation) -> Bytes {
        let r = &self.routes[&market];
        let sources: Vec<&Leg> = r.direct.iter().chain(&r.to_conn).collect();
        let mut out = Vec::new();
        let mut push = |leg: &Leg, flags: u8, amount: u128| {
            out.extend_from_slice(leg.pool.as_slice());
            out.push(flags | leg.zero_for_one as u8);
            out.extend_from_slice(&amount.to_be_bytes());
        };
        for (i, &u) in alloc.source_units.iter().enumerate() {
            if u > 0 {
                push(sources[i], 0, (grid.unit * U256::from(u)).to::<u128>());
            }
        }
        let used: Vec<(usize, u64)> = alloc.sink_units.iter().copied().enumerate().filter(|(_, u)| *u > 0).collect();
        let mut remaining: u64 = used.iter().map(|(_, u)| u).sum();
        for (n, (s, u)) in used.iter().enumerate() {
            let bps = if n + 1 == used.len() { 10_000 } else { u * 10_000 / remaining };
            remaining -= u;
            push(&r.sinks[*s], 4, bps as u128);
        }
        out.into()
    }
}

/// Multipliers on the price of route gas tried by `allocate_within`, in order (fewer legs and
/// cheaper legs as it rises).
pub const GAS_PENALTIES: [f64; 5] = [1.0, 10.0, 100.0, 1_000.0, 10_000.0];

/// `allocate` within a gas budget (None = no limit): re-runs it with route gas priced
/// GAS_PENALTIES times higher (same grid, no RPC) until the swap fits, like Kyber's gas ladder.
/// `usd_per_gas` prices route gas in loan-token units. Ok(None) = nothing profitable;
/// Err(gas) = profitable allocations exist but none fits (gas of the last one tried).
pub fn allocate_within(
    grid: &Grid,
    n_direct: usize,
    repay: impl Fn(U256) -> U256,
    gas_per_extra_leg: u64,
    usd_per_gas: f64,
    to_units: impl Fn(U256) -> f64,
    gas_budget: Option<u64>,
) -> Result<Option<Allocation>, u64> {
    let mut over = None;
    for k in GAS_PENALTIES {
        let Some(a) = allocate(grid, n_direct, &repay, gas_per_extra_leg, |gas| gas as f64 * usd_per_gas * k, &to_units) else {
            break;
        };
        if gas_budget.is_none_or(|b| a.est_swap_gas <= b) {
            return Ok(Some(a));
        }
        over = Some(a.est_swap_gas);
    }
    over.map_or(Ok(None), Err)
}

/// Best allocation: greedy over all source legs, and each source leg alone (extra legs cost
/// fixed gas, which greedy-by-marginal-output does not see). Returns None if nothing is profitable.
/// `repay(seize)` is the exact Morpho repayment; `gas_cost`/`to_units` convert to loan-token units.
pub fn allocate(
    grid: &Grid,
    n_direct: usize,
    repay: impl Fn(U256) -> U256,
    gas_per_extra_leg: u64,
    gas_cost: impl Fn(u64) -> f64,
    to_units: impl Fn(U256) -> f64,
) -> Option<Allocation> {
    let n = grid.sources.len();
    if n == 0 || grid.unit.is_zero() {
        return None;
    }
    let sink = SinkCurve::build(grid);
    let mut best = greedy(grid, n_direct, &sink, &vec![true; n], &repay, gas_per_extra_leg, &gas_cost, &to_units);
    for s in 0..n {
        let mut only = vec![false; n];
        only[s] = true;
        if let Some(c) = greedy(grid, n_direct, &sink, &only, &repay, gas_per_extra_leg, &gas_cost, &to_units) {
            if best.as_ref().is_none_or(|b| c.0 > b.0) {
                best = Some(c);
            }
        }
    }
    best.filter(|b| b.0 > 0.0).map(|b| b.1)
}

#[allow(clippy::too_many_arguments)]
fn greedy(
    grid: &Grid,
    n_direct: usize,
    sink: &SinkCurve,
    allowed: &[bool],
    repay: &impl Fn(U256) -> U256,
    gas_per_extra_leg: u64,
    gas_cost: &impl Fn(u64) -> f64,
    to_units: &impl Fn(U256) -> f64,
) -> Option<(f64, Allocation)> {
    let n = grid.sources.len();
    let k_max = grid.sources[0].out.len() as u64;
    let mut units = vec![0u64; n];
    let mut direct_out = U256::ZERO;
    let mut conn_total = U256::ZERO;
    let mut best: Option<(f64, Allocation)> = None;
    for step in 1..=k_max {
        let v_now = sink.value_at(conn_total, grid.conn_unit);
        let mut pick = None;
        let mut pick_gain = U256::ZERO;
        for s in 0..n {
            if !allowed[s] || units[s] >= k_max {
                continue;
            }
            let c = &grid.sources[s];
            let delta = c.at(units[s] + 1).saturating_sub(c.at(units[s]));
            // Connector legs are worth what the extra connector sells for on the shared sinks.
            let gain = if s < n_direct { delta } else { sink.value_at(conn_total + delta, grid.conn_unit).saturating_sub(v_now) };
            if gain > pick_gain {
                pick_gain = gain;
                pick = Some(s);
            }
        }
        let Some(s) = pick else { break };
        let c = &grid.sources[s];
        let delta = c.at(units[s] + 1).saturating_sub(c.at(units[s]));
        units[s] += 1;
        if s < n_direct {
            direct_out += delta;
        } else {
            conn_total += delta;
        }
        let total_out = direct_out + sink.value_at(conn_total, grid.conn_unit);
        let seize = grid.unit * U256::from(step);
        let used_sources = units.iter().filter(|u| **u > 0).count() as u64;
        let sink_idx = if conn_total.is_zero() { 0 } else { sink.index_for(conn_total, grid.conn_unit) };
        let swap_gas = units.iter().enumerate().map(|(i, &u)| grid.sources[i].gas_at(u)).sum::<u64>()
            + sink.gas.get(sink_idx).copied().unwrap_or(0)
            + gas_per_extra_leg * used_sources.saturating_sub(1);
        let value = to_units(total_out) - to_units(repay(seize)) - gas_cost(swap_gas);
        if best.as_ref().is_none_or(|b| value > b.0) {
            let sink_units = if conn_total.is_zero() { vec![] } else { sink.split.get(sink_idx).cloned().unwrap_or_default() };
            best = Some((value, Allocation { source_units: units.clone(), sink_units, seize, expected_out: total_out, est_swap_gas: swap_gas }));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(v: &[u64]) -> Curve {
        Curve { out: v.iter().map(|x| U256::from(*x)).collect(), gas: vec![100_000; v.len()] }
    }
    fn u(x: U256) -> f64 {
        x.to::<u64>() as f64
    }
    fn repay90(s: U256) -> U256 {
        s * U256::from(9) / U256::from(10)
    }

    #[test]
    fn greedy_splits_direct_legs_by_marginal_output() {
        // Two direct legs, 4-unit grid (unit = 100 collateral -> repay 90 per unit).
        let g = Grid { unit: U256::from(100u64), sources: vec![curve(&[120, 230, 280, 300]), curve(&[110, 215, 315, 410])], ..Default::default() };
        let a = allocate(&g, 2, repay90, 0, |_| 0.0, u).unwrap();
        // 120(p0), 110(p0), 110(p1), 105(p1) -> 2/2 split = 445 out for 360 repaid.
        assert_eq!(a.source_units, vec![2, 2]);
        assert_eq!(a.expected_out, U256::from(445u64));
        assert!(a.sink_units.is_empty());
    }

    #[test]
    fn gas_budget_forces_fewer_legs() {
        // The 2/2 split of greedy_splits_direct_legs_by_marginal_output uses both legs: 100k each
        // (a leg's gas is per quoted amount) + 60k for the extra leg = 260k, net 445 - 360 = 85
        // before gas. p1 alone (4 units) nets 50 for 100k. At 3e-5 per gas the split wins (85 -
        // 7.8 > 50 - 3); one penalty step (x10) makes a single leg win (85 - 78 < 50 - 30; p0 x2 ties p1 x4).
        let g = Grid { unit: U256::from(100u64), sources: vec![curve(&[120, 230, 280, 300]), curve(&[110, 215, 315, 410])], ..Default::default() };
        let free = allocate_within(&g, 2, repay90, 60_000, 3e-5, u, None).unwrap().unwrap();
        assert_eq!((free.source_units.clone(), free.est_swap_gas), (vec![2, 2], 260_000));
        let fit = allocate_within(&g, 2, repay90, 60_000, 3e-5, u, Some(200_000)).unwrap().unwrap();
        assert_eq!((fit.source_units.iter().filter(|u| **u > 0).count(), fit.est_swap_gas), (1, 100_000), "one leg within budget: {fit:?}");
        // Nothing small enough stays profitable: the gas of the last allocation tried comes back.
        assert_eq!(allocate_within(&g, 2, repay90, 60_000, 3e-5, u, Some(50_000)).unwrap_err(), 100_000);
        // Unprofitable at any price: Ok(None).
        let poor = Grid { unit: U256::from(100u64), sources: vec![curve(&[50, 90, 120, 140])], ..Default::default() };
        assert!(allocate_within(&poor, 1, repay90, 60_000, 1e-6, u, Some(1_000_000)).unwrap().is_none());
    }

    #[test]
    fn fixed_leg_gas_prefers_single_leg() {
        // Greedy alone would start on p1 (101 > 100) and then pay a second leg's fixed gas;
        // p0 alone for all 4 units (400 - 360 - 10 = 30) must win.
        let g = Grid { unit: U256::from(100u64), sources: vec![curve(&[100, 200, 300, 400]), curve(&[101, 150, 160, 170])], ..Default::default() };
        let a = allocate(&g, 2, repay90, 200_000, |gas| gas as f64 / 10_000.0, u).unwrap();
        assert_eq!(a.source_units, vec![4, 0], "{a:?}");
    }

    #[test]
    fn shared_sink_is_not_double_counted() {
        // Two connector legs each turn 1 collateral unit into 10 K. The single sink sells the
        // first 10 K for 100 L but the next 10 K for only 40 L (steep slippage). A direct leg pays
        // a flat 95 L per unit. Quoting the two connector paths independently would value each at
        // 100 L/unit; the shared-sink model must send one unit via K and the rest direct.
        let g = Grid {
            unit: U256::from(100u64),
            sources: vec![curve(&[95, 190, 285, 380]), curve(&[10, 20, 30, 40]), curve(&[10, 20, 30, 40])],
            conn_unit: U256::from(10u64),
            sinks: vec![curve(&[100, 140, 150, 155])],
        };
        let a = allocate(&g, 1, repay90, 0, |_| 0.0, u).unwrap();
        assert_eq!(a.source_units[1] + a.source_units[2], 1, "{a:?}");
        assert_eq!(a.source_units[0], 3);
        assert_eq!(a.expected_out, U256::from(385u64));
        assert_eq!(a.sink_units, vec![1]);
    }

    #[test]
    fn unprofitable_returns_none() {
        let g = Grid { unit: U256::from(100u64), sources: vec![curve(&[80, 150])], ..Default::default() };
        assert!(allocate(&g, 1, |s| s, 0, |_| 0.0, u).is_none());
    }

    #[test]
    fn encode_shares_last_sink_takes_rest() {
        let pool = |b: u8| Address::repeat_byte(b);
        let mut r = V3Router { factories: vec![], connectors: vec![], quoter_code: Bytes::new(), routes: HashMap::new(), max_legs: 4, grid_points: 4 };
        r.routes.insert(
            0,
            MarketRoutes {
                direct: vec![Leg { pool: pool(1), zero_for_one: false, label: "d".into() }],
                to_conn: vec![Leg { pool: pool(2), zero_for_one: false, label: "c".into() }],
                sinks: vec![
                    Leg { pool: pool(3), zero_for_one: true, label: "s1".into() },
                    Leg { pool: pool(4), zero_for_one: true, label: "s2".into() },
                ],
            },
        );
        let g = Grid { unit: U256::from(5u64), ..Default::default() };
        let a = Allocation { source_units: vec![1, 2], sink_units: vec![3, 1], ..Default::default() };
        let b = r.encode(0, &g, &a);
        assert_eq!(b.len(), 4 * 37);
        let amt = |i: usize| u128::from_be_bytes(b[i * 37 + 21..i * 37 + 37].try_into().unwrap());
        assert_eq!((b[20], amt(0)), (0, 5)); // direct, exact 5
        assert_eq!((b[57], amt(1)), (0, 10)); // connector, exact 10
        assert_eq!((b[94], amt(2)), (0b101, 7_500)); // sink1: zfo + share flag, 3/4 of balance
        assert_eq!((b[131], amt(3)), (0b101, 10_000)); // sink2: the rest
    }
}
