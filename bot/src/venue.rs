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

use crate::{dex::ExitRoute, executor::Payload, protocol::LendingProtocol};

/// Execute a simulation as block `number` with timestamp `time`.
#[derive(Debug, Clone, Copy)]
pub struct SimAt {
    pub number: u64,
    pub time: u64,
}

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

/// A simulation that ran and reverted; `block_number` = the block the node simulated (see
/// SimResult::block_number).
#[derive(Debug)]
pub struct SimReverted {
    pub message: String,
    pub block_number: Option<u64>,
    pub probe_price: Option<U256>,
}

impl std::fmt::Display for SimReverted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sim reverted: {}", self.message)
    }
}

impl std::error::Error for SimReverted {}

pub trait LiquidationVenue<L: LendingProtocol>: Send + Sync {
    /// The contract the liquidation tx is sent to (also the sender/recipient of exit swaps).
    fn target(&self) -> Address;

    /// The liquidation of `seize` collateral of `user` in `market`, selling it along `route`, as a
    /// sendable payload (calldata + where minProfit sits so it can be patched at send time).
    fn encode(&self, market: &L::Market, user: Address, seize: U256, route: &ExitRoute, min_profit: U256, deadline_block: u64) -> Payload;

    /// Simulates `calldata` against `target()` at `block` (plus `overrides`), as the next block at
    /// `at` when given; `probe` = an oracle whose price to read in the simulated state.
    fn simulate<P: Provider>(
        &self,
        p: &P,
        calldata: Bytes,
        block: BlockId,
        overrides: &StateOverride,
        at: Option<SimAt>,
        probe: Option<Address>,
    ) -> impl Future<Output = Result<SimResult>> + Send;

    /// Which gas limits `calldata` succeeds with (diagnostics only, BOT_GAS_PROBE).
    fn probe_gas<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, overrides: &StateOverride, limits: &[u64]) -> impl Future<Output = Vec<(u64, bool)>> + Send;
}
