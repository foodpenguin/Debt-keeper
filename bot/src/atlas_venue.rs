//! Chainlink SVR / Atlas as a liquidation venue, SIMULATION ONLY: a liquidation is not sent as a
//! transaction but carried by a solver operation that Atlas executes right after the oracle update
//! it auctions. `AtlasLiquidationSolver.sol` runs the liquidation through a MorphoLiquidator it owns
//! (executor = profit receiver = the solver), buys the ETH bid with part of the loan token, pays it
//! to the execution environment and settles with Atlas.
//!
//! What is here: encoding the solver call (`atlasSolverCall` + its solverOpData) and simulating it
//! against `sim/AtlasStub.sol` standing in for the Atlas escrow, so a valuation can see whether the
//! op would succeed and what it nets after the bid and its gas. The op pays its gas in ETH out of
//! the proceeds (`gasPayWei`, bought with the bid in one swap); Atlas credits the bond with any
//! excess over the real charge and takes any shortfall from it. Also the gas maths of an op in a
//! metacall (`OpGasPrice`): what to pay, and Atlas's maximum liability that the bonded atlETH must
//! cover before Atlas runs the op. Building, signing and submitting the `SolverOperation` is in
//! atlas_submit.rs.

use std::collections::{HashMap, HashSet};

use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes, U256, address, aliases::U24},
    providers::Provider,
    rpc::types::state::{AccountOverride, StateOverride},
    sol,
    sol_types::{SolCall, SolValue},
};
use anyhow::{Context, Result};

use crate::{
    dex::ExitRoute,
    executor::{MinProfitSlot, Payload},
    morpho::{Morpho, onchain::MarketMeta, onchain::multicall},
    protocol::Pair,
    sim::{self, LiquidatorCode, Simulator},
    venue::{LiquidationVenue, SimAt, SimFee, SimResult},
};

use crate::contracts::{IAtlasSolver, SolverCall, SwapStep};

sol! {
    struct ExactOutputSingleParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 amountOut; uint256 amountInMaximum; uint160 sqrtPriceLimitX96; }
    interface IBidRouter { function exactOutputSingle(ExactOutputSingleParams params) external payable returns (uint256 amountIn); }
    interface IV3Factory { function getPool(address a, address b, uint24 fee) external view returns (address); }
    interface IV3Pool { function liquidity() external view returns (uint128); }
}

/// The deepest loan-token/WETH Uniswap-V3 pool's fee tier per loan token, probed at startup so the
/// bid swap does not fail on a thin or missing pool at one fixed fee (2026-11-13: BidSwapFailed). A
/// WETH loan needs no swap and is skipped; a token with no pool at any `tiers` is absent (the bid
/// swap falls back to the configured `bid_fee`).
pub async fn probe_bid_fees<P: Provider>(p: &P, factory: Address, weth: Address, loan_tokens: &[Address], tiers: &[u32]) -> HashMap<Address, u32> {
    let tokens: Vec<Address> = loan_tokens.iter().copied().filter(|t| *t != weth).collect::<HashSet<_>>().into_iter().collect();
    if tokens.is_empty() || tiers.is_empty() {
        return HashMap::new();
    }
    let pool_calls: Vec<(Address, Bytes)> =
        tokens.iter().flat_map(|t| tiers.iter().map(move |fee| (factory, IV3Factory::getPoolCall { a: *t, b: weth, fee: U24::from(*fee) }.abi_encode().into()))).collect();
    let pools: Vec<Option<Address>> = match multicall(p, &pool_calls, BlockId::latest()).await {
        Ok(r) => r.iter().map(|b| b.as_ref().and_then(|b| IV3Factory::getPoolCall::abi_decode_returns(b).ok()).filter(|a| !a.is_zero())).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "bid-fee probe: getPool failed; bid swap uses the default fee");
            return HashMap::new();
        }
    };
    let targets: Vec<(usize, Address)> = pools.iter().enumerate().filter_map(|(i, a)| a.map(|a| (i, a))).collect();
    let liq_calls: Vec<(Address, Bytes)> = targets.iter().map(|(_, a)| (*a, IV3Pool::liquidityCall {}.abi_encode().into())).collect();
    let liqs = multicall(p, &liq_calls, BlockId::latest()).await.unwrap_or_default();
    let mut liquidity = vec![0u128; pool_calls.len()];
    for ((i, _), r) in targets.iter().zip(liqs) {
        liquidity[*i] = r.and_then(|b| IV3Pool::liquidityCall::abi_decode_returns(&b).ok()).unwrap_or(0);
    }
    let mut out = HashMap::new();
    for (ti, t) in tokens.iter().enumerate() {
        let base = ti * tiers.len();
        if let Some((best, _)) = (0..tiers.len()).map(|k| (tiers[k], liquidity[base + k])).filter(|(_, l)| *l > 0).max_by_key(|(_, l)| *l) {
            out.insert(*t, best);
        }
    }
    out
}

