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


use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};

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

use crate::morpho::onchain::MarketMeta;

/// Which copy of a Chainlink report a transaction carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Path {
    /// The public `transmit`. On an SVR feed it does not move what Morpho reads yet.
    Primary,
    /// `transmitSecondary` inside an Atlas metacall: what an SVR proxy (and so Morpho) reads.
    Secondary,
    /// An SVR primary that no auction carried: the proxy serves it once it is older than the
    /// aggregator's cutoff (10 s on the Base SVR feeds checked).
    Expired,
}

/// Who may move an aggregator's answer: `aggregator.getTransmitters()` are forwarder contracts
/// (each with `getAuthorizedSenders()` EOAs) or, for direct transmits, the EOAs themselves.
#[derive(Debug, Clone, Default)]
pub struct Authorized {
    /// forwarder -> EOAs allowed to call it
    pub forwarders: HashMap<Address, HashSet<Address>>,
    /// EOAs allowed to call `transmit` on the aggregator directly
    pub direct: HashSet<Address>,
}

/// aggregator -> its authorized transmitters. Shared with the price model, which refreshes it
/// when a feed's aggregator or transmitter set changes.
pub type Watched = Arc<RwLock<HashMap<Address, Authorized>>>;

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

/// `PriceModel::for_test`'s market: (scale, base feed indexes, quote feed indexes).
#[cfg(test)]
type TestMarket = (U256, Vec<usize>, Vec<usize>);

#[derive(Debug, Clone)]
struct MarketFeeds {
    scale: U256,
    base: Vec<usize>,
    quote: Vec<usize>,
}

