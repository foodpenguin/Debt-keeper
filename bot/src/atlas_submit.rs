//! Bidding in Chainlink SVR auctions (Fastlane Atlas): a solver operation built from a valued
//! auction, EIP-712 signed by the bonded solver EOA, and submitted to the auction's bid endpoint.
//!
//! Checked against the deployed contracts on Base (2026-10-29):
//! - the signed struct is `SolverOperation(address from,address to,uint256 value,uint256 gas,
//!   uint256 maxFeePerGas,uint256 deadline,address solver,address control,bytes32 userOpHash,
//!   address bidToken,uint256 bidAmount,bytes data)` (the op without its signature), under the
//!   domain `AtlasVerification` / its `eip712Domain()` version, verifying contract =
//!   `Atlas.VERIFICATION()`. The digest equals `AtlasVerification.getSolverPayload(op)`
//!   (`e2e_solver_op_digest_matches_atlas_verification`);
//! - the signature is the raw secp256k1 signature of that digest, `r || s || v` with v = 27 / 28
//!   (Fastlane's atlas-sdk-go: `utils.SignMessage(hash)`);
//! - submission (docs.chain.link/data-feeds/svr-feeds/searcher-onboarding-atlas): JSON-RPC
//!   `solver_submitSolverOperation` with `[{ auction_id, auction_solution }]`, the op as hex
//!   quantities (atlas-sdk-go `SolverOperationRaw`), sent over the gateway's solver WebSocket.
//!   Its HTTP root does not have the method (-32601 "Method not found", which refused the first
//!   live submissions); over the WebSocket the same op shape with an unknown auction id is parsed
//!   and refused with "Auction not found" (probed 2026-11-19). The answer of an accepted op is
//!   `"result": null`.
//!
//! Operator prerequisites (never done by the bot): atlETH bonded by the signer EOA
//! (`Atlas.depositAndBond`), a deployed AtlasLiquidationSolver whose `executor` is that EOA, and its
//! MorphoLiquidator. See docs/architecture.md ("Bidding in SVR auctions").

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy::{
    primitives::{Address, B256, Bytes, U256},
    providers::Provider,
    signers::{SignerSync, local::PrivateKeySigner},
    sol,
    sol_types::{Eip712Domain, SolCall, SolStruct},
};
use anyhow::{Context, Result, anyhow, bail};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

sol! {
    /// The signed part of an Atlas solver operation (EIP-712 primary type).
    #[derive(Debug, PartialEq)]
    struct SolverOperation {
        address from;
        address to;
        uint256 value;
        uint256 gas;
        uint256 maxFeePerGas;
        uint256 deadline;
        address solver;
        address control;
        bytes32 userOpHash;
        address bidToken;
        uint256 bidAmount;
        bytes data;
    }
    interface IAtlasViews {
        function VERIFICATION() external view returns (address);
        function balanceOfBonded(address account) external view returns (uint256);
    }
    interface IEip712 {
        function eip712Domain() external view returns (bytes1 fields, string name, string version, uint256 chainId, address verifyingContract, bytes32 salt, uint256[] extensions);
    }
    interface IDappControl {
        function getSolverGasLimit() external view returns (uint32);
    }
}

/// The Chainlink SVR bid endpoint: its solver WebSocket (the HTTP root has no submit method).
pub const SVR_BID_ENDPOINT: &str = "wss://svr-bid-endpoint.chain.link/ws/solver";

/// Atlas's EIP-712 domain, as the deployed AtlasVerification reports it.
pub async fn read_domain<P: Provider>(p: &P, atlas: Address) -> Result<Eip712Domain> {
    let call = |to: Address, data: Vec<u8>| alloy::rpc::types::TransactionRequest::default().to(to).input(Bytes::from(data).into());
    let out = p.call(call(atlas, IAtlasViews::VERIFICATIONCall {}.abi_encode())).await.context("Atlas.VERIFICATION()")?;
    let verification = IAtlasViews::VERIFICATIONCall::abi_decode_returns(&out)?;
    let out = p.call(call(verification, IEip712::eip712DomainCall {}.abi_encode())).await.context("AtlasVerification.eip712Domain()")?;
    let d = IEip712::eip712DomainCall::abi_decode_returns(&out)?;
    anyhow::ensure!(d.name == "AtlasVerification" && d.verifyingContract == verification, "unexpected Atlas EIP-712 domain: {} at {}", d.name, d.verifyingContract);
    Ok(Eip712Domain::new(Some(d.name.into()), Some(d.version.into()), Some(d.chainId), Some(d.verifyingContract), None))
}

/// atlETH bonded by `account` (what Atlas charges the solver's gas against).
pub async fn bonded<P: Provider>(p: &P, atlas: Address, account: Address) -> Result<U256> {
    let data = IAtlasViews::balanceOfBondedCall { account }.abi_encode();
    let out = p.call(alloy::rpc::types::TransactionRequest::default().to(atlas).input(Bytes::from(data).into())).await?;
    Ok(IAtlasViews::balanceOfBondedCall::abi_decode_returns(&out)?)
}