/// minProfit inside `atlasSolverCall` calldata: selector + 6 head words, solverOpData's length
/// word, then its 4th head word (calls offset, bidSwap offset, profitToken, minProfit, gasPayWei).
pub const SOLVER_MIN_PROFIT: MinProfitSlot = MinProfitSlot { offset: 4 + 10 * 32, width: 32 };

/// Metacall gas Atlas counts in a solver's maximum liability besides the solver ops' and userOp's
/// gas limits and the DappControl's dappGasLimit: the bundler's buffer on the tx gas limit (~406k on
/// the SVR metacalls) plus Atlas's own fixed terms, less the tx's intrinsic calldata cost. Measured
/// on a real Base metacall (`AtlasBondGas.fork.t.sol`): 485k for a light solver op calldata (a
/// heavier calldata leaves less); 500k keeps a margin, so the bond check errs on refusing.
pub const LIABILITY_OVERHEAD_GAS: u64 = 500_000;

/// The metacall gas outside the solver ops that Atlas counts in a solver's maximum liability.
pub fn non_solver_gas(user_op_gas: u64, dapp_gas_limit: u64) -> u64 {
    user_op_gas + dapp_gas_limit + LIABILITY_OVERHEAD_GAS
}

/// What Atlas charges a solver op per unit of gas: the auction's max fee (the real gas price is at
/// most that) plus the escrow's surcharges (Atlas's and the bundler's, in basis points).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpGasPrice {
    pub max_fee_wei: u128,
    pub surcharge_bps: u64,
}

impl OpGasPrice {
    /// `surcharge` as a fraction (0.1 = 10 %).
    pub fn new(max_fee_wei: u128, surcharge: f64) -> Self {
        Self { max_fee_wei, surcharge_bps: (surcharge * 10_000.0).round().max(0.0) as u64 }
    }

    /// One unit of gas with surcharges, in wei, rounded up (so gas derived from a budget never
    /// charges more than the budget).
    fn per_gas(&self) -> U256 {
        (U256::from(self.max_fee_wei) * U256::from(10_000 + self.surcharge_bps)).div_ceil(U256::from(10_000u64))
    }

    fn charge(&self, gas: u64) -> U256 {
        U256::from(gas) * U256::from(self.max_fee_wei) * U256::from(10_000 + self.surcharge_bps) / U256::from(10_000u64)
    }

    /// The ETH an op of `op_gas` pays toward its own gas: its gas plus the metacall's fixed gas that
    /// Atlas charges the winner (METACALL_FIXED_GAS), as the bid basis prices them. Erring high is
    /// harmless: Atlas credits the excess back to the bond.
    pub fn gas_pay_wei(&self, op_gas: u64) -> U256 {
        self.charge(op_gas + METACALL_FIXED_GAS)
    }

    /// Atlas's maximum gas liability for an op of `op_gas` (its `solverOp.gas`): what the bonded
    /// atlETH must cover before Atlas runs the op (else InsufficientEscrow).
    pub fn liability_wei(&self, op_gas: u64, non_solver_gas: u64) -> U256 {
        self.charge(op_gas + non_solver_gas)
    }

