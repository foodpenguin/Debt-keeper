//! Test doubles shared by the generic-core tests: a toy lending protocol that has nothing to do with
//! Morpho, and a venue that encodes but never simulates. They prove the engine and the planner do
//! not depend on one protocol or one contract.

use std::{collections::HashMap, sync::Mutex};

use alloy::{
    eips::BlockId,
    primitives::{Address, B256, Bytes, U256},
    providers::Provider,
    rpc::types::state::StateOverride,
};
use anyhow::Result;
use futures::future::BoxFuture;

use crate::{
    config::Config,
    dex::{Dex, ExitRoute, SwapCall},
    executor::{MinProfitSlot, Payload},
    protocol::{BorrowerInfo, DirectLiquidation, Evaluation, LendingProtocol, LiquidationOption, LiquidationTx, MarketInfo, Pair, Touched},
    venue::{LiquidationVenue, SimAt, SimFee, SimResult},
};

#[derive(Clone, Debug)]
pub struct Mk {
    pub key: B256,
    pub name: String,
}
impl MarketInfo for Mk {
    fn key(&self) -> B256 {
        self.key
    }
    fn name(&self) -> &str {
        &self.name
    }
}

pub fn key(n: u8) -> B256 {
    B256::repeat_byte(n)
}
pub fn user(n: u8) -> Address {
    Address::repeat_byte(n)
}

/// Position = (debt, collateral) in whole units, snapshot = collateral price,
/// hf = collateral * price / debt; a liquidatable position may seize all its collateral and
/// repays 1 debt unit per collateral unit. "On-chain" state is a map the test edits.
#[derive(Default)]
pub struct Toy {
    pub state: Mutex<HashMap<(B256, Address), (u64, u64)>>,
    pub events: Mutex<Touched<&'static str>>,
    pub borrowers: Vec<(usize, Address)>,
}

impl LendingProtocol for Toy {
    type Market = Mk;
    type Position = (u64, u64);
    type Snapshot = f64;
    type Health = ();
    type Quote = ();
    type Liquidation = &'static str;

    async fn load_markets<P: Provider>(&self, _p: &P) -> Result<Vec<Mk>> {
        Ok(vec![Mk { key: key(1), name: "alpha".into() }, Mk { key: key(2), name: "beta".into() }])
    }
    fn borrower_job(&self, _http: reqwest::Client, markets: &[Mk]) -> BoxFuture<'static, Vec<(usize, Result<Vec<BorrowerInfo>>)>> {
        let (n, borrowers) = (markets.len(), self.borrowers.clone());
        Box::pin(async move {
            (0..n)
                .map(|i| (i, Ok(borrowers.iter().filter(|(m, _)| *m == i).map(|(_, u)| BorrowerInfo { user: *u, api_health_factor: Some(0.5) }).collect())))
                .collect()
        })
    }
    async fn load_positions<P: Provider>(&self, _p: &P, keys: &[(B256, Address)], _block: BlockId) -> Result<Vec<Option<(u64, u64)>>> {
        let s = self.state.lock().unwrap();
        Ok(keys.iter().map(|k| Some(s.get(k).copied().unwrap_or((0, 0)))).collect())
    }
    fn has_debt(&self, p: &(u64, u64)) -> bool {
        p.0 > 0
    }
    async fn touched<P: Provider>(&self, _p: &P, _markets: &[B256], _from: u64, _to: u64) -> Result<Touched<&'static str>> {
        Ok(std::mem::take(&mut *self.events.lock().unwrap()))
    }
    async fn snapshots<P: Provider>(&self, _p: &P, markets: &[Mk], _block: BlockId, _timestamp: u64, _prev: Option<&[Option<f64>]>) -> Result<Vec<Option<f64>>> {
        Ok(markets.iter().map(|_| Some(1.0)).collect())
    }
    fn evaluate(&self, _m: &Mk, price: &f64, p: &(u64, u64)) -> Evaluation<(), ()> {
        let hf = p.1 as f64 * price / p.0 as f64;
        let options = if hf < 1.0 { vec![LiquidationOption { pair: TOY_PAIR, max_seize: U256::from(p.1), quote: None, bonus: 0.0 }] } else { vec![] };
        Evaluation { health: (), hf, debt: p.0 as f64, main: TOY_PAIR, options }
    }
    fn token_decimals(&self, _m: &Mk, _token: Address) -> u8 {
        0
    }
    fn repay_for_seize(&self, _m: &Mk, _snap: &f64, _pair: Pair, seize: U256) -> U256 {
        seize
    }
    fn collateral_value(&self, _m: &Mk, _snap: &f64, _pair: Pair, amount: U256) -> U256 {
        amount
    }
    fn loan_usd(&self, _m: &Mk, _snap: &f64, _debt: Address, _eth_usd: f64) -> Option<f64> {
        Some(1.0)
    }
    fn pairs(&self, _m: &Mk) -> Vec<Pair> {
        vec![TOY_PAIR]
    }
}

