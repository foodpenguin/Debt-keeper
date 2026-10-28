//! Flashblocks stream (Base, until Denim): ~200 ms sub-blocks pushed over WebSocket as
//! brotli-compressed JSON. Message `index` 0 opens a block (`base`), 1..=10 add transactions
//! (`diff.transactions`, raw EIP-2718 bytes). There are no receipts in the stream, so oracle
//! updates are recognised from the transactions themselves: a Chainlink transmitter calls
//! `forward(aggregator, transmit(...))` on a forwarder (or `transmit` directly), and the new
//! answer is the median of the report's observations (checked equal to `AnswerUpdated`).
//!
//! Also records when each transaction first appeared in the stream, so that a real liquidation
//! can be placed in its flashblock and compared with when we would have been ready.

use std::{
    collections::{HashMap, HashSet},
    io::Read,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use alloy::{
    consensus::{Transaction, TxEnvelope, transaction::SignerRecoverable},
    eips::eip2718::Decodable2718,
    primitives::{Address, B256, Bytes, U256, keccak256},
    sol,
    sol_types::SolCall,
};
use anyhow::{Context, Result};
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

sol! {
    function forward(address target, bytes data);
    function transmit(bytes32[3] reportContext, bytes report, bytes32[] rs, bytes32[] ss, bytes32 rawVs);
    // Chainlink SVR (DualAggregator): the auctioned copy of the same report, sent inside an Atlas
    // metacall as userOp.data = DappControl.update(forwarder, forward(aggregator, transmitSecondary(..))).
    function transmitSecondary(bytes32[3] reportContext, bytes report, bytes32[] rs, bytes32[] ss, bytes32 rawVs);
    function update(address forwarder, bytes data);
    struct UserOperation { address from; address to; uint256 value; uint256 gas; uint256 maxFeePerGas; uint256 nonce; uint256 deadline; address dapp; address control; uint32 callConfig; uint32 dappGasLimit; uint32 solverGasLimit; uint24 bundlerSurchargeRate; address sessionKey; bytes data; bytes signature; }
    struct SolverOperation { address from; address to; uint256 value; uint256 gas; uint256 maxFeePerGas; uint256 deadline; address solver; address control; bytes32 userOpHash; address bidToken; uint256 bidAmount; bytes data; bytes signature; }
    struct DAppOperation { address from; address to; uint256 nonce; uint256 deadline; address control; address bundler; bytes32 userOpHash; bytes32 callChainHash; bytes signature; }
    function metacall(UserOperation userOp, SolverOperation[] solverOps, DAppOperation dAppOp, address gasRefundBeneficiary);
    // OCR2 median report layout (decoded without a selector).
    function report(uint32 observationsTimestamp, bytes32 rawObservers, int192[] observations, int192 juelsPerFeeCoin);
}

/// Which copy of a Chainlink report a transaction carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Path {
    /// The public `transmit`. On an SVR feed it does not move what Morpho reads yet.
    Primary,
    /// `transmitSecondary` inside an Atlas metacall: what an SVR proxy (and so Morpho) reads.
    Secondary,
    /// An SVR primary that no auction carried: the proxy serves it once it is older than the
    /// aggregator's cutoff (10 s on the Base SVR feeds checked).
    Expired,
}

/// A new Chainlink answer seen in a flashblock.
#[derive(Debug, Clone)]
pub struct OracleUpdate {
    pub aggregator: Address,
    pub answer: U256,
    pub path: Path,
    pub block: u64,
    /// Timestamp of the block the tx is in (from that block's first flashblock; 0 if unseen).
    pub block_ts: u64,
    pub index: u64,
    pub tx: B256,
    pub seen: Instant,
}

#[derive(Debug, Clone, Copy)]
pub struct TxSeen {
    pub block: u64,
    pub index: u64,
    pub at: Instant,
}

/// tx hash -> first flashblock it appeared in (recent blocks only).
pub type SeenTxs = Arc<Mutex<HashMap<B256, TxSeen>>>;

/// What the stream reports to the main loop.
#[derive(Debug, Clone)]
pub enum FbEvent {
    Oracle(OracleUpdate),
    /// Index 0 of a block arrived: its number and timestamp (SVR cutoffs are timed on it).
    Opened { block: u64, timestamp: u64, seen: Instant },
    /// A Chainlink SVR auction on our chain (observe-only solver phase 0; see atlas.rs).
    Auction(crate::atlas::Auction),
}

