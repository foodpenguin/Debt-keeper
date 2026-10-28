//! Flashblock logs_bloom: bits set per flashblock, cumulative or not, and whether the Aero pools
//! show up. Read-only.
use alloy::primitives::{Bloom, BloomInput, address};
use futures::StreamExt;
use std::io::Read;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pools = [address!("422df9aa896e883c9cdcbad309c0785b7364a1cf"), address!("a4fdd479eda160671636e2ecf8f993cbf86258a8"), address!("c85d5cb72d1aad6f152a8decad228ae4a59ac9b3"), address!("d0acf293547457fb4fe9ee32ca93e89497626816")];
    let random = address!("1234567890abcdef1234567890abcdef12345678");
    let (mut ws, _) = tokio_tungstenite::connect_async("wss://mainnet.flashblocks.base.org/ws").await?;
    let mut prev = Bloom::default();
    let mut n = 0;
    while let Some(Ok(m)) = ws.next().await {
        let tokio_tungstenite::tungstenite::Message::Binary(b) = m else { continue };
        let mut s = String::new();
        brotli::Decompressor::new(&b[..], 4096).read_to_string(&mut s)?;
        let v: serde_json::Value = serde_json::from_str(&s)?;
        let bloom: Bloom = v["diff"]["logs_bloom"].as_str().unwrap_or("0x").parse().unwrap_or_default();
        let bits: u32 = bloom.0.iter().map(|x| x.count_ones()).sum();
        let superset = (bloom | prev) == bloom;
        let hits: Vec<bool> = pools.iter().map(|p| bloom.contains_input(BloomInput::Raw(p.as_slice()))).collect();
        println!("{}.{} txs={} bits={bits}/2048 superset_of_prev={superset} pools={hits:?} random={}", v["metadata"]["block_number"], v["index"], v["diff"]["transactions"].as_array().map_or(0, |a| a.len()), bloom.contains_input(BloomInput::Raw(random.as_slice())));
        prev = bloom;
        n += 1;
        if n >= 25 { break; }
    }
    Ok(())
}