/// A public SVR primary seen but not yet in effect. Morpho reads it only once an Atlas metacall
/// carries it (`Secondary`) or it is older than the aggregator's cutoff (`Expired`). More than one
/// can be in flight at once for the same aggregator when primaries arrive faster than the cutoff
/// (ETH/USD in a fast market updates roughly every cutoff length), so they are kept per aggregator
/// in a queue, in arrival (round) order.
#[derive(Debug, Clone)]
struct Pending {
    answer: U256,
    /// Timestamp of the block the primary was seen in (0 before any block is known).
    ts: u64,
    /// Round from the oracle log; `None` from the flashblock decoder, which carries no round.
    round: Option<u32>,
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
    /// SVR aggregator -> its in-flight public primaries, oldest first (see `Pending`). The chain
    /// serves the newest one that has become effective, so an earlier primary is kept until it
    /// takes effect or a later round supersedes it; it is never overwritten by a newer primary.
    pending: HashMap<Address, Vec<Pending>>,
    /// SVR aggregator -> its cutoff (seconds), read from the DualAggregator's `CutoffTimeSet` log.
    /// Absent until a log arrives: `due` then falls back to the config `svr_cutoff_secs`.
    cutoffs: HashMap<Address, u64>,
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
                // Queued, not overwritten: an earlier primary still waiting for its cutoff must
                // survive so it takes effect when the chain serves it (see `Pending`, `due`).
                self.pending.entry(aggregator).or_default().push(Pending { answer, ts: block_ts, round: None });
                return Applied::Deferred(self.affected(&touched, Some((aggregator, answer))));
            }
            (true, Path::Secondary | Path::Expired) => {
                // The chain now serves `answer` (an auctioned secondary, or a primary past its
                // cutoff): that pending primary and every older one are consumed, since
                // latestRoundData only moves forward and never serves a superseded round.
                let hit = if let Some(q) = self.pending.get_mut(&aggregator) {
                    match q.iter().position(|p| p.answer == answer) {
                        Some(pos) => {
                            q.drain(..=pos);
                            Some(q.is_empty())
                        }
                        None => None,
                    }
                } else {
                    None
                };
                match hit {
                    Some(empty) => {
                        if empty {
                            self.pending.remove(&aggregator);
                        }
                    }
                    None if touched.iter().all(|&i| self.feeds[i].answer == answer) => return Applied::Moved(vec![]),
                    None => return Applied::Rejected("secondary/expiry answer does not match a pending primary"),
                }
            }
            (false, Path::Primary) => {}
        }
        for &i in &touched {
            self.feeds[i].answer = answer;
        }
        Applied::Moved(self.affected(&touched, None))
    }

    /// A public transmit of `answer` for `round` (oracle logs): `apply` as Primary, remembering the
    /// round of a deferred SVR answer for its secondary.
    pub fn apply_transmission(&mut self, aggregator: Address, round: u32, answer: U256, block_ts: u64) -> Applied {
        // A redelivered log for a round already queued (e.g. after a WS reconnect) adds nothing.
        if self.pending.get(&aggregator).is_some_and(|q| q.iter().any(|p| p.round == Some(round))) {
            return self.deferred_preview(aggregator, answer);
        }
        let r = self.apply(aggregator, answer, Path::Primary, block_ts);
        if matches!(r, Applied::Deferred(_))
            && let Some(p) = self.pending.get_mut(&aggregator).and_then(|q| q.last_mut())
        {
            p.round = Some(round);
        }
        r
    }

    /// The SVR secondary of `round` (oracle logs: the round only): the queued primary of that round
    /// takes effect (and supersedes any older still-pending primary). None when no queued primary
    /// has that round (a missed transmit; the block path's resync catches up).
    pub fn apply_secondary_round(&mut self, aggregator: Address, round: u32, block_ts: u64) -> Option<Applied> {
        let answer = self.pending.get(&aggregator)?.iter().find(|p| p.round == Some(round))?.answer;
        Some(self.apply(aggregator, answer, Path::Secondary, block_ts))
    }

    /// The `Deferred` preview an SVR primary of `answer` would give, changing nothing.
    fn deferred_preview(&self, aggregator: Address, answer: U256) -> Applied {
        let touched: Vec<usize> = (0..self.feeds.len()).filter(|&i| self.feeds[i].aggregator == aggregator).collect();
        Applied::Deferred(self.affected(&touched, Some((aggregator, answer))))
    }

    /// Forgets the pending primaries of `aggregator` (a log one came from was removed by a reorg).
    /// The whole queue is dropped; the block path's resync rebuilds from the sealed state.
    pub fn invalidate(&mut self, aggregator: Address) {
        self.pending.remove(&aggregator);
    }

    /// The aggregators behind the modelled feeds, sorted (the oracle logs filter).
    pub fn aggregators(&self) -> Vec<Address> {
        let mut a: Vec<Address> = self.feeds.iter().map(|f| f.aggregator).collect();
        a.sort();
        a.dedup();
        a
    }

    /// Records an aggregator's SVR cutoff from a `CutoffTimeSet` log (seconds). Returns whether it
    /// changed, for logging. The value takes effect on the next `due` evaluation.
    pub fn set_cutoff(&mut self, aggregator: Address, cutoff_secs: u64) -> bool {
        self.cutoffs.insert(aggregator, cutoff_secs) != Some(cutoff_secs)
    }

    /// The cutoff (seconds) in effect for `aggregator`: its logged value, else the config `default`.
    pub fn cutoff_for(&self, aggregator: Address, default: u64) -> u64 {
        self.cutoffs.get(&aggregator).copied().unwrap_or(default)
    }

    /// Pending SVR primaries no auction carried, now old enough to be what the proxy serves in a
    /// block with timestamp `ts`: the proxy switches once `ts > primary_ts + cutoff` (strictly; on
    /// Base an unauctioned update took effect ~12 s = 6 blocks later). The cutoff is each
    /// aggregator's logged `CutoffTimeSet` value, falling back to `default` (config
    /// `svr_cutoff_secs`) until its first log. `ts` is the block timestamp in whole seconds (Denim
    /// has no sub-second granularity), matching the on-chain `block.timestamp` comparison.
    pub fn due(&self, ts: u64, default: u64) -> Vec<(Address, U256)> {
        self.pending
            .iter()
            .filter_map(|(agg, q)| {
                let cutoff = self.cutoff_for(*agg, default);
                // The queue is in round order, so the newest primary past its cutoff is the last
                // expired entry; any older expired one it skipped is superseded (`apply` drains it).
                q.iter().rev().find(|p| p.ts > 0 && ts > p.ts + cutoff).map(|p| (*agg, p.answer))
            })
            .collect()
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
            // The chain caught up with a queued SVR answer (its Atlas metacall, or the cutoff):
            // that primary and every older one it superseded are consumed.
            if let Some(q) = self.pending.get_mut(&feed.aggregator)
                && let Some(pos) = q.iter().position(|p| p.answer == feed.answer)
            {
                q.drain(..=pos);
                if q.is_empty() {
                    self.pending.remove(&feed.aggregator);
                }
            }
            self.feeds[i] = feed;
        }
        if self.compute(&m) != Some(onchain) {
            warn!(market, "oracle model still differs from the chain after resync");
        }
    }

    /// A model for unit tests: feeds as (proxy, aggregator, answer, svr), markets as (scale, base
    /// feed indexes, quote feed indexes).
    #[cfg(test)]
    pub fn for_test(feeds: Vec<(Address, Address, U256, bool)>, markets: Vec<Option<TestMarket>>) -> Self {
        PriceModel {
            feeds: feeds.into_iter().map(|(proxy, aggregator, answer, svr)| Feed { proxy, aggregator, answer, svr }).collect(),
            markets: markets.into_iter().map(|m| m.map(|(scale, base, quote)| MarketFeeds { scale, base, quote })).collect(),
            watched: Watched::default(),
            pending: HashMap::new(),
            cutoffs: HashMap::new(),
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
    fn svr_secondary_round_takes_the_pending_answer_of_that_round() {
        // Oracle logs: NewTransmission(round, answer), then SecondaryRoundIdUpdated(round) with no answer.
        let mut m = model(true);
        let old = m.price(1).unwrap();
        let new = U256::from(127_840_411u64);
        assert_eq!(m.apply_transmission(XRP, 0x735, new, 1000), Applied::Deferred(vec![(1, new * e(28))]));
        assert_eq!(m.price(1), Some(old));
        // Another round's secondary does not put this answer in effect.
        assert_eq!(m.apply_secondary_round(XRP, 0x734, 1002), None);
        assert_eq!(m.price(1), Some(old));
        assert_eq!(m.apply_secondary_round(XRP, 0x735, 1002), Some(Applied::Moved(vec![(1, new * e(28))])));
        assert_eq!(m.price(1), Some(new * e(28)));
        // Nothing pending any more.
        assert_eq!(m.apply_secondary_round(XRP, 0x735, 1004), None);
        assert!(m.due(10_000, 10).is_empty());
    }

    #[test]
    fn invalidate_forgets_the_pending_answer() {
        let mut m = model(true);
        m.apply_transmission(XRP, 7, U256::from(127_840_411u64), 1000);
        m.invalidate(XRP);
        assert_eq!(m.apply_secondary_round(XRP, 7, 1002), None);
        assert!(m.due(10_000, 10).is_empty());
        assert_eq!(m.aggregators(), { let mut a = vec![ETH, USDC, XRP]; a.sort(); a });
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
    fn a_logged_cutoff_overrides_the_config_default_per_aggregator() {
        let mut m = model(true);
        let new = U256::from(127_840_411u64);
        assert!(matches!(m.apply(XRP, new, Path::Primary, 1000), Applied::Deferred(_)));
        // Default cutoff 10 s (strict >): a block at 1010 is not yet due, 1011 is.
        assert!(m.due(1010, 10).is_empty());
        assert_eq!(m.due(1011, 10), vec![(XRP, new)]);
        // CutoffTimeSet moved XRP's cutoff to 30 s: now due only past 1030, config default ignored.
        assert!(m.set_cutoff(XRP, 30));
        assert!(!m.set_cutoff(XRP, 30), "no change reported when the value is the same");
        assert_eq!(m.cutoff_for(XRP, 10), 30);
        assert!(m.due(1030, 10).is_empty());
        assert_eq!(m.due(1031, 10), vec![(XRP, new)]);
        // An aggregator with no logged cutoff still uses the default.
        assert_eq!(m.cutoff_for(ETH, 10), 10);
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

    #[test]
    fn two_primaries_in_flight_each_take_effect_at_their_own_cutoff() {
        // D1: a second primary for the same aggregator arrives before the first is past its cutoff.
        // The model must keep the first (a newer primary used to overwrite it), so the chain's
        // switch at first_ts + cutoff is not read as a missed update.
        let mut m = model(true);
        let old = m.price(1).unwrap();
        let a1 = U256::from(127_840_411u64);
        let a2 = U256::from(128_500_000u64);
        // Primary #1 at ts 1000 (round 0x10), primary #2 at ts 1030 (round 0x11), both deferred.
        assert_eq!(m.apply_transmission(XRP, 0x10, a1, 1000), Applied::Deferred(vec![(1, a1 * e(28))]));
        assert_eq!(m.apply_transmission(XRP, 0x11, a2, 1030), Applied::Deferred(vec![(1, a2 * e(28))]));
        assert_eq!(m.price(1), Some(old), "nothing in effect yet");
        // Cutoff 30 s. At 1031 only #1 is due; the chain serves a1, not the newer a2.
        assert!(m.due(1030, 30).is_empty());
        assert_eq!(m.due(1031, 30), vec![(XRP, a1)]);
        assert_eq!(m.apply(XRP, a1, Path::Expired, 1031), Applied::Moved(vec![(1, a1 * e(28))]));
        assert_eq!(m.price(1), Some(a1 * e(28)));
        // #2 is still pending and becomes due at its own cutoff, 1060.
        assert!(m.due(1060, 30).is_empty());
        assert_eq!(m.due(1061, 30), vec![(XRP, a2)]);
        assert_eq!(m.apply(XRP, a2, Path::Expired, 1061), Applied::Moved(vec![(1, a2 * e(28))]));
        assert_eq!(m.price(1), Some(a2 * e(28)));
        assert!(m.due(9000, 30).is_empty(), "both consumed");
    }

    #[test]
    fn when_a_head_gap_passes_both_cutoffs_the_chain_serves_the_newest() {
        // If the loop skips the block where #1 would have taken effect, the next head sees both
        // past their cutoff: latestRoundData shows the newest (a2), and the older is superseded.
        let mut m = model(true);
        let a1 = U256::from(127_840_411u64);
        let a2 = U256::from(128_500_000u64);
        m.apply_transmission(XRP, 0x10, a1, 1000);
        m.apply_transmission(XRP, 0x11, a2, 1005);
        assert_eq!(m.due(1016, 10), vec![(XRP, a2)]);
        assert_eq!(m.apply(XRP, a2, Path::Expired, 1016), Applied::Moved(vec![(1, a2 * e(28))]));
        assert!(m.due(9000, 10).is_empty(), "the older primary was superseded, not applied after");
        assert_eq!(m.price(1), Some(a2 * e(28)));
    }

    #[test]
    fn an_auction_for_the_newer_round_supersedes_an_earlier_pending_primary() {
        // An Atlas metacall carrying the newer round makes the chain jump to it; the earlier
        // un-auctioned primary is never served and must be dropped, not left to expire later.
        let mut m = model(true);
        let a1 = U256::from(127_840_411u64);
        let a2 = U256::from(128_500_000u64);
        m.apply_transmission(XRP, 0x10, a1, 1000);
        m.apply_transmission(XRP, 0x11, a2, 1005);
        assert_eq!(m.apply_secondary_round(XRP, 0x11, 1006), Some(Applied::Moved(vec![(1, a2 * e(28))])));
        assert_eq!(m.price(1), Some(a2 * e(28)));
        assert!(m.due(9000, 10).is_empty(), "the earlier primary was superseded");
        // A redelivered log for round 0x10 after it was superseded cannot resurrect it.
        assert_eq!(m.apply_secondary_round(XRP, 0x10, 1008), None);
    }

    #[test]
    fn a_redelivered_transmission_log_does_not_queue_a_duplicate() {
        // After a WS reconnect the same NewTransmission can arrive twice; the second is a no-op.
        let mut m = model(true);
        let a1 = U256::from(127_840_411u64);
        assert_eq!(m.apply_transmission(XRP, 0x10, a1, 1000), Applied::Deferred(vec![(1, a1 * e(28))]));
        assert_eq!(m.apply_transmission(XRP, 0x10, a1, 1000), Applied::Deferred(vec![(1, a1 * e(28))]));
        // One queued entry: it takes effect once, and nothing is left afterwards.
        assert_eq!(m.due(1011, 10), vec![(XRP, a1)]);
        assert_eq!(m.apply(XRP, a1, Path::Expired, 1011), Applied::Moved(vec![(1, a1 * e(28))]));
        assert!(m.due(9000, 10).is_empty());
    }
}
