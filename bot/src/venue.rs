//! The seam for the contract that executes a liquidation (`LiquidationVenue`): how a planned exit is
//! encoded into the transaction, and how that exact transaction is simulated. `sim::Simulator`
//! (MorphoLiquidator) is the first implementation; a liquidation contract for another protocol
//! implements the same trait and reuses the planner.

use std::future::Future;

use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes, U256},
    providers::Provider,
    rpc::types::state::StateOverride,
};
use anyhow::Result;

use crate::{
    dex::ExitRoute,
    executor::Payload,
    protocol::{LendingProtocol, Pair},
};
pub use onchain_core::sim::{SimAt, SimFee, SimReverted};

/// A venue's simulation, decoded: the profit its contract reports.
pub struct SimResult {
    pub profit: U256,
    pub gas_used: u64,
    pub l1_fee_wei: U256,
    /// Number of the block the node simulated (None if it did not say). On a "pending" base it
    /// shows whether the node really built on the in-progress block or on a stale one.
    pub block_number: Option<u64>,
    /// The probed oracle's `price()` in the simulated state, read just before the call.
    pub probe_price: Option<U256>,
}

pub trait LiquidationVenue<L: LendingProtocol>: Send + Sync {
    /// The contract the liquidation tx (or operation) is sent to.
    fn target(&self) -> Address;

    /// The contract that runs the exit swaps: an aggregator route is built for it as sender and
    /// recipient. The target itself unless the venue nests the liquidation in another contract (an
    /// Atlas solver liquidates through a liquidator it owns).
    fn swap_recipient(&self) -> Address {
        self.target()
    }

    /// Whether `encode` takes an `ExitRoute::Packed` (the self-built direct router's compact route).
    /// A venue without a packed entry only gets aggregator (`ExitRoute::Call`) exits.
    fn packed_routes(&self) -> bool {
        true
    }

    /// The liquidation of `seize` of `pair.collateral` from `user` in `market`, repaying `repay` of `pair.debt`
    /// (`LendingProtocol::repay_for_seize`), selling the collateral along `route`, as a sendable
    /// payload (calldata + where minProfit sits so it can be patched at send time). A venue that
    /// funds the repayment itself (flash loan) borrows `repay`; a callback venue may ignore it.
    #[allow(clippy::too_many_arguments)]
    fn encode(&self, market: &L::Market, user: Address, pair: Pair, seize: U256, repay: U256, route: &ExitRoute, min_profit: U256, deadline_block: u64) -> Payload;

    /// Simulates `calldata` against `target()` at `block` (plus `overrides`), as the next block at
    /// `at` when given, priced at `fee`; `probe` = an oracle whose price to read in the simulated
    /// state.
    #[allow(clippy::too_many_arguments)]
    fn simulate<P: Provider>(
        &self,
        p: &P,
        calldata: Bytes,
        block: BlockId,
        overrides: &StateOverride,
        at: Option<SimAt>,
        fee: Option<SimFee>,
        probe: Option<Address>,
    ) -> impl Future<Output = Result<SimResult>> + Send;

    /// Which gas limits `calldata` succeeds with (diagnostics only, BOT_GAS_PROBE).
    fn probe_gas<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, overrides: &StateOverride, limits: &[u64]) -> impl Future<Output = Vec<(u64, bool)>> + Send;
}