    /// The largest `solverOp.gas` whose liability `bonded` covers (0 if none).
    pub fn affordable_op_gas(&self, bonded: U256, non_solver_gas: u64) -> u64 {
        let per = self.per_gas();
        if per.is_zero() {
            return u64::MAX;
        }
        let total: u64 = (bonded / per).try_into().unwrap_or(u64::MAX);
        total.saturating_sub(non_solver_gas)
    }
}
/// Where the solver lives in simulations when no deployed address is configured.
pub const SIM_SOLVER: Address = address!("00000000000000000000000000000000000b0773");
/// Receives the bid in simulations (a real metacall names its own execution environment).
pub const SIM_EXECUTION_ENVIRONMENT: Address = address!("00000000000000000000000000000000000e0e0e");

/// The solver setup shared by every simulated operation.
pub struct AtlasSolver {
    /// The Atlas escrow: the only caller the solver accepts (here: where the stub is injected).
    pub atlas: Address,
    pub solver: Address,
    /// The solver operation signer (`solverOpFrom`).
    pub signer: Address,
    pub weth: Address,
    pub bid_router: Address,
    pub bid_fee: u32,
    /// Per loan-token bid-swap fee tier (the deepest loan/WETH pool), probed at startup; a token not
    /// here uses `bid_fee`.
    bid_fees: HashMap<Address, u32>,
    /// The MorphoLiquidator the solver liquidates through.
    pub liquidator: Simulator,
    solver_code: Option<LiquidatorCode>,
    stub: Bytes,
}

