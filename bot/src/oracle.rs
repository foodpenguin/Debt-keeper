//! Local Morpho oracle prices for same-block (Flashblocks) mode.
//!
//! MorphoChainlinkOracleV2.price() = SCALE_FACTOR.mulDiv(B1 * B2, Q1 * Q2), where each factor is
//! a Chainlink feed's latest answer (1 for an unused slot) and vault slots are unused. Knowing
//! the feed answers, a new answer seen in a flashblock gives the new Morpho price without any RPC.
//! A market is on the fast path only if the model reproduces `oracle.price()` exactly at startup
//! (markets with ERC-4626 vault factors or other oracle types are left to the block path).
//!
//! The model also owns the table of who may update each aggregator (see `flashblocks::Authorized`),
//! shared with the stream so that a swapped aggregator or transmitter set is picked up live.


use std::collections::HashMap;

use alloy::{
    eips::BlockId,
    primitives::{Address, U256, U512},
    providers::Provider,
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
};
use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::{
    flashblocks::{Authorized, Path, Watched},
    morpho::onchain::MarketMeta,
};

sol! {
    interface IMorphoChainlinkOracleV2 {
        function BASE_FEED_1() external view returns (address);
        function BASE_FEED_2() external view returns (address);
        function QUOTE_FEED_1() external view returns (address);
        function QUOTE_FEED_2() external view returns (address);
        function BASE_VAULT() external view returns (address);
        function QUOTE_VAULT() external view returns (address);
        function SCALE_FACTOR() external view returns (uint256);
        function price() external view returns (uint256);
    }
    interface IChainlinkProxy {
        function aggregator() external view returns (address);
        function latestRoundData() external view returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound);
    }
    interface IOcr2Aggregator { function getTransmitters() external view returns (address[]); }
    interface IAuthorizedForwarder { function getAuthorizedSenders() external view returns (address[]); }
    interface ITypeAndVersion { function typeAndVersion() external view returns (string); }
}

async fn call<P: Provider, C: SolCall>(p: &P, to: Address, c: C, block: BlockId) -> Result<C::Return> {
    let tx = TransactionRequest::default().to(to).input(c.abi_encode().into());
    let out = p.call(tx).block(block).await?;
    Ok(C::abi_decode_returns(&out)?)
}

/// `aggregator.getTransmitters()`, each resolved to a forwarder (with its authorized senders) or,
/// when it has no `getAuthorizedSenders()`, a direct-transmit EOA.
async fn load_authorized<P: Provider>(p: &P, aggregator: Address, block: BlockId) -> Result<Authorized> {
    let transmitters = call(p, aggregator, IOcr2Aggregator::getTransmittersCall {}, block).await.context("aggregator.getTransmitters()")?;
    let senders = futures::future::join_all(transmitters.iter().map(|t| call(p, *t, IAuthorizedForwarder::getAuthorizedSendersCall {}, block))).await;
    let mut auth = Authorized::default();
    for (t, s) in transmitters.into_iter().zip(senders) {
        match s {
            Ok(list) => {
                auth.forwarders.insert(t, list.into_iter().collect());
            }
            Err(_) => {
                auth.direct.insert(t);
            }
        }
    }
    Ok(auth)
}

#[derive(Debug, Clone)]
struct Feed {
    proxy: Address,
    aggregator: Address,
    answer: U256,
    /// Chainlink SVR (DualAggregator): what the proxy, and so Morpho, reads moves only with the
    /// auctioned `transmitSecondary` (in an Atlas metacall) or once the primary is older than the
    /// aggregator's cutoff, not with the public `transmit`.
    svr: bool,
}

#[derive(Debug, Clone)]
struct MarketFeeds {
    scale: U256,
    base: Vec<usize>,
    quote: Vec<usize>,
}

