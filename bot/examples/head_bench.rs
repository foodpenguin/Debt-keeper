//! New-block signal race: WebSocket newHeads (per endpoint) vs the Flashblocks stream, where
//! index 0 of block N+1 means block N is sealed. Prints arrival lag vs the fastest source.
//!   cargo run --release --example head_bench -- [seconds]
use std::{
    collections::HashMap,
    io::Read,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use futures::StreamExt;

type Arrivals = Arc<Mutex<HashMap<(String, u64), Instant>>>;

fn env_var(name: &str) -> Option<String> {
    let text = std::fs::read_to_string("../.env").ok()?;
    text.lines().filter_map(|l| l.trim().split_once('=')).find(|(k, _)| k.trim() == name).map(|(_, v)| v.trim().to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let secs: u64 = std::env::args().nth(1).map(|s| s.parse().unwrap()).unwrap_or(60);
    let arr: Arrivals = Arc::default();
    let mut ws = vec![("tenderly".to_string(), "wss://base.gateway.tenderly.co".to_string())];
    if let Some(u) = env_var("BASE_RPC_4") {
        ws.push(("alchemy".into(), u.replacen("https://", "wss://", 1)));
    }
    if let Some(u) = env_var("BASE_RPC_1") {
        ws.push(("drpc".into(), u.replacen("https://", "wss://", 1)));
    }
    for (name, url) in ws {
        let arr = arr.clone();
        tokio::spawn(async move {
            let Ok(p) = ProviderBuilder::new().connect_ws(WsConnect::new(url)).await else { return eprintln!("{name}: connect failed") };
            let Ok(sub) = p.subscribe_blocks().await else { return eprintln!("{name}: subscribe failed") };
            let mut s = sub.into_stream();
            while let Some(h) = s.next().await {
                arr.lock().unwrap().entry((name.clone(), h.number)).or_insert_with(Instant::now);
            }
        });
    }
    {
        let arr = arr.clone();
        tokio::spawn(async move {
            let (mut s, _) = tokio_tungstenite::connect_async("wss://mainnet.flashblocks.base.org/ws").await.unwrap();
            while let Some(Ok(m)) = s.next().await {
                let tokio_tungstenite::tungstenite::Message::Binary(b) = m else { continue };
                let now = Instant::now();
                let mut txt = String::new();
                if brotli::Decompressor::new(&b[..], 4096).read_to_string(&mut txt).is_err() {
                    continue;
                }
                let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or_default();
                if v["index"].as_u64() == Some(0)
                    && let Some(bn) = v["base"]["block_number"].as_str()
                {
                    let n = u64::from_str_radix(bn.trim_start_matches("0x"), 16).unwrap_or(0);
                    // index 0 of N opens block N: block N-1 is sealed
                    arr.lock().unwrap().entry(("fb-index0".into(), n - 1)).or_insert(now);
                }
            }
        });
    }
    tokio::time::sleep(Duration::from_secs(secs)).await;
    let arr = arr.lock().unwrap().clone();
    let mut by_block: HashMap<u64, Vec<(String, Instant)>> = HashMap::new();
    for ((src, n), t) in arr {
        by_block.entry(n).or_default().push((src, t));
    }
    let mut lag: HashMap<String, Vec<i64>> = HashMap::new();
    let mut wins: HashMap<String, u32> = HashMap::new();
    for (_, v) in by_block.iter().filter(|(_, v)| v.len() >= 3) {
        let first = v.iter().map(|x| x.1).min().unwrap();
        for (src, t) in v {
            lag.entry(src.clone()).or_default().push(t.duration_since(first).as_millis() as i64);
            if *t == first {
                *wins.entry(src.clone()).or_default() += 1;
            }
        }
    }
    println!("{:<12} {:>6} {:>8} {:>8} {:>8}", "source", "wins", "lag p50", "lag p90", "n");
    for (src, mut v) in lag {
        v.sort();
        println!("{:<12} {:>6} {:>6}ms {:>6}ms {:>8}", src, wins.get(&src).unwrap_or(&0), v[v.len() / 2], v[(v.len() * 9) / 10], v.len());
    }
    Ok(())
}