pub struct Stream {
    pub updates: mpsc::Receiver<FbEvent>,
    /// For other feeds (the Atlas auctions) to share the same event loop.
    pub sender: mpsc::Sender<FbEvent>,
    pub seen: SeenTxs,
    /// Last message time (stream health).
    pub last_msg: Arc<Mutex<Option<Instant>>>,
    /// Newest block known to be sealed: index 0 of block N opens N, so N - 1 is final. Arrives
    /// before any RPC node serves that block (measured 2026-10-18: alchemy WS +59 ms, drpc +123 ms,
    /// tenderly +356 ms), so it is a wake-up call to fetch the head, never a source of block data.
    pub sealed: watch::Receiver<u64>,
}

const KEEP_BLOCKS: u64 = 60;

#[derive(Deserialize)]
struct Msg {
    #[serde(default)]
    index: u64,
    #[serde(default)]
    base: Option<MsgBase>,
    #[serde(default)]
    diff: Option<MsgDiff>,
    #[serde(default)]
    metadata: Option<MsgMeta>,
}
#[derive(Deserialize)]
struct MsgBase {
    block_number: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    base_fee_per_gas: Option<String>,
}
#[derive(Deserialize)]
struct MsgDiff {
    #[serde(default)]
    transactions: Vec<Bytes>,
}
#[derive(Deserialize)]
struct MsgMeta {
    block_number: u64,
}

/// One decoded flashblock.
pub struct Decoded {
    pub block: u64,
    pub index: u64,
    pub txs: Vec<Bytes>,
    /// Block timestamp and base fee, carried by index 0 only.
    pub timestamp: Option<u64>,
    /// Parsed from the flashblock base; unused on the liquidation path.
    #[allow(dead_code)]
    pub base_fee: Option<u128>,
}

pub fn decode_message(compressed: &[u8]) -> Result<Decoded> {
    let mut s = String::new();
    brotli::Decompressor::new(compressed, 4096).read_to_string(&mut s).context("brotli")?;
    let m: Msg = serde_json::from_str(&s).context("flashblock json")?;
    let block = match (&m.metadata, &m.base) {
        (Some(md), _) => md.block_number,
        (None, Some(b)) => u64::from_str_radix(b.block_number.trim_start_matches("0x"), 16)?,
        _ => anyhow::bail!("flashblock without block number"),
    };
    let timestamp = m.base.as_ref().and_then(|b| b.timestamp.as_deref()).and_then(|t| u64::from_str_radix(t.trim_start_matches("0x"), 16).ok());
    let base_fee = m.base.as_ref().and_then(|b| b.base_fee_per_gas.as_deref()).and_then(|t| u128::from_str_radix(t.trim_start_matches("0x"), 16).ok());
    Ok(Decoded { block, index: m.index, txs: m.diff.map(|d| d.transactions).unwrap_or_default(), timestamp, base_fee })
}

/// Who may move an aggregator's answer: `aggregator.getTransmitters()` are forwarder contracts
/// (each with `getAuthorizedSenders()` EOAs) or, for direct transmits, the EOAs themselves.
#[derive(Debug, Clone, Default)]
pub struct Authorized {
    /// forwarder -> EOAs allowed to call it
    pub forwarders: HashMap<Address, HashSet<Address>>,
    /// EOAs allowed to call `transmit` on the aggregator directly
    pub direct: HashSet<Address>,
}

/// aggregator -> its authorized transmitters. Shared with the price model, which refreshes it
/// when a feed's aggregator or transmitter set changes.
pub type Watched = Arc<RwLock<HashMap<Address, Authorized>>>;

