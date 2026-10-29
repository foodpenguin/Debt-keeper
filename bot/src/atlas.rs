//! Chainlink SVR / Atlas solver auctions, OBSERVE ONLY (solver phase 0): subscribes to the public
//! solver feed and forwards Base auctions for our DappControl into the flashblock event channel.
//! Nothing is ever submitted; the main loop values each auction (what we could have bid) and
//! journals it for comparison with the winning on-chain bid.
//!
//! Feed (docs.chain.link/data-feeds/svr-feeds/searcher-onboarding-atlas; checked 2026-10-18):
//! `solver_subscribe ["userOperations"]` -> `solver_subscription` notifications whose
//! `partial_user_operation` carries chainId, control, deadline (block), maxFeePerGas, userOpHash
//! and `hints { aggregator, medianPrice, rawReport, forwardData }`. All chains share the feed.

use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::flashblocks::FbEvent;

/// One SVR auction on our chain.
#[derive(Debug, Clone)]
pub struct Auction {
    pub id: String,
    pub aggregator: Address,
    pub median: U256,
    /// Last block the bundle may land in.
    pub deadline: u64,
    pub max_fee_per_gas: u128,
    pub user_op_hash: B256,
    pub seen: Instant,
}

#[derive(Deserialize)]
struct Notification {
    params: Option<NotifParams>,
}
#[derive(Deserialize)]
struct NotifParams {
    result: NotifResult,
}
#[derive(Deserialize)]
struct NotifResult {
    auction_id: String,
    partial_user_operation: PartialUserOp,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialUserOp {
    chain_id: String,
    control: Address,
    deadline: String,
    max_fee_per_gas: String,
    user_op_hash: B256,
    hints: Hints,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Hints {
    aggregator: Address,
    median_price: String,
}

fn hex_u128(s: &str) -> Option<u128> {
    u128::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

/// Our-chain auction from one feed message, or None (other chains/controls, acks, garbage).
pub fn parse(text: &str, chain_id: u64, control: Address, seen: Instant) -> Option<Auction> {
    let n: Notification = serde_json::from_str(text).ok()?;
    let r = n.params?.result;
    let op = r.partial_user_operation;
    if hex_u128(&op.chain_id)? != chain_id as u128 || op.control != control {
        return None;
    }
    Some(Auction {
        id: r.auction_id,
        aggregator: op.hints.aggregator,
        median: U256::from_str_radix(op.hints.median_price.trim_start_matches("0x"), 16).ok()?,
        deadline: hex_u128(&op.deadline)? as u64,
        max_fee_per_gas: hex_u128(&op.max_fee_per_gas)?,
        user_op_hash: op.user_op_hash,
        seen,
    })
}

/// Connects (and reconnects) in the background; our auctions go into `tx`.
pub fn spawn(url: String, chain_id: u64, control: Address, tx: mpsc::Sender<FbEvent>) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match tokio_tungstenite::connect_async(url.as_str()).await {
                Ok((mut ws, _)) => {
                    let sub = r#"{"jsonrpc":"2.0","id":1,"method":"solver_subscribe","params":["userOperations"]}"#;
                    if ws.send(Message::Text(sub.into())).await.is_err() {
                        continue;
                    }
                    info!("atlas auction feed subscribed (observe only)");
                    backoff = Duration::from_secs(1);
                    // Auctions on all chains arrive every few seconds: a minute of silence is a dead socket.
                    while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(60), ws.next()).await {
                        match msg {
                            Ok(Message::Text(t)) => {
                                if let Some(a) = parse(&t, chain_id, control, Instant::now())
                                    && tx.try_send(FbEvent::Auction(a)).is_err()
                                {
                                    warn!("flashblock event channel full; auction dropped");
                                }
                            }
                            Ok(Message::Ping(p)) => {
                                let _ = ws.send(Message::Pong(p)).await;
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    warn!("atlas auction feed stalled or closed; reconnecting");
                }
                Err(e) => warn!(error = %e, backoff_s = backoff.as_secs(), "atlas auction feed connect failed"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const BASE_CONTROL: Address = address!("a5E1a36938769cbd5a26f5e19D8FCB379f597c83");

    fn msg(chain: &str, control: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"solver_subscription","params":{{"result":{{"auction_id":"e1c063a1-04f9-4e91-944c-a57cd9b12ca8","partial_user_operation":{{"chainId":"{chain}","control":"{control}","dapp":"{control}","deadline":"0x324c1a0","from":"0x0000000000000000000000000000000000000000","gas":"0x7a120","hints":{{"aggregator":"0xfa9ebD1FC381C5Babb495BB53664ae9B6665E4E4","forwardData":"0x6fadcf72","medianPrice":"0x79eb09b","rawReport":"0x00"}},"maxFeePerGas":"0x29b92700","to":"0x583dcfef0d240dc80753f0f0b26513fee27d9b77","userOpHash":"0xea7f2e8a47878e899dc463f7e6953e704faffe313bae85d242fbec9d29d14990"}}}},"subscription":"dbc15101"}}}}"#
        )
    }

    #[test]
    fn keeps_only_our_chain_and_control() {
        let t = Instant::now();
        let a = parse(&msg("0x2105", "0xa5e1a36938769cbd5a26f5e19d8fcb379f597c83"), 8453, BASE_CONTROL, t).expect("base auction");
        assert_eq!(a.aggregator, address!("fa9ebD1FC381C5Babb495BB53664ae9B6665E4E4"));
        assert_eq!(a.median, U256::from(127_840_411u64));
        assert_eq!((a.deadline, a.max_fee_per_gas), (0x324c1a0, 0x29b92700));
        // Arbitrum (0xa4b1) or another control: ignored. Subscription ack: ignored.
        assert!(parse(&msg("0xa4b1", "0xa5e1a36938769cbd5a26f5e19d8fcb379f597c83"), 8453, BASE_CONTROL, t).is_none());
        assert!(parse(&msg("0x2105", "0xe15bba987c002ecc3586e81244517877d294d291"), 8453, BASE_CONTROL, t).is_none());
        assert!(parse(r#"{"jsonrpc":"2.0","result":"dbc15101","id":1}"#, 8453, BASE_CONTROL, t).is_none());
    }
}
