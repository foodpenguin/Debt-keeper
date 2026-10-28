//! How fresh is each endpoint's flashblock ("pending") state? On each flashblock the last tx's
//! receipt is asked from every endpoint right away (a flashblocks-aware node serves preconfirmed
//! receipts), plus eth_call getBlockNumber on "pending". Read-only.
//! Endpoints: PROBE_RPCS = "name=url,name=url" (urls are never printed).
use std::{io::Read, time::Instant};

use alloy::primitives::keccak256;
use futures::StreamExt;
use serde_json::{Value, json};

async fn rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> Option<Value> {
    let r = http.post(url).json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).send().await.ok()?;
    r.json::<Value>().await.ok()?.get("result").cloned()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let eps: Vec<(String, String)> = std::env::var("PROBE_RPCS")?.split(',').filter_map(|s| s.split_once('=').map(|(a, b)| (a.to_string(), b.to_string()))).collect();
    let http = reqwest::Client::builder().pool_idle_timeout(None).build()?;
    for (_, u) in &eps {
        rpc(&http, u, "eth_chainId", json!([])).await; // warm
    }
    let (mut ws, _) = tokio_tungstenite::connect_async("wss://mainnet.flashblocks.base.org/ws").await?;
    let mut stats: Vec<(u32, u32, u32, u128)> = vec![(0, 0, 0, 0); eps.len()]; // (asked, receipt found, pending block == fb block, ms sum)
    let mut n = 0;
    while let Some(Ok(m)) = ws.next().await {
        let tokio_tungstenite::tungstenite::Message::Binary(b) = m else { continue };
        let at = Instant::now();
        let mut s = String::new();
        brotli::Decompressor::new(&b[..], 4096).read_to_string(&mut s)?;
        let v: Value = serde_json::from_str(&s)?;
        let index = v["index"].as_u64().unwrap_or(0);
        let block = v["metadata"]["block_number"].as_u64().unwrap_or(0);
        let Some(last) = v["diff"]["transactions"].as_array().and_then(|t| t.last()).and_then(|t| t.as_str()) else { continue };
        if index == 0 {
            continue;
        }
        let hash = format!("{}", keccak256(alloy::hex::decode(last)?));
        let res = futures::future::join_all(eps.iter().map(|(_, u)| {
            let (http, hash) = (&http, hash.clone());
            async move {
                let t = Instant::now();
                let (rc, bn) = tokio::join!(
                    rpc(http, u, "eth_getTransactionReceipt", json!([hash])),
                    rpc(http, u, "eth_call", json!([{"to": "0xcA11bde05977b3631167028862bE2a173976CA11", "data": "0x42cbb15c"}, "pending"]))
                );
                let bn = bn.and_then(|x| x.as_str().map(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)));
                (rc.is_some_and(|r| !r.is_null()), bn, t.elapsed().as_millis())
            }
        }))
        .await;
        let line: Vec<String> = eps
            .iter()
            .zip(&res)
            .enumerate()
            .map(|(i, ((name, _), (found, bn, ms)))| {
                stats[i].0 += 1;
                stats[i].1 += u32::from(*found);
                stats[i].2 += u32::from(*bn == Some(block));
                stats[i].3 += ms;
                format!("{name}: receipt={} pending_block={:+} {ms}ms", if *found { "Y" } else { "-" }, bn.map_or(-99, |x| x as i64 - block as i64))
            })
            .collect();
        println!("fb {block}.{index} (+{}ms decode) {}", at.elapsed().as_millis(), line.join(" | "));
        n += 1;
        if n >= 40 {
            break;
        }
    }
    for (i, (name, _)) in eps.iter().enumerate() {
        let (a, f, p, ms) = stats[i];
        println!("{name}: receipt found {f}/{a}, pending == flashblock's block {p}/{a}, avg {} ms", ms / a.max(1) as u128);
    }
    Ok(())
}
