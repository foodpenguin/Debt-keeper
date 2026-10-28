//! Dump the first messages of a Flashblocks WebSocket stream (format discovery).
//! cargo run --release --example fb_probe -- wss://mainnet.flashblocks.base.org/ws
use futures::StreamExt;
use std::io::Read;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args().nth(1).unwrap_or_else(|| "wss://mainnet.flashblocks.base.org/ws".into());
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let t0 = std::time::Instant::now();
    for i in 0..12 {
        let Some(msg) = ws.next().await else { break };
        let msg = msg?;
        let ms = t0.elapsed().as_millis();
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => println!("#{i} +{ms}ms TEXT {} bytes: {}", t.len(), &t[..t.len().min(300)]),
            tokio_tungstenite::tungstenite::Message::Binary(b) => {
                let mut out = String::new();
                let dec = brotli::Decompressor::new(&b[..], 4096).read_to_string(&mut out);
                match dec {
                    Ok(_) => println!("#{i} +{ms}ms BINARY {} bytes (brotli -> {} bytes): {}", b.len(), out.len(), &out[..out.len().min(400)]),
                    Err(e) => println!("#{i} +{ms}ms BINARY {} bytes, not brotli ({e}): {:02x?}", b.len(), &b[..b.len().min(32)]),
                }
            }
            other => println!("#{i} +{ms}ms {other:?}"),
        }
    }
    Ok(())
}