/// The most gas a solver operation of this DappControl may take.
pub async fn solver_gas_limit<P: Provider>(p: &P, control: Address) -> Result<u64> {
    let data = IDappControl::getSolverGasLimitCall {}.abi_encode();
    let out = p.call(alloy::rpc::types::TransactionRequest::default().to(control).input(Bytes::from(data).into())).await?;
    Ok(IDappControl::getSolverGasLimitCall::abi_decode_returns(&out)? as u64)
}

/// A signed solver operation.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedSolverOp {
    pub op: SolverOperation,
    /// 65 bytes, `r || s || v`, v = 27 / 28.
    pub signature: Bytes,
}

impl SignedSolverOp {
    /// The op as the bid endpoint takes it (atlas-sdk-go `SolverOperationRaw`).
    pub fn to_json(&self) -> Value {
        let q = |x: U256| format!("{x:#x}");
        let o = &self.op;
        json!({
            "from": o.from, "to": o.to, "value": q(o.value), "gas": q(o.gas), "maxFeePerGas": q(o.maxFeePerGas),
            "deadline": q(o.deadline), "solver": o.solver, "control": o.control, "userOpHash": o.userOpHash,
            "bidToken": o.bidToken, "bidAmount": q(o.bidAmount), "data": o.data, "signature": self.signature,
        })
    }
}

/// The EIP-712 digest Atlas verifies (`AtlasVerification.getSolverPayload`).
pub fn digest(op: &SolverOperation, domain: &Eip712Domain) -> B256 {
    op.eip712_signing_hash(domain)
}

pub fn sign(op: SolverOperation, domain: &Eip712Domain, signer: &PrivateKeySigner) -> Result<SignedSolverOp> {
    anyhow::ensure!(op.from == signer.address(), "solver op from {} is not the signer {}", op.from, signer.address());
    let sig = signer.sign_hash_sync(&digest(&op, domain)).context("signing the solver op")?;
    Ok(SignedSolverOp { op, signature: Bytes::from(sig.as_bytes().to_vec()) })
}

/// The JSON-RPC request submitting `op` to auction `auction_id`.
pub fn submit_request(auction_id: &str, op: &SignedSolverOp) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "solver_submitSolverOperation", "params": [{ "auction_id": auction_id, "auction_solution": op.to_json() }] })
}

/// The endpoint's JSON-RPC answer to a submission: Ok = accepted (`"result": null`, also taken as
/// the string `"null"` the docs show), Err = refused with the endpoint's error.
pub fn answer(resp: &Value) -> Result<Value> {
    if let Some(e) = resp.get("error") {
        bail!("bid endpoint refused the solver op: {}", e.to_string().chars().take(200).collect::<String>());
    }
    Ok(match resp.get("result") {
        Some(Value::String(s)) if s == "null" => Value::Null,
        Some(r) => r.clone(),
        None => Value::Null,
    })
}

/// How a submission ended when it did not fail.
#[derive(Debug, Clone, PartialEq)]
pub enum Submitted {
    /// The endpoint accepted the op (its `result`); whether it wins is decided on-chain.
    Accepted(Value),
    /// The op was written to the socket but no answer came (within the timeout, or before the
    /// socket was lost; the reason): it may be in the auction, so it is treated as submitted (its
    /// slot kept, its liability counted, its Atlas result looked for). An answer arriving after the
    /// caller gave up is still logged (refused: "ATLAS solver op refused after its submission timed
    /// out").
    Unanswered(String),
}

/// How long a submission waits for the endpoint's answer, a reconnect included, before it counts
/// as not submitted (or `Unanswered` once written). The gateway answers in tens of ms from a
/// nearby host; an auction's window is a few seconds, so a stuck socket must not hold the planner.
pub const SUBMIT_TIMEOUT: Duration = Duration::from_millis(1_000);

/// The bid socket's clocks (`BidTiming::DEFAULT` in production; shorter ones in tests).
#[derive(Debug, Clone, Copy)]
struct BidTiming {
    /// Housekeeping tick: overdue answers, ping, silence.
    tick: Duration,
    /// Ping interval while idle (the socket is silent between submissions).
    ping_every: Duration,
    /// Nothing received for this long (not even a pong): the socket is dead, reconnect.
    silent_for: Duration,
    /// A written request still unanswered after this long: the socket is taken as dead (a
    /// half-open connection takes writes and answers nothing) and reconnected.
    answer_within: Duration,
    /// A write (request or ping) that does not complete in this long tears the socket down.
    write_within: Duration,
    /// Reconnect backoff, doubled per failed connect or short-lived connection.
    backoff_min: Duration,
    backoff_max: Duration,
    /// A connection that lasted this long resets the backoff (the next reconnect is immediate);
    /// one dropped sooner (accepted, then closed) backs off, so a refusing endpoint is not hammered.
    stable_for: Duration,
}

