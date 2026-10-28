//! Can an RPC return the receipt of a tx that so far exists only in a flashblock?
//! Takes the first tx of each new flashblock and asks each endpoint for its receipt at once.
use futures::StreamExt;
use std::io::Read;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let text = std::fs::read_to_string("../.env").unwrap_or_default();
    let alchemy = text.lines().find_map(|l| l.strip_prefix("BASE_RPC_4=")).unwrap().to_string();
    let eps = [("alchemy", alchemy), ("base-preconf", "https://mainnet-preconf.base.org".to_string()), ("tenderly", "https://base.gateway.tenderly.co".to_string())];
    let http = reqwest::Client::new();
    let (mut ws, _) = tokio_tungstenite::connect_async("wss://mainnet.flashblocks.base.org/ws").await?;
    let mut n = 0;
    while let Some(Ok(m)) = ws.next().await {
        let tokio_tungstenite::tungstenite::Message::Binary(b) = m else { continue };
        let mut s = String::new();
        brotli::Decompressor::new(&b[..], 4096).read_to_string(&mut s)?;
        let v: serde_json::Value = serde_json::from_str(&s)?;
        let Some(raw) = v["diff"]["transactions"].as_array().and_then(|a| a.last()).and_then(|t| t.as_str()) else { continue };
        let raw = alloy::hex::decode(raw)?;
        let h = alloy::primitives::keccak256(&raw);
        let t0 = std::time::Instant::now();
        let mut line = format!("fb {} tx {}:", v["index"], &h.to_string()[..10]);
        for (name, url) in &eps {
            let r: serde_json::Value = http.post(url).json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_getTransactionReceipt","params":[h]})).send().await?.json().await?;
            let st = r["result"]["status"].as_str().unwrap_or("none");
            line += &format!(" {name}={st}@{}ms", t0.elapsed().as_millis());
        }
        println!("{line}");
        n += 1;
        if n >= 8 { break; }
    }
    Ok(())
}
