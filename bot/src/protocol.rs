//! The seam a lending protocol plugs into the generic health-factor engine (`engine::Engine`).
//!
//! The engine owns the chain-agnostic loop state: which positions are tracked, which markets exist,
//! incremental refresh (re-read only what events touched), dust filtering and HF ordering. A
//! protocol supplies how to read and interpret its own state. Morpho Blue (`morpho::Morpho`) is the
//! first implementation; a new protocol implements this trait and reuses the engine unchanged.
//!
//! Every method keeps the optimised shape of the original Morpho loop: batched reads (one call per
//! block for all snapshots, one for all touched positions), and an optional previous snapshot so a
//! protocol can do a one-round-trip refresh.

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    future::Future,
};

use alloy::{
    eips::BlockId,
    primitives::{Address, B256, U256},
    providers::Provider,
};
use anyhow::Result;
use futures::future::BoxFuture;

/// Static facts about one market the engine needs to index it.
pub trait MarketInfo {
    /// Stable on-chain key of the market (Morpho market id; an address, left-padded, elsewhere).
    fn key(&self) -> B256;
    /// Human name from the config, used in logs and the journal.
    fn name(&self) -> &str;
}

/// A borrower as reported by the protocol's indexer/API, used to discover positions that events
/// have not shown yet.
#[derive(Debug, Clone)]
pub struct BorrowerInfo {
    pub user: Address,
    /// The indexer's own health factor (validation only).
    pub api_health_factor: Option<f64>,
}

/// What a block range of protocol events changed.
#[derive(Debug, Clone)]
pub struct Touched<L> {
    /// market key -> accounts whose position may have changed.
    pub positions: HashMap<B256, HashSet<Address>>,
    /// Liquidations that happened in the range, with their market key.
    pub liquidations: Vec<(B256, L)>,
}

impl<L> Default for Touched<L> {
    fn default() -> Self {
        Self { positions: HashMap::new(), liquidations: Vec::new() }
    }
}

/// One position evaluated against a market snapshot.
#[derive(Debug, Clone)]
pub struct Evaluation<H, Q> {
    /// Protocol-specific health details (kept for planners/logs of that protocol).
    pub health: H,
    /// Health factor; < 1 means liquidatable.
    pub hf: f64,
    /// Debt in loan-token units.
    pub debt: f64,
    /// Largest collateral amount a liquidation may seize now (0 when not liquidatable).
    pub max_seize: U256,
    /// What liquidating `max_seize` would repay/seize, when liquidatable.
    pub quote: Option<Q>,
    /// Seized value at the oracle price minus repaid debt, in loan-token units (before swap costs
    /// and gas).
    pub bonus: f64,
}

