//! Health-factor engine, generic over the lending protocol (`protocol::LendingProtocol`).
//! Positions live in memory: loaded once in full, then refreshed only when a protocol event
//! touches them. Each block costs one snapshot batch + one event query, whatever the protocol.

use std::collections::{HashMap, HashSet};

use alloy::{
    eips::BlockId,
    primitives::{Address, B256, U256},
    providers::Provider,
};
use anyhow::Result;
use serde::Serialize;
use tracing::{info, warn};

use crate::{
    config::Config,
    protocol::{BorrowerInfo, LendingProtocol, MarketInfo, Touched},
};

pub struct Engine<P, L: LendingProtocol> {
    pub provider: P,
    pub cfg: Config,
    pub http: reqwest::Client,
    pub protocol: L,
    /// Markets in config order; the index is the market index used everywhere else.
    pub metas: Vec<L::Market>,
    market_index: HashMap<B256, usize>,
    /// (market index, user) -> raw on-chain position
    pub positions: HashMap<(usize, Address), L::Position>,
    /// API health factors from the last borrower refresh (validation only).
    pub api_hf: HashMap<(usize, Address), f64>,
    /// Every position we have ever tracked with debt (loss analysis: "untracked" vs "same block").
    pub ever_tracked: HashSet<(usize, Address)>,
}

/// One tracked position evaluated against its market's snapshot.
pub struct PositionView<L: LendingProtocol> {
    pub market: usize,
    pub user: Address,
    pub health: L::Health,
    pub hf: f64,
    /// Debt in loan-token units.
    pub debt: f64,
    pub max_seize: U256,
    pub quote: Option<L::Quote>,
    /// Seized value at the oracle price minus repaid debt, in loan-token units (before swap costs and gas).
    pub bonus: f64,
}

// Manual impls: a derive would also require the protocol type itself to be Clone/Debug.
impl<L: LendingProtocol> Clone for PositionView<L> {
    fn clone(&self) -> Self {
        Self { market: self.market, user: self.user, health: self.health.clone(), hf: self.hf, debt: self.debt, max_seize: self.max_seize, quote: self.quote.clone(), bonus: self.bonus }
    }
}
impl<L: LendingProtocol> std::fmt::Debug for PositionView<L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PositionView")
            .field("market", &self.market)
            .field("user", &self.user)
            .field("health", &self.health)
            .field("hf", &self.hf)
            .field("debt", &self.debt)
            .field("max_seize", &self.max_seize)
            .field("quote", &self.quote)
            .field("bonus", &self.bonus)
            .finish()
    }
}

#[derive(Serialize)]
pub struct JournalRow<'a> {
    pub kind: &'a str,
    pub block: u64,
    pub timestamp: u64,
    pub market: &'a str,
    pub user: Address,
    pub hf: f64,
    pub debt: f64,
    pub seize: String,
    pub repay: String,
    pub bonus: f64,
}

impl<P: Provider, L: LendingProtocol> Engine<P, L> {
    pub async fn new(provider: P, cfg: Config, protocol: L) -> Result<Self> {
        let metas = protocol.load_markets(&provider).await?;
        let market_index = metas.iter().enumerate().map(|(i, m)| (m.key(), i)).collect();
        // Public APIs (Kyber, protocol indexers) sit behind Cloudflare, which rejects clients without a User-Agent.
        let http = reqwest::Client::builder().user_agent(concat!("liquidation-framework/", env!("CARGO_PKG_VERSION"))).build()?;
        Ok(Self { provider, cfg, http, protocol, metas, market_index, positions: HashMap::new(), api_hf: HashMap::new(), ever_tracked: HashSet::new() })
    }

    /// Pulls borrower lists from the protocol's indexer and loads any position we are not tracking yet.
    pub async fn refresh_borrowers(&mut self, block: BlockId) -> Result<()> {
        let lists = self.borrower_fetch_job().await;
        self.merge_borrowers(lists, block).await.map(|_| ())
    }