impl AtlasSolver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(atlas: Address, solver: Option<Address>, signer: Address, weth: Address, bid_router: Address, bid_fee: u32, liquidator: Simulator, solver_code: Option<LiquidatorCode>, stub: Bytes) -> Self {
        Self { atlas, solver: solver.unwrap_or(SIM_SOLVER), signer, weth, bid_router, bid_fee, bid_fees: HashMap::new(), liquidator, solver_code: if solver.is_some() { None } else { solver_code }, stub }
    }

    /// Sets the per-loan-token bid-swap fee tiers from `probe_bid_fees`.
    pub fn with_bid_fees(mut self, bid_fees: HashMap<Address, u32>) -> Self {
        self.bid_fees = bid_fees;
        self
    }

    /// `atlasSolverCall` for one liquidation (`inner` = MorphoLiquidator calldata) that repays in
    /// `loan_token`, bids `bid_wei` in ETH and pays `gas_pay_wei` of its gas in ETH; both are bought
    /// with the loan token unless it is WETH. As Atlas calls it in a simulation (execution
    /// environment = a placeholder).
    pub fn calldata(&self, loan_token: Address, inner: &Bytes, bid_wei: U256, min_profit: U256, gas_pay_wei: U256) -> Bytes {
        IAtlasSolver::atlasSolverCallCall {
            solverOpFrom: self.signer,
            executionEnvironment: SIM_EXECUTION_ENVIRONMENT,
            bidToken: Address::ZERO,
            bidAmount: bid_wei,
            solverOpData: self.op_data(loan_token, inner, bid_wei, min_profit, gas_pay_wei),
            extraReturnData: Bytes::new(),
        }
        .abi_encode()
        .into()
    }

    /// The solver operation's `data` (what Atlas hands the solver as `solverOpData`):
    /// `(calls, bidSwap, profitToken, minProfit, gasPayWei)`. The bid swap buys the bid plus the gas
    /// pay, so changing `gas_pay_wei` (at submit, from the simulated gas) rebuilds both together.
    pub fn op_data(&self, loan_token: Address, inner: &Bytes, bid_wei: U256, min_profit: U256, gas_pay_wei: U256) -> Bytes {
        let calls = vec![SolverCall { target: self.liquidator.liquidator, data: inner.clone() }];
        (calls, self.bid_swap(loan_token, bid_wei + gas_pay_wei), loan_token, min_profit, gas_pay_wei).abi_encode_params().into()
    }

    /// The bid swap: buy `eth_wei` (bid + gas pay) of WETH with the loan token (none if the loan is
    /// WETH, unwrapped by the solver, or nothing to buy).
    fn bid_swap(&self, loan_token: Address, eth_wei: U256) -> SwapStep {
        if loan_token == self.weth || eth_wei.is_zero() {
            return SwapStep { target: Address::ZERO, tokenIn: Address::ZERO, amountIn: U256::ZERO, data: Bytes::new() };
        }
        let buy = IBidRouter::exactOutputSingleCall {
            params: ExactOutputSingleParams {
                tokenIn: loan_token,
                tokenOut: self.weth,
                fee: alloy::primitives::Uint::from(self.bid_fees.get(&loan_token).copied().unwrap_or(self.bid_fee)),
                recipient: self.solver,
                amountOut: eth_wei,
                amountInMaximum: U256::from(u128::MAX),
                sqrtPriceLimitX96: Default::default(),
            },
        };
        // Exact allowance of the whole balance; the router pulls only what the bid costs.
        SwapStep { target: self.bid_router, tokenIn: loan_token, amountIn: U256::MAX, data: buy.abi_encode().into() }
    }

    /// The inner MorphoLiquidator batch calldata for `plan` (N positions in one flash loan, a single
    /// exit over all the collateral): the `data` of one SolverCall, wrapped by `calldata`/`op_data`
    /// exactly like a single-position inner. Its own minProfit is 0 — the solver enforces the op's
    /// minProfit at SOLVER_MIN_PROFIT. `deadline_block` is the auction's.
    pub fn batch_inner(&self, meta: &MarketMeta, plan: &crate::planner::BundlePlan, deadline_block: u64) -> Bytes {
        let exit = batch_exit_bytes(&plan.exit, meta.params.collateralToken, plan.total_seize);
        self.liquidator.calldata_batch(&meta.params, &plan.items, plan.flash_assets, &exit, U256::ZERO, deadline_block)
    }

    /// Atlas stand-in (reporting `gas_liability_wei`, stub storage slot 0) + the solver (if not
    /// deployed) + its liquidator (if not deployed).
    pub(crate) fn overrides(&self, extra: &StateOverride, gas_liability_wei: U256) -> StateOverride {
        let mut ov = self.liquidator.overrides(extra);
        let liability = std::iter::once((alloy::primitives::B256::ZERO, alloy::primitives::B256::from(gas_liability_wei))).collect();
        ov.insert(self.atlas, AccountOverride { code: Some(self.stub.clone()), state: Some(liability), ..Default::default() });
        if let Some(code) = &self.solver_code {
            ov.insert(self.solver, code.account(self.signer, &[self.liquidator.liquidator, self.bid_router]));
        }
        ov
    }

    /// Runs `calldata` as Atlas would call the solver, the stand-in reporting `gas_liability_wei`
    /// (so the op pays its gasPayWei, up to it, as on chain); the profit is the net after the bid and
    /// the gas paid in ETH.
    #[allow(clippy::too_many_arguments)]
    pub async fn simulate_op<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, extra: &StateOverride, at: Option<SimAt>, fee: Option<SimFee>, gas_liability_wei: U256) -> Result<SimResult> {
        let r = sim::simulate_call(p, self.atlas, self.solver, calldata, block, &self.overrides(extra, gas_liability_wei), at, fee, None).await?;
        let profit = IAtlasSolver::atlasSolverCallCall::abi_decode_returns(&r.return_data).context("solver returned no profit")?;
        Ok(SimResult { profit, gas_used: r.gas_used, l1_fee_wei: r.l1_fee_wei, block_number: r.block_number, probe_price: None })
    }
}

/// The solver venue at one bid: what a liquidation planned for this auction would net if carried
/// by a solver operation bidding `bid_wei` and paying `gas_pay_wei` of its gas in ETH, Atlas
/// reporting a gas liability of `gas_liability_wei` (its maximum for the op).
pub struct AtlasBid<'a> {
    pub solver: &'a AtlasSolver,
    pub bid_wei: U256,
    pub gas_pay_wei: U256,
    pub gas_liability_wei: U256,
}

