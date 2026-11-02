//! Read-only probe of how a Base Denim node (200 ms blocks, no Flashblocks or pending state) answers
//! what the bot relies on: the block time, the "pending" tag (nonce and block), newHeads and a logs
//! subscription. Run it against Base Sepolia (Denim since 2026-10-21) before a mainnet fork:
//!
//!   cargo run --example denim_probe -- --http https://sepolia.base.org --ws wss://sepolia.base.org
//!
//! Sends nothing. The nonce of a wallet with txs in flight can be checked by hand with
//! `cast nonce <addr> --block pending` while a transfer is pending.

use std::time::{Duration, Instant};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::Address,
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::Filter,
};
use anyhow::{Context, Result};
use futures::StreamExt;

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() -> Result<()> {
    let http = arg("--http").context("--http <url>")?;
    let ws = arg("--ws");
    let who: Address = arg("--address").map(|a| a.parse()).transpose()?.unwrap_or(Address::with_last_byte(0x11));
    let secs: u64 = arg("--secs").map(|s| s.parse()).transpose()?.unwrap_or(30);
    let p = ProviderBuilder::new().connect_http(http.parse()?);

    println!("chain_id: {}", p.get_chain_id().await?);
    let latest = p.get_block_by_number(BlockNumberOrTag::Latest).await?.context("latest block")?;
    let back = p.get_block_by_number(BlockNumberOrTag::Number(latest.header.number - 500)).await?.context("block -500")?;
    println!("block_time_s (500 blocks): {:.3}", (latest.header.timestamp - back.header.timestamp) as f64 / 500.0);

    match p.get_block_by_number(BlockNumberOrTag::Pending).await {
        Ok(Some(b)) => println!("pending block: number {} (latest {}), {}", b.header.number, latest.header.number, if b.header.number <= latest.header.number + 1 { "no pending state beyond the next block" } else { "ahead" }),
        Ok(None) => println!("pending block: none"),
        Err(e) => println!("pending block: refused ({e})"),
    }
    let latest_nonce = p.get_transaction_count(who).await?;
    match p.get_transaction_count(who).pending().await {
        Ok(n) => println!("pending nonce of {who}: {n} (latest {latest_nonce}){}", if n == latest_nonce { ": equal (executor: cancel_stuck never fires, keep max_inflight = 1)" } else { "" }),
        Err(e) => println!("pending nonce of {who}: refused ({e}): the executor falls back to latest"),
    }

    let Some(ws) = ws else {
        println!("no --ws: subscriptions not probed");
        return Ok(());
    };
    let wp = ProviderBuilder::new().connect_ws(WsConnect::new(ws)).await.context("ws connect")?;
    let mut heads = wp.subscribe_blocks().await?.into_stream();
    let mut logs = wp.subscribe_logs(&Filter::new()).await?.into_stream();
    let (mut gaps, mut last, mut n_logs, mut removed, mut with_ts, mut log_delay_ms) = (vec![], None::<Instant>, 0u64, 0u64, 0u64, vec![]);
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        tokio::select! {
            h = heads.next() => {
                let Some(_) = h else { break };
                let now = Instant::now();
                if let Some(l) = last {
                    gaps.push(now.duration_since(l).as_millis() as u64);
                }
                last = Some(now);
            }
            l = logs.next() => {
                let Some(l) = l else { break };
                n_logs += 1;
                removed += l.removed as u64;
                if let Some(ts) = l.block_timestamp {
                    with_ts += 1;
                    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
                    log_delay_ms.push(now_ms - ts as i64 * 1000);
                }
            }
            _ = tokio::time::sleep_until(end.into()) => break,
        }
    }
    gaps.sort_unstable();
    log_delay_ms.sort_unstable();
    let p50 = |v: &Vec<u64>| v.get(v.len() / 2).copied();
    println!("newHeads: {} heads in {secs} s, interval p50 {:?} ms", gaps.len() + 1, p50(&gaps));
    println!("logs: {n_logs} in {secs} s, removed {removed}, with blockTimestamp {with_ts}, arrival - block time p50 {:?} ms (includes this host's clock offset)", log_delay_ms.get(log_delay_ms.len() / 2));
    Ok(())
}
