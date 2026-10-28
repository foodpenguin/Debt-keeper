//! Simulation of a full liquidation with eth_call. Before the contract is deployed, its runtime
//! bytecode (immutables patched) is injected at a placeholder address via state override.

use std::collections::HashMap;

use alloy::{
    eips::BlockId,
    network::TransactionBuilder,
    primitives::{Address, B256, Bytes, U256, address},
    providers::Provider,
    rpc::types::{
        TransactionRequest,
        state::{AccountOverride, StateOverride},
    },
    sol,
    sol_types::SolCall,
};
use anyhow::{Context, Result, bail};

use crate::{
    dex::ExitRoute,
    executor::{MinProfitSlot, Payload},
    morpho::{Morpho, onchain::{MarketMeta, MarketParams}},
    venue::LiquidationVenue,
};
pub use crate::venue::{SimAt, SimResult, SimReverted};

/// Packed entry (MorphoLiquidator fallback): bytes [125, 141) hold minProfit as uint128.
pub const PACKED_MIN_PROFIT: MinProfitSlot = MinProfitSlot { offset: 125, width: 16 };
/// ABI `liquidate(...)`: selector + 9 head words before minProfit (5 MarketParams words,
/// borrower, seizedAssets, repaidShares, swaps offset).
pub const ABI_LIQUIDATE_MIN_PROFIT: MinProfitSlot = MinProfitSlot { offset: 4 + 9 * 32, width: 32 };

/// Where the liquidator lives in simulations when no deployed address is configured.
pub const SIM_LIQUIDATOR: Address = address!("00000000000000000000000000000000000b0771");
const GAS_PRICE_ORACLE: Address = address!("420000000000000000000000000000000000000F");
/// `price()` (Morpho oracle interface).
const PRICE_SELECTOR: [u8; 4] = [0xa0, 0x35, 0xb1, 0xfe];

sol! {
    struct LMarketParams { address loanToken; address collateralToken; address oracle; address irm; uint256 lltv; }
    struct SwapStep { address target; address tokenIn; uint256 amountIn; bytes data; }
    interface IMorphoLiquidator {
        function liquidate(LMarketParams marketParams, address borrower, uint256 seizedAssets, uint256 repaidShares, SwapStep[] swaps, uint256 minProfit, uint256 deadlineBlock) external returns (uint256 profit);
        function liquidateRoute(LMarketParams marketParams, address borrower, uint256 seizedAssets, bytes route, uint256 minProfit, uint256 deadlineBlock) external returns (uint256 profit);
    }
    interface IAllowance {
        function allowance(address owner, address spender) external view returns (uint256);
    }
    interface IGasPriceOracle {
        function getL1FeeUpperBound(uint256 unsignedTxSize) external view returns (uint256);
    }
}

pub struct LiquidatorCode {
    runtime: Bytes,
    /// Storage slots by variable label, from the artifact's storageLayout.
    slots: HashMap<String, (U256, u64)>,
}

impl LiquidatorCode {
    /// Runtime bytecode exactly as deployment would produce it: the constructor is executed in an
    /// eth_call (contract creation, no `to`), which returns the runtime code with every immutable
    /// (MORPHO, PROFIT_RECEIVER, ...) filled in. Storage set by the constructor is not kept, so the
    /// simulator overrides the slots it needs (see `overrides`).
    pub async fn build<P: Provider>(p: &P, path: &str, morpho: Address, owner: Address, executor: Address, receiver: Address) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path} (run `forge build` in contracts/)"))?;
        let a: serde_json::Value = serde_json::from_str(&text)?;
        let mut init: Vec<u8> = a["bytecode"]["object"].as_str().context("no bytecode")?.parse::<Bytes>()?.to_vec();
        for arg in [morpho, owner, executor, receiver] {
            init.extend_from_slice(B256::left_padding_from(arg.as_slice()).as_slice());
        }
        let tx = TransactionRequest::default().with_from(executor).with_deploy_code(Bytes::from(init));
        let runtime = p.call(tx).await.context("constructor simulation failed")?;
        if runtime.is_empty() {
            bail!("constructor simulation returned no code");
        }
        let mut slots = HashMap::new();
        for v in a["storageLayout"]["storage"].as_array().context("artifact has no storageLayout (set extra_output in foundry.toml)")? {
            let label = v["label"].as_str().unwrap_or_default().to_string();
            let slot: U256 = v["slot"].as_str().unwrap_or("0").parse()?;
            slots.insert(label, (slot, v["offset"].as_u64().unwrap_or(0)));
        }
        for needed in ["owner", "executor", "paused", "allowedTarget"] {
            if !slots.contains_key(needed) {
                bail!("storageLayout lacks `{needed}`: contract and bot are out of sync");
            }
        }
        Ok(Self { runtime, slots })
    }
}

