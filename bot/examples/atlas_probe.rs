//! Read-only look at the Chainlink SVR / Atlas solver auction feed: subscribe, dump messages.
//! Never submits anything.  cargo run --release --example atlas_probe -- <seconds> <out.jsonl>
use futures::{SinkExt, StreamExt};
use std::io::Write;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let secs: u64 = a.next().map(|s| s.parse().unwrap()).unwrap_or(60);
    let out = a.next().unwrap_or_else(|| "atlas_probe.jsonl".into());
    let (mut ws, resp) = tokio_tungstenite::connect_async("wss://svr-bid-endpoint.chain.link/ws/solver").await?;
    eprintln!("connected: HTTP {}", resp.status());
    ws.send(Message::Text(r#"{"jsonrpc":"2.0","id":1,"method":"solver_subscribe","params":["userOperations"]}"#.into())).await?;
    let mut f = std::fs::File::create(out)?;
    let t0 = std::time::Instant::now();
    let mut n = 0;
    while t0.elapsed().as_secs() < secs {
        let Ok(Some(m)) = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await else { continue };
        match m? {
            Message::Text(t) => {
                n += 1;
                writeln!(f, "{{\"t_ms\":{},\"msg\":{}}}", t0.elapsed().as_millis(), t)?;
                if n <= 3 {
                    eprintln!("msg #{n} ({} bytes): {}", t.len(), &t[..t.len().min(600)]);
                }
            }
            Message::Ping(p) => ws.send(Message::Pong(p)).await?,
            other => eprintln!("other: {other:?}"),
        }
    }
    eprintln!("{n} text messages in {secs}s");
    Ok(())
}
