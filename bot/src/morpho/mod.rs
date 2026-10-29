//! Morpho Blue — the first `LendingProtocol` implementation.
//!
//! The application code (main / planner / live / loss) is still Morpho-specific and uses the
//! aliases at the bottom (`MorphoEngine`, `PositionView`, `evaluate_position`); the generic engine
//! itself never names Morpho.

pub mod api;
pub mod events;
pub mod math;
pub mod onchain;

use alloy::{
    eips::BlockId,
    primitives::{Address, B256, U256, address},
    providers::Provider,
    sol_types::SolCall,
};
use anyhow::Result;
use futures::future::BoxFuture;
use tracing::info;

use self::{
    math::{Health, LiquidationQuote, ORACLE_PRICE_SCALE},
    onchain::{MarketMeta, MarketSnapshot},
};
use crate::{
    config::Config,
    protocol::{BorrowerInfo, DirectLiquidation, Evaluation, LendingProtocol, LiquidationOption, LiquidationTx, MarketInfo, Pair, Touched},
};

/// Morpho Blue on one chain: the singleton address, its indexer API and the configured markets.
#[derive(Debug, Clone)]
pub struct Morpho {
    pub address: Address,
    api_url: String,
    chain_id: u64,
    /// (name, market id) in config order.
    markets: Vec<(String, B256)>,
    /// Max blocks per eth_getLogs call.
    log_chunk: u64,
}

impl Morpho {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            address: cfg.morpho.address,
            api_url: cfg.morpho.api.clone(),
            chain_id: cfg.chain_id,
            markets: cfg.morpho.markets.iter().map(|m| (m.name.clone(), m.id)).collect(),
            log_chunk: cfg.log_chunk,
        }
    }
}

#[cfg(test)]
impl Morpho {
    /// An instance for unit tests (its address/API are never contacted by the pure methods).
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self::for_tests_at(Address::ZERO)
    }

    /// The same, at a real Morpho address (for venues that build calls to it).
    #[cfg(test)]
    pub fn for_tests_at(address: Address) -> Self {
        Self { address, api_url: String::new(), chain_id: 8453, markets: vec![], log_chunk: 100 }
    }
}

impl MarketInfo for MarketMeta {
    fn key(&self) -> B256 {
        self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
}

impl LendingProtocol for Morpho {
    type Market = MarketMeta;
    /// (borrowShares, collateral)
    type Position = (U256, U256);
    type Snapshot = MarketSnapshot;
    type Health = Health;
    type Quote = LiquidationQuote;
    type Liquidation = events::LiquidationEvent;

    async fn load_markets<P: Provider>(&self, p: &P) -> Result<Vec<MarketMeta>> {
        let mut metas = Vec::with_capacity(self.markets.len());
        for (name, id) in &self.markets {
            let meta = onchain::load_market_meta(p, self.address, *id, name).await?;
            // The id is keccak256(abi.encode(params)); a mismatch means a wrong id or a lying RPC.
            let computed = alloy::primitives::keccak256(alloy::sol_types::SolValue::abi_encode(&meta.params));
            anyhow::ensure!(computed == *id, "market {name}: params hash {computed} != configured id");
            info!(market = %meta.name, loan = %meta.loan_symbol, collateral = %meta.collateral_symbol, lltv = math::units(meta.params.lltv, 18), "market loaded");
            metas.push(meta);
        }
        Ok(metas)
    }

    fn borrower_job(&self, http: reqwest::Client, markets: &[MarketMeta]) -> BoxFuture<'static, Vec<(usize, Result<Vec<BorrowerInfo>>)>> {
        let url = self.api_url.clone();
        let chain = self.chain_id;
        let ids: Vec<(usize, B256)> = markets.iter().enumerate().map(|(i, m)| (i, m.id)).collect();
        Box::pin(async move {
            let mut out = Vec::with_capacity(ids.len());
            for (i, id) in ids {
                out.push((i, api::fetch_borrowers(&http, &url, chain, id).await));
            }
            out
        })
    }

    async fn load_positions<P: Provider>(&self, p: &P, keys: &[(B256, Address)], block: BlockId) -> Result<Vec<Option<(U256, U256)>>> {
        onchain::load_positions(p, self.address, keys, block).await
    }

    fn has_debt(&self, position: &(U256, U256)) -> bool {
        !position.0.is_zero()
    }

    async fn touched<P: Provider>(&self, p: &P, markets: &[B256], from: u64, to: u64) -> Result<Touched<events::LiquidationEvent>> {
        events::touched_positions(p, self.address, markets, from, to, self.log_chunk).await
    }

    async fn snapshots<P: Provider>(
        &self,
        p: &P,
        markets: &[MarketMeta],
        block: BlockId,
        timestamp: u64,
        prev: Option<&[Option<MarketSnapshot>]>,
    ) -> Result<Vec<Option<MarketSnapshot>>> {
        onchain::load_snapshots(p, self.address, markets, block, timestamp, prev).await
    }

    fn evaluate(&self, meta: &MarketMeta, snap: &MarketSnapshot, position: &(U256, U256)) -> Evaluation<Health, LiquidationQuote> {
        let (borrow_shares, collateral) = *position;
        let lltv = meta.params.lltv;
        let health = math::health(borrow_shares, collateral, snap.price, lltv, snap.totals);
        // An isolated market: one pair, one option when liquidatable.
        let options = if health.is_liquidatable() && !collateral.is_zero() {
            let seize = math::max_seizable(borrow_shares, collateral, snap.price, lltv, snap.totals);
            let q = math::quote_by_seized(seize, snap.price, lltv, snap.totals);
            let seized_value = math::mul_div_down(seize, snap.price, ORACLE_PRICE_SCALE);
            let bonus = math::units(seized_value.saturating_sub(q.repaid_assets), meta.loan_decimals)
                - math::units(q.repaid_assets.saturating_sub(seized_value), meta.loan_decimals);
            vec![LiquidationOption { pair: pair_of(meta), max_seize: seize, quote: Some(q), bonus }]
        } else {
            vec![]
        };
        Evaluation { hf: health.factor(), debt: math::units(health.borrowed, meta.loan_decimals), health, main: pair_of(meta), options }
    }