/// The toy's one pair: collateral 0xc0.., debt 0x10...
pub const TOY_PAIR: Pair = Pair { collateral: Address::repeat_byte(0xc0), debt: Address::repeat_byte(0x10) };

/// Toy liquidation call: `liquidate(user, repay)` on a fixed protocol address (0x70..).
impl DirectLiquidation for Toy {
    fn liquidation_call(&self, _m: &Mk, user: Address, _pair: Pair, _seize: U256, repay: U256) -> LiquidationTx {
        let mut d = user.to_vec();
        d.extend_from_slice(&repay.to_be_bytes::<32>());
        LiquidationTx { target: Address::repeat_byte(0x70), calldata: Bytes::from(d), debt_token: Address::repeat_byte(0x10), collateral_token: Address::repeat_byte(0xc0) }
    }
}

/// A liquidation contract that encodes but never simulates.
pub struct MockVenue;

impl LiquidationVenue<Toy> for MockVenue {
    fn target(&self) -> Address {
        Address::repeat_byte(0x11)
    }
    fn encode(&self, _m: &Mk, user: Address, _pair: Pair, seize: U256, _repay: U256, route: &ExitRoute, min_profit: U256, _deadline: u64) -> Payload {
        let tag = match route {
            ExitRoute::Packed(_) => 1u8,
            ExitRoute::Call { .. } => 2u8,
        };
        let mut d = vec![tag];
        d.extend_from_slice(user.as_slice());
        d.extend_from_slice(&seize.to_be_bytes::<32>());
        d.extend_from_slice(&min_profit.to_be_bytes::<32>());
        Payload { to: Address::repeat_byte(0x11), calldata: Bytes::from(d), min_profit: MinProfitSlot { offset: 53, width: 32 } }
    }
    async fn simulate<P: Provider>(&self, _p: &P, _calldata: Bytes, _block: BlockId, _overrides: &StateOverride, _at: Option<SimAt>, _fee: Option<SimFee>, _probe: Option<Address>) -> Result<SimResult> {
        anyhow::bail!("mock venue: no simulation")
    }
    async fn probe_gas<P: Provider>(&self, _p: &P, _calldata: Bytes, _block: BlockId, _overrides: &StateOverride, _limits: &[u64]) -> Vec<(u64, bool)> {
        vec![]
    }
}

/// An account-level toy (Aave-style): one pool; an account holds collateral in two reserves (A, B)
/// and owes two (X, Y), all in whole units; the snapshot is the four prices. Health is computed
/// over the whole account (liquidation threshold 0.8); a liquidation repays up to half of one debt
/// reserve and seizes 1.05x its value from one collateral reserve. Every (collateral, debt) pair the
/// account holds is an option, largest bonus (in USD) first.
#[derive(Default)]
pub struct ToyPool {
    pub accounts: Mutex<HashMap<Address, [u64; 4]>>,
}

pub const RES_A: Address = Address::repeat_byte(0xa1);
pub const RES_B: Address = Address::repeat_byte(0xb1);
pub const RES_X: Address = Address::repeat_byte(0xe1);
pub const RES_Y: Address = Address::repeat_byte(0xf1);
const POOL_TOKENS: [Address; 4] = [RES_A, RES_B, RES_X, RES_Y];
const POOL_BONUS: f64 = 1.05;