impl LiquidationVenue<Morpho> for AtlasBid<'_> {
    fn target(&self) -> Address {
        self.solver.solver
    }

    /// The swaps run inside the solver's MorphoLiquidator, so that is where an aggregator route
    /// must deliver.
    fn swap_recipient(&self) -> Address {
        self.solver.liquidator.liquidator
    }

    fn encode(&self, m: &MarketMeta, user: Address, pair: Pair, seize: U256, repay: U256, route: &ExitRoute, min_profit: U256, deadline_block: u64) -> Payload {
        let inner = self.solver.liquidator.encode(m, user, pair, seize, repay, route, U256::ZERO, deadline_block);
        Payload { to: self.solver.solver, calldata: self.solver.calldata(pair.debt, &inner.calldata, self.bid_wei, min_profit, self.gas_pay_wei), min_profit: SOLVER_MIN_PROFIT }
    }

    async fn simulate<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, overrides: &StateOverride, at: Option<SimAt>, fee: Option<SimFee>, _probe: Option<Address>) -> Result<SimResult> {
        self.solver.simulate_op(p, calldata, block, overrides, at, fee, self.gas_liability_wei).await
    }

    async fn probe_gas<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, overrides: &StateOverride, limits: &[u64]) -> Vec<(u64, bool)> {
        sim::probe_gas_call(p, self.solver.atlas, self.solver.solver, calldata, block, &self.solver.overrides(overrides, self.gas_liability_wei), limits).await
    }
}

// ---- bundle sizing: an Atlas solver op liquidates several positions of a market in one flash loan

/// Bundle valuation: gas per extra liquidated borrower inside one batch solver op. Calibrated on the
/// 2026-11-07 02:53 cbXRP auction on an archive fork (`AtlasBundleGas.fork.t.sol`): the flash-loan
/// batch adds ~34k per position (plus its 36-byte item calldata); 40k leaves a margin.
pub const BUNDLE_GAS_PER_POSITION: u64 = 40_000;
/// Bundle valuation: gas of a batch solver op besides the per-position liquidations and the exit
/// swap (solver wrapper, the Morpho flash loan, the first cold liquidation, the bid swap, settle).
/// Fork-measured intercept ~500k (1 position minus a trivial exit = 540k).
pub const BUNDLE_OVERHEAD_GAS: u64 = 500_000;
/// Gas inside the metacall that Atlas charges the winner but that is not the solver call (Atlas
/// verification, preOps, the oracle `transmitSecondary`, intrinsic + calldata): ~430k observed. A
/// cost only — it does not consume `solverGasLimit`, so it is not in the capacity maths, only the bid.
pub const METACALL_FIXED_GAS: u64 = 430_000;
/// Least gas worth leaving a bundle's exit swap (a 1-2 path exit: ~0.5-1.6M at cbXRP depth).
const MIN_BUNDLE_SWAP_GAS: u64 = 1_000_000;

/// How many positions one solver op can liquidate: the fewer of what `solver_gas_limit` leaves after
/// the overhead and the swap floor, and the configured `max_bundle_positions`.
pub fn bundle_capacity(solver_gas_limit: u64, max_positions: usize) -> usize {
    let by_gas = solver_gas_limit.saturating_sub(BUNDLE_OVERHEAD_GAS + MIN_BUNDLE_SWAP_GAS) / BUNDLE_GAS_PER_POSITION;
    (by_gas as usize).min(max_positions)
}

/// Gas a bundle of `n` positions leaves its swap inside one solver op.
pub fn bundle_swap_budget(solver_gas_limit: u64, n: usize) -> u64 {
    solver_gas_limit.saturating_sub(BUNDLE_OVERHEAD_GAS + BUNDLE_GAS_PER_POSITION * n as u64)
}

/// Safety on the *simulated* batch gas when it reprices the bid (`batch_op`). The simulation runs
/// the real op at block N, but Atlas charges the winner the gas the metacall actually uses at
/// execution (block N+1), which can drift a little higher (cold→warm storage, a position healed in
/// between). Atlas's gas *price* is fixed (the SVR auction's 15.005 gwei), so only the units move; a
/// small factor covers that drift. 1.08 keeps the sim-based bid from overshooting the real charge
/// while still recapturing most of the slack the estimate leaves (measured on live batch ops:
/// simulated/estimated median ~0.7, range 0.43–1.29). The conservative estimate still backs the
/// budget and the bond liability — only the bid uses this.
pub const SIM_GAS_SAFETY: f64 = 1.08;

