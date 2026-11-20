//! Bidding policies: the seam a binary plugs its strategy into (`Runner::build`). The runner
//! applies the hard limits (kill switch, budgets, the wallet's worst-case check) after the policy,
//! so a policy decides how much to bid, never how much may be spent.

use crate::{
    bid::{Bid, BidCurve},
    config::Config,
    planner::Plan,
};

/// What to bid in a Chainlink SVR / Atlas auction.
pub trait AuctionBidPolicy: Send + Sync + 'static {
    /// The bid in USD for an auction worth `net_usd` to us after the op's gas. `market` is the
    /// market of a solver op, None for the whole auction's bundle (the journaled valuation).
    /// None = do not bid.
    fn bid_usd(&self, market: Option<&str>, net_usd: f64, eth_usd: f64) -> Option<f64>;

    /// The bid with the whole context of the op (what the runner calls). The default ignores all but
    /// `bid_usd`'s inputs; a policy that discounts the value for what can change before the op
    /// executes (by market, size or the price move) overrides this.
    fn bid(&self, ctx: &AuctionBidCtx<'_>) -> Option<f64> {
        self.bid_usd(ctx.market, ctx.net_usd, ctx.eth_usd)
    }
}

/// What an auction bid is decided on (`AuctionBidPolicy::bid`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AuctionBidCtx<'a> {
    /// The market of the solver op; None for the whole auction's bundle (the journaled valuation).
    pub market: Option<&'a str>,
    /// What the op nets us after its gas (USD), valued on the state the auction arrived at: the bid
    /// basis.
    pub net_usd: f64,
    /// The liquidation bonus the op takes (USD at the auction price).
    pub bonus_usd: f64,
    /// Positions the op liquidates.
    pub positions: usize,
    /// The auction's move of the market's oracle price (new / old - 1; negative = the collateral fell).
    pub price_move: f64,
    pub eth_usd: f64,
}

/// The block path's priority fee.
pub trait PriorityBidPolicy: Send + Sync + 'static {
    /// The bid for `plan` in `market`; `competition` is the market's `WinningBids::ratio_quantile`
    /// (None = too few observations).
    fn bid(&self, market: &str, plan: &Plan, competition: Option<f64>, eth_usd: f64) -> Bid;
}

/// Net x (1 - `[atlas] margin`), the same in every market.
pub struct DefaultAuctionBid {
    pub margin: f64,
}

impl AuctionBidPolicy for DefaultAuctionBid {
    fn bid_usd(&self, _market: Option<&str>, net_usd: f64, _eth_usd: f64) -> Option<f64> {
        Some(net_usd * (1.0 - self.margin))
    }
}

/// The `[exec]` bid curve (profit share, outbidding the competition when it is known).
pub struct DefaultPriorityBid(pub BidCurve);

impl PriorityBidPolicy for DefaultPriorityBid {
    fn bid(&self, _market: &str, plan: &Plan, competition: Option<f64>, eth_usd: f64) -> Bid {
        self.0.priority(competition, plan.profit_usd.unwrap_or(0.0), plan.net_usd.unwrap_or(0.0), 0.0, plan.gas_used.unwrap_or(0), eth_usd)
    }
}

/// The default policies for `cfg`.
pub fn defaults(cfg: &Config) -> (DefaultAuctionBid, DefaultPriorityBid) {
    (DefaultAuctionBid { margin: cfg.atlas.margin }, DefaultPriorityBid(cfg.exec.bid_curve()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_auction_bid_keeps_the_margin() {
        let p = DefaultAuctionBid { margin: 0.1 };
        assert_eq!(p.bid_usd(None, 100.0, 2400.0), Some(90.0));
        assert_eq!(p.bid_usd(Some("WETH/USDC"), 0.0, 2400.0), Some(0.0));
    }

    #[test]
    fn the_default_context_bid_is_bid_usd() {
        let p = DefaultAuctionBid { margin: 0.1 };
        let ctx = AuctionBidCtx { market: Some("WETH/USDC"), net_usd: 100.0, bonus_usd: 120.0, positions: 3, price_move: -0.01, eth_usd: 2400.0 };
        assert_eq!(p.bid(&ctx), p.bid_usd(Some("WETH/USDC"), 100.0, 2400.0));
    }

    #[test]
    fn default_priority_bid_is_the_bid_curve() {
        let curve = BidCurve { base_wei: 1_000_000, max_wei: 1_000_000_000, share: 0.3, share_large: 0.3, large_net_usd: 0.0, quantile: 0.9, outbid_bps: 100, min_samples: 3 };
        let plan = Plan { profit_usd: Some(10.0), net_usd: Some(9.0), gas_used: Some(500_000), ..Default::default() };
        for competition in [None, Some(1e5)] {
            assert_eq!(DefaultPriorityBid(curve).bid("m", &plan, competition, 2400.0), curve.priority(competition, 10.0, 9.0, 0.0, 500_000, 2400.0));
        }
    }
}