    /// The slow part of a refresh (full indexer walk), as a self-contained future that owns
    /// everything it needs: it can be `tokio::spawn`ed so the block loop keeps running.
    pub fn borrower_fetch_job(&self) -> impl std::future::Future<Output = Vec<(usize, Result<Vec<BorrowerInfo>>)>> + Send + 'static {
        self.protocol.borrower_job(self.http.clone(), &self.metas)
    }

    /// Applies fetched borrower lists: records API HFs and loads positions we do not track yet
    /// (usually few, since events already discover new borrowers). Returns how many were new.
    pub async fn merge_borrowers(&mut self, lists: Vec<(usize, Result<Vec<BorrowerInfo>>)>, block: BlockId) -> Result<usize> {
        let mut new_keys = Vec::new();
        for (i, list) in lists {
            let name = self.metas[i].name();
            match list {
                Ok(list) => {
                    info!(market = %name, borrowers = list.len(), "borrowers fetched");
                    for b in list {
                        if let Some(h) = b.api_health_factor {
                            self.api_hf.insert((i, b.user), h);
                        }
                        if !self.positions.contains_key(&(i, b.user)) {
                            new_keys.push((i, b.user));
                        }
                    }
                }
                // Keep the previous list: a flaky API must not blind the bot.
                Err(e) => warn!(market = %name, error = %e, "borrower refresh failed, keeping old list"),
            }
        }
        self.reload(&new_keys, block).await?;
        info!(new = new_keys.len(), tracked = self.positions.len(), "positions loaded");
        Ok(new_keys.len())
    }

    /// Re-reads the given positions from chain.
    pub async fn reload(&mut self, keys: &[(usize, Address)], block: BlockId) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let ids: Vec<(B256, Address)> = keys.iter().map(|(i, u)| (self.metas[*i].key(), *u)).collect();
        let res = self.protocol.load_positions(&self.provider, &ids, block).await?;
        for (key, pos) in keys.iter().zip(res) {
            match pos {
                Some(p) if self.protocol.has_debt(&p) => {
                    self.positions.insert(*key, p);
                    self.ever_tracked.insert(*key);
                }
                Some(_) => {
                    self.positions.remove(key);
                }
                None => warn!(user = %key.1, "position read failed"),
            }
        }
        Ok(())
    }

    /// Protocol events in `[from, to]` for `markets` (keys from `market_ids`).
    pub async fn touched(&self, markets: &[B256], from: u64, to: u64) -> Result<Touched<L::Liquidation>> {
        self.protocol.touched(&self.provider, markets, from, to).await
    }

    /// Applies fetched events by re-reading every touched position at `block`.
    /// New borrowers seen in events are picked up here too. Returns how many were re-read,
    /// plus the liquidations that happened in the range (market index attached).
    pub async fn apply_touched(&mut self, touched: Touched<L::Liquidation>, block: u64) -> Result<(usize, Vec<(usize, L::Liquidation)>)> {
        let mut keys: HashSet<(usize, Address)> = HashSet::new();
        for (id, users) in touched.positions {
            if let Some(&i) = self.market_index.get(&id) {
                keys.extend(users.into_iter().map(|u| (i, u)));
            }
        }
        let keys: Vec<_> = keys.into_iter().collect();
        self.reload(&keys, block.into()).await?;
        let liqs = touched.liquidations.into_iter().filter_map(|(m, l)| self.market_index.get(&m).map(|&i| (i, l))).collect();
        Ok((keys.len(), liqs))
    }

    pub fn market_by_name(&self, name: &str) -> Option<usize> {
        self.metas.iter().position(|m| m.name().eq_ignore_ascii_case(name))
    }

    /// ETH price in USD for gas accounting: from the protocol's markets if it can tell, else the
    /// configured fallback.
    pub fn eth_usd(&self, snaps: &[Option<L::Snapshot>]) -> f64 {
        self.protocol.eth_usd(&self.metas, snaps).unwrap_or(self.cfg.exec.eth_usd_fallback)
    }

    pub async fn snapshots(&self, block: BlockId, timestamp: u64) -> Result<Vec<Option<L::Snapshot>>> {
        self.protocol.snapshots(&self.provider, &self.metas, block, timestamp, None).await
    }

    /// One-round-trip snapshot reusing last block's snapshots where the protocol can.
    pub async fn snapshots_fast(&self, block: BlockId, timestamp: u64, prev: &[Option<L::Snapshot>]) -> Result<Vec<Option<L::Snapshot>>> {
        self.protocol.snapshots(&self.provider, &self.metas, block, timestamp, Some(prev)).await
    }

    pub fn market_ids(&self) -> Vec<B256> {
        self.metas.iter().map(|m| m.key()).collect()
    }

    /// Pure in-memory evaluation of all tracked positions against the snapshots. Dust is skipped.
    pub fn evaluate(&self, snaps: &[Option<L::Snapshot>]) -> Vec<PositionView<L>> {
        self.evaluate_with(|mi| snaps[mi].as_ref())
    }

    /// Same as `evaluate`, for one market only (e.g. right after its oracle moved mid-block).
    pub fn evaluate_market(&self, market: usize, snap: &L::Snapshot) -> Vec<PositionView<L>> {
        self.evaluate_with(|mi| (mi == market).then_some(snap))
    }

    /// Positions of every market `snap_of` returns a snapshot for, sorted by HF (lowest first).
    fn evaluate_with<'s>(&self, snap_of: impl Fn(usize) -> Option<&'s L::Snapshot>) -> Vec<PositionView<L>> {
        let mut views: Vec<PositionView<L>> = self
            .positions
            .iter()
            .filter_map(|(&(mi, user), pos)| {
                let snap = snap_of(mi)?;
                let v = view(&self.protocol, mi, user, &self.metas[mi], snap, pos);
                (v.debt >= self.cfg.dust_debt).then_some(v)
            })
            .collect();
        views.sort_by(|a, b| a.hf.total_cmp(&b.hf));
        views
    }
}