    fn token_decimals(&self, m: &MarketMeta, token: Address) -> u8 {
        if token == m.params.collateralToken { m.collateral_decimals } else { m.loan_decimals }
    }

    fn repay_for_seize(&self, m: &MarketMeta, snap: &MarketSnapshot, _pair: Pair, seize: U256) -> U256 {
        math::quote_by_seized(seize, snap.price, m.params.lltv, snap.totals).repaid_assets
    }

    fn collateral_value(&self, _m: &MarketMeta, snap: &MarketSnapshot, _pair: Pair, amount: U256) -> U256 {
        math::mul_div_down(amount, snap.price, ORACLE_PRICE_SCALE)
    }

    /// USD stablecoins at 1, WETH at the ETH price; anything else has no model (refused at start).
    fn loan_usd(&self, _m: &MarketMeta, _snap: &MarketSnapshot, debt_token: Address, eth_usd: f64) -> Option<f64> {
        loan_usd_model(self.chain_id, debt_token, eth_usd)
    }

    fn pairs(&self, m: &MarketMeta) -> Vec<Pair> {
        vec![pair_of(m)]
    }

    /// ETH price in USD from a tracked WETH/USD* market.
    fn eth_usd(&self, markets: &[MarketMeta], snaps: &[Option<MarketSnapshot>]) -> Option<f64> {
        markets
            .iter()
            .zip(snaps)
            .find(|(m, _)| m.params.collateralToken == WETH && loan_usd_model(self.chain_id, m.params.loanToken, 0.0) == Some(1.0))
            .and_then(|(m, s)| s.as_ref().map(|s| math::u256_f64(s.price) / 1e36 * 10f64.powi(m.collateral_decimals as i32 - m.loan_decimals as i32)))
    }
}

/// Morpho's `liquidate` with empty callback data pulls the repayment from the caller after sending
/// the collateral, so a caller that already holds the repayment (a flash-funded liquidator) can use
/// it like any callback-free protocol.
impl DirectLiquidation for Morpho {
    fn liquidation_call(&self, m: &MarketMeta, user: Address, _pair: Pair, seize: U256, _repay: U256) -> LiquidationTx {
        let calldata = onchain::IMorpho::liquidateCall { marketParams: m.params.clone(), borrower: user, seizedAssets: seize, repaidShares: U256::ZERO, data: Default::default() }.abi_encode().into();
        LiquidationTx { target: self.address, calldata, debt_token: m.params.loanToken, collateral_token: m.params.collateralToken }
    }
}

/// USD stablecoins on Base the bot prices at 1, by ADDRESS (a token's symbol is whatever its deployer
/// chose; a fake "USDC" must not be priced at a dollar). Checked on-chain 2026-10-28.
const BASE_USD_STABLES: [Address; 5] = [
    address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"), // USDC
    address!("d9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA"), // USDbC
    address!("fde4C96c8593536E31F229EA8f37b2ADa2699bb2"), // USDT
    address!("50c5725949A6F0c72E6C4a641F24049A917DB0Cb"), // DAI
    address!("820C137fa70C8691f0e44Dc420a5e53c168921Dc"), // USDS
];
/// WETH on Base (and every OP-stack chain).
const WETH: Address = address!("4200000000000000000000000000000000000006");

/// USD per loan token by address: Base USD stablecoins at 1, WETH at `eth_usd`, else None (other
/// chains have no model yet: their markets are refused rather than mispriced).
/// The one pair of an isolated Morpho market.
pub fn pair_of(m: &MarketMeta) -> Pair {
    Pair { collateral: m.params.collateralToken, debt: m.params.loanToken }
}

pub fn loan_usd_model(chain_id: u64, token: Address, eth_usd: f64) -> Option<f64> {
    match chain_id {
        8453 if BASE_USD_STABLES.contains(&token) => Some(1.0),
        8453 if token == WETH => Some(eth_usd),
        _ => None,
    }
}

/// The engine bound to Morpho, as the application uses it.
pub type MorphoEngine<P> = crate::engine::Engine<P, Morpho>;
/// A tracked Morpho position evaluated against its market snapshot.
pub type PositionView = crate::engine::PositionView<Morpho>;

/// Evaluates one Morpho position (borrow shares, collateral) outside the engine's tracked set.
pub fn evaluate_position(morpho: &Morpho, meta: &MarketMeta, snap: &MarketSnapshot, market: usize, user: Address, borrow_shares: U256, collateral: U256) -> PositionView {
    crate::engine::view(morpho, market, user, meta, snap, &(borrow_shares, collateral))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loan_tokens_are_priced_by_address_not_symbol() {
        let usdc = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        assert_eq!(loan_usd_model(8453, usdc, 2400.0), Some(1.0));
        assert_eq!(loan_usd_model(8453, WETH, 2400.0), Some(2400.0));
        // A token calling itself "USDC" at another address, or the same address on another chain.
        assert_eq!(loan_usd_model(8453, Address::repeat_byte(0x11), 2400.0), None);
        assert_eq!(loan_usd_model(1, usdc, 2400.0), None);
    }
}