/// True if `owner` has granted `spender` an effectively unlimited allowance.
pub async fn has_standing_allowance<P: Provider>(p: &P, token: Address, owner: Address, spender: Address) -> Result<bool> {
    let data: Bytes = IAllowance::allowanceCall { owner, spender }.abi_encode().into();
    let out = p.call(TransactionRequest::default().with_to(token).with_input(data)).await?;
    Ok(IAllowance::allowanceCall::abi_decode_returns(&out)? > U256::from(u128::MAX))
}

/// Finds the storage key of `allowance(owner, spender)` in an ERC-20 by probing candidate
/// mapping slots in ONE eth_call: every candidate key gets a distinct value via stateDiff and the
/// returned allowance tells which one the token reads. Covers Solidity (keccak(spender .
/// keccak(owner . slot))) and Vyper-style (keccak(owner . keccak(spender . slot))) layouts.
pub async fn discover_allowance_key<P: Provider>(p: &P, token: Address, owner: Address, spender: Address) -> Result<B256> {
    use alloy::primitives::keccak256;
    let pad = |a: Address| B256::left_padding_from(a.as_slice());
    let nested = |outer: Address, inner: Address, slot: u64| {
        let mut x = [0u8; 64];
        x[..32].copy_from_slice(pad(outer).as_slice());
        x[32..].copy_from_slice(B256::from(U256::from(slot)).as_slice());
        let h = keccak256(x);
        let mut y = [0u8; 64];
        y[..32].copy_from_slice(pad(inner).as_slice());
        y[32..].copy_from_slice(h.as_slice());
        keccak256(y)
    };
    for (_layout, (outer, inner)) in [("solidity", (owner, spender)), ("vyper", (spender, owner))] {
        let mut diff = HashMap::new();
        for slot in 0..64u64 {
            diff.insert(nested(outer, inner, slot), B256::from(U256::from(1_000_000 + slot)));
        }
        let mut ov = StateOverride::default();
        ov.insert(token, AccountOverride { state_diff: Some(diff.into_iter().collect()), ..Default::default() });
        let data: Bytes = IAllowance::allowanceCall { owner, spender }.abi_encode().into();
        let out = p.call(TransactionRequest::default().with_to(token).with_input(data)).overrides(ov).await?;
        let v = IAllowance::allowanceCall::abi_decode_returns(&out)?;
        if v >= U256::from(1_000_000u64) && v < U256::from(1_000_064u64) {
            let slot = (v - U256::from(1_000_000u64)).to::<u64>();
            return Ok(nested(outer, inner, slot));
        }
    }
    bail!("allowance slot of token {token} not found in slots 0..64")
}

pub struct Simulator {
    pub liquidator: Address,
    pub executor: Address,
    /// Some(code) = inject the contract at `liquidator` (not deployed yet).
    code: Option<LiquidatorCode>,
    /// Swap targets the injected contract allowlists (mirrors what the owner sets on-chain).
    allowed_targets: Vec<Address>,
    /// (token, storage key) of the standing Morpho allowance to inject per loan token.
    approvals: Vec<(Address, B256)>,
}

impl Simulator {
    pub fn new(liquidator: Option<Address>, executor: Address, code: Option<LiquidatorCode>, allowed_targets: Vec<Address>) -> Self {
        Self {
            liquidator: liquidator.unwrap_or(SIM_LIQUIDATOR),
            executor,
            code: if liquidator.is_some() { None } else { code },
            allowed_targets,
            approvals: Vec::new(),
        }
    }

    /// Registers the standing Morpho allowance of a loan token (injected contract only).
    pub fn add_approval(&mut self, token: Address, key: B256) {
        self.approvals.push((token, key));
    }

