//! Flashblock freshness bench: how much later than the official WebSocket stream does each RPC
//! expose the same flashblock in its "pending" block? Also times eth_call / eth_simulateV1 on
//! "pending". Reads endpoint URLs from ../.env (names only are printed).
//!
//!   cargo run --release --example fb_bench -- [seconds] [poll_ms]
use std::{
    collections::HashMap,
    io::Read,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures::StreamExt;
use serde_json::{Value, json};

const WS: &str = "wss://mainnet.flashblocks.base.org/ws";
/// Flashblocks seen over WS: (block, index, cumulative tx count, arrival ms).
type Seen = Arc<Mutex<Vec<(u64, u64, usize, i64)>>>;

fn env_urls() -> Vec<(String, String)> {
    let text = std::fs::read_to_string("../.env").unwrap_or_default();
    let vars: HashMap<String, String> = text
        .lines()
        .filter_map(|l| l.trim().split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect();
    let mut out = vec![];
    for (name, var) in [("alchemy", "BASE_RPC_4"), ("drpc", "BASE_RPC_1"), ("ankr", "BASE_RPC_3"), ("infura", "BASE_RPC_2")] {
        if let Some(u) = vars.get(var) {
            out.push((name.to_string(), u.clone()));
        }
    }
    out.push(("tenderly".into(), "https://base.gateway.tenderly.co".into()));
    out.push(("base-preconf".into(), "https://mainnet-preconf.base.org".into()));
    out
}

async fn rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> Option<Value> {
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
    let r: Value = http.post(url).json(&body).send().await.ok()?.json().await.ok()?;
    r.get("result").cloned()
}

fn hexnum(v: &Value) -> u64 {
    v.as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0)
}

fn pct(mut v: Vec<i64>, p: f64) -> i64 {
    if v.is_empty() {
        return 0;
    }
    v.sort();
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let secs: u64 = args.next().map(|s| s.parse().unwrap()).unwrap_or(60);
    let poll_ms: u64 = args.next().map(|s| s.parse().unwrap()).unwrap_or(50);
    let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(secs);

    let ws_seen: Seen = Arc::default();
    let ws_task = {
        let ws_seen = ws_seen.clone();
        tokio::spawn(async move {
            let (mut ws, _) = tokio_tungstenite::connect_async(WS).await.expect("ws connect");
            let mut cum: HashMap<u64, usize> = HashMap::new();
            let mut block_of: HashMap<String, u64> = HashMap::new();
            while Instant::now() < deadline {
                let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(3), ws.next()).await else { break };
                let tokio_tungstenite::tungstenite::Message::Binary(b) = msg else { continue };
                let ms = t0.elapsed().as_millis() as i64;
                let mut s = String::new();
                if brotli::Decompressor::new(&b[..], 4096).read_to_string(&mut s).is_err() {
                    continue;
                }
                let v: Value = serde_json::from_str(&s).unwrap_or_default();
                let pid = v["payload_id"].as_str().unwrap_or("").to_string();
                let idx = v["index"].as_u64().unwrap_or(0);
                if let Some(bn) = v["base"]["block_number"].as_str() {
                    block_of.insert(pid.clone(), u64::from_str_radix(bn.trim_start_matches("0x"), 16).unwrap_or(0));
                }
                let block = v["metadata"]["block_number"].as_u64().or_else(|| block_of.get(&pid).copied()).unwrap_or(0);
                let n = v["diff"]["transactions"].as_array().map_or(0, |a| a.len());
                let c = cum.entry(block).or_default();
                *c += n;
                ws_seen.lock().unwrap().push((block, idx, *c, ms));
            }
        })
    };

    // RPC pollers: name -> [(pending block number, tx count, ms)]
    let urls = env_urls();
    let mut polls = vec![];
    for (name, url) in urls.clone() {
        let http = http.clone();
        polls.push(tokio::spawn(async move {
            let mut samples = vec![];
            let mut errors = 0u32;
            while Instant::now() < deadline {
                let start = Instant::now();
                match rpc(&http, &url, "eth_getBlockByNumber", json!(["pending", false])).await {
                    Some(b) => samples.push((hexnum(&b["number"]), b["transactions"].as_array().map_or(0, |a| a.len()), t0.elapsed().as_millis() as i64)),
                    None => errors += 1,
                }
                let spent = start.elapsed();
                if spent < Duration::from_millis(poll_ms) {
                    tokio::time::sleep(Duration::from_millis(poll_ms) - spent).await;
                }
            }
            (name, samples, errors)
        }));
    }
    ws_task.await?;
    let ws_seen = ws_seen.lock().unwrap().clone();
    println!("ws flashblocks seen: {} over {secs}s", ws_seen.len());
    println!("{:<13} {:>7} {:>6} | lag vs WS (ms): {:>5} {:>5} {:>5} | seen   | errors", "endpoint", "polls", "rt_ms", "p10", "p50", "p90");
    for p in polls {
        let (name, samples, errors) = p.await?;
        let rt = if samples.len() > 1 { (secs * 1000) as i64 / samples.len() as i64 } else { 0 };
        let mut lags = vec![];
        let mut missed = 0;
        for &(block, idx, c, t_ws) in &ws_seen {
            if idx == 0 || c == 0 {
                continue;
            }
            match samples.iter().find(|s| s.0 == block && s.1 >= c) {
                Some(s) => lags.push(s.2 - t_ws),
                None => missed += 1,
            }
        }
        let seen = lags.len();
        println!(
            "{:<13} {:>7} {:>6} | {:>21} {:>5} {:>5} {:>5} | {:>3}/{:<3} | {}",
            name,
            samples.len(),
            rt,
            "",
            pct(lags.clone(), 0.1),
            pct(lags.clone(), 0.5),
            pct(lags, 0.9),
            seen,
            seen + missed,
            errors
        );
    }

    // eth_call and eth_simulateV1 on "pending": Morpho market(WETH/USDC id)
    let call = json!([{"to":"0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb","data":"0x5c60e39a8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda"},"pending"]);
    let sim = json!([{"blockStateCalls":[{"calls":[{"to":"0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb","data":"0x5c60e39a8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda"}]}]},"pending"]);
    println!("\n{:<13} {:>18} {:>22}", "endpoint", "eth_call pending", "eth_simulateV1 pending");
    for (name, url) in urls {
        let mut c_ms = vec![];
        let mut s_ms = vec![];
        let (mut c_ok, mut s_ok) = (true, true);
        for _ in 0..5 {
            let t = Instant::now();
            c_ok &= rpc(&http, &url, "eth_call", call.clone()).await.is_some();
            c_ms.push(t.elapsed().as_millis() as i64);
            let t = Instant::now();
            s_ok &= rpc(&http, &url, "eth_simulateV1", sim.clone()).await.is_some();
            s_ms.push(t.elapsed().as_millis() as i64);
        }
        let f = |ok: bool, v: Vec<i64>| if ok { format!("ok p50 {} ms", pct(v, 0.5)) } else { "FAIL".into() };
        println!("{:<13} {:>18} {:>22}", name, f(c_ok, c_ms), f(s_ok, s_ms));
    }
    Ok(())
}