fn pool_price(snap: &[f64; 4], token: Address) -> f64 {
    POOL_TOKENS.iter().position(|t| *t == token).map_or(0.0, |i| snap[i])
}

impl LendingProtocol for ToyPool {
    type Market = Mk;
    type Position = [u64; 4];
    type Snapshot = [f64; 4];
    type Health = ();
    type Quote = ();
    type Liquidation = &'static str;

    async fn load_markets<P: Provider>(&self, _p: &P) -> Result<Vec<Mk>> {
        Ok(vec![Mk { key: key(9), name: "pool".into() }])
    }
    fn borrower_job(&self, _http: reqwest::Client, markets: &[Mk]) -> BoxFuture<'static, Vec<(usize, Result<Vec<BorrowerInfo>>)>> {
        let users: Vec<Address> = self.accounts.lock().unwrap().keys().copied().collect();
        let n = markets.len();
        Box::pin(async move { (0..n).map(|i| (i, Ok(users.iter().map(|u| BorrowerInfo { user: *u, api_health_factor: None }).collect()))).collect() })
    }
    async fn load_positions<P: Provider>(&self, _p: &P, keys: &[(B256, Address)], _block: BlockId) -> Result<Vec<Option<[u64; 4]>>> {
        let a = self.accounts.lock().unwrap();
        Ok(keys.iter().map(|(_, u)| Some(a.get(u).copied().unwrap_or_default())).collect())
    }
    fn has_debt(&self, p: &[u64; 4]) -> bool {
        p[2] + p[3] > 0
    }
    async fn touched<P: Provider>(&self, _p: &P, _markets: &[B256], _from: u64, _to: u64) -> Result<Touched<&'static str>> {
        Ok(Touched::default())
    }
    async fn snapshots<P: Provider>(&self, _p: &P, markets: &[Mk], _block: BlockId, _timestamp: u64, _prev: Option<&[Option<[f64; 4]>]>) -> Result<Vec<Option<[f64; 4]>>> {
        Ok(markets.iter().map(|_| Some([1.0; 4])).collect())
    }
    fn evaluate(&self, _m: &Mk, snap: &[f64; 4], p: &[u64; 4]) -> Evaluation<(), ()> {
        let collateral_usd = p[0] as f64 * snap[0] + p[1] as f64 * snap[1];
        let debt_usd = p[2] as f64 * snap[2] + p[3] as f64 * snap[3];
        let hf = if debt_usd > 0.0 { 0.8 * collateral_usd / debt_usd } else { f64::INFINITY };
        let mut options = vec![];
        if hf < 1.0 {
            for (ci, collateral) in [(0, RES_A), (1, RES_B)] {
                for (di, debt) in [(2, RES_X), (3, RES_Y)] {
                    if p[ci] == 0 || p[di] == 0 {
                        continue;
                    }
                    let repay_usd = (0.5 * p[di] as f64 * snap[di]).min(p[ci] as f64 * snap[ci] / POOL_BONUS);
                    let seize = (repay_usd * POOL_BONUS / snap[ci]).floor() as u64;
                    let bonus = (seize as f64 * snap[ci] - repay_usd) / snap[di];
                    options.push(LiquidationOption { pair: Pair { collateral, debt }, max_seize: U256::from(seize), quote: None, bonus });
                }
            }
            options.sort_by(|a, b| (b.bonus * pool_price(snap, b.pair.debt)).total_cmp(&(a.bonus * pool_price(snap, a.pair.debt))));
        }
        Evaluation { health: (), hf, debt: p[2] as f64 + p[3] as f64, main: Pair { collateral: RES_A, debt: RES_X }, options }
    }
    fn token_decimals(&self, _m: &Mk, _token: Address) -> u8 {
        0
    }
    fn repay_for_seize(&self, _m: &Mk, snap: &[f64; 4], pair: Pair, seize: U256) -> U256 {
        let s: u64 = seize.saturating_to();
        U256::from((s as f64 * pool_price(snap, pair.collateral) / POOL_BONUS / pool_price(snap, pair.debt)).ceil() as u64)
    }
    fn collateral_value(&self, _m: &Mk, snap: &[f64; 4], pair: Pair, amount: U256) -> U256 {
        let a: u64 = amount.saturating_to();
        U256::from((a as f64 * pool_price(snap, pair.collateral) / pool_price(snap, pair.debt)) as u64)
    }
    fn loan_usd(&self, _m: &Mk, snap: &[f64; 4], debt: Address, _eth_usd: f64) -> Option<f64> {
        Some(pool_price(snap, debt)).filter(|p| *p > 0.0)
    }
    fn pairs(&self, _m: &Mk) -> Vec<Pair> {
        [RES_A, RES_B].iter().flat_map(|c| [RES_X, RES_Y].map(|d| Pair { collateral: *c, debt: d })).collect()
    }
}