/// Evaluates one position and attaches its market index and account.
pub fn view<L: LendingProtocol>(protocol: &L, market: usize, user: Address, m: &L::Market, snap: &L::Snapshot, position: &L::Position) -> PositionView<L> {
    let e = protocol.evaluate(m, snap, position);
    PositionView { market, user, health: e.health, hf: e.hf, debt: e.debt, max_seize: e.max_seize, quote: e.quote, bonus: e.bonus }
}

#[cfg(test)]
mod tests {
    //! The engine driven by a toy protocol that has nothing to do with Morpho: proves the generic
    //! core (discovery, debt-free pruning, event -> market mapping, HF order, dust filter) does not
    //! depend on any one protocol.
    use alloy::providers::ProviderBuilder;

    use super::*;
    use crate::testkit::{Toy, cfg, key, user};

    #[tokio::test]
    async fn engine_runs_on_a_non_morpho_protocol() {
        // Never contacted: the toy protocol ignores the provider.
        let provider = ProviderBuilder::new().connect_http("http://127.0.0.1:1".parse().unwrap());
        let toy = Toy { borrowers: vec![(0, user(1)), (0, user(2)), (1, user(3))], ..Default::default() };
        {
            let mut s = toy.state.lock().unwrap();
            s.insert((key(1), user(1)), (100, 90)); // hf 0.9
            s.insert((key(1), user(2)), (100, 150)); // hf 1.5
            s.insert((key(2), user(3)), (2, 1)); // hf 0.5 but dust (debt 2 < 5)
        }
        let mut e = Engine::new(provider, cfg(), toy).await.unwrap();
        assert_eq!(e.market_ids(), vec![key(1), key(2)]);
        assert_eq!(e.market_by_name("BETA"), Some(1));

        // Discovery through the indexer job loads every reported borrower.
        e.refresh_borrowers(BlockId::latest()).await.unwrap();
        assert_eq!(e.positions.len(), 3);
        assert_eq!(e.api_hf.get(&(0, user(1))), Some(&0.5));

        // Evaluation: lowest HF first, dust dropped.
        let views = e.evaluate(&[Some(1.0), Some(1.0)]);
        assert_eq!(views.iter().map(|v| (v.market, v.user)).collect::<Vec<_>>(), vec![(0, user(1)), (0, user(2))]);
        assert!((views[0].hf - 0.9).abs() < 1e-9);
        assert_eq!(e.evaluate_market(1, &1.0).len(), 0, "market 1 only holds dust");

        // Events: user 2 repaid in full (pruned), user 4 borrowed on market 1 (picked up), a
        // liquidation on market 0 is mapped to its index, one on an unknown market is dropped.
        {
            let mut s = e.protocol.state.lock().unwrap();
            s.insert((key(1), user(2)), (0, 150));
            s.insert((key(2), user(4)), (50, 40));
        }
        let mut t = Touched::default();
        t.positions.entry(key(1)).or_default().insert(user(2));
        t.positions.entry(key(2)).or_default().insert(user(4));
        t.positions.entry(key(9)).or_default().insert(user(5));
        t.liquidations = vec![(key(1), "liq-a"), (key(9), "liq-unknown")];
        *e.protocol.events.lock().unwrap() = t;
        let touched = e.touched(&e.market_ids(), 1, 2).await.unwrap();
        let (reread, liqs) = e.apply_touched(touched, 2).await.unwrap();
        assert_eq!(reread, 2, "only keys on known markets are re-read");
        assert_eq!(liqs, vec![(0, "liq-a")]);
        assert!(!e.positions.contains_key(&(0, user(2))), "debt-free position is pruned");
        assert_eq!(e.positions.get(&(1, user(4))), Some(&(50, 40)));
        assert!(e.ever_tracked.contains(&(0, user(2))), "still remembered for loss analysis");

        // No market price available from the toy protocol -> configured fallback.
        assert_eq!(e.eth_usd(&[Some(1.0), Some(1.0)]), 1234.0);
    }
}
