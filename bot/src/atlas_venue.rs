//! Chainlink SVR / Atlas as a liquidation venue, SIMULATION ONLY: a liquidation is not sent as a
//! transaction but carried by a solver operation that Atlas executes right after the oracle update
//! it auctions. `AtlasLiquidationSolver.sol` runs the liquidation through a MorphoLiquidator it owns
//! (executor = profit receiver = the solver), buys the ETH bid with part of the loan token, pays it
//! to the execution environment and settles with Atlas.
//!
//! What is here: encoding the solver call (`atlasSolverCall` + its solverOpData) and simulating it
//! against `sim/AtlasStub.sol` standing in for the Atlas escrow (no gas liability), so a valuation
//! can see whether the op would succeed and what it nets after the bid. What is NOT here: building
//! and signing the `SolverOperation` (EIP-712 over Atlas's typehash) and submitting it to the
//! auction endpoint; those are a later step.

use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes, U256, address},
    providers::Provider,
    rpc::types::state::{AccountOverride, StateOverride},
    sol,
    sol_types::{SolCall, SolValue},
};
use anyhow::{Context, Result};

use crate::{
    dex::ExitRoute,
    executor::{MinProfitSlot, Payload},
    morpho::{Morpho, onchain::MarketMeta},
    sim::{self, LiquidatorCode, Simulator},
    venue::{LiquidationVenue, SimAt, SimFee, SimResult},
};

use crate::contracts::{IAtlasSolver, SolverCall, SwapStep};

sol! {
    struct ExactOutputSingleParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 amountOut; uint256 amountInMaximum; uint160 sqrtPriceLimitX96; }
    interface IBidRouter { function exactOutputSingle(ExactOutputSingleParams params) external payable returns (uint256 amountIn); }
}

/// minProfit inside `atlasSolverCall` calldata: selector + 6 head words, solverOpData's length
/// word, then its 4th head word (calls offset, bidSwap offset, profitToken, minProfit).
pub const SOLVER_MIN_PROFIT: MinProfitSlot = MinProfitSlot { offset: 4 + 10 * 32, width: 32 };
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
    /// The MorphoLiquidator the solver liquidates through.
    pub liquidator: Simulator,
    solver_code: Option<LiquidatorCode>,
    stub: Bytes,
}

impl AtlasSolver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(atlas: Address, solver: Option<Address>, signer: Address, weth: Address, bid_router: Address, bid_fee: u32, liquidator: Simulator, solver_code: Option<LiquidatorCode>, stub: Bytes) -> Self {
        Self { atlas, solver: solver.unwrap_or(SIM_SOLVER), signer, weth, bid_router, bid_fee, liquidator, solver_code: if solver.is_some() { None } else { solver_code }, stub }
    }

    /// `atlasSolverCall` for one liquidation (`inner` = MorphoLiquidator calldata) that repays in
    /// `loan_token` and bids `bid_wei` in ETH; the bid is bought with the loan token unless it is WETH.
    pub fn calldata(&self, loan_token: Address, inner: &Bytes, bid_wei: U256, min_profit: U256) -> Bytes {
        let calls = vec![SolverCall { target: self.liquidator.liquidator, data: inner.clone() }];
        let bid_swap = if loan_token == self.weth || bid_wei.is_zero() {
            SwapStep { target: Address::ZERO, tokenIn: Address::ZERO, amountIn: U256::ZERO, data: Bytes::new() }
        } else {
            let buy = IBidRouter::exactOutputSingleCall {
                params: ExactOutputSingleParams {
                    tokenIn: loan_token,
                    tokenOut: self.weth,
                    fee: alloy::primitives::Uint::from(self.bid_fee),
                    recipient: self.solver,
                    amountOut: bid_wei,
                    amountInMaximum: U256::from(u128::MAX),
                    sqrtPriceLimitX96: Default::default(),
                },
            };
            // Exact allowance of the whole balance; the router pulls only what the bid costs.
            SwapStep { target: self.bid_router, tokenIn: loan_token, amountIn: U256::MAX, data: buy.abi_encode().into() }
        };
        let op_data = (calls, bid_swap, loan_token, min_profit).abi_encode_params();
        IAtlasSolver::atlasSolverCallCall {
            solverOpFrom: self.signer,
            executionEnvironment: SIM_EXECUTION_ENVIRONMENT,
            bidToken: Address::ZERO,
            bidAmount: bid_wei,
            solverOpData: op_data.into(),
            extraReturnData: Bytes::new(),
        }
        .abi_encode()
        .into()
    }

    /// Atlas stand-in + the solver (if not deployed) + its liquidator (if not deployed).
    fn overrides(&self, extra: &StateOverride) -> StateOverride {
        let mut ov = self.liquidator.overrides(extra);
        ov.insert(self.atlas, AccountOverride { code: Some(self.stub.clone()), ..Default::default() });
        if let Some(code) = &self.solver_code {
            ov.insert(self.solver, code.account(self.signer, &[self.liquidator.liquidator, self.bid_router]));
        }
        ov
    }

    /// Runs `calldata` as Atlas would call the solver; the profit is the net after the bid.
    pub async fn simulate_op<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, extra: &StateOverride, at: Option<SimAt>, fee: Option<SimFee>) -> Result<SimResult> {
        let r = sim::simulate_call(p, self.atlas, self.solver, calldata, block, &self.overrides(extra), at, fee, None).await?;
        let profit = IAtlasSolver::atlasSolverCallCall::abi_decode_returns(&r.return_data).context("solver returned no profit")?;
        Ok(SimResult { profit, gas_used: r.gas_used, l1_fee_wei: r.l1_fee_wei, block_number: r.block_number, probe_price: None })
    }
}

