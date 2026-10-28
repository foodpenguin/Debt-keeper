//! `FlashLiquidator.sol` as a `LiquidationVenue`: for protocols whose liquidation pulls the
//! repayment up front (no callback), the repayment is flash-borrowed from a `funding::FlashSource`,
//! the protocol's own liquidation call (`DirectLiquidation::liquidation_call`) repays the debt and
//! sends the collateral, an aggregator swap sells it, and the flash debt is repaid.
//!
//! Only `ExitRoute::Call` exits: the contract has no packed-route entry (`packed_routes` = false),
//! so the planner never routes the self-built direct router's packed route here.

use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes, U256, address},
    providers::Provider,
    rpc::types::state::StateOverride,
    sol_types::SolCall,
};
use anyhow::Result;

use crate::{
    dex::ExitRoute,
    executor::{MinProfitSlot, Payload},
    funding::FlashSource,
    protocol::DirectLiquidation,
    sim::{self, LiquidatorCode},
    venue::{LiquidationVenue, SimAt, SimFee, SimResult},
};

pub use crate::contracts::IFlashLiquidator;
use crate::contracts::{LiquidationCall, SwapStep};

/// ABI `liquidate(...)`: selector + 6 head words before minProfit (funding, debtToken, debtAmount,
/// collateralToken, liquidation offset, swaps offset).
pub const FLASH_MIN_PROFIT: MinProfitSlot = MinProfitSlot { offset: 4 + 6 * 32, width: 32 };

/// Where the flash liquidator lives in simulations when no deployed address is configured.
pub const SIM_FLASH_LIQUIDATOR: Address = address!("00000000000000000000000000000000000b0774");

pub struct FlashLiquidatorVenue<L> {
    /// Builds the protocol's own liquidation call.
    pub protocol: L,
    pub liquidator: Address,
    pub executor: Address,
    pub source: Box<dyn FlashSource>,
    /// Borrowed on top of the planned repayment (bps), so interest accrued between planning and
    /// inclusion cannot make the protocol pull more than was borrowed. Unspent headroom goes back
    /// to the funder with the loan.
    pub headroom_bps: u32,
    /// Some(code) = inject the contract at `liquidator` (not deployed yet).
    code: Option<LiquidatorCode>,
    /// Allowlists of the injected contract: lending protocols (liquidation targets) and swap routers.
    protocols: Vec<Address>,
    swap_targets: Vec<Address>,
}

impl<L: DirectLiquidation> FlashLiquidatorVenue<L> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(protocol: L, liquidator: Option<Address>, executor: Address, source: Box<dyn FlashSource>, headroom_bps: u32, code: Option<LiquidatorCode>, protocols: Vec<Address>, swap_targets: Vec<Address>) -> Self {
        Self {
            protocol,
            liquidator: liquidator.unwrap_or(SIM_FLASH_LIQUIDATOR),
            executor,
            source,
            headroom_bps,
            code: if liquidator.is_some() { None } else { code },
            protocols,
            swap_targets,
        }
    }

    /// What is flash-borrowed for a liquidation repaying `repay`.
    pub fn borrow_for(&self, repay: U256) -> U256 {
        repay + repay * U256::from(self.headroom_bps) / U256::from(10_000u64) + U256::from(1u8)
    }

    fn overrides(&self, extra: &StateOverride) -> StateOverride {
        let mut ov = extra.clone();
        if let Some(code) = &self.code {
            ov.insert(self.liquidator, code.account_with(self.executor, &[("allowedTarget", &self.swap_targets), ("allowedProtocol", &self.protocols)]));
        }
        ov
    }
}

impl<L: DirectLiquidation> LiquidationVenue<L> for FlashLiquidatorVenue<L> {
    fn target(&self) -> Address {
        self.liquidator
    }

    fn packed_routes(&self) -> bool {
        false
    }

