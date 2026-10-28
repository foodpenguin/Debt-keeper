//! Liquidation bot framework.
//!
//! - `protocol`: the lending-protocol seam (`LendingProtocol`, `DirectLiquidation`); `morpho` is
//!   the first implementation;
//! - `engine`: the generic health-factor engine (`Engine<P, L>`); `planner`: exit routing, sizing
//!   and simulation of one liquidation or a bundle;
//! - `venue`: the liquidation-contract seam (`LiquidationVenue`): `sim::Simulator`
//!   (MorphoLiquidator), `flash_venue` (FlashLiquidator), `atlas_venue` (Atlas solver);
//! - `funding`: flash sources; `dex` / `router` / `v3router`: exit routing;
//! - `risk`: the send/skip decision; `oracle`, `flashblocks`: same-block oracle updates;
//! - `atlas`: Chainlink SVR / Atlas auction valuation; `loss`, `dash`, `live`: observability.
//!
//! The plumbing (RPC pool, heads, executor, simulation, gates) comes from `onchain-core` and is
//! re-exported here under its usual module names.

pub use onchain_core::{actor, bid, chain, executor, gates, heads, num, rpc};

pub mod atlas;
pub mod atlas_submit;
pub mod atlas_venue;
pub mod config;
pub mod contracts;
pub mod dash;
pub mod dex;
pub mod engine;
pub mod flash_venue;
pub mod flashblocks;
pub mod funding;
pub mod live;
pub mod loss;
pub mod morpho;
pub mod oracle;
pub mod planner;
pub mod protocol;
pub mod risk;
pub mod router;
pub mod sim;
pub mod v3router;
pub mod venue;

#[cfg(test)]
mod e2e;
#[cfg(test)]
pub(crate) mod testkit;