impl BidTiming {
    const DEFAULT: Self = Self {
        tick: Duration::from_millis(250),
        ping_every: Duration::from_secs(5),
        silent_for: Duration::from_secs(15),
        answer_within: Duration::from_secs(3),
        write_within: Duration::from_secs(1),
        backoff_min: Duration::from_millis(250),
        backoff_max: Duration::from_secs(10),
        stable_for: Duration::from_secs(30),
    };

    fn next_backoff(&self, b: Duration) -> Duration {
        (b * 2).clamp(self.backoff_min, self.backoff_max)
    }
}

const QUEUED: u8 = 0;
const WRITTEN: u8 = 1;
const CANCELLED: u8 = 2;

/// What the socket task tells a submission: the endpoint's answer, or that the socket was lost
/// after the request was written (so it may have reached the endpoint).
enum Reply {
    Answer(Value),
    Lost(String),
}

/// One submission handed to the socket task: the request, where its answer goes, and whether it
/// was written (QUEUED -> WRITTEN by the task, QUEUED -> CANCELLED by a caller that timed out, so a
/// request is never written after its caller gave up).
struct BidRequest {
    req: Value,
    reply: oneshot::Sender<Reply>,
    state: Arc<AtomicU8>,
}

/// A written request awaiting its answer.
struct Pending {
    reply: oneshot::Sender<Reply>,
    written: Instant,
    /// `params[0].auction_id`, for logging an answer that comes after its caller gave up.
    auction: String,
}

/// A standing WebSocket to the bid endpoint, connected at start and reconnected in the background
/// (pinged while idle), so a submission costs one round trip. Answers are matched to requests by
/// their JSON-RPC id (a number, or the same number as a string); an error answer with no usable id
/// (`"id": null`, the JSON-RPC form for a request the endpoint could not parse) goes to the only
/// request in flight, or is logged when there is not exactly one. Other frames are ignored.
/// Separate from the auction feed's socket: the feed is read by its own task, and sharing it would
/// mean routing writes and answers through it.
#[derive(Clone)]
pub struct BidSocket {
    tx: mpsc::Sender<BidRequest>,
    next_id: Arc<AtomicU64>,
    connected: watch::Receiver<bool>,
}

impl BidSocket {
    /// Spawns the socket task (needs a tokio runtime).
    pub fn spawn(url: String) -> Self {
        Self::spawn_with(url, BidTiming::DEFAULT)
    }

    fn spawn_with(url: String, timing: BidTiming) -> Self {
        let (tx, rx) = mpsc::channel(16);
        let (up, connected) = watch::channel(false);
        tokio::spawn(run_bid_socket(url, rx, up, timing));
        Self { tx, next_id: Arc::new(AtomicU64::new(1)), connected }
    }

    /// Whether the socket is connected now.
    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    /// Waits up to `wait` for the socket to be connected; false = still not.
    pub async fn wait_connected(&self, wait: Duration) -> bool {
        let mut c = self.connected.clone();
        tokio::time::timeout(wait, c.wait_for(|up| *up)).await.is_ok_and(|r| r.is_ok())
    }

    /// Sends one JSON-RPC request (its `id` is set here) and waits up to `timeout` for its answer.
    /// Err = refused by the endpoint, or never written (not sent); a written request without an
    /// answer is `Unanswered`, never Err.
    pub async fn submit(&self, mut req: Value, timeout: Duration) -> Result<Submitted> {
        req["id"] = json!(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (reply, answer_rx) = oneshot::channel();
        let state = Arc::new(AtomicU8::new(QUEUED));
        let ask = async {
            self.tx.send(BidRequest { req, reply, state: state.clone() }).await.map_err(|_| anyhow!("bid socket task ended"))?;
            answer_rx.await.map_err(|_| anyhow!("bid socket closed before the endpoint answered"))
        };
        let written = || state.load(Ordering::SeqCst) == WRITTEN;
        match tokio::time::timeout(timeout, ask).await {
            Ok(Ok(Reply::Answer(resp))) => answer(&resp).map(Submitted::Accepted),
            Ok(Ok(Reply::Lost(why))) => Ok(Submitted::Unanswered(why)),
            Ok(Err(e)) if written() => Ok(Submitted::Unanswered(e.to_string())),
            Ok(Err(e)) => Err(e.context("op not sent")),
            Err(_) => match state.compare_exchange(QUEUED, CANCELLED, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => bail!("bid endpoint not reachable within {} ms (op not sent)", timeout.as_millis()),
                Err(_) => Ok(Submitted::Unanswered(format!("no answer within {} ms", timeout.as_millis()))),
            },
        }
    }
}

/// The JSON-RPC id of an answer: a number, or the same number as a string.
fn answer_id(v: &Value) -> Option<u64> {
    let id = v.get("id")?;
    id.as_u64().or_else(|| id.as_str().and_then(|s| s.parse().ok()))
}

/// Hands an answer to its submission; one whose caller already gave up is logged instead.
fn deliver(p: Pending, v: Value) {
    if let Err(Reply::Answer(v)) = p.reply.send(Reply::Answer(v)) {
        match answer(&v) {
            Err(e) => warn!(auction = %p.auction, error = %e, "ATLAS solver op refused after its submission timed out"),
            Ok(r) => info!(auction = %p.auction, result = %r, "atlas solver op accepted after its submission timed out"),
        }
    }
}

/// One write, bounded: a stalled send must not stop reads, answers or the silence check.
async fn write_within<S: futures::Sink<Message> + Unpin>(sink: &mut S, m: Message, within: Duration) -> Result<(), &'static str> {
    match tokio::time::timeout(within, sink.send(m)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err("write failed"),
        Err(_) => Err("write stalled"),
    }
}