/// The solver venue at one bid: what a liquidation planned for this auction would net if carried
/// by a solver operation bidding `bid_wei`.
pub struct AtlasBid<'a> {
    pub solver: &'a AtlasSolver,
    pub bid_wei: U256,
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

    fn encode(&self, m: &MarketMeta, user: Address, seize: U256, repay: U256, route: &ExitRoute, min_profit: U256, deadline_block: u64) -> Payload {
        let inner = self.solver.liquidator.encode(m, user, seize, repay, route, U256::ZERO, deadline_block);
        Payload { to: self.solver.solver, calldata: self.solver.calldata(m.params.loanToken, &inner.calldata, self.bid_wei, min_profit), min_profit: SOLVER_MIN_PROFIT }
    }

    async fn simulate<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, overrides: &StateOverride, at: Option<SimAt>, fee: Option<SimFee>, _probe: Option<Address>) -> Result<SimResult> {
        self.solver.simulate_op(p, calldata, block, overrides, at, fee).await
    }

    async fn probe_gas<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, overrides: &StateOverride, limits: &[u64]) -> Vec<(u64, bool)> {
        sim::probe_gas_call(p, self.solver.atlas, self.solver.solver, calldata, block, &self.solver.overrides(overrides), limits).await
    }
}

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
    fn wraps_the_liquidation_in_a_solver_call_that_buys_and_pays_the_bid() {
        let s = solver();
        let usdc = Address::repeat_byte(0x10);
        let venue = AtlasBid { solver: &s, bid_wei: U256::from(5u8) };
        assert_eq!((venue.target(), venue.swap_recipient()), (SIM_SOLVER, Address::repeat_byte(0x11)), "aggregator routes deliver to the inner liquidator");
        let p = venue.encode(&meta(usdc), Address::repeat_byte(7), U256::from(90u64), U256::from(80u64), &ExitRoute::Packed(Bytes::from_static(&[1, 2])), U256::from(3u8), 9);
        assert_eq!(p.to, SIM_SOLVER);
        let call = IAtlasSolver::atlasSolverCallCall::abi_decode(&p.calldata).unwrap();
        assert_eq!((call.solverOpFrom, call.executionEnvironment, call.bidToken, call.bidAmount), (Address::repeat_byte(0x5e), SIM_EXECUTION_ENVIRONMENT, Address::ZERO, U256::from(5u8)));
        let (calls, bid_swap, token, min) = <(Vec<SolverCall>, SwapStep, Address, U256)>::abi_decode_params(&call.solverOpData).unwrap();
        assert_eq!((calls.len(), calls[0].target), (1, Address::repeat_byte(0x11)), "through the solver's liquidator");
        assert_eq!(calls[0].data[0], 0x00, "the liquidator's packed entry");
        assert_eq!((bid_swap.target, bid_swap.tokenIn, token, min), (Address::repeat_byte(0x22), usdc, usdc, U256::from(3u8)));
        let buy = IBidRouter::exactOutputSingleCall::abi_decode(&bid_swap.data).unwrap();
        assert_eq!((buy.params.tokenOut, buy.params.amountOut, buy.params.recipient), (Address::repeat_byte(0x0e), U256::from(5u8), SIM_SOLVER));
        // minProfit sits at SOLVER_MIN_PROFIT.
        let patched = patch_min_profit(&p.calldata, p.min_profit, U256::from(4242u64)).unwrap();
        let again = IAtlasSolver::atlasSolverCallCall::abi_decode(&patched).unwrap();
        assert_eq!(<(Vec<SolverCall>, SwapStep, Address, U256)>::abi_decode_params(&again.solverOpData).unwrap().3, U256::from(4242u64));
        // A WETH loan (or no bid) needs no bid swap.
        let weth_loan = s.calldata(Address::repeat_byte(0x0e), &Bytes::new(), U256::from(5u8), U256::ZERO);
        let op = IAtlasSolver::atlasSolverCallCall::abi_decode(&weth_loan).unwrap().solverOpData;
        assert_eq!(<(Vec<SolverCall>, SwapStep, Address, U256)>::abi_decode_params(&op).unwrap().1.target, Address::ZERO);
    }
}