/// New answer if `raw` carries a Chainlink report for a watched aggregator, from an authorized
/// party. Anyone can put a lookalike in a block (even a reverting tx still shows up in the
/// stream), so:
/// - a public `transmit`: the forwarder (`to`) must be one of the aggregator's transmitters and
///   the recovered signer one of that forwarder's authorized senders (or, direct, a transmitter);
/// - an SVR `transmitSecondary` inside an Atlas metacall (`to == atlas`): the forwarder must be a
///   transmitter and the metacall's DappControl one of its authorized senders. The tx signer is
///   just the bundler, so this is weaker; the price model therefore only lets a secondary answer
///   take effect when it equals a primary answer already accepted for that aggregator.
/// Non-positive answers are rejected (the price model divides by them).
pub fn oracle_update(raw: &[u8], watched: &HashMap<Address, Authorized>, atlas: Address) -> Option<(Address, U256, Path)> {
    let tx = TxEnvelope::decode_2718(&mut &raw[..]).ok()?; // OP deposit txs (0x7e) are skipped
    let input = tx.input();
    if tx.to() == Some(atlas) {
        if !input.starts_with(&metacallCall::SELECTOR) {
            return None;
        }
        let mc = metacallCall::abi_decode(input).ok()?;
        let up = updateCall::abi_decode(&mc.userOp.data).ok()?;
        let fwd = forwardCall::abi_decode(&up.data).ok()?;
        let auth = watched.get(&fwd.target)?;
        if !auth.forwarders.get(&up.forwarder)?.contains(&mc.userOp.control) {
            return None;
        }
        let t = transmitSecondaryCall::abi_decode(&fwd.data).ok()?;
        return Some((fwd.target, median(&t.report)?, Path::Secondary));
    }
    let fwd;
    let (target, inner, via): (Address, &[u8], Option<Address>) = if input.starts_with(&forwardCall::SELECTOR) {
        fwd = forwardCall::abi_decode(input).ok()?;
        (fwd.target, &fwd.data[..], Some(tx.to()?))
    } else {
        (tx.to()?, &input[..], None)
    };
    let auth = watched.get(&target)?;
    if !inner.starts_with(&transmitCall::SELECTOR) {
        return None;
    }
    // Cheap checks first; the signature recovery (~50 µs) only runs for real candidates.
    let allowed = match via {
        Some(forwarder) => auth.forwarders.get(&forwarder)?,
        None => &auth.direct,
    };
    if !allowed.contains(&tx.recover_signer().ok()?) {
        return None;
    }
    let t = transmitCall::abi_decode(inner).ok()?;
    Some((target, median(&t.report)?, Path::Primary))
}

/// OCR2 median report -> the answer the aggregator stores (observations[len / 2]), if positive.
fn median(report: &[u8]) -> Option<U256> {
    let r = reportCall::abi_decode_raw(report).ok()?;
    let m = *r.observations.get(r.observations.len() / 2)?;
    if m.is_negative() || m.is_zero() {
        return None;
    }
    let limbs = m.into_raw().into_limbs();
    Some(U256::from_limbs([limbs[0], limbs[1], limbs[2], 0]))
}

/// Connects (and reconnects, with backoff) in the background and hands every decoded flashblock to
/// `on_msg` with its arrival time. `stream` names it in the logs.
fn run_stream(url: String, stream: &'static str, mut on_msg: impl FnMut(Decoded, Instant) + Send + 'static) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match tokio_tungstenite::connect_async(url.as_str()).await {
                Ok((mut ws, _)) => {
                    info!(stream, "flashblocks stream connected");
                    backoff = Duration::from_secs(1);
                    while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
                        let Ok(tokio_tungstenite::tungstenite::Message::Binary(b)) = msg else {
                            if msg.is_err() {
                                break;
                            }
                            continue;
                        };
                        let at = Instant::now();
                        match decode_message(&b) {
                            Ok(d) => on_msg(d, at),
                            Err(e) => warn!(stream, error = %e, "flashblock decode failed"),
                        }
                    }
                    warn!(stream, "flashblocks stream stalled or closed; reconnecting");
                }
                Err(e) => warn!(stream, error = %e, backoff_s = backoff.as_secs(), "flashblocks connect failed"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
}