    pub fn calldata(
        &self,
        mp: &MarketParams,
        borrower: Address,
        seized: U256,
        swaps: Vec<SwapStep>,
        min_profit: U256,
        deadline_block: u64,
    ) -> Bytes {
        IMorphoLiquidator::liquidateCall {
            marketParams: LMarketParams { loanToken: mp.loanToken, collateralToken: mp.collateralToken, oracle: mp.oracle, irm: mp.irm, lltv: mp.lltv },
            borrower,
            seizedAssets: seized,
            repaidShares: U256::ZERO,
            swaps,
            minProfit: min_profit,
            deadlineBlock: U256::from(deadline_block),
        }
        .abi_encode()
        .into()
    }

    /// Packed production calldata (contract fallback): ~1/2 the bytes of the ABI entry, lower L1
    /// fee and intrinsic gas. Layout must match MorphoLiquidator.fallback().
    pub fn calldata_packed(&self, mp: &MarketParams, borrower: Address, seized: U256, route: &[u8], min_profit: U256, deadline_block: u64) -> Bytes {
        let mut d = Vec::with_capacity(145 + route.len());
        d.push(0x00);
        d.extend_from_slice(mp.loanToken.as_slice());
        d.extend_from_slice(mp.collateralToken.as_slice());
        d.extend_from_slice(mp.oracle.as_slice());
        d.extend_from_slice(mp.irm.as_slice());
        d.extend_from_slice(&mp.lltv.to::<u64>().to_be_bytes());
        d.extend_from_slice(borrower.as_slice());
        d.extend_from_slice(&seized.to::<u128>().to_be_bytes());
        d.extend_from_slice(&min_profit.to::<u128>().to_be_bytes());
        d.extend_from_slice(&(deadline_block as u32).to_be_bytes());
        d.extend_from_slice(route);
        d.into()
    }

    fn overrides(&self, extra: &StateOverride) -> StateOverride {
        let mut ov = extra.clone();
        if let Some(code) = &self.code {
            let word = |a: Address| B256::left_padding_from(a.as_slice());
            // Build slot words byte by byte: variables can share a slot (executor + paused do), and
            // Solidity packs a value at `offset` into bytes [32-offset-len, 32-offset) of the word.
            let mut words: HashMap<B256, [u8; 32]> = HashMap::new();
            let mut put = |label: &str, value: &[u8]| {
                let (slot, offset) = code.slots[label];
                let w = words.entry(B256::from(slot)).or_insert([0u8; 32]);
                let end = 32 - offset as usize;
                w[end - value.len()..end].copy_from_slice(value);
            };
            // owner = executor = profitReceiver = our executor address; not paused, no pending owner.
            put("owner", self.executor.as_slice());
            put("executor", self.executor.as_slice());
            put("paused", &[0u8]);
            let mut state: HashMap<B256, B256> = words.into_iter().map(|(k, v)| (k, B256::from(v))).collect();
            // allowedTarget[t] lives at keccak256(pad(t) . pad(mappingSlot)).
            let map_slot = B256::from(code.slots["allowedTarget"].0);
            for t in &self.allowed_targets {
                let mut preimage = [0u8; 64];
                preimage[..32].copy_from_slice(word(*t).as_slice());
                preimage[32..].copy_from_slice(map_slot.as_slice());
                state.insert(alloy::primitives::keccak256(preimage), B256::with_last_byte(1));
            }
            ov.insert(self.liquidator, AccountOverride { code: Some(code.runtime.clone()), state: Some(state.into_iter().collect()), ..Default::default() });
            // Standing Morpho allowance (the owner sets it once on the real contract).
            for (token, key) in &self.approvals {
                let entry = ov.entry(*token).or_default();
                let mut diff: HashMap<B256, B256> = entry.state_diff.clone().map(|d| d.into_iter().collect()).unwrap_or_default();
                diff.insert(*key, B256::from(U256::MAX));
                entry.state_diff = Some(diff.into_iter().collect());
            }
        }
        ov
    }

