//! Solver phase 0 scoring: for each `atlas_auction` journal row, find the on-chain Atlas metacall
//! with the same userOpHash and compare our hypothetical bid with the winning one.
//!   cargo run --release --example atlas_match -- <journal.jsonl> > matches.jsonl
//! RPC: MATCH_RPC (default mainnet.base.org, which the bot itself does not use); ~2 req/s.
use alloy::{
    primitives::{Address, B256, Bytes, U256, address, b256},
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
const LIQUIDATE: B256 = b256!("a4946ede45d0c6f06a0f5ce92c9ad3b4751452d2fe0e25010783bcab57a67e41");

async fn rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    for attempt in 0..6 {
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
        if let Ok(r) = http.post(url).json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).send().await {
            if let Ok(v) = r.json::<Value>().await {
                if let Some(x) = v.get("result") {
                    return Ok(x.clone());
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(800 * (attempt + 1))).await;
    }
    anyhow::bail!("{method} failed")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("journal path");
    let url = std::env::var("MATCH_RPC").unwrap_or_else(|_| "https://mainnet.base.org".into());
    let http = reqwest::Client::new();
    for line in std::fs::read_to_string(path)?.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else { continue };
        if row["kind"] != "atlas_auction" {
            continue;
        }
        let want: B256 = row["user_op_hash"].as_str().unwrap_or_default().parse()?;
        let (from, to) = (row["head"].as_u64().unwrap_or(0), row["deadline"].as_u64().unwrap_or(0));
        let eth_usd = row["eth_usd"].as_f64().unwrap_or(0.0);
        let logs = rpc(&http, &url, "eth_getLogs", json!([{"address": ATLAS, "fromBlock": format!("0x{from:x}"), "toBlock": format!("0x{:x}", to.max(from) + 2)}])).await?;
        let mut hashes: Vec<String> = logs.as_array().into_iter().flatten().filter_map(|l| l["transactionHash"].as_str().map(String::from)).collect();
        hashes.dedup();
        // bid_basis "bundle_net": bid from the one-exit bundle quote; absent (older journals): from the
        // per-position sum, which overstates what one solver op can net.
        let ours = json!({ "bid_usd": row["bid_usd"], "bid_basis": row["bid_basis"].as_str().unwrap_or("per_position_sum"), "bundle_net_usd": row["bundle_net_usd"], "net_usd": row["net_usd"], "liquidatable": row["liquidatable"] });
        let mut out = json!({ "auction_id": row["auction_id"], "ours": ours, "found": false });
        for h in hashes {
            let tx = rpc(&http, &url, "eth_getTransactionByHash", json!([h])).await?;
            let input: Bytes = tx["input"].as_str().unwrap_or("0x").parse()?;
            let Ok(mc) = metacallCall::abi_decode(&input) else { continue };
            if !mc.solverOps.iter().any(|s| s.userOpHash == want) {
                continue;
            }
            let rc = rpc(&http, &url, "eth_getTransactionReceipt", json!([h])).await?;
            let mut winner = json!(null);
            let mut liqs = 0;
            for l in rc["logs"].as_array().into_iter().flatten() {
                let t: Vec<B256> = l["topics"].as_array().into_iter().flatten().filter_map(|x| x.as_str()?.parse().ok()).collect();
                if t.first() == Some(&LIQUIDATE) {
                    liqs += 1;
                }
                if t.first() == Some(&SolverTxResult::SIGNATURE_HASH) {
                    let data: Bytes = l["data"].as_str().unwrap_or("0x").parse()?;
                    if let Ok(e) = SolverTxResult::decode_raw_log(t.iter().copied(), &data) {
                        if e.success && winner.is_null() {
                            let bid = e.bidAmount.to_string().parse::<f64>().unwrap_or(0.0) / 1e18;
                            winner = json!({ "solver": e.solverTo, "bid_eth": bid, "bid_usd": bid * eth_usd });
                        }
                    }
                }
            }
            let bids: Vec<f64> = mc.solverOps.iter().map(|s| s.bidAmount.to_string().parse::<f64>().unwrap_or(0.0) / 1e18 * eth_usd).collect();
            out = json!({ "auction_id": row["auction_id"], "tx": h, "block": tx["blockNumber"], "found": true, "ours": ours,
                          "winner": winner, "bids_usd": bids, "liquidations_in_tx": liqs });
            break;
        }
        println!("{out}");
    }
    let _ = U256::ZERO;
    Ok(())
}
