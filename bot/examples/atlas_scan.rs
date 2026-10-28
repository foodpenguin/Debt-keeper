//! Decode Fastlane Atlas `metacall` transactions (Chainlink SVR auctions on Base): every solver op
//! offered (solver, EOA, bid), the on-chain result of each attempted op, bundler payment and gas,
//! plus the ETH/USD price at that block. One JSON line per tx.
//!   cargo run --release --example atlas_scan -- <tx-hashes.txt> > atlas.jsonl
//! RPC: ATLAS_RPC (default Tenderly public gateway).
use alloy::{
    primitives::{Address, B256, Bytes, U256},
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
    event MetacallResult(address indexed bundler, address indexed user, bool solverSuccessful, uint256 ethPaidToBundler, uint256 netGasSurcharge);
    function latestRoundData() returns (uint80, int256, uint256, uint256, uint80);
}

const ETH_USD_FEED: &str = "0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70";

async fn rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    for attempt in 0..4 {
        let r: Value = http.post(url).json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).send().await?.json().await?;
        if let Some(v) = r.get("result") {
            return Ok(v.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt + 1))).await;
    }
    anyhow::bail!("{method} failed")
}

fn u(v: &Value) -> u128 {
    v.as_str().and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("tx hash file");
    let url = std::env::var("ATLAS_RPC").unwrap_or_else(|_| "https://base.gateway.tenderly.co".into());
    let http = reqwest::Client::new();
    let hashes: Vec<String> = std::fs::read_to_string(path)?.lines().map(|l| l.split_whitespace().next().unwrap_or("").to_string()).filter(|s| s.starts_with("0x")).collect();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
    let mut tasks = vec![];
    for h in hashes {
        let (http, url, sem) = (http.clone(), url.clone(), sem.clone());
        tasks.push(tokio::spawn(async move {
            let _p = sem.acquire().await;
            let tx = rpc(&http, &url, "eth_getTransactionByHash", json!([h])).await?;
            let rc = rpc(&http, &url, "eth_getTransactionReceipt", json!([h])).await?;
            let block = u(&tx["blockNumber"]) as u64;
            let input: Bytes = tx["input"].as_str().unwrap_or("0x").parse()?;
            let call = metacallCall::abi_decode(&input)?;
            let ops: Vec<Value> = call
                .solverOps
                .iter()
                .map(|s| json!({"from": s.from, "solver": s.solver, "bidToken": s.bidToken, "bid": s.bidAmount.to_string(), "gas": s.gas.to_string(), "maxFeePerGas": s.maxFeePerGas.to_string()}))
                .collect();
            let mut results = vec![];
            let mut bundler_paid = U256::ZERO;
            for log in rc["logs"].as_array().into_iter().flatten() {
                let topics: Vec<B256> = log["topics"].as_array().into_iter().flatten().filter_map(|t| t.as_str()?.parse().ok()).collect();
                let data: Bytes = log["data"].as_str().unwrap_or("0x").parse()?;
                if topics.first() == Some(&SolverTxResult::SIGNATURE_HASH) {
                    let e = SolverTxResult::decode_raw_log(topics.iter().copied(), &data)?;
                    results.push(json!({"solver": e.solverTo, "from": e.solverFrom, "bid": e.bidAmount.to_string(), "executed": e.executed, "success": e.success, "result": e.result.to_string()}));
                } else if topics.first() == Some(&MetacallResult::SIGNATURE_HASH) {
                    bundler_paid = MetacallResult::decode_raw_log(topics.iter().copied(), &data)?.ethPaidToBundler;
                }
            }
            let px = rpc(&http, &url, "eth_call", json!([{"to": ETH_USD_FEED, "data": Bytes::from(latestRoundDataCall {}.abi_encode())}, format!("0x{block:x}")])).await?;
            let px: Bytes = px.as_str().unwrap_or("0x").parse()?;
            let eth_usd = latestRoundDataCall::abi_decode_returns(&px).map(|r| r._1.as_i64() as f64 / 1e8).unwrap_or(0.0);
            anyhow::Ok(json!({
                "tx": h, "block": block, "index": u(&tx["transactionIndex"]), "bundler": tx["from"], "user": call.userOp.from,
                "control": call.userOp.control, "dapp": call.userOp.dapp, "n_ops": ops.len(), "ops": ops, "results": results,
                "bundler_paid_wei": bundler_paid.to_string(), "gas_used": u(&rc["gasUsed"]), "eff_gas_price": u(&rc["effectiveGasPrice"]),
                "l1_fee": u(&rc["l1Fee"]), "eth_usd": eth_usd, "gas_refund_to": call.gasRefundBeneficiary,
            }))
        }));
    }
    for t in tasks {
        match t.await? {
            Ok(v) => println!("{v}"),
            Err(e) => eprintln!("skip: {e}"),
        }
    }
    let _ = Address::ZERO;
    Ok(())
}