/// Extra safety on an *aggregator* exit's gas in the bundle estimate (on top of KYBER_GAS_SAFETY):
/// the aggregator's own estimate is less reliable than the self-built direct router's (seen
/// 2026-11-13, a Kyber-exit batch ran ~1.2x its estimate), so its bid is priced and its budget
/// checked more conservatively. The direct route (exact quoter) takes no extra factor.
pub const BUNDLE_AGG_SWAP_SAFETY: f64 = 1.3;

/// Whether a batch op's simulated gas leaves enough headroom under `solver_gas_limit` to send: the
/// real sim gas (not the estimate) is gated so an under-estimated aggregator exit that would run
/// close to the cap is not sent (it risks running out of gas on-chain or being rejected by Atlas).
pub fn fits_solver_gas(gas_used: u64, solver_gas_limit: u64, headroom: f64) -> bool {
    (gas_used as f64) <= solver_gas_limit as f64 * headroom
}

/// Exit modes in the batch liquidator's `exit` bytes (match MorphoLiquidator MODE_ROUTE/MODE_STEPS).
const EXIT_MODE_ROUTE: u8 = 0x01;
const EXIT_MODE_STEPS: u8 = 0x02;

/// The batch liquidator's `exit`: a mode tag, then the packed route hops (direct) or the
/// abi-encoded `SwapStep[]` selling all the collateral through an aggregator (route built for
/// `total_seize`).
fn batch_exit_bytes(exit: &ExitRoute, collateral: Address, total_seize: U256) -> Vec<u8> {
    match exit {
        ExitRoute::Packed(route) => {
            let mut d = Vec::with_capacity(1 + route.len());
            d.push(EXIT_MODE_ROUTE);
            d.extend_from_slice(route);
            d
        }
        ExitRoute::Call { target, calldata } => {
            let steps = vec![SwapStep { target: *target, tokenIn: collateral, amountIn: total_seize, data: calldata.clone() }];
            let mut d = Vec::with_capacity(1 + 256);
            d.push(EXIT_MODE_STEPS);
            d.extend_from_slice(&steps.abi_encode());
            d
        }
    }
}