/// Outcome of `PriceModel::apply`.
#[derive(Debug, PartialEq)]
pub enum Applied {
    /// In effect now: (market, new price) for each market it moves.
    Moved(Vec<(usize, U256)>),
    /// SVR primary: (market, price once in effect); nothing changed yet.
    Deferred(Vec<(usize, U256)>),
    Rejected(&'static str),
}

#[derive(Debug, Default)]
pub struct PriceModel {
    feeds: Vec<Feed>,
    /// Per market (same index as Engine::metas): None = not on the fast path.
    markets: Vec<Option<MarketFeeds>>,
    watched: Watched,
    /// SVR aggregator -> (public answer seen but not yet in effect, timestamp of its block).
    pending: HashMap<Address, (U256, u64)>,
}

impl PriceModel {
    pub async fn load<P: Provider>(p: &P, metas: &[MarketMeta], block: BlockId) -> Self {
        let mut model = PriceModel::default();
        for meta in metas {
            let m = match model.load_market(p, meta.params.oracle, block).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(market = %meta.name, error = %e, "oracle not modelled: block path only for this market");
                    None
                }
            };
            model.markets.push(m);
        }
        let fast: Vec<&str> = metas.iter().zip(&model.markets).filter(|(_, m)| m.is_some()).map(|(meta, _)| meta.name.as_str()).collect();
        let transmitters: usize = model.watched.read().unwrap().values().map(|a| a.forwarders.len() + a.direct.len()).sum();
        info!(markets = ?fast, feeds = model.feeds.len(), transmitters, "oracle price model loaded");
        model
    }

    async fn load_market<P: Provider>(&mut self, p: &P, oracle: Address, block: BlockId) -> Result<Option<MarketFeeds>> {
        use IMorphoChainlinkOracleV2 as O;
        let vaults = (call(p, oracle, O::BASE_VAULTCall {}, block).await, call(p, oracle, O::QUOTE_VAULTCall {}, block).await);
        let (Ok(bv), Ok(qv)) = vaults else { return Ok(None) }; // not a ChainlinkOracleV2
        if !bv.is_zero() || !qv.is_zero() {
            return Ok(None); // vault conversion factors: not modelled
        }
        let scale = call(p, oracle, O::SCALE_FACTORCall {}, block).await?;
        let slots = [
            call(p, oracle, O::BASE_FEED_1Call {}, block).await?,
            call(p, oracle, O::BASE_FEED_2Call {}, block).await?,
            call(p, oracle, O::QUOTE_FEED_1Call {}, block).await?,
            call(p, oracle, O::QUOTE_FEED_2Call {}, block).await?,
        ];
        let mut idx = [None; 4];
        for (i, proxy) in slots.iter().enumerate() {
            if !proxy.is_zero() {
                idx[i] = Some(self.feed_index(p, *proxy, block).await?);
            }
        }
        let m = MarketFeeds { scale, base: idx[..2].iter().flatten().copied().collect(), quote: idx[2..].iter().flatten().copied().collect() };
        let onchain = call(p, oracle, O::priceCall {}, block).await?;
        let model = self.compute(&m);
        if model != Some(onchain) {
            warn!(%oracle, ?model, %onchain, "oracle model mismatch: block path only for this market");
            return Ok(None);
        }
        Ok(Some(m))
    }

    async fn read_feed<P: Provider>(p: &P, proxy: Address, block: BlockId) -> Result<Feed> {
        let aggregator = call(p, proxy, IChainlinkProxy::aggregatorCall {}, block).await.context("feed.aggregator()")?;
        let r = call(p, proxy, IChainlinkProxy::latestRoundDataCall {}, block).await.context("feed.latestRoundData()")?;
        anyhow::ensure!(r.answer.is_positive(), "non-positive feed answer");
        let svr = call(p, aggregator, ITypeAndVersion::typeAndVersionCall {}, block).await.is_ok_and(|t| t.starts_with("DualAggregator"));
        Ok(Feed { proxy, aggregator, answer: r.answer.into_raw(), svr })
    }

    async fn feed_index<P: Provider>(&mut self, p: &P, proxy: Address, block: BlockId) -> Result<usize> {
        if let Some(i) = self.feeds.iter().position(|f| f.proxy == proxy) {
            return Ok(i);
        }
        let feed = Self::read_feed(p, proxy, block).await?;
        self.watch(p, feed.aggregator, block).await?;
        self.feeds.push(feed);
        Ok(self.feeds.len() - 1)
    }

    /// Loads (or refreshes) the authorized transmitters of `aggregator` into the shared table.
    async fn watch<P: Provider>(&self, p: &P, aggregator: Address, block: BlockId) -> Result<()> {
        let auth = load_authorized(p, aggregator, block).await?;
        anyhow::ensure!(!auth.forwarders.is_empty() || !auth.direct.is_empty(), "aggregator {aggregator} has no transmitters");
        self.watched.write().unwrap().insert(aggregator, auth);
        Ok(())
    }

    /// Checked: None on a zero quote product or a result above U256 (so a bad answer can never
    /// panic the bot; it just leaves the market to the block path for that update).
    fn compute(&self, m: &MarketFeeds) -> Option<U256> {
        self.compute_with(m, None)
    }

    /// Price with one aggregator's answer replaced (a preview that changes nothing).
    fn compute_with(&self, m: &MarketFeeds, replace: Option<(Address, U256)>) -> Option<U256> {
        let answer = |i: usize| match replace {
            Some((agg, a)) if self.feeds[i].aggregator == agg => a,
            _ => self.feeds[i].answer,
        };
        let prod = |ix: &[usize]| ix.iter().try_fold(U512::from(1u8), |acc, &i| acc.checked_mul(U512::from(answer(i))));
        let (num, den) = (prod(&m.base)?, prod(&m.quote)?);
        if den.is_zero() {
            return None;
        }
        let q = U512::from(m.scale).checked_mul(num)? / den;
        (q <= U512::from(U256::MAX)).then(|| U256::from_limbs_slice(&q.as_limbs()[..4]))
    }

    /// The aggregator -> authorized transmitters table the flashblock stream filters with.
    pub fn watched(&self) -> Watched {
        self.watched.clone()
    }

    /// Whether `market` is on the fast path (its price may still be momentarily unknown).
    pub fn is_modelled(&self, market: usize) -> bool {
        self.markets.get(market).is_some_and(|m| m.is_some())
    }

    pub fn price(&self, market: usize) -> Option<U256> {
        self.markets.get(market)?.as_ref().and_then(|m| self.compute(m))
    }

    /// A new answer from `aggregator` (seen in a flashblock), by the path it came:
    /// - plain feed, primary: in effect now -> `Moved`;
    /// - SVR feed, primary: Morpho does not read it yet -> kept pending with the timestamp of its
    ///   block, `Deferred` with the prices it would give. It takes effect either in the Atlas
    ///   metacall carrying the same report (`Secondary`, typically the next block) or, if no
    ///   auction carries it, in the first block more than the cutoff later (`Expired`; see `due`);
    /// - SVR feed, secondary or expired equal to the pending primary: in effect now -> `Moved`. A
    ///   secondary that does not match is `Rejected`: its authorization is only structural (see
    ///   flashblocks::oracle_update), so it may only time a price we already trust, not set one.
    pub fn apply(&mut self, aggregator: Address, answer: U256, path: Path, block_ts: u64) -> Applied {
        let touched: Vec<usize> = (0..self.feeds.len()).filter(|&i| self.feeds[i].aggregator == aggregator).collect();
        if answer.is_zero() || touched.is_empty() {
            return Applied::Moved(vec![]);
        }
        let svr = touched.iter().any(|&i| self.feeds[i].svr);
        match (svr, path) {
            (false, Path::Secondary | Path::Expired) => return Applied::Rejected("secondary/expiry for a non-SVR feed"),
            (true, Path::Primary) => {
                self.pending.insert(aggregator, (answer, block_ts));
                return Applied::Deferred(self.affected(&touched, Some((aggregator, answer))));
            }
            (true, Path::Secondary | Path::Expired) => match self.pending.get(&aggregator) {
                Some((p, _)) if *p == answer => {
                    self.pending.remove(&aggregator);
                }
                _ if touched.iter().all(|&i| self.feeds[i].answer == answer) => return Applied::Moved(vec![]),
                _ => return Applied::Rejected("secondary answer does not match the pending primary"),
            },
            (false, Path::Primary) => {}
        }
        for &i in &touched {
            self.feeds[i].answer = answer;
        }
        Applied::Moved(self.affected(&touched, None))
    }

    /// Pending SVR primaries no auction carried, now old enough to be what the proxy serves in a
    /// block with timestamp `ts`: the proxy switches once `ts > primary_ts + cutoff` (strictly;
    /// on Base an unauctioned update took effect ~12 s = 6 blocks later).
    pub fn due(&self, ts: u64, cutoff_secs: u64) -> Vec<(Address, U256)> {
        self.pending.iter().filter(|(_, (_, pts))| *pts > 0 && ts > pts + cutoff_secs).map(|(a, (ans, _))| (*a, *ans)).collect()
    }

    /// (market, price) if `aggregator` answered `answer`, without changing anything (to value an
    /// SVR auction before its update lands).
    pub fn preview(&self, aggregator: Address, answer: U256) -> Vec<(usize, U256)> {
        let touched: Vec<usize> = (0..self.feeds.len()).filter(|&i| self.feeds[i].aggregator == aggregator).collect();
        if answer.is_zero() || touched.is_empty() {
            return vec![];
        }
        self.affected(&touched, Some((aggregator, answer)))
    }

    /// (market, price) for every market using one of `feeds`.
    fn affected(&self, feeds: &[usize], replace: Option<(Address, U256)>) -> Vec<(usize, U256)> {
        self.markets
            .iter()
            .enumerate()
            .filter_map(|(mi, m)| m.as_ref().filter(|m| m.base.iter().chain(&m.quote).any(|f| feeds.contains(f))).and_then(|m| Some((mi, self.compute_with(m, replace)?))))
            .collect()
    }

    /// Block path cross-check: the sealed block's oracle price disagrees with the model (a missed
    /// update, a reverted transmit, an aggregator swap): re-read that market's feeds, and the
    /// authorized transmitters of any aggregator that changed.
    pub async fn resync<P: Provider>(&mut self, p: &P, market: usize, onchain: U256, block: BlockId) {
        let Some(Some(m)) = self.markets.get(market).cloned() else { return };
        if self.compute(&m) == Some(onchain) {
            return;
        }
        for &i in m.base.iter().chain(&m.quote) {
            let Ok(feed) = Self::read_feed(p, self.feeds[i].proxy, block).await else { continue };
            if feed.aggregator != self.feeds[i].aggregator {
                info!(proxy = %feed.proxy, old = %self.feeds[i].aggregator, new = %feed.aggregator, "chainlink aggregator swapped");
                if let Err(e) = self.watch(p, feed.aggregator, block).await {
                    warn!(error = %e, "could not load the new aggregator's transmitters: same-block mode blind for it");
                }
            }
            // The chain caught up with a pending SVR answer (its Atlas metacall, or the cutoff).
            if self.pending.get(&feed.aggregator).is_some_and(|(a, _)| *a == feed.answer) {
                self.pending.remove(&feed.aggregator);
            }
            self.feeds[i] = feed;
        }
        if self.compute(&m) != Some(onchain) {
            warn!(market, "oracle model still differs from the chain after resync");
        }
    }

    #[cfg(test)]
    pub fn for_test(feeds: Vec<(Address, Address, U256, bool)>, markets: Vec<Option<(U256, Vec<usize>, Vec<usize>)>>) -> Self {
        PriceModel {
            feeds: feeds.into_iter().map(|(proxy, aggregator, answer, svr)| Feed { proxy, aggregator, answer, svr }).collect(),
            markets: markets.into_iter().map(|m| m.map(|(scale, base, quote)| MarketFeeds { scale, base, quote })).collect(),
            watched: Watched::default(),
            pending: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn e(n: u64) -> U256 {
        U256::from(10u64).pow(U256::from(n))
    }
    const ETH: Address = address!("05c84a58FE042275b37db038bAAcD15F410c7bB0");
    const USDC: Address = address!("68bE4C50235205Ede361ac8244B1ee221CDDA5E2");
    const XRP: Address = address!("fa9ebD1FC381C5Babb495BB53664ae9B6665E4E4");

    /// WETH/USDC = ETH/USD / USDC/USD (scale 1e24), cbXRP/USDC = XRP/USD (scale 1e28), one unmodelled.
    fn model(svr: bool) -> PriceModel {
        PriceModel::for_test(
            vec![(Address::repeat_byte(1), ETH, U256::from(241_536_000_000u64), svr), (Address::repeat_byte(2), USDC, U256::from(100_000_000u64), false), (Address::repeat_byte(3), XRP, U256::from(129_000_000u64), svr)],
            vec![Some((e(24), vec![0], vec![1])), Some((e(28), vec![2], vec![])), None],
        )
    }

    #[test]
    fn plain_feed_moves_only_markets_using_it() {
        let mut m = model(false);
        assert_eq!(m.price(0), Some(U256::from(241_536_000_000u64) * e(16)));
        assert_eq!(m.apply(ETH, U256::from(241_544_504_811u64), Path::Primary, 1000), Applied::Moved(vec![(0, U256::from(241_544_504_811u64) * e(16))]));
        assert_eq!(m.apply(Address::repeat_byte(9), U256::from(1u64), Path::Primary, 1000), Applied::Moved(vec![]));
        assert_eq!(m.price(2), None);
        // A "secondary" for a plain feed is not a thing.
        assert!(matches!(m.apply(ETH, U256::from(1u64), Path::Secondary, 1002), Applied::Rejected(_)));
    }

    #[test]
    fn svr_primary_waits_for_its_secondary() {
        let mut m = model(true);
        let old = m.price(1).unwrap();
        let new = U256::from(127_840_411u64);
        // Public transmit: Morpho still reads the old price; we get the future one as a preview.
        assert_eq!(m.apply(XRP, new, Path::Primary, 1000), Applied::Deferred(vec![(1, new * e(28))]));
        assert_eq!(m.price(1), Some(old));
        // A secondary with another answer cannot set a price (it is only structurally authorized).
        assert!(matches!(m.apply(XRP, U256::from(1u64), Path::Secondary, 1002), Applied::Rejected(_)));
        assert_eq!(m.price(1), Some(old));
        // The Atlas metacall carrying the same report: now in effect.
        assert_eq!(m.apply(XRP, new, Path::Secondary, 1002), Applied::Moved(vec![(1, new * e(28))]));
        assert_eq!(m.price(1), Some(new * e(28)));
        // A second copy of that secondary changes nothing.
        assert_eq!(m.apply(XRP, new, Path::Secondary, 1002), Applied::Moved(vec![]));
    }

    #[test]
    fn bad_answers_never_panic() {
        let mut m = model(false);
        // Zero answer: ignored, price unchanged.
        assert_eq!(m.apply(USDC, U256::ZERO, Path::Primary, 1000), Applied::Moved(vec![]));
        assert_eq!(m.price(0), Some(U256::from(241_536_000_000u64) * e(16)));
        // Absurd base answer: the product overflows U256 -> no price instead of a panic.
        assert_eq!(m.apply(ETH, U256::MAX, Path::Primary, 1000), Applied::Moved(vec![]));
        assert_eq!(m.price(0), None);
    }

    #[test]
    fn svr_primary_no_auction_takes_effect_after_cutoff() {
        let mut m = model(true);
        let old = m.price(1).unwrap();
        let new = U256::from(127_840_411u64);
        // Public transmit in a block with timestamp 1000; no Atlas metacall follows.
        assert!(matches!(m.apply(XRP, new, Path::Primary, 1000), Applied::Deferred(_)));
        // Cutoff 10 s, strict: a block at 1010 still reads the old price (measured on-chain).
        assert!(m.due(1010, 10).is_empty());
        assert_eq!(m.due(1012, 10), vec![(XRP, new)]);
        assert_eq!(m.price(1), Some(old));
        assert_eq!(m.apply(XRP, new, Path::Expired, 1012), Applied::Moved(vec![(1, new * e(28))]));
        assert!(m.due(1100, 10).is_empty(), "applied once");
        // Expiry of an answer that is not the pending one changes nothing.
        assert!(matches!(m.apply(XRP, U256::from(5u64), Path::Expired, 1200), Applied::Rejected(_)));
        assert_eq!(m.price(1), Some(new * e(28)));
    }
}
