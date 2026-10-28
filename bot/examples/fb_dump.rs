//! Save N decoded Flashblocks messages (one JSON per line) for offline inspection / test fixtures.
//! cargo run --release --example fb_dump -- <out.jsonl> [n] [url]
use futures::StreamExt;
use std::io::{Read, Write};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("out path");
    let n: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(30);
    let url = args.next().unwrap_or_else(|| "wss://mainnet.flashblocks.base.org/ws".into());
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let mut f = std::fs::File::create(out)?;
    let mut got = 0;
    while got < n {
        let Some(msg) = ws.next().await else { break };
        if let tokio_tungstenite::tungstenite::Message::Binary(b) = msg? {
            let mut s = String::new();
            brotli::Decompressor::new(&b[..], 4096).read_to_string(&mut s)?;
            writeln!(f, "{s}")?;
            got += 1;
        }
    }
    Ok(())
}