/// Connects (and reconnects) to the stream in the background.
pub fn spawn(url: String, watched: Watched, atlas: Address) -> Stream {
    let (tx, rx) = mpsc::channel(256);
    let sender = tx.clone();
    let seen: SeenTxs = Arc::default();
    let last_msg: Arc<Mutex<Option<Instant>>> = Arc::default();
    let (seen2, last2) = (seen.clone(), last_msg.clone());
    let (sealed_tx, sealed) = watch::channel(0u64);
    let mut newest = 0u64;
    let mut block_ts: HashMap<u64, u64> = HashMap::new();
    run_stream(url, "oracles", move |Decoded { block, index, txs, timestamp, .. }, at| {
        *last2.lock().unwrap() = Some(at);
        if let (0, Some(ts)) = (index, timestamp) {
            block_ts.insert(block, ts);
            block_ts.retain(|b, _| b + KEEP_BLOCKS > block);
            if tx.try_send(FbEvent::Opened { block, timestamp: ts, seen: at }).is_err() {
                warn!("flashblock event channel full; block-open event dropped");
            }
        }
        let this_ts = block_ts.get(&block).copied().unwrap_or(0);
        if index == 0 && block > 0 {
            sealed_tx.send_if_modified(|s| {
                let new = block - 1 > *s;
                if new {
                    *s = block - 1;
                }
                new
            });
        }
        {
            let mut s = seen2.lock().unwrap();
            if block > newest {
                newest = block;
                s.retain(|_, v| v.block + KEEP_BLOCKS > block);
            }
            for raw in &txs {
                s.entry(keccak256(raw)).or_insert(TxSeen { block, index, at });
            }
        }
        for raw in &txs {
            let found = oracle_update(raw, &watched.read().unwrap(), atlas);
            if let Some((aggregator, answer, path)) = found {
                let u = FbEvent::Oracle(OracleUpdate { aggregator, answer, path, block, block_ts: this_ts, index, tx: keccak256(raw), seen: at });
                if tx.try_send(u).is_err() {
                    warn!("oracle update channel full; update dropped");
                }
            }
        }
    });
    Stream { updates: rx, sender, seen, last_msg, sealed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use std::io::Write;

    const ETH_USD_AGGREGATOR: Address = address!("05c84a58FE042275b37db038bAAcD15F410c7bB0");
    const ATLAS: Address = address!("583dcFef0D240DC80753F0F0B26513feE27D9B77");

    fn fixture() -> Vec<u8> {
        let hex = std::fs::read_to_string("tests/fixtures/chainlink_eth_usd_transmit.hex").unwrap();
        alloy::hex::decode(hex.trim()).unwrap()
    }

    #[test]
    fn decodes_chainlink_transmit_via_forwarder() {
        // Real Base tx 0x293d82a0...: forwarder 0xB2777b58 .forward(ETH/USD aggregator, transmit(...)),
        // sent by 0xB9e44696 (one of the forwarder's getAuthorizedSenders()).
        // AnswerUpdated in its receipt says 241544504811.
        let raw = fixture();
        assert_eq!(keccak256(&raw), alloy::primitives::b256!("293d82a083a767be6c6cac5b8078c585ca572106eed1083272ca74ea5398ed49"));
        let fwd = address!("B2777b58F50388a9acF264023722C83bE025dC1d");
        let sender = address!("B9e44696B045ab005eA956253d8676F656eEBC60");
        let auth = |f: Address, s: Address| HashMap::from([(ETH_USD_AGGREGATOR, Authorized { forwarders: HashMap::from([(f, HashSet::from([s]))]), direct: HashSet::new() })]);
        assert_eq!(oracle_update(&raw, &auth(fwd, sender), ATLAS), Some((ETH_USD_AGGREGATOR, U256::from(241_544_504_811u64), Path::Primary)));
        // A lookalike from an unknown forwarder or an unauthorized sender is ignored.
        assert_eq!(oracle_update(&raw, &auth(Address::repeat_byte(1), sender), ATLAS), None);
        assert_eq!(oracle_update(&raw, &auth(fwd, Address::repeat_byte(2)), ATLAS), None);
        // Not watched -> ignored.
        assert_eq!(oracle_update(&raw, &HashMap::new(), ATLAS), None);
        // Garbage / deposit-like bytes -> ignored, no panic.
        assert_eq!(oracle_update(&[0x7e, 1, 2, 3], &auth(fwd, sender), ATLAS), None);
    }

    #[test]
    fn decodes_svr_secondary_inside_atlas_metacall() {
        // Real Base tx 0x22982ae3... (block 52741703): Atlas metacall whose userOp is
        // DappControl.update(forwarder 0x79fC1d9e, forward(XRP/USD DualAggregator, transmitSecondary)),
        // followed by 15 Morpho liquidations. XRP/USD read 127840411 after that block.
        let hex = std::fs::read_to_string("tests/fixtures/atlas_metacall_xrp.hex").unwrap();
        let raw = alloy::hex::decode(hex.trim()).unwrap();
        let xrp = address!("fa9ebD1FC381C5Babb495BB53664ae9B6665E4E4");
        let fwd = address!("79fC1d9e8A7425aDC44183bf8e1efbCBf752dE48");
        let control = address!("a5E1a36938769cbd5a26f5e19D8FCB379f597c83");
        let auth = |c: Address| HashMap::from([(xrp, Authorized { forwarders: HashMap::from([(fwd, HashSet::from([c]))]), direct: HashSet::new() })]);
        assert_eq!(oracle_update(&raw, &auth(control), ATLAS), Some((xrp, U256::from(127_840_411u64), Path::Secondary)));
        // DappControl not an authorized sender of that forwarder -> ignored.
        assert_eq!(oracle_update(&raw, &auth(Address::repeat_byte(3)), ATLAS), None);
        // Aggregator not watched -> ignored.
        assert_eq!(oracle_update(&raw, &HashMap::new(), ATLAS), None);
        // Some other contract than Atlas -> not read as a metacall.
        assert_eq!(oracle_update(&raw, &auth(control), Address::repeat_byte(4)), None);
    }

    #[test]
    fn decodes_every_feed_like_answer_updated() {
        // Real Base transmits for several Chainlink feeds: "aggregator answer rawtx",
        // answer = AnswerUpdated's indexed `current`. FB_FIXTURE=<file> runs a larger set. The
        // authorization is taken from the tx itself (its forwarder and signer), so this checks
        // the decoding; the sender checks are covered above.
        let path = std::env::var("FB_FIXTURE").unwrap_or_else(|_| "tests/fixtures/chainlink_transmits.txt".into());
        let text = std::fs::read_to_string(&path).unwrap();
        let mut n = 0;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let mut it = line.split_whitespace();
            let agg: Address = it.next().unwrap().parse().unwrap();
            let answer: U256 = it.next().unwrap().parse().unwrap();
            let raw = alloy::hex::decode(it.next().unwrap()).unwrap();
            let tx = TxEnvelope::decode_2718(&mut &raw[..]).unwrap();
            let auth = Authorized { forwarders: HashMap::from([(tx.to().unwrap(), HashSet::from([tx.recover_signer().unwrap()]))]), direct: HashSet::new() };
            assert_eq!(oracle_update(&raw, &HashMap::from([(agg, auth)]), ATLAS), Some((agg, answer, Path::Primary)), "tx {}", keccak256(&raw));
            n += 1;
        }
        assert!(n >= 9, "fixture too small: {n}");
    }

    #[test]
    fn decodes_compressed_flashblock_messages() {
        let raw = fixture();
        let json = serde_json::json!({
            "payload_id": "0x01", "index": 3,
            "diff": { "transactions": [format!("0x{}", alloy::hex::encode(&raw))] },
            "metadata": { "block_number": 52738345u64 }
        })
        .to_string();
        let mut comp = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut comp, 4096, 5, 22);
            w.write_all(json.as_bytes()).unwrap();
        }
        let Decoded { block, index, txs, timestamp, base_fee } = decode_message(&comp).unwrap();
        assert_eq!((timestamp, base_fee), (None, None), "only index 0 carries the block timestamp and base fee");
        assert_eq!((block, index, txs.len()), (52738345, 3, 1));
        assert_eq!(&txs[0][..], &raw[..]);
        // index 0 carries the block number in `base` (hex) instead of `metadata`.
        let open = serde_json::json!({ "payload_id": "0x01", "index": 0, "base": { "block_number": "0x324b94", "timestamp": "0x6ad3cf8f", "base_fee_per_gas": "0x5b8d80" }, "diff": { "transactions": [] } }).to_string();
        let mut comp = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut comp, 4096, 5, 22);
            w.write_all(open.as_bytes()).unwrap();
        }
        let d = decode_message(&comp).unwrap();
        assert_eq!((d.block, d.timestamp, d.base_fee), (0x324b94, Some(0x6ad3cf8f), Some(6_000_000)));
    }
}
