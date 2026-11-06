//! Chainlink oracle updates from canonical-block logs (`eth_subscribe("logs")`), for the price model
//! the Atlas auctions are valued with. Base Denim removes the Flashblocks stream and pending state,
//! so updates are read from sealed blocks.
//!
//! Two events are read, each handled on its own (checked 2026-11-02 on the XRP/USD DualAggregator,
//! round 0x735; fixture `tests/fixtures/oracle_logs/xrp_round_0x735.json`):
//! - `NewTransmission`: the public transmit, with the answer. On an SVR feed it does not move what
//!   Morpho reads yet (the price model keeps it pending).
//! - `SecondaryRoundIdUpdated`: the SVR secondary written by the auction winner's Atlas metacall. It
//!   carries the round only; the answer is the pending one of that round.
//! - `CutoffTimeSet(uint32)`: the DualAggregator's own setter log (`setCutoffTime(uint32)`), the
//!   window after which an unauctioned SVR primary the proxy serves (see `oracle::PriceModel::due`).
//!   The new cutoff (seconds) is the single non-indexed argument, so the value is read straight from
//!   the log — the aggregator exposes no getter for it (checked on the Base DualAggregator bytecode
//!   2026-11-06). The config `svr_cutoff_secs` is only the startup default until the first log.
//!
//! `AnswerUpdated` is not read: an SVR feed emits it at the public transmit too, before the price is
//! in effect. The block path cross-checks the model against each block's snapshot (runner.rs), which
//! covers missed events, reverted transmits and swapped aggregators.

use std::{sync::Arc, time::Duration};

use alloy::{
    primitives::{Address, B256, U256},
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::{Filter, Log},
    sol,
    sol_types::SolEvent,
};
use futures::StreamExt;
use tokio::sync::{Notify, mpsc, watch};
use tracing::{info, warn};

use crate::runner::SideEvent;

sol! {
    event NewTransmission(uint32 indexed aggregatorRoundId, int192 answer, address transmitter, uint32 observationsTimestamp, int192[] observations, bytes observers, int192 juelsPerFeeCoin, bytes32 configDigest, uint40 epochAndRound);
    event SecondaryRoundIdUpdated(uint32 indexed secondaryRoundId);
    event CutoffTimeSet(uint32 cutoffTime);
}

/// Where an oracle event sits in the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogPos {
    pub block: u64,
    /// The block's timestamp when the node put it in the log (Base does), else 0.
    pub block_ts: u64,
    pub tx: B256,
    pub index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleLogEvent {
    /// A public transmit of `answer` for `round`.
    Transmission { aggregator: Address, round: u32, answer: U256, at: LogPos },
    /// The SVR secondary of `round` (Atlas metacall): the pending answer of that round takes effect.
    Secondary { aggregator: Address, round: u32, at: LogPos },
    /// The DualAggregator's SVR cutoff changed to `cutoff_secs` (seconds): the window an unauctioned
    /// primary waits before the proxy serves it.
    CutoffChanged { aggregator: Address, cutoff_secs: u32, at: LogPos },
    /// A log of `aggregator` was removed by a reorg: what the model took from it may be wrong.
    Removed { aggregator: Address },
    /// The subscription was (re)established after a gap: events may have been missed.
    Reconnected,
}

impl OracleLogEvent {
    /// The block of a transmission or secondary. A cutoff change is applied the moment it arrives
    /// (it does not depend on the block's timestamp), so it is deliberately not held by position.
    pub fn pos(&self) -> Option<LogPos> {
        match self {
            OracleLogEvent::Transmission { at, .. } | OracleLogEvent::Secondary { at, .. } => Some(*at),
            _ => None,
        }
    }
}

