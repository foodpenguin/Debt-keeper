//! Temporary: decode the journal's liquidation txs (Atlas metacall bids, winners) and match them
//! to our atlas_auction valuations. RPC: MATCH_RPC; ~2 req/s.
use std::collections::{BTreeMap, BTreeSet};

use alloy::{
    primitives::{Address, B256, Bytes, address},
    sol,
    sol_types::{SolCall, SolEvent},
};
use serde_json::{Value, json};

sol! {
    struct UserOperation { address from; address to; uint256 value; uint256 gas; uint256 maxFeePerGas; uint256 nonce; uint256 deadline; address dapp; address control; uint32 callConfig; uint32 dappGasLimit; uint32 solverGasLimit; uint24 bundlerSurchargeRate; address sessionKey; bytes data; bytes signature; }
    struct SolverOperation { address from; address to; uint256 value; uint256 gas; uint256 maxFeePerGas; uint256 deadline; address solver; address control; bytes32 userOpHash; address bidToken; uint256 bidAmount; bytes data; bytes signature; }
    struct DAppOperation { address from; address to; uint256 nonce; uint256 deadline; address control; address bundler; bytes32 userOpHash; bytes32 callChainHash; bytes signature; }
    function metacall(UserOperation userOp, SolverOperation[] solverOps, DAppOperation dAppOp, address gasRefundBeneficiary);
    event SolverTxResult(address indexed solverTo, address indexed solverFrom, address indexed dAppControl, address bidToken, uint256 bidAmount, bool executed, bool success, uint256 result);
}
const ATLAS: Address = address!("583dcFef0D240DC80753F0F0B26513feE27D9B77");

async fn rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    for attempt in 0..6 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Ok(r) = http.post(url).json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).send().await
            && let Ok(v) = r.json::<Value>().await
            && let Some(x) = v.get("result")
        {
            return Ok(x.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(1000 * (attempt + 1))).await;
    }
    anyhow::bail!("{method} failed")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("journal path");
    let since = std::env::args().nth(2).map(|s| s.parse::<u64>().unwrap()).unwrap_or(0);
    let url = std::env::var("MATCH_RPC")?;
    let http = reqwest::Client::new();
    let mut txs: BTreeMap<String, (u64, f64, usize, String, String)> = BTreeMap::new(); // tx -> block, gross, n, liquidator, market
    let mut auctions: BTreeMap<B256, Value> = BTreeMap::new();
    for line in std::fs::read_to_string(path)?.lines() {
        let Ok(r) = serde_json::from_str::<Value>(line) else { continue };
        match r["kind"].as_str() {
            Some("liquidation") if r["block"].as_u64().unwrap_or(0) >= since => {
                let e = txs.entry(r["tx"].as_str().unwrap().to_string()).or_insert((r["block"].as_u64().unwrap(), 0.0, 0, r["liquidator"].as_str().unwrap().into(), r["market"].as_str().unwrap().into()));
                e.1 += r["gross_bonus"].as_f64().unwrap_or(0.0);
                e.2 += 1;
            }
            Some("atlas_auction") if r["liquidatable"].as_u64().unwrap_or(0) > 0 => {
                if let Ok(h) = r["user_op_hash"].as_str().unwrap_or_default().parse::<B256>() {
                    auctions.insert(h, r);
                }
            }
            _ => {}
        }
    }
    let mut bidders_all = BTreeSet::new();
    let mut winners_all = BTreeSet::new();
    for (h, (block, gross, n, liquidator, market)) in &txs {
        let tx = rpc(&http, &url, "eth_getTransactionByHash", json!([h])).await?;
        let to: Address = tx["to"].as_str().unwrap_or_default().parse().unwrap_or_default();
        if to != ATLAS {
            println!("{}", json!({"tx": h, "block": block, "atlas": false, "market": market, "n": n, "gross": gross, "liquidator": liquidator, "eoa": tx["from"]}));
            continue;
        }
        let input: Bytes = tx["input"].as_str().unwrap_or("0x").parse()?;
        let mc = metacallCall::abi_decode(&input)?;
        let rc = rpc(&http, &url, "eth_getTransactionReceipt", json!([h])).await?;
        let mut results = vec![];
        for l in rc["logs"].as_array().into_iter().flatten() {
            let t: Vec<B256> = l["topics"].as_array().into_iter().flatten().filter_map(|x| x.as_str()?.parse().ok()).collect();
            if t.first() == Some(&SolverTxResult::SIGNATURE_HASH) {
                let data: Bytes = l["data"].as_str().unwrap_or("0x").parse()?;
                if let Ok(e) = SolverTxResult::decode_raw_log(t.iter().copied(), &data) {
                    results.push(json!({"solver": e.solverTo, "bid_eth": e.bidAmount.to_string().parse::<f64>().unwrap() / 1e18, "executed": e.executed, "success": e.success}));
                    if e.success {
                        winners_all.insert(e.solverTo);
                    }
                }
            }
        }
        let uoh = mc.solverOps.first().map(|s| s.userOpHash).unwrap_or_default();
        let ours = auctions.get(&uoh);
        let eth_usd = ours.and_then(|a| a["eth_usd"].as_f64()).unwrap_or(0.0);
        let bids: Vec<Value> = mc.solverOps.iter().map(|s| {
            bidders_all.insert(s.solver);
            json!({"solver": s.solver, "bid_eth": s.bidAmount.to_string().parse::<f64>().unwrap() / 1e18})
        }).collect();
        println!("{}", json!({"tx": h, "block": block, "atlas": true, "market": market, "n": n, "gross": gross, "solver_ops": bids, "results": results, "eth_usd": eth_usd,
            "ours": ours.map(|a| json!({"auction": a["auction_id"], "liquidatable": a["liquidatable"], "bonus_usd": a["bonus_usd"], "net_usd": a["net_usd"], "bid_usd": a["bid_usd"]}))}));
    }
    eprintln!("distinct bidding solver contracts: {}  distinct winners: {}", bidders_all.len(), winners_all.len());
    eprintln!("bidders: {bidders_all:?}");
    Ok(())
}