/// A lending protocol the engine can track positions on.
pub trait LendingProtocol: Send + Sync + 'static {
    /// Static facts about one market, loaded once at start.
    type Market: MarketInfo + Clone + Debug + Send + Sync + 'static;
    /// One account's on-chain position in one market, as `load_positions` reads it.
    type Position: Copy + Debug + Send + Sync + 'static;
    /// Per-block market state needed to evaluate positions (price, accrued totals, ...).
    type Snapshot: Clone + Debug + Send + Sync + 'static;
    /// Protocol-specific health details carried on each evaluated position.
    type Health: Clone + Debug + Send + Sync + 'static;
    /// What a liquidation of a position would repay and seize.
    type Quote: Clone + Debug + Send + Sync + 'static;
    /// A liquidation observed on-chain.
    type Liquidation: Clone + Debug + Send + Sync + 'static;

    /// Loads (and validates) the configured markets.
    fn load_markets<P: Provider>(&self, p: &P) -> impl Future<Output = Result<Vec<Self::Market>>> + Send;

    /// Fetches borrower lists for every market from the protocol's indexer, as a self-contained
    /// future (owns everything it needs) so it can be spawned while the block loop keeps running.
    /// Entry `i` is for `markets[i]`; a failed fetch is an `Err` for that market only.
    fn borrower_job(&self, http: reqwest::Client, markets: &[Self::Market]) -> BoxFuture<'static, Vec<(usize, Result<Vec<BorrowerInfo>>)>>;

    /// Reads the given (market key, account) positions at `block`, in one batch. `None` = the read
    /// failed for that entry.
    fn load_positions<P: Provider>(&self, p: &P, keys: &[(B256, Address)], block: BlockId) -> impl Future<Output = Result<Vec<Option<Self::Position>>>> + Send;

    /// Whether a position still carries debt (positions without debt are dropped from tracking).
    fn has_debt(&self, position: &Self::Position) -> bool;

    /// Position-changing events and liquidations in `[from, to]` for the given markets.
    fn touched<P: Provider>(&self, p: &P, markets: &[B256], from: u64, to: u64) -> impl Future<Output = Result<Touched<Self::Liquidation>>> + Send;

    /// One snapshot per market at `block`, accrued to `timestamp`. `prev` (last block's snapshots)
    /// lets an implementation save a round trip. `None` = that market's read failed.
    fn snapshots<P: Provider>(
        &self,
        p: &P,
        markets: &[Self::Market],
        block: BlockId,
        timestamp: u64,
        prev: Option<&[Option<Self::Snapshot>]>,
    ) -> impl Future<Output = Result<Vec<Option<Self::Snapshot>>>> + Send;

    /// Pure evaluation of one position against its market's snapshot. `&self` gives access to
    /// protocol-wide parameters (close factor, global bonus, e-mode tables, ...).
    fn evaluate(&self, market: &Self::Market, snap: &Self::Snapshot, position: &Self::Position) -> Evaluation<Self::Health, Self::Quote>;

    // ---- what the generic planner needs to size and price a liquidation

    /// Tokens a liquidation's exit sells and receives: (collateral, loan).
    fn exit_tokens(&self, market: &Self::Market) -> (Address, Address);

    /// Decimals of the loan token: profits and gross margins are in loan-token units.
    fn loan_decimals(&self, market: &Self::Market) -> u8;

    /// Debt a liquidation repays when it seizes `seize` collateral at this snapshot, with the
    /// protocol's own rounding.
    fn repay_for_seize(&self, market: &Self::Market, snap: &Self::Snapshot, seize: U256) -> U256;

    /// Value of `amount` collateral in loan-token base units at the protocol's oracle price.
    fn collateral_value(&self, market: &Self::Market, snap: &Self::Snapshot, amount: U256) -> U256;

    /// USD per whole loan token (profits, gas trade-offs and bids are compared in USD). None = no
    /// price model for this loan token: such a market is refused rather than mispriced.
    fn loan_usd(&self, market: &Self::Market, snap: &Self::Snapshot, eth_usd: f64) -> Option<f64>;

    /// ETH/USD derived from tracked markets, for gas accounting; `None` = use the configured
    /// fallback.
    fn eth_usd(&self, _markets: &[Self::Market], _snaps: &[Option<Self::Snapshot>]) -> Option<f64> {
        None
    }
}

/// The protocol's own liquidation call, as a contract that already holds the repayment makes it
/// (no callback): `target` pulls the debt token from the caller and sends it the collateral.
#[derive(Debug, Clone)]
pub struct LiquidationTx {
    pub target: Address,
    pub calldata: alloy::primitives::Bytes,
    /// What the call pulls (and what the profit is counted in).
    pub debt_token: Address,
    /// What the call sends.
    pub collateral_token: Address,
}

/// A protocol that can be liquidated by a caller holding the repayment up front (Aave-style
/// `liquidationCall`; Morpho's `liquidate` without callback data works the same way). This is what
/// `flash_venue::FlashLiquidatorVenue` needs to flash-fund a liquidation.
pub trait DirectLiquidation: LendingProtocol {
    /// The call that seizes `seize` collateral of `user` in `market` while repaying `repay` of debt.
    fn liquidation_call(&self, market: &Self::Market, user: Address, seize: U256, repay: U256) -> LiquidationTx;
}