/// The mock venue on the account-level toy: the pair goes into the calldata, so tests can see
/// which one the planner liquidates (`[tag, user, collateral, debt, seize, minProfit]`).
impl LiquidationVenue<ToyPool> for MockVenue {
    fn target(&self) -> Address {
        Address::repeat_byte(0x11)
    }
    fn encode(&self, _m: &Mk, user: Address, pair: Pair, seize: U256, _repay: U256, route: &ExitRoute, min_profit: U256, _deadline: u64) -> Payload {
        let mut d = vec![if matches!(route, ExitRoute::Packed(_)) { 1u8 } else { 2u8 }];
        for a in [user, pair.collateral, pair.debt] {
            d.extend_from_slice(a.as_slice());
        }
        d.extend_from_slice(&seize.to_be_bytes::<32>());
        d.extend_from_slice(&min_profit.to_be_bytes::<32>());
        Payload { to: Address::repeat_byte(0x11), calldata: Bytes::from(d), min_profit: MinProfitSlot { offset: 93, width: 32 } }
    }
    async fn simulate<P: Provider>(&self, _p: &P, _calldata: Bytes, _block: BlockId, _overrides: &StateOverride, _at: Option<SimAt>, _fee: Option<SimFee>, _probe: Option<Address>) -> Result<SimResult> {
        anyhow::bail!("mock venue: no simulation")
    }
    async fn probe_gas<P: Provider>(&self, _p: &P, _calldata: Bytes, _block: BlockId, _overrides: &StateOverride, _limits: &[u64]) -> Vec<(u64, bool)> {
        vec![]
    }
}

/// An aggregator that pays 2 loan units per collateral unit at 100k swap gas, routed via 0x22..
pub struct MockDex;

impl Dex for MockDex {
    type Quote = U256;
    fn name(&self) -> &'static str {
        "mockdex"
    }
    async fn quote_within(&self, _token_in: Address, _token_out: Address, amount_in: U256, _gas_budget: u64) -> Result<U256> {
        Ok(amount_in * U256::from(2u8))
    }
    fn amount_out(&self, quote: &U256) -> U256 {
        *quote
    }
    fn swap_gas(&self, _quote: &U256) -> Option<u64> {
        Some(100_000)
    }
    async fn build(&self, quote: &U256, _sender: Address, _recipient: Address, _slippage_bps: u32) -> Result<SwapCall> {
        Ok(SwapCall { target: Address::repeat_byte(0x22), calldata: Bytes::from(quote.to_be_bytes::<32>().to_vec()) })
    }
}

/// A liquidation-bot config with dust 5.0 and ETH/USD fallback 1234.
pub fn cfg() -> Config {
    toml::from_str(
        r#"
        chain_id = 8453
        journal_path = "x"
        morpho = { address = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb", api = "x", markets = [] }
        dust_debt = 5.0
        [exec]
        executor = "0x000000000000000000000000000000000000dEaD"
        liquidator_artifact = "x"
        kyber_api = "x"
        kyber_client_id = "x"
        slippage_bps = 50
        priority_fee_wei = 1
        replan_blocks = 1
        max_plans_per_block = 1
        eth_usd_fallback = 1234.0
        "#,
    )
    .unwrap()
}