/// The event `log` carries, if it is one of ours. Non-positive answers are dropped (the price model
/// divides by them).
pub fn decode(log: &Log) -> Option<OracleLogEvent> {
    let aggregator = log.address();
    if log.removed {
        return Some(OracleLogEvent::Removed { aggregator });
    }
    let at = LogPos { block: log.block_number?, block_ts: log.block_timestamp.unwrap_or(0), tx: log.transaction_hash?, index: log.log_index? };
    match log.topic0()? {
        t if *t == NewTransmission::SIGNATURE_HASH => {
            let e = NewTransmission::decode_log(&log.inner).ok()?.data;
            if e.answer.is_negative() || e.answer.is_zero() {
                return None;
            }
            let limbs = e.answer.into_raw().into_limbs();
            Some(OracleLogEvent::Transmission { aggregator, round: e.aggregatorRoundId, answer: U256::from_limbs([limbs[0], limbs[1], limbs[2], 0]), at })
        }
        t if *t == SecondaryRoundIdUpdated::SIGNATURE_HASH => {
            let e = SecondaryRoundIdUpdated::decode_log(&log.inner).ok()?.data;
            Some(OracleLogEvent::Secondary { aggregator, round: e.secondaryRoundId, at })
        }
        t if *t == CutoffTimeSet::SIGNATURE_HASH => {
            let e = CutoffTimeSet::decode_log(&log.inner).ok()?.data;
            Some(OracleLogEvent::CutoffChanged { aggregator, cutoff_secs: e.cutoffTime, at })
        }
        _ => None,
    }
}

/// Controls the subscription task.
#[derive(Clone)]
pub struct OracleLogs {
    addresses: Arc<watch::Sender<Vec<Address>>>,
    reconnect: Arc<Notify>,
}

impl OracleLogs {
    /// Subscribes to these aggregators from now on (an aggregator was swapped).
    pub fn set_addresses(&self, mut addresses: Vec<Address>) {
        addresses.sort();
        self.addresses.send_if_modified(|cur| {
            let new = *cur != addresses;
            if new {
                *cur = addresses;
            }
            new
        });
    }

    /// Drops the connection and subscribes again (events seem to be missing).
    pub fn reconnect(&self) {
        self.reconnect.notify_one();
    }

    /// A handle with no subscription behind it (tests).
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        OracleLogs { addresses: Arc::new(watch::channel(vec![]).0), reconnect: Arc::new(Notify::new()) }
    }
}

