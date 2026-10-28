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
    protocol::{BorrowerInfo, DirectLiquidation, Evaluation, LendingProtocol, LiquidationTx, MarketInfo, Touched},
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
        let max_seize = if hf < 1.0 { U256::from(p.1) } else { U256::ZERO };
        Evaluation { health: (), hf, debt: p.0 as f64, max_seize, quote: None, bonus: 0.0 }
    }
    fn exit_tokens(&self, _m: &Mk) -> (Address, Address) {
        (Address::repeat_byte(0xc0), Address::repeat_byte(0x10))
    }
    fn loan_decimals(&self, _m: &Mk) -> u8 {
        0
    }
    fn repay_for_seize(&self, _m: &Mk, _snap: &f64, seize: U256) -> U256 {
        seize
    }
    fn collateral_value(&self, _m: &Mk, _snap: &f64, amount: U256) -> U256 {
        amount
    }
    fn loan_usd(&self, _m: &Mk, _snap: &f64, _eth_usd: f64) -> Option<f64> {
        Some(1.0)
    }
}

/// Toy liquidation call: `liquidate(user, repay)` on a fixed protocol address (0x70..).
impl DirectLiquidation for Toy {
    fn liquidation_call(&self, _m: &Mk, user: Address, _seize: U256, repay: U256) -> LiquidationTx {
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
    fn encode(&self, _m: &Mk, user: Address, seize: U256, _repay: U256, route: &ExitRoute, min_profit: U256, _deadline: u64) -> Payload {
        let tag = match route {
            ExitRoute::Packed(_) => 1u8,
            ExitRoute::Call { .. } => 2u8,
        };
        let mut d = vec![tag];
        d.extend_from_slice(user.as_slice());
        d.extend_from_slice(&seize.to_be_bytes::<32>());
        d.extend_from_slice(&min_profit.to_be_bytes::<32>());
        Payload { to: self.target(), calldata: Bytes::from(d), min_profit: MinProfitSlot { offset: 53, width: 32 } }
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