    /// Runs the liquidation as `executor` on top of `block`'s state; returns the contract's
    /// reported profit and gas. `at` executes it as the NEXT block would (number + timestamp):
    /// candidates are flagged with interest accrued to the next block, so simulating at the
    /// current block's timestamp would wrongly report "healthy" for interest-only crossings.
    ///
    /// One `eth_simulateV1` call returns both the profit and the gas actually used. (eth_call +
    /// eth_estimateGas cannot do this: Base's reth nodes ignore block overrides in estimateGas,
    /// so the estimate would run at the wrong block.) There is no made-up fallback: the gas drives
    /// net profit, the max_gas check and the live gas limit, so without a real measurement the
    /// plan must not go forward.
    ///
    /// `probe` = an oracle whose `price()` is read in the same simulated state just before the
    /// call: same request, so same node and state, i.e. the price the liquidation really saw.
    pub async fn simulate<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, extra: &StateOverride, at: Option<SimAt>, probe: Option<Address>) -> Result<SimResult> {
        let ov = self.overrides(extra);
        let call = serde_json::json!({
            "from": self.executor,
            "to": self.liquidator,
            "data": calldata,
            "gas": "0x1c9c380", // 30M: generous ceiling, gasUsed is what we report
        });
        let mut calls: Vec<serde_json::Value> = probe
            .map(|oracle| serde_json::json!({ "from": self.executor, "to": oracle, "data": Bytes::from_static(&PRICE_SELECTOR), "gas": "0x7a120" }))
            .into_iter()
            .collect();
        let idx = calls.len();
        calls.push(call);
        let mut block_calls = serde_json::json!({ "stateOverrides": ov, "calls": calls });
        if let Some(a) = at {
            block_calls["blockOverrides"] = serde_json::json!({ "number": format!("0x{:x}", a.number), "time": format!("0x{:x}", a.time) });
        }
        let payload = serde_json::json!({ "blockStateCalls": [block_calls], "validation": false });
        let tag = match block {
            BlockId::Number(alloy::eips::BlockNumberOrTag::Number(n)) => serde_json::json!(format!("0x{n:x}")),
            BlockId::Number(alloy::eips::BlockNumberOrTag::Pending) => serde_json::json!("pending"),
            _ => serde_json::json!("latest"),
        };
        // The L1 fee only depends on the calldata size: fetch it alongside the simulation.
        let (r, l1) = tokio::join!(p.raw_request::<_, serde_json::Value>("eth_simulateV1".into(), (payload, tag)), self.l1_fee(p, calldata.len(), block));
        let r = r.map_err(|e| anyhow::anyhow!("simulation unavailable (eth_simulateV1): {e}"))?;
        let block_number = r[0]["number"].as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok());
        let probe_price = probe.and_then(|_| {
            let pc = &r[0]["calls"][0];
            let data: Bytes = pc["returnData"].as_str().filter(|_| pc["status"].as_str() == Some("0x1"))?.parse().ok()?;
            (data.len() == 32).then(|| U256::from_be_slice(&data))
        });
        let c = &r[0]["calls"][idx];
        if c["status"].as_str() != Some("0x1") {
            let msg = c["error"]["message"].as_str().unwrap_or("reverted");
            let data = c["returnData"].as_str().or_else(|| c["error"]["data"].as_str()).unwrap_or("");
            return Err(SimReverted { message: format!("{msg} {data}"), block_number, probe_price }.into());
        }
        let out: Bytes = c["returnData"].as_str().context("simulateV1: no returnData")?.parse()?;
        // Both entrypoints return a single uint256 profit.
        let profit = IMorphoLiquidator::liquidateCall::abi_decode_returns(&out)?;
        let gas_used = c["gasUsed"]
            .as_str()
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .context("simulateV1: no gasUsed")?;
        let l1_fee_wei = l1.unwrap_or(U256::ZERO);
        Ok(SimResult { profit, gas_used, l1_fee_wei, block_number, probe_price })
    }

    /// Debug aid: does the call succeed under each fixed gas limit?
    pub async fn probe_gas<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, extra: &StateOverride, limits: &[u64]) -> Vec<(u64, bool)> {
        let ov = self.overrides(extra);
        let mut out = Vec::new();
        for &g in limits {
            let tx = TransactionRequest::default().with_from(self.executor).with_to(self.liquidator).with_input(calldata.clone()).with_gas_limit(g);
            out.push((g, p.call(tx).block(block).overrides(ov.clone()).await.is_ok()));
        }
        out
    }

    async fn l1_fee<P: Provider>(&self, p: &P, calldata_len: usize, block: BlockId) -> Result<U256> {
        let data: Bytes = IGasPriceOracle::getL1FeeUpperBoundCall { unsignedTxSize: U256::from(calldata_len + 100) }.abi_encode().into();
        let tx = TransactionRequest::default().with_to(GAS_PRICE_ORACLE).with_input(data);
        let out = p.call(tx).block(block).await?;
        Ok(IGasPriceOracle::getL1FeeUpperBoundCall::abi_decode_returns(&out)?)
    }
}

