//! Exit routing: how seized collateral is sold inside the liquidation transaction.
//!
//! - `ExitRoute` is the vocabulary between exit routers and the liquidation venue
//!   (`venue::LiquidationVenue`), which encodes it into its contract call.
//! - `Dex` is the seam for external swap aggregators (KyberSwap first: `router::Kyber`); another
//!   aggregator (Odos, 1inch, ...) implements it and the planner uses it unchanged.
//! - The framework's own on-chain router (`v3router::V3Router`) is not a `Dex`: it builds a packed
//!   multi-pool route the liquidation contract executes itself (`ExitRoute::Packed`), allocating
//!   the sale across pools under a gas budget, which a quote/build interface cannot express.

use std::future::Future;

use alloy::primitives::{Address, Bytes, U256};
use anyhow::Result;

/// A planned exit, ready to be encoded by the liquidation contract.
#[derive(Debug, Clone)]
pub enum ExitRoute {
    /// Multi-pool route in the framework's packed hop format (37 bytes per hop: pool, direction,
    /// share), executed by the liquidation contract's own swap code against V3-style pools.
    Packed(Bytes),
    /// One external call (e.g. an aggregator router) the liquidation contract makes with the
    /// seized collateral. `target` must be on the contract's allowlist.
    Call { target: Address, calldata: Bytes },
}

/// The router call an aggregator built for a quote.
#[derive(Debug, Clone)]
pub struct SwapCall {
    pub target: Address,
    pub calldata: Bytes,
}

/// An external swap aggregator.
pub trait Dex: Send + Sync {
    /// A priced route, carried from `quote_within` to `build`.
    type Quote: Send + Sync;

    /// Short name, used as `Plan::router` ("kyber").
    fn name(&self) -> &'static str;

    /// Best route selling `amount_in` of `token_in` for `token_out` whose swap is expected to fit
    /// `gas_budget`.
    fn quote_within(&self, token_in: Address, token_out: Address, amount_in: U256, gas_budget: u64) -> impl Future<Output = Result<Self::Quote>> + Send;

    /// Output of a quote, in `token_out` base units.
    fn amount_out(&self, quote: &Self::Quote) -> U256;

    /// Expected real gas of the quoted swap inside our liquidation; `None` = no estimate.
    fn swap_gas(&self, quote: &Self::Quote) -> Option<u64>;

    /// The router call for `quote`; `sender` holds the tokens and calls the router.
    fn build(&self, quote: &Self::Quote, sender: Address, recipient: Address, slippage_bps: u32) -> impl Future<Output = Result<SwapCall>> + Send;
}