    fn encode(&self, market: &L::Market, user: Address, seize: U256, repay: U256, route: &ExitRoute, min_profit: U256, deadline_block: u64) -> Payload {
        let tx = self.protocol.liquidation_call(market, user, seize, repay);
        // No packed entry: a packed route (which the planner never sends here) yields no swap and
        // fails the profit check in simulation instead of being misread.
        let swaps = match route {
            // Allowance on the whole collateral balance: a protocol that derives the collateral from
            // the debt repaid (Aave-style, with a protocol fee and rounding) may send slightly more
            // or less than planned. The route itself sells what it was quoted for; any rest is swept.
            ExitRoute::Call { target, calldata } => vec![SwapStep { target: *target, tokenIn: tx.collateral_token, amountIn: U256::MAX, data: calldata.clone() }],
            ExitRoute::Packed(_) => vec![],
        };
        let calldata = IFlashLiquidator::liquidateCall {
            funding: self.source.mode(),
            debtToken: tx.debt_token,
            debtAmount: self.borrow_for(repay),
            collateralToken: tx.collateral_token,
            liquidation: LiquidationCall { target: tx.target, data: tx.calldata },
            swaps,
            minProfit: min_profit,
            deadlineBlock: U256::from(deadline_block),
        }
        .abi_encode()
        .into();
        Payload { to: self.liquidator, calldata, min_profit: FLASH_MIN_PROFIT }
    }

    async fn simulate<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, overrides: &StateOverride, at: Option<SimAt>, fee: Option<SimFee>, probe: Option<Address>) -> Result<SimResult> {
        let r = sim::simulate_call(p, self.executor, self.liquidator, calldata, block, &self.overrides(overrides), at, fee, probe).await?;
        let profit = IFlashLiquidator::liquidateCall::abi_decode_returns(&r.return_data)?;
        Ok(SimResult { profit, gas_used: r.gas_used, l1_fee_wei: r.l1_fee_wei, block_number: r.block_number, probe_price: r.probe })
    }

    async fn probe_gas<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, overrides: &StateOverride, limits: &[u64]) -> Vec<(u64, bool)> {
        sim::probe_gas_call(p, self.executor, self.liquidator, calldata, block, &self.overrides(overrides), limits).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        executor::patch_min_profit,
        funding::{MorphoBlue, UniswapV4},
        testkit::{Mk, Toy, key, user},
    };

    fn venue(source: Box<dyn FlashSource>) -> FlashLiquidatorVenue<Toy> {
        FlashLiquidatorVenue::new(Toy::default(), Some(Address::repeat_byte(0xf1)), Address::repeat_byte(0xee), source, 10, None, vec![], vec![])
    }

    #[test]
    fn encodes_the_protocol_call_inside_a_flash_funded_liquidation() {
        let v = venue(Box::new(UniswapV4 { pool_manager: Address::repeat_byte(4) }));
        let m = Mk { key: key(1), name: "alpha".into() };
        let route = ExitRoute::Call { target: Address::repeat_byte(0x22), calldata: Bytes::from_static(b"swap") };
        let p = v.encode(&m, user(7), U256::from(90u64), U256::from(10_000u64), &route, U256::from(5u8), 42);
        assert_eq!(p.to, Address::repeat_byte(0xf1));
        let call = IFlashLiquidator::liquidateCall::abi_decode(&p.calldata).unwrap();
        assert_eq!(call.funding, 2, "V4 flash accounting");
        // 10_000 repay + 10 bps headroom + 1 wei.
        assert_eq!(call.debtAmount, U256::from(10_011u64));
        let tx = Toy::default().liquidation_call(&m, user(7), U256::from(90u64), U256::from(10_000u64));
        assert_eq!((call.debtToken, call.collateralToken, call.liquidation.target, call.liquidation.data), (tx.debt_token, tx.collateral_token, tx.target, tx.calldata));
        assert_eq!((call.swaps.len(), call.swaps[0].target, call.swaps[0].amountIn), (1, Address::repeat_byte(0x22), U256::MAX), "allowance on what actually arrived");
        assert_eq!((call.minProfit, call.deadlineBlock), (U256::from(5u8), U256::from(42u8)));
        // The executor patches minProfit at FLASH_MIN_PROFIT.
        let patched = patch_min_profit(&p.calldata, p.min_profit, U256::from(777u64)).unwrap();
        assert_eq!(IFlashLiquidator::liquidateCall::abi_decode(&patched).unwrap().minProfit, U256::from(777u64));
        assert!(!LiquidationVenue::<Toy>::packed_routes(&v));
        let morpho = venue(Box::new(MorphoBlue { address: Address::repeat_byte(5) }));
        let p = morpho.encode(&m, user(7), U256::from(90u64), U256::from(10_000u64), &ExitRoute::Packed(Bytes::new()), U256::ZERO, 42);
        let call = IFlashLiquidator::liquidateCall::abi_decode(&p.calldata).unwrap();
        assert_eq!((call.funding, call.swaps.len()), (1, 0), "Morpho flash loan; a packed route carries no swap");
    }
}