/// MorphoLiquidator as the liquidation venue for Morpho Blue. A packed route goes through the
/// compact fallback entry; an external call becomes the one swap step of the ABI `liquidate`.
impl LiquidationVenue<Morpho> for Simulator {
    fn target(&self) -> Address {
        self.liquidator
    }

    fn encode(&self, m: &MarketMeta, user: Address, seize: U256, route: &ExitRoute, min_profit: U256, deadline_block: u64) -> Payload {
        match route {
            ExitRoute::Packed(r) => Payload { to: self.liquidator, calldata: self.calldata_packed(&m.params, user, seize, r, min_profit, deadline_block), min_profit: PACKED_MIN_PROFIT },
            ExitRoute::Call { target, calldata } => {
                let swaps = vec![SwapStep { target: *target, tokenIn: m.params.collateralToken, amountIn: seize, data: calldata.clone() }];
                Payload { to: self.liquidator, calldata: self.calldata(&m.params, user, seize, swaps, min_profit, deadline_block), min_profit: ABI_LIQUIDATE_MIN_PROFIT }
            }
        }
    }

    // Inherent methods take precedence over trait methods, so these delegate to the Simulator's own.
    async fn simulate<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, overrides: &StateOverride, at: Option<SimAt>, probe: Option<Address>) -> Result<SimResult> {
        Simulator::simulate(self, p, calldata, block, overrides, at, probe).await
    }

    async fn probe_gas<P: Provider>(&self, p: &P, calldata: Bytes, block: BlockId, overrides: &StateOverride, limits: &[u64]) -> Vec<(u64, bool)> {
        Simulator::probe_gas(self, p, calldata, block, overrides, limits).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JSON-RPC mock. `sim_ok`: eth_simulateV1 returns a successful call (profit 42, 123456 gas);
    /// otherwise it fails like an endpoint without the method. eth_call always returns 42.
    async fn mock_rpc(sim_ok: bool) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    // Read until the whole body (content-length) has arrived: headers and body
                    // can come in separate TCP reads.
                    let mut data = Vec::new();
                    let mut buf = vec![0u8; 65536];
                    let text = loop {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            break String::from_utf8_lossy(&data).to_string();
                        }
                        data.extend_from_slice(&buf[..n]);
                        let t = String::from_utf8_lossy(&data).to_string();
                        if let Some((head, body)) = t.split_once("\r\n\r\n") {
                            let len = head
                                .lines()
                                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                                .unwrap_or(0);
                            if body.len() >= len {
                                break t;
                            }
                        }
                    };
                    let body = text.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("{}");
                    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
                    let id = v["id"].clone();
                    let payload = if v["method"] == "eth_simulateV1" && !sim_ok {
                        format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32601,"message":"the method eth_simulateV1 does not exist"}}}}"#)
                    } else if v["method"] == "eth_simulateV1" {
                        // A two-call request = oracle probe first (answers 777), then the liquidation.
                        let probed = v["params"][0]["blockStateCalls"][0]["calls"].as_array().is_some_and(|c| c.len() == 2);
                        let probe = if probed { format!(r#"{{"status":"0x1","gasUsed":"0x5208","returnData":"0x{:064x}","logs":[]}},"#, 777) } else { String::new() };
                        format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":[{{"number":"0xa","calls":[{probe}{{"status":"0x1","gasUsed":"0x1e240","returnData":"0x{:064x}","logs":[]}}]}}]}}"#,
                            42
                        )
                    } else {
                        format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"0x{:064x}"}}"#, 42)
                    };
                    let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{payload}", payload.len());
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn no_simulation_means_error_not_a_guessed_gas() {
        use alloy::providers::ProviderBuilder;
        let p = ProviderBuilder::new().connect_http(mock_rpc(false).await.parse().unwrap());
        let sim = Simulator::new(Some(Address::repeat_byte(7)), Address::repeat_byte(8), None, vec![]);
        let r = sim.simulate(&p, Bytes::from(vec![1u8, 2, 3]), BlockId::latest(), &StateOverride::default(), None, None).await;
        let err = r.err().expect("must fail instead of inventing a gas value").to_string();
        assert!(err.contains("simulation unavailable"), "{err}");
    }

    #[tokio::test]
    async fn simulate_v1_reports_profit_and_real_gas() {
        use alloy::providers::ProviderBuilder;
        let p = ProviderBuilder::new().connect_http(mock_rpc(true).await.parse().unwrap());
        let sim = Simulator::new(Some(Address::repeat_byte(7)), Address::repeat_byte(8), None, vec![]);
        let at = Some(SimAt { number: 10, time: 20 });
        let r = sim.simulate(&p, Bytes::from(vec![1u8, 2, 3]), BlockId::number(9), &StateOverride::default(), at, None).await.unwrap();
        assert_eq!(r.profit, U256::from(42u64));
        assert_eq!(r.gas_used, 123_456);
        assert_eq!(r.block_number, Some(10));
        assert_eq!(r.probe_price, None);
    }

    #[tokio::test]
    async fn oracle_probe_reads_price_and_keeps_call_result() {
        use alloy::providers::ProviderBuilder;
        let p = ProviderBuilder::new().connect_http(mock_rpc(true).await.parse().unwrap());
        let sim = Simulator::new(Some(Address::repeat_byte(7)), Address::repeat_byte(8), None, vec![]);
        let r = sim.simulate(&p, Bytes::from(vec![1u8, 2, 3]), BlockId::pending(), &StateOverride::default(), None, Some(Address::repeat_byte(9))).await.unwrap();
        assert_eq!(r.probe_price, Some(U256::from(777u64)));
        assert_eq!((r.profit, r.gas_used), (U256::from(42u64), 123_456), "the liquidation is the second call");
    }

    #[test]
    fn overrides_merge_shared_slots() {
        // executor (offset 0) and paused (offset 20) share slot 1, as in the optimized contract.
        let mut slots = HashMap::new();
        slots.insert("owner".to_string(), (U256::from(0), 0u64));
        slots.insert("executor".to_string(), (U256::from(1), 0));
        slots.insert("paused".to_string(), (U256::from(1), 20));
        slots.insert("allowedTarget".to_string(), (U256::from(4), 0));
        let code = LiquidatorCode { runtime: Bytes::new(), slots };
        let exec = Address::repeat_byte(0xEE);
        let sim = Simulator::new(None, exec, Some(code), vec![Address::repeat_byte(0xAA)]);
        let ov = sim.overrides(&StateOverride::default());
        let st = ov[&SIM_LIQUIDATOR].state.clone().unwrap();
        let slot1 = st[&B256::with_last_byte(1)];
        assert_eq!(&slot1[12..], exec.as_slice(), "executor must survive the paused write");
        assert_eq!(slot1[11], 0, "paused byte");
        assert_eq!(st.len(), 3, "2 slots + 1 allowlist entry");
    }

    #[test]
    fn packed_layout() {
        let sim = Simulator::new(None, Address::ZERO, None, vec![]);
        let mp = MarketParams {
            loanToken: Address::repeat_byte(1),
            collateralToken: Address::repeat_byte(2),
            oracle: Address::repeat_byte(3),
            irm: Address::repeat_byte(4),
            lltv: U256::from(860_000_000_000_000_000u64),
        };
        let route = [9u8; 37];
        let d = sim.calldata_packed(&mp, Address::repeat_byte(5), U256::from(7u64), &route, U256::from(8u64), 0x01020304);
        assert_eq!(d.len(), 145 + 37);
        assert_eq!(d[0], 0);
        assert_eq!(&d[1..21], mp.loanToken.as_slice());
        assert_eq!(&d[61..81], mp.irm.as_slice());
        assert_eq!(u64::from_be_bytes(d[81..89].try_into().unwrap()), 860_000_000_000_000_000);
        assert_eq!(&d[89..109], Address::repeat_byte(5).as_slice());
        assert_eq!(u128::from_be_bytes(d[109..125].try_into().unwrap()), 7);
        assert_eq!(u128::from_be_bytes(d[125..141].try_into().unwrap()), 8);
        assert_eq!(&d[141..145], &[1, 2, 3, 4]);
        assert_eq!(&d[145..], &route);
    }
}

/// Bytecode that returns `price` for any call: stands in for a Morpho oracle in "what if" runs.
pub fn constant_price_oracle(price: U256) -> Bytes {
    let mut code = vec![0x7f];
    code.extend_from_slice(&price.to_be_bytes::<32>());
    code.extend_from_slice(&[0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
    code.into()
}