/// Subscribes in the background (reconnecting) to the oracle events of `addresses` on `ws_url`; the
/// events go to `tx`. `name` is the endpoint's name, the only thing logged about it.
pub fn spawn(name: String, ws_url: String, mut addresses: Vec<Address>, tx: mpsc::Sender<SideEvent>) -> OracleLogs {
    addresses.sort();
    let (addr_tx, mut addr_rx) = watch::channel(addresses);
    let reconnect = Arc::new(Notify::new());
    let handle = OracleLogs { addresses: Arc::new(addr_tx), reconnect: reconnect.clone() };
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(500);
        let mut connected_before = false;
        loop {
            let addrs = addr_rx.borrow_and_update().clone();
            let r = async {
                let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_url.as_str())).await?;
                let filter = Filter::new().address(addrs.clone()).event_signature(vec![NewTransmission::SIGNATURE_HASH, SecondaryRoundIdUpdated::SIGNATURE_HASH, CutoffTimeSet::SIGNATURE_HASH]);
                let mut stream = provider.subscribe_logs(&filter).await?.into_stream();
                info!(endpoint = %name, aggregators = addrs.len(), "oracle logs subscribed");
                if connected_before && tx.send(SideEvent::Oracle(OracleLogEvent::Reconnected)).await.is_err() {
                    return Ok(true);
                }
                connected_before = true;
                backoff = Duration::from_millis(500);
                loop {
                    tokio::select! {
                        log = stream.next() => match log {
                            Some(log) => {
                                if let Some(ev) = decode(&log)
                                    && tx.try_send(SideEvent::Oracle(ev)).is_err()
                                {
                                    if tx.is_closed() {
                                        return Ok(true);
                                    }
                                    warn!("side event channel full; oracle log dropped (the block path resyncs the model)");
                                }
                            }
                            None => return anyhow::Ok(false),
                        },
                        changed = addr_rx.changed() => {
                            if changed.is_err() {
                                return Ok(true);
                            }
                            info!(endpoint = %name, "oracle aggregators changed; subscribing again");
                            return Ok(false);
                        }
                        _ = reconnect.notified() => {
                            warn!(endpoint = %name, "oracle logs reconnecting on request");
                            return Ok(false);
                        }
                    }
                }
            }
            .await;
            match r {
                Ok(true) => return,
                Ok(false) => {}
                // The error text can embed the URL (and its key), so only the name is logged.
                Err(_) => {
                    warn!(endpoint = %name, backoff_ms = backoff.as_millis() as u64, "oracle logs subscription failed; reconnecting");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    });
    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const XRP: Address = address!("fa9ebD1FC381C5Babb495BB53664ae9B6665E4E4");

    fn fixture() -> Vec<Log> {
        serde_json::from_str(&std::fs::read_to_string("tests/fixtures/oracle_logs/xrp_round_0x735.json").unwrap()).unwrap()
    }

    #[test]
    fn decodes_the_public_transmit_and_its_secondary() {
        // Block 52741702: the public transmit (NewTransmission, NewRound, AnswerUpdated); block
        // 52741703: the Atlas metacall's secondary, round only.
        let evs: Vec<OracleLogEvent> = fixture().iter().filter_map(decode).collect();
        assert_eq!(evs.len(), 2, "NewRound and AnswerUpdated are not ours: {evs:?}");
        match &evs[0] {
            OracleLogEvent::Transmission { aggregator, round, answer, at } => {
                assert_eq!((*aggregator, *round, *answer), (XRP, 0x735, U256::from(127_840_411u64)));
                assert_eq!(at.block, 52_741_702);
                assert!(at.block_ts > 0, "Base puts blockTimestamp in its logs");
            }
            e => panic!("{e:?}"),
        }
        match &evs[1] {
            OracleLogEvent::Secondary { aggregator, round, at } => {
                assert_eq!((*aggregator, *round, at.block), (XRP, 0x735, 52_741_703));
                assert_eq!(at.tx, "0x22982ae30e036921c78ace99a2034f939e4c20f4a549ec1bb1a19c0e8f12459a".parse::<B256>().unwrap());
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn cutoff_event_signature_is_the_onchain_topic() {
        // Guards the ABI against the DualAggregator's real event (setCutoffTime(uint32) =
        // 0xb17f2a6b; its log is CutoffTimeSet(uint32)), so a wrong signature can't silently stop
        // cutoff updates. Topic verified on Base 2026-11-06.
        assert_eq!(
            CutoffTimeSet::SIGNATURE_HASH,
            "0xb24a681ce3399a408a89fd0c2b59dfc24bdad592b1c7ec7671cf060596c1c4d1".parse::<B256>().unwrap()
        );
    }

    #[test]
    fn decodes_a_cutoff_change() {
        use alloy::sol_types::SolEvent;
        let mut log = fixture().remove(0);
        log.inner = alloy::primitives::Log { address: XRP, data: CutoffTimeSet { cutoffTime: 60 }.encode_log_data() };
        match decode(&log) {
            Some(OracleLogEvent::CutoffChanged { aggregator, cutoff_secs, at }) => {
                assert_eq!((aggregator, cutoff_secs, at.block), (XRP, 60, 52_741_702));
            }
            e => panic!("{e:?}"),
        }
        // A cutoff change is applied the moment it arrives, not held by block position.
        assert_eq!(decode(&log).and_then(|e| e.pos()), None);
    }

    #[test]
    fn a_removed_log_is_reported_as_such() {
        let mut log = fixture().remove(0);
        log.removed = true;
        assert_eq!(decode(&log), Some(OracleLogEvent::Removed { aggregator: XRP }));
    }

    #[test]
    fn non_positive_answers_and_unknown_events_are_dropped() {
        let logs = fixture();
        // NewRound (index 1) is not ours.
        assert_eq!(decode(&logs[1]), None);
        // A zero answer in a NewTransmission.
        let mut zero = logs[0].clone();
        let mut data = zero.inner.data.data.to_vec();
        data[..32].fill(0);
        zero.inner.data = alloy::primitives::LogData::new_unchecked(zero.inner.data.topics().to_vec(), data.into());
        assert_eq!(decode(&zero), None);
    }

    #[test]
    fn a_log_without_its_block_timestamp_still_decodes() {
        let mut log = fixture().remove(0);
        log.block_timestamp = None;
        assert_eq!(decode(&log).and_then(|e| e.pos()).map(|p| p.block_ts), Some(0));
    }
}