/// The bid socket's task: (re)connects, writes queued requests, routes answers by id.
async fn run_bid_socket(url: String, mut rx: mpsc::Receiver<BidRequest>, up: watch::Sender<bool>, t: BidTiming) {
    let mut backoff = Duration::ZERO;
    loop {
        if rx.is_closed() {
            return;
        }
        if !backoff.is_zero() {
            tokio::time::sleep(backoff).await;
        }
        let ws = match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                backoff = t.next_backoff(backoff);
                warn!(error = %e, backoff_ms = backoff.as_millis() as u64, "atlas bid socket connect failed");
                continue;
            }
        };
        let _ = up.send(true);
        info!("atlas bid socket connected");
        let since = Instant::now();
        let (mut sink, mut stream) = ws.split();
        let mut pending: HashMap<u64, Pending> = HashMap::new();
        let mut tick = tokio::time::interval(t.tick);
        let (mut last_rx, mut last_ping) = (Instant::now(), Instant::now());
        let why = loop {
            tokio::select! {
                r = rx.recv() => {
                    let Some(BidRequest { req, reply, state }) = r else { return };
                    // A caller that already timed out has cancelled it: never write it then.
                    if state.compare_exchange(QUEUED, WRITTEN, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                        continue;
                    }
                    let id = req["id"].as_u64().unwrap_or_default();
                    let auction = req["params"][0]["auction_id"].as_str().unwrap_or_default().to_string();
                    let text = req.to_string();
                    // Pending before the write: a failed write may still have reached the endpoint,
                    // so its caller hears `Lost` (counted as submitted), not "not sent".
                    pending.insert(id, Pending { reply, written: Instant::now(), auction });
                    if let Err(why) = write_within(&mut sink, Message::Text(text.into()), t.write_within).await {
                        break why;
                    }
                }
                m = stream.next() => {
                    last_rx = Instant::now();
                    match m {
                        // A ping is answered by tungstenite itself; only the time it came matters.
                        Some(Ok(Message::Text(text))) => {
                            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                            if let Some(p) = answer_id(&v).and_then(|id| pending.remove(&id)) {
                                deliver(p, v);
                            } else if v.get("error").is_some() {
                                if pending.len() == 1 {
                                    let id = *pending.keys().next().expect("one pending");
                                    deliver(pending.remove(&id).expect("one pending"), v);
                                } else {
                                    let e = v["error"].to_string().chars().take(200).collect::<String>();
                                    warn!(error = %e, in_flight = pending.len(), "ATLAS bid endpoint error not matched to a request");
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break "closed by the endpoint",
                        Some(Err(_)) => break "read failed",
                        Some(Ok(_)) => {}
                    }
                }
                _ = tick.tick() => {
                    if pending.values().any(|p| p.written.elapsed() > t.answer_within) {
                        break "no answer to a written request";
                    }
                    if last_rx.elapsed() > t.silent_for {
                        break "silent";
                    }
                    if last_ping.elapsed() >= t.ping_every {
                        last_ping = Instant::now();
                        if let Err(why) = write_within(&mut sink, Message::Ping(Vec::new().into()), t.write_within).await {
                            break why;
                        }
                    }
                }
            }
        };
        let _ = up.send(false);
        for (_, p) in pending.drain() {
            let _ = p.reply.send(Reply::Lost(format!("bid socket {why} after the op was written")));
        }
        backoff = if since.elapsed() >= t.stable_for { Duration::ZERO } else { t.next_backoff(backoff) };
        warn!(why, backoff_ms = backoff.as_millis() as u64, "atlas bid socket lost; reconnecting");
    }
}

/// What a submission needs besides the auction: the signer, the domain, the bid socket, and the
/// one-op-per-block rule (an EOA's second op in the same block is dropped by Atlas: use another
/// EOA for parallel auctions).
pub struct Submitter {
    pub signer: PrivateKeySigner,
    pub domain: Eip712Domain,
    pub atlas: Address,
    pub control: Address,
    pub socket: BidSocket,
    /// Most gas an op may declare (the DappControl's limit, read at start).
    pub gas_limit: u64,
    last_deadline: Mutex<u64>,
}

impl Submitter {
    pub fn new(signer: PrivateKeySigner, domain: Eip712Domain, atlas: Address, control: Address, socket: BidSocket, gas_limit: u64) -> Self {
        Self { signer, domain, atlas, control, socket, gas_limit, last_deadline: Mutex::new(0) }
    }

    /// Submits a signed op to its auction over the bid socket (`SUBMIT_TIMEOUT`).
    pub async fn submit(&self, auction_id: &str, op: &SignedSolverOp) -> Result<Submitted> {
        self.socket.submit(submit_request(auction_id, op), SUBMIT_TIMEOUT).await
    }

    /// Claims the slot for an op landing by `deadline`; false = this EOA already has one there.
    pub fn claim(&self, deadline: u64) -> bool {
        let mut last = self.last_deadline.lock().unwrap();
        if *last == deadline {
            return false;
        }
        *last = deadline;
        true
    }

    /// Gives the slot for `deadline` back (the op claimed for it was not accepted).
    pub fn unclaim(&self, deadline: u64) {
        let mut last = self.last_deadline.lock().unwrap();
        if *last == deadline {
            *last = 0;
        }
    }

    /// The op for an auction: `data` = the solver's `solverOpData`, `gas` = what it may use, the
    /// auction's own max fee and deadline, an ETH bid.
    #[allow(clippy::too_many_arguments)]
    pub fn op(&self, solver: Address, user_op_hash: B256, deadline: u64, max_fee_per_gas: u128, gas: u64, bid_wei: U256, data: Bytes) -> Result<SolverOperation> {
        anyhow::ensure!(gas <= self.gas_limit, "solver op gas {gas} > the DappControl's limit {}", self.gas_limit);
        Ok(SolverOperation {
            from: self.signer.address(),
            to: self.atlas,
            value: U256::ZERO,
            gas: U256::from(gas),
            maxFeePerGas: U256::from(max_fee_per_gas),
            deadline: U256::from(deadline),
            solver,
            control: self.control,
            userOpHash: user_op_hash,
            bidToken: Address::ZERO,
            bidAmount: bid_wei,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;

    sol! {
        /// The full op (with its signature), as AtlasVerification takes it.
        struct SolverOperationFull {
            address from;
            address to;
            uint256 value;
            uint256 gas;
            uint256 maxFeePerGas;
            uint256 deadline;
            address solver;
            address control;
            bytes32 userOpHash;
            address bidToken;
            uint256 bidAmount;
            bytes data;
            bytes signature;
        }
        interface IPayload {
            function getSolverPayload(SolverOperationFull op) external view returns (bytes32);
        }
    }

    const ATLAS: Address = address!("583dcFef0D240DC80753F0F0B26513feE27D9B77");
    const VERIFICATION: Address = address!("6e1886aEca75160BAa5610B7c1D3a895198C61cf");

    /// The op whose digest was read from AtlasVerification.getSolverPayload on Base.
    fn known_op() -> SolverOperation {
        SolverOperation {
            from: Address::with_last_byte(1),
            to: ATLAS,
            value: U256::ZERO,
            gas: U256::from(500_000u64),
            maxFeePerGas: U256::from(1_000u64),
            deadline: U256::from(100u64),
            solver: Address::with_last_byte(2),
            control: Address::with_last_byte(3),
            userOpHash: B256::repeat_byte(0x11),
            bidToken: Address::ZERO,
            bidAmount: U256::from(7u64),
            data: Bytes::from_static(&[0x12, 0x34]),
        }
    }

    fn base_domain() -> Eip712Domain {
        Eip712Domain::new(Some("AtlasVerification".into()), Some("1.6.4".into()), Some(U256::from(8453u64)), Some(VERIFICATION), None)
    }

    #[test]
    fn the_digest_is_atlas_verifications_solver_payload() {
        // AtlasVerification(0x6e18..).getSolverPayload(known_op) on Base, eth_call 2026-10-29.
        let expected: B256 = "0x3d82094946536f803345146b77b20a1646f1b9d6fef4a200f10f7c04aa1beab2".parse().unwrap();
        assert_eq!(digest(&known_op(), &base_domain()), expected);
    }

    #[test]
    fn ops_are_signed_by_the_solver_eoa_with_v_27_or_28_and_serialised_like_the_sdk() {
        let signer = PrivateKeySigner::random();
        let mut op = known_op();
        assert!(sign(op.clone(), &base_domain(), &signer).is_err(), "only the op's own `from` signs");
        op.from = signer.address();
        let s = sign(op.clone(), &base_domain(), &signer).unwrap();
        assert_eq!(s.signature.len(), 65);
        assert!(matches!(s.signature[64], 27 | 28));
        let sig = alloy::primitives::Signature::try_from(&s.signature[..]).unwrap();
        assert_eq!(sig.recover_address_from_prehash(&digest(&op, &base_domain())).unwrap(), signer.address());
        let r = submit_request("e1c063a1", &s);
        assert_eq!(r["method"], "solver_submitSolverOperation");
        let sol = &r["params"][0]["auction_solution"];
        assert_eq!((r["params"][0]["auction_id"].as_str(), sol["gas"].as_str(), sol["bidAmount"].as_str(), sol["data"].as_str()), (Some("e1c063a1"), Some("0x7a120"), Some("0x7"), Some("0x1234")));
        assert_eq!(sol["value"], "0x0");
    }

    #[tokio::test]
    async fn one_op_per_block_and_within_the_dapp_gas_limit() {
        let s = Submitter::new(PrivateKeySigner::random(), base_domain(), ATLAS, Address::with_last_byte(3), BidSocket::spawn("ws://127.0.0.1:1".into()), 6_000_000);
        assert!(s.claim(100) && !s.claim(100) && s.claim(101));
        // An op that was not accepted gives its slot back; an older deadline does not touch it.
        s.unclaim(100);
        assert!(!s.claim(101));
        s.unclaim(101);
        assert!(s.claim(101));
        assert!(s.op(Address::with_last_byte(2), B256::ZERO, 100, 1, 6_000_001, U256::ZERO, Bytes::new()).is_err());
        let op = s.op(Address::with_last_byte(2), B256::ZERO, 100, 1, 600_000, U256::from(5u8), Bytes::new()).unwrap();
        assert_eq!((op.from, op.to, op.bidToken, op.deadline), (s.signer.address(), ATLAS, Address::ZERO, U256::from(100u8)));
    }

    /// What the mock bid endpoint does with each request it reads, in order (over all connections).
    #[derive(Clone)]
    enum Step {
        /// Answer this JSON-RPC body (`{id}` is replaced by the request's id), after an unrelated frame.
        Answer(&'static str),
        /// The same, this many ms later.
        Late(&'static str, u64),
        /// Read it, never answer.
        Silent,
        /// Close the connection without answering.
        Close,
    }

    /// Short clocks, so a test sees an overdue answer, a backoff or a reconnect in well under a second.
    const FAST: BidTiming = BidTiming {
        tick: Duration::from_millis(20),
        ping_every: Duration::from_secs(1),
        silent_for: Duration::from_secs(5),
        answer_within: Duration::from_millis(300),
        write_within: Duration::from_secs(1),
        backoff_min: Duration::from_millis(100),
        backoff_max: Duration::from_millis(400),
        stable_for: Duration::from_secs(10),
    };

    /// A local WebSocket bid endpoint following `steps`; hands back every request it reads and the
    /// number of connections it accepted.
    async fn mock_bid_ws(steps: Vec<Step>) -> (String, mpsc::UnboundedReceiver<Value>, Arc<AtomicU64>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", l.local_addr().unwrap());
        let (seen, rx) = mpsc::unbounded_channel();
        let steps = Arc::new(Mutex::new(steps.into_iter()));
        let conns = Arc::new(AtomicU64::new(0));
        let counted = conns.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = l.accept().await {
                let (seen, steps) = (seen.clone(), steps.clone());
                counted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await else { return };
                    while let Some(Ok(m)) = ws.next().await {
                        let Message::Text(t) = m else { continue };
                        let req: Value = serde_json::from_str(&t).unwrap();
                        let id = req["id"].clone();
                        let _ = seen.send(req);
                        let step = steps.lock().unwrap().next().unwrap_or(Step::Silent);
                        let (body, after) = match step {
                            Step::Answer(body) => (body, 0),
                            Step::Late(body, ms) => (body, ms),
                            Step::Silent => continue,
                            Step::Close => {
                                let _ = ws.close(None).await;
                                return;
                            }
                        };
                        tokio::time::sleep(Duration::from_millis(after)).await;
                        let _ = ws.send(Message::Text(r#"{"jsonrpc":"2.0","method":"other","params":{}}"#.into())).await;
                        let _ = ws.send(Message::Text(body.replace("{id}", &id.to_string()).into())).await;
                    }
                });
            }
        });
        (url, rx, conns)
    }

    fn signed_known_op() -> SignedSolverOp {
        let signer = PrivateKeySigner::random();
        let mut op = known_op();
        op.from = signer.address();
        sign(op, &base_domain(), &signer).unwrap()
    }

    fn unanswered(r: Result<Submitted>) -> String {
        match r {
            Ok(Submitted::Unanswered(why)) => why,
            other => panic!("expected Unanswered, got {other:?}"),
        }
    }

    #[test]
    fn answers_null_or_the_string_null_are_accepted_and_errors_refused() {
        assert_eq!(answer(&json!({ "jsonrpc": "2.0", "id": 1, "result": null })).unwrap(), Value::Null);
        assert_eq!(answer(&json!({ "jsonrpc": "2.0", "id": 1, "result": "null" })).unwrap(), Value::Null);
        let err = answer(&json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32601, "message": "Method not found" } })).unwrap_err().to_string();
        assert!(err.contains("Method not found"), "{err}");
        assert_eq!((answer_id(&json!({ "id": 7 })), answer_id(&json!({ "id": "7" })), answer_id(&json!({ "id": null }))), (Some(7), Some(7), None));
    }

    #[tokio::test]
    async fn submission_goes_over_the_websocket_and_surfaces_a_refusal() {
        let s = signed_known_op();
        let (url, mut seen, _) = mock_bid_ws(vec![
            Step::Answer(r#"{"jsonrpc":"2.0","id":{id},"result":null}"#),
            Step::Answer(r#"{"jsonrpc":"2.0","id":{id},"error":{"code":-32602,"message":"Auction not found"}}"#),
        ])
        .await;
        let sock = BidSocket::spawn(url);
        assert!(sock.wait_connected(Duration::from_secs(5)).await && sock.is_connected());
        assert_eq!(sock.submit(submit_request("auction-1", &s), SUBMIT_TIMEOUT).await.unwrap(), Submitted::Accepted(Value::Null));
        let mut want = submit_request("auction-1", &s);
        let got = seen.recv().await.unwrap();
        want["id"] = got["id"].clone();
        assert_eq!(got, want, "exactly the signed op, over the socket");
        let err = sock.submit(submit_request("auction-1", &s), SUBMIT_TIMEOUT).await.unwrap_err().to_string();
        assert!(err.contains("Auction not found"), "{err}");
        assert_ne!(seen.recv().await.unwrap()["id"], got["id"], "each request has its own id");
    }

    #[tokio::test]
    async fn an_id_as_a_string_or_an_error_with_a_null_id_reaches_its_request() {
        let s = signed_known_op();
        let (url, _seen, _) = mock_bid_ws(vec![
            Step::Answer(r#"{"jsonrpc":"2.0","id":"{id}","result":null}"#),
            Step::Answer(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid request"}}"#),
        ])
        .await;
        let sock = BidSocket::spawn_with(url, FAST);
        assert!(sock.wait_connected(Duration::from_secs(5)).await);
        assert_eq!(sock.submit(submit_request("a", &s), SUBMIT_TIMEOUT).await.unwrap(), Submitted::Accepted(Value::Null));
        // The only request in flight: the refusal is its own, not a silent "unanswered".
        let err = sock.submit(submit_request("a", &s), SUBMIT_TIMEOUT).await.unwrap_err().to_string();
        assert!(err.contains("Invalid request"), "{err}");
    }

    #[tokio::test]
    async fn a_written_unanswered_op_counts_as_submitted_and_an_unsent_one_fails() {
        let s = signed_known_op();
        let (url, _seen, _) = mock_bid_ws(vec![Step::Silent]).await;
        let sock = BidSocket::spawn(url);
        assert!(sock.wait_connected(Duration::from_secs(5)).await);
        assert!(unanswered(sock.submit(submit_request("a", &s), Duration::from_millis(300)).await).contains("no answer within 300 ms"));
        // Nothing listens there: never connected, so never written.
        let down = BidSocket::spawn("ws://127.0.0.1:1".into());
        assert!(!down.is_connected());
        let err = down.submit(submit_request("a", &s), Duration::from_millis(300)).await.unwrap_err().to_string();
        assert!(err.contains("op not sent"), "{err}");
    }

    #[tokio::test]
    async fn an_op_written_before_the_endpoint_drops_the_socket_counts_as_submitted_then_it_reconnects() {
        let s = signed_known_op();
        let (url, _seen, conns) = mock_bid_ws(vec![Step::Close, Step::Answer(r#"{"jsonrpc":"2.0","id":{id},"result":"null"}"#)]).await;
        let sock = BidSocket::spawn_with(url, FAST);
        assert!(sock.wait_connected(Duration::from_secs(5)).await);
        // It may have reached the auction: Unanswered (slot and liability kept), not "not sent".
        let why = unanswered(sock.submit(submit_request("a", &s), SUBMIT_TIMEOUT).await);
        assert!(why.contains("closed by the endpoint after the op was written"), "{why}");
        assert_eq!(sock.submit(submit_request("a", &s), SUBMIT_TIMEOUT).await.unwrap(), Submitted::Accepted(Value::Null));
        assert_eq!(conns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_socket_that_takes_writes_but_never_answers_is_reconnected() {
        // A half-open connection: the write succeeds, nothing comes back.
        let s = signed_known_op();
        let (url, _seen, conns) = mock_bid_ws(vec![Step::Silent, Step::Answer(r#"{"jsonrpc":"2.0","id":{id},"result":null}"#)]).await;
        let sock = BidSocket::spawn_with(url, FAST);
        assert!(sock.wait_connected(Duration::from_secs(5)).await);
        let why = unanswered(sock.submit(submit_request("a", &s), Duration::from_secs(2)).await);
        assert!(why.contains("no answer to a written request"), "{why}");
        assert_eq!(sock.submit(submit_request("a", &s), SUBMIT_TIMEOUT).await.unwrap(), Submitted::Accepted(Value::Null));
        assert_eq!(conns.load(Ordering::SeqCst), 2, "reconnected after answer_within, not after the silence timeout");
    }

    #[tokio::test]
    async fn a_late_answer_does_not_disturb_the_socket() {
        let s = signed_known_op();
        let (url, _seen, conns) = mock_bid_ws(vec![
            Step::Late(r#"{"jsonrpc":"2.0","id":{id},"error":{"code":-32602,"message":"Auction not found"}}"#, 150),
            Step::Answer(r#"{"jsonrpc":"2.0","id":{id},"result":null}"#),
        ])
        .await;
        let sock = BidSocket::spawn_with(url, FAST);
        assert!(sock.wait_connected(Duration::from_secs(5)).await);
        // Given up on after 50 ms; the refusal at 150 ms is logged (no caller left), within answer_within.
        unanswered(sock.submit(submit_request("a", &s), Duration::from_millis(50)).await);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(sock.submit(submit_request("a", &s), SUBMIT_TIMEOUT).await.unwrap(), Submitted::Accepted(Value::Null));
        assert_eq!(conns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_endpoint_that_accepts_then_closes_is_reconnected_with_backoff() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", l.local_addr().unwrap());
        let conns = Arc::new(AtomicU64::new(0));
        let counted = conns.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = l.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await {
                    let _ = ws.close(None).await;
                }
            }
        });
        let _sock = BidSocket::spawn_with(url, FAST);
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        // 0, +100, +200, +400, +400 ms...: a handful, not a tight loop.
        let n = conns.load(Ordering::SeqCst);
        assert!((2..=6).contains(&n), "{n} connections in 1 s");
    }


    /// Network, no key or bond: a well-formed op signed by a random key for an auction that does
    /// not exist, sent to the real bid endpoint. It must be parsed (refused for the auction), not
    /// rejected for its format or method: the check the first live submissions lacked.
    #[tokio::test]
    #[ignore = "network: run with --ignored"]
    async fn e2e_bid_endpoint_parses_our_op_over_the_websocket() {
        let s = signed_known_op();
        let sock = BidSocket::spawn(SVR_BID_ENDPOINT.into());
        assert!(sock.wait_connected(Duration::from_secs(10)).await, "bid socket did not connect");
        let id = format!("{}", alloy::primitives::keccak256(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos().to_be_bytes()));
        let auction = format!("{}-{}-{}-{}-{}", &id[2..10], &id[10..14], &id[14..18], &id[18..22], &id[22..34]);
        let err = sock.submit(submit_request(&auction, &s), Duration::from_secs(5)).await.unwrap_err().to_string();
        assert!(!err.contains("Method not found") && !err.contains("Invalid parameter format"), "{err}");
        assert!(err.contains("Auction not found"), "{err}");
    }

    /// Read-only, public RPC: the domain read from the deployed contracts, and our digest of a
    /// random op equal to AtlasVerification.getSolverPayload of it.
    #[tokio::test]
    #[ignore = "network: run with --ignored"]
    async fn e2e_solver_op_digest_matches_atlas_verification() {
        let url = std::env::var("BASE_FORK_URL").unwrap_or_else(|_| "https://base.gateway.tenderly.co".into());
        let p = alloy::providers::ProviderBuilder::new().connect_http(url.parse().unwrap());
        let domain = read_domain(&p, ATLAS).await.unwrap();
        assert_eq!(domain.verifying_contract, Some(VERIFICATION));
        let mut op = known_op();
        let r = alloy::primitives::keccak256(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos().to_be_bytes());
        (op.userOpHash, op.bidAmount, op.data) = (r, U256::from(123_456_789u64), Bytes::from(alloy::primitives::keccak256(r).to_vec()));
        let full = SolverOperationFull { from: op.from, to: op.to, value: op.value, gas: op.gas, maxFeePerGas: op.maxFeePerGas, deadline: op.deadline, solver: op.solver, control: op.control, userOpHash: op.userOpHash, bidToken: op.bidToken, bidAmount: op.bidAmount, data: op.data.clone(), signature: Bytes::new() };
        let req = alloy::rpc::types::TransactionRequest::default().to(VERIFICATION).input(Bytes::from(IPayload::getSolverPayloadCall { op: full }.abi_encode()).into());
        let on_chain = IPayload::getSolverPayloadCall::abi_decode_returns(&p.call(req).await.unwrap()).unwrap();
        assert_eq!(digest(&op, &domain), on_chain);
        assert!(solver_gas_limit(&p, address!("a5E1a36938769cbd5a26f5e19D8FCB379f597c83")).await.unwrap() > 0);
    }
}