/// What `Planner::quote_bundle` charges a solver op besides its swap.
pub const ATLAS_BUNDLE_GAS: crate::planner::BundleGas = crate::planner::BundleGas { per_position: BUNDLE_GAS_PER_POSITION, overhead: BUNDLE_OVERHEAD_GAS, agg_swap_safety: BUNDLE_AGG_SWAP_SAFETY };

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{executor::patch_min_profit, morpho::onchain::MarketParams};

    fn solver() -> AtlasSolver {
        let liq = Simulator::new(Some(Address::repeat_byte(0x11)), SIM_SOLVER, None, vec![]);
        AtlasSolver::new(Address::repeat_byte(0xa7), None, Address::repeat_byte(0x5e), Address::repeat_byte(0x0e), Address::repeat_byte(0x22), 500, liq, None, Bytes::new())
    }

    fn meta(loan: Address) -> MarketMeta {
        MarketMeta {
            id: Default::default(),
            name: "t".into(),
            params: MarketParams { loanToken: loan, collateralToken: Address::repeat_byte(0xc0), oracle: Address::ZERO, irm: Address::ZERO, lltv: U256::from(860_000_000_000_000_000u64) },
            loan_decimals: 6,
            collateral_decimals: 18,
            loan_symbol: "USDC".into(),
            collateral_symbol: "X".into(),
        }
    }

    #[test]
    fn solver_op_capacity() {
        // 6M solverGasLimit: 500k overhead + 1M swap floor leave room for 112 by gas, but the
        // configured cap (20) wins.
        assert_eq!(bundle_capacity(6_000_000, 20), 20);
        assert_eq!(bundle_capacity(6_000_000, 200), 112, "gas-limited above the cap");
        // A 10-position bundle leaves 6M - 500k - 400k = 5.1M for the swap.
        assert_eq!(bundle_swap_budget(6_000_000, 10), 5_100_000);
        assert_eq!(bundle_capacity(1_000_000, 20), 0);
    }

    #[test]
    fn gas_send_guard_rejects_ops_near_the_solver_limit() {
        // 6M limit, 0.9 headroom: send up to 5.4M of real sim gas, not beyond.
        assert!(fits_solver_gas(4_000_000, 6_000_000, 0.9));
        assert!(fits_solver_gas(5_400_000, 6_000_000, 0.9));
        // The 2026-11-13 case: a Kyber-exit batch that really used 5.95M is not sent.
        assert!(!fits_solver_gas(5_950_000, 6_000_000, 0.9));
    }

    /// The solverOpData tuple: (calls, bidSwap, profitToken, minProfit, gasPayWei).
    type OpData = (Vec<SolverCall>, SwapStep, Address, U256, U256);

    #[test]
    fn wraps_the_liquidation_in_a_solver_call_that_buys_and_pays_the_bid_and_the_gas() {
        let s = solver();
        let usdc = Address::repeat_byte(0x10);
        let venue = AtlasBid { solver: &s, bid_wei: U256::from(5u8), gas_pay_wei: U256::from(2u8), gas_liability_wei: U256::from(9u8) };
        assert_eq!((venue.target(), venue.swap_recipient()), (SIM_SOLVER, Address::repeat_byte(0x11)), "aggregator routes deliver to the inner liquidator");
        let p = venue.encode(&meta(usdc), Address::repeat_byte(7), crate::morpho::pair_of(&meta(usdc)), U256::from(90u64), U256::from(80u64), &ExitRoute::Packed(Bytes::from_static(&[1, 2])), U256::from(3u8), 9);
        assert_eq!(p.to, SIM_SOLVER);
        let call = IAtlasSolver::atlasSolverCallCall::abi_decode(&p.calldata).unwrap();
        assert_eq!((call.solverOpFrom, call.executionEnvironment, call.bidToken, call.bidAmount), (Address::repeat_byte(0x5e), SIM_EXECUTION_ENVIRONMENT, Address::ZERO, U256::from(5u8)));
        let (calls, bid_swap, token, min, gas_pay) = <OpData>::abi_decode_params(&call.solverOpData).unwrap();
        assert_eq!((calls.len(), calls[0].target), (1, Address::repeat_byte(0x11)), "through the solver's liquidator");
        assert_eq!(calls[0].data[0], 0x00, "the liquidator's packed entry");
        assert_eq!((bid_swap.target, bid_swap.tokenIn, token, min, gas_pay), (Address::repeat_byte(0x22), usdc, usdc, U256::from(3u8), U256::from(2u8)));
        let buy = IBidRouter::exactOutputSingleCall::abi_decode(&bid_swap.data).unwrap();
        assert_eq!((buy.params.tokenOut, buy.params.amountOut, buy.params.recipient), (Address::repeat_byte(0x0e), U256::from(7u8), SIM_SOLVER), "buys the bid plus the gas pay");
        // minProfit sits at SOLVER_MIN_PROFIT (the gas pay after it is untouched).
        let patched = patch_min_profit(&p.calldata, p.min_profit, U256::from(4242u64)).unwrap();
        let again = <OpData>::abi_decode_params(&IAtlasSolver::atlasSolverCallCall::abi_decode(&patched).unwrap().solverOpData).unwrap();
        assert_eq!((again.3, again.4), (U256::from(4242u64), U256::from(2u8)));
        // A WETH loan needs no bid swap (the solver unwraps it); a zero bid still buys the gas pay.
        let weth_loan = s.calldata(Address::repeat_byte(0x0e), &Bytes::new(), U256::from(5u8), U256::ZERO, U256::from(2u8));
        let op = IAtlasSolver::atlasSolverCallCall::abi_decode(&weth_loan).unwrap().solverOpData;
        assert_eq!(<OpData>::abi_decode_params(&op).unwrap().1.target, Address::ZERO);
        let gas_only = <OpData>::abi_decode_params(&s.op_data(usdc, &Bytes::new(), U256::ZERO, U256::ZERO, U256::from(2u8))).unwrap().1;
        assert_eq!(IBidRouter::exactOutputSingleCall::abi_decode(&gas_only.data).unwrap().params.amountOut, U256::from(2u8));
        assert_eq!(<OpData>::abi_decode_params(&s.op_data(usdc, &Bytes::new(), U256::ZERO, U256::ZERO, U256::ZERO)).unwrap().1.target, Address::ZERO, "nothing to buy");
    }

    #[test]
    fn the_stub_reports_the_liability_it_is_given() {
        let s = solver();
        let ov = s.overrides(&StateOverride::default(), U256::from(12345u64));
        let stub = ov.get(&Address::repeat_byte(0xa7)).expect("stub at the atlas address");
        assert_eq!(stub.state.as_ref().unwrap().get(&alloy::primitives::B256::ZERO), Some(&alloy::primitives::B256::from(U256::from(12345u64))), "slot 0 = gasLiability");
    }

    /// The bond-check maths against the real metacall (`AtlasBondGas.fork.t.sol`): at 15.005 gwei
    /// and 10 % surcharge, a 2.2M op with userOp 0.5M + dappGasLimit 2M faced a 0.0856 ETH threshold
    /// (2,985,228 non-solver gas). The bot's liability (500k overhead) must not be below it.
    #[test]
    fn op_gas_maths_cover_the_measured_liability() {
        let price = OpGasPrice::new(15_005_000_000, 0.1);
        assert_eq!(price.surcharge_bps, 1000);
        let ns = non_solver_gas(500_000, 2_000_000);
        assert_eq!(ns, 3_000_000);
        let liability = price.liability_wei(2_200_000, ns);
        let measured = U256::from(85_584_793_090_820_312u64);
        assert!(liability >= measured, "liability {liability} >= measured threshold {measured}");
        assert!(liability - measured < measured / U256::from(100u64), "within 1 % above it");
        // Gas pay: (op gas + the metacall's fixed gas) x price x 1.1.
        assert_eq!(price.gas_pay_wei(170_000), U256::from(600_000u64) * U256::from(15_005_000_000u64) * U256::from(11u64) / U256::from(10u64));
        // Affordable op gas: the inverse of the liability, never above what the bond covers.
        let bonded = U256::from(100_000_000_000_000_000u64); // 0.1 ETH
        let g = price.affordable_op_gas(bonded, ns);
        assert!(price.liability_wei(g, ns) <= bonded && price.liability_wei(g + 1_000, ns) > bonded, "{g}");
        assert_eq!(price.affordable_op_gas(U256::from(1u8), ns), 0, "a bond below the non-solver part affords nothing");
    }

    #[test]
    fn batch_inner_builds_a_batch_entry_with_the_right_exit_mode() {
        let s = solver();
        let m = meta(Address::repeat_byte(0x10));
        let plan = crate::planner::BundlePlan {
            items: vec![(Address::repeat_byte(0xB0), U256::from(100u64)), (Address::repeat_byte(0xB1), U256::from(200u64))],
            total_seize: U256::from(300u64),
            flash_assets: U256::from(280u64),
            exit: ExitRoute::Packed(Bytes::from_static(&[7, 8, 9])),
            router: "direct",
            gas_est: 1_000_000,
        };
        let inner = s.batch_inner(&m, &plan, 42);
        assert_eq!(inner[0], 0x01, "batch packed tag");
        assert_eq!(u16::from_be_bytes(inner[125..127].try_into().unwrap()), 2, "two positions");
        // Exit starts after the 127-byte head and 2 x 36-byte items: MODE_ROUTE then the route.
        assert_eq!(&inner[199..], &[EXIT_MODE_ROUTE, 7, 8, 9]);

        // A Kyber exit: MODE_STEPS + abi.encode(SwapStep[]) selling the whole seized amount.
        let plan2 = crate::planner::BundlePlan { exit: ExitRoute::Call { target: Address::repeat_byte(0x22), calldata: Bytes::from_static(&[0xde, 0xad]) }, ..plan.clone() };
        let inner2 = s.batch_inner(&m, &plan2, 42);
        assert_eq!(inner2[199], EXIT_MODE_STEPS);
        let steps = <Vec<SwapStep>>::abi_decode(&inner2[200..]).unwrap();
        assert_eq!((steps.len(), steps[0].target, steps[0].tokenIn, steps[0].amountIn), (1, Address::repeat_byte(0x22), m.params.collateralToken, U256::from(300u64)));
    }
}
