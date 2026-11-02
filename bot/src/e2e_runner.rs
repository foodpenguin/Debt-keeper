//! End-to-end tests of the whole watch loop (`Runner`) on a LOCAL anvil fork of Base (never
//! mainnet): live sends with the per-tx cost cap, the ledger across a restart, the daily loss stop,
//! and an Atlas auction valued from a mock auction feed with the price model on the oracle logs.
//!
//! Run: cargo test e2e_runner -- --ignored --test-threads 1   (needs `anvil` on PATH and network
//! access: the fork, the Morpho API for the borrower list. BASE_FORK_URL optional.)

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use alloy::{
    network::{EthereumWallet, TransactionBuilder},
    node_bindings::{Anvil, AnvilInstance},
    primitives::{Address, B256, Bytes, U256, address, b256},
    providers::{Provider, ProviderBuilder, ext::AnvilApi},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol,
};

use crate::{
    config::Config,
    morpho::onchain,
    policy,
    runner::{Runner, RunnerInputs},
    sim,
};

sol! {
    #[sol(rpc)]
    interface ILiqAdmin2 { function approveMorpho(address token, bool allowed) external; }
    #[sol(rpc)]
    interface IOraclePrice2 { function price() external view returns (uint256); }
    #[sol(rpc)]
    interface IFeedProxy {
        function aggregator() external view returns (address);
        function latestRoundData() external view returns (uint80, int256, uint256, uint256, uint80);
    }
}

const MORPHO: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const WETH_MARKET: B256 = b256!("8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda");
/// Chainlink ETH/USD proxy on Base (the WETH/USDC market's base feed).
const ETH_USD_PROXY: Address = address!("71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70");
/// A WETH/USDC borrower the 10% drop makes liquidatable (the same one e2e.rs uses).
const WETH_BORROWER: Address = address!("33B5a89294e778d14e8Ce4b0828889B6b81b6488");

/// A mock of the Morpho GraphQL API listing only `borrowers` (the real list would make the fork load
/// hundreds of positions from the upstream RPC).
async fn mock_morpho_api(borrowers: Vec<Address>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let items: Vec<serde_json::Value> = borrowers.iter().map(|b| serde_json::json!({ "healthFactor": 1.05, "user": { "address": format!("{b:#x}") }, "state": { "borrowShares": "1" } })).collect();
    let body = serde_json::json!({ "data": { "marketPositions": { "items": items } } }).to_string();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = sock.read(&mut buf).await;
                let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

struct Harness {
    anvil: AnvilInstance,
    dir: PathBuf,
    morpho_api: String,
    exec_key: PrivateKeySigner,
    liq: Address,
}

impl Harness {
    /// An anvil fork mining a block a second, with a MorphoLiquidator deployed and set up like the
    /// owner would; `crash` makes WETH/USDC borrowers liquidatable (oracle code -> a constant price
    /// 10% lower).
    async fn new(name: &str, crash: bool) -> Self {
        let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).with_test_writer().try_init();
        let fork = std::env::var("BASE_FORK_URL").unwrap_or_else(|_| "https://base.gateway.tenderly.co".into());
        let anvil = Anvil::new().fork(fork).chain_id(8453).block_time(1).timeout(60_000).spawn();
        let owner_key: PrivateKeySigner = anvil.keys()[0].clone().into();
        let exec_key: PrivateKeySigner = anvil.keys()[1].clone().into();
        let receiver = anvil.addresses()[2];
        let owner_p = ProviderBuilder::new().wallet(EthereumWallet::from(owner_key.clone())).connect_http(anvil.endpoint_url());
        let artifact: serde_json::Value = serde_json::from_str(&std::fs::read_to_string("../contracts/out/MorphoLiquidator.sol/MorphoLiquidator.json").unwrap()).unwrap();
        let mut init: Vec<u8> = artifact["bytecode"]["object"].as_str().unwrap().parse::<Bytes>().unwrap().to_vec();
        for a in [MORPHO, owner_key.address(), exec_key.address(), receiver] {
            init.extend_from_slice(B256::left_padding_from(a.as_slice()).as_slice());
        }
        let rc = owner_p.send_transaction(TransactionRequest::default().with_deploy_code(Bytes::from(init))).await.unwrap().get_receipt().await.unwrap();
        let liq = rc.contract_address.expect("deployed");
        ILiqAdmin2::new(liq, &owner_p).approveMorpho(USDC, true).send().await.unwrap().get_receipt().await.unwrap();
        if crash {
            let meta = onchain::load_market_meta(&owner_p, MORPHO, WETH_MARKET, "WETH/USDC").await.unwrap();
            let p0 = IOraclePrice2::new(meta.params.oracle, &owner_p).price().call().await.unwrap();
            owner_p.anvil_set_code(meta.params.oracle, sim::constant_price_oracle(p0 * U256::from(9) / U256::from(10))).await.unwrap();
        }
        let dir = std::env::temp_dir().join(format!("e2e-runner-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let morpho_api = mock_morpho_api(vec![WETH_BORROWER]).await;
        Self { anvil, dir, morpho_api, exec_key, liq }
    }

    /// A config for this fork: live mode on the deployed liquidator, direct router only, every file
    /// in the test directory. `extra` is appended (sections or keys of [exec] via `exec_extra`).
    fn config(&self, live: bool, exec_extra: &str, extra: &str) -> PathBuf {
        let d = self.dir.display();
        let out = std::env::current_dir().unwrap().join("../contracts/out");
        let out = out.display();
        let text = format!(
            r#"chain_id = 8453
poll_ms = 500
journal_path = "{d}/journal.jsonl"
log_chunk = 1000

[[rpc]]
name = "anvil"
http = "{http}"
ws = "{ws}"
priority = 0

[exec]
mode = "{mode}"
executor = "{exec}"
liquidator = "{liq}"
liquidator_artifact = "{out}/MorphoLiquidator.sol/MorphoLiquidator.json"
kyber_api = "http://127.0.0.1:9"
kyber_client_id = "e2e"
slippage_bps = 50
priority_fee_wei = 1000000
replan_blocks = 1
max_plans_per_block = 3
# The fork answers slowly (state comes from the upstream RPC): room for a send to land.
deadline_secs = 30.0
eth_usd_fallback = 2400.0
min_net_usd = 0.01
min_wallet_eth = 0.0
extra_broadcast = []
kill_switch_file = "{d}/KILL"
nonce_file = "{d}/nonce"
ledger_file = "{d}/ledger"
max_inflight = 1
{exec_extra}

[router]
mode = "direct"
quoter_artifact = "{out}/GridQuoter.sol/GridQuoter.json"

[[router.factories]]
name = "uniV3"
address = "0x33128a8fC17869897dcE68Ed026d694621f6FDfD"
kind = "fee"
params = [500, 3000]

[morpho]
address = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb"
api = "{api}"

[[morpho.markets]]
name = "WETH/USDC"
id = "0x8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda"

[dashboard]
db_path = "{d}/metrics.db"
html_path = "{d}/dashboard.html"
log_path = "{d}/run.log"
live_path = "{d}/positions_live.json"
{extra}
"#,
            http = self.anvil.endpoint(),
            ws = self.anvil.ws_endpoint(),
            mode = if live { "live" } else { "shadow" },
            exec = self.exec_key.address(),
            liq = self.liq,
            api = self.morpho_api,
        );
        let path = self.dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn journal(&self) -> String {
        std::fs::read_to_string(self.dir.join("journal.jsonl")).unwrap_or_default()
    }

    /// Runs the watch loop until `done(journal)` or `secs` pass; returns the journal.
    async fn run(&self, cfg_path: &Path, live: bool, secs: u64, done: impl Fn(&str) -> bool) -> String {
        let cfg = Config::load(cfg_path.to_str().unwrap()).unwrap();
        let (auction, priority) = policy::defaults(&cfg);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let signer = live.then(|| self.exec_key.clone());
        let runner = Runner::build(RunnerInputs { cfg, signer, from_block: None, shutdown: Some(stop_rx) }, auction, priority).await.unwrap();
        // The loop and the watcher run on this task (the loop's future is not Send).
        let watcher = async {
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(secs) && !done(&self.journal()) {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let _ = stop_tx.send(true);
        };
        let (r, ()) = tokio::time::timeout(Duration::from_secs(secs + 60), async { tokio::join!(runner.run(), watcher) }).await.expect("the watch loop stops on shutdown");
        r.unwrap();
        self.journal()
    }
}

fn rows(journal: &str, kind: &str) -> Vec<serde_json::Value> {
    let tag = format!("\"kind\":\"{kind}\"");
    journal.lines().filter(|l| l.contains(&tag)).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

#[tokio::test]
#[ignore = "needs anvil + network; run with --ignored"]
async fn e2e_runner_live_send_settles_into_the_ledger_and_survives_a_restart() {
    let h = Harness::new("live", true).await;
    let cfg = h.config(true, "", "");
    let journal = h.run(&cfg, true, 150, |j| j.contains("\"success\":true")).await;
    let sent = rows(&journal, "sent");
    assert!(!sent.is_empty(), "a liquidation was sent:\n{}", journal.lines().filter(|l| l.contains("plan") || l.contains("refused")).take(5).collect::<Vec<_>>().join("\n"));
    let outcomes = rows(&journal, "outcome");
    assert!(outcomes.iter().any(|o| o["outcome"]["success"] == serde_json::json!(true)), "a send settled successfully: {outcomes:?}");
    // The ledger: written in live mode, with the tx's PnL (USDC paid to the receiver minus its gas).
    let ledger = std::fs::read_to_string(h.dir.join("ledger")).expect("ledger written");
    let (version, json) = ledger.split_once('\n').unwrap();
    assert_eq!(version, "v1");
    let stored: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    let pnl = stored["block_pnl_usd"].as_f64().unwrap();
    assert!(pnl > 0.0, "the liquidation's profit is in the day's PnL: {stored}");

    // A restart reads the ledger back: the day's PnL is not reset (a few blocks, no new send).
    let _ = h.run(&cfg, true, 8, |_| false).await;
    let after: serde_json::Value = serde_json::from_str(std::fs::read_to_string(h.dir.join("ledger")).unwrap().split_once('\n').unwrap().1.trim()).unwrap();
    assert!(after["block_pnl_usd"].as_f64().unwrap() >= pnl, "restart kept the day's PnL: {after}");
}

#[tokio::test]
#[ignore = "needs anvil + network; run with --ignored"]
async fn e2e_runner_tx_cost_cap_refuses_and_journals() {
    let h = Harness::new("cap", true).await;
    // One wei of headroom: no liquidation fits, every approved plan is refused, nothing is sent.
    let cfg = h.config(true, "max_tx_cost_eth = 0.000000000000000001", "");
    let journal = h.run(&cfg, true, 90, |j| j.contains("\"kind\":\"refused\"")).await;
    let refused = rows(&journal, "refused");
    assert!(!refused.is_empty(), "a refusal was journaled");
    assert_eq!(refused[0]["reason"], "tx_cost_cap");
    assert!(refused[0]["worst_eth"].as_f64().unwrap() > refused[0]["cap_eth"].as_f64().unwrap());
    assert!(rows(&journal, "sent").is_empty(), "nothing past the cap is sent");
}

#[tokio::test]
#[ignore = "needs anvil + network; run with --ignored"]
async fn e2e_runner_daily_loss_limit_writes_the_kill_switch() {
    let h = Harness::new("loss", true).await;
    // A limit below zero: the first block's PnL (0) is already "past" it.
    let cfg = h.config(true, "daily_loss_limit_usd = -1000.0", "");
    let journal = h.run(&cfg, true, 60, |j| j.contains("\"kind\":\"kill_switch\"")).await;
    assert!(!rows(&journal, "kill_switch").is_empty(), "the loss stop was journaled");
    let kill = std::fs::read_to_string(h.dir.join("KILL")).expect("kill switch file written");
    assert!(kill.contains("daily loss"), "{kill}");
    // From then on the risk gate refuses every plan.
    let after: Vec<serde_json::Value> = rows(&journal, "plan").into_iter().filter(|p| p["decision"]["action"] == "send").collect();
    let sent = rows(&journal, "sent");
    assert!(sent.len() <= after.len(), "no send without an approval");
}

/// A mock of the Chainlink SVR solver feed: accepts one connection, takes the subscription, sends
/// `msg`, then keeps the socket open.
async fn mock_auction_feed(msg: String) -> String {
    use futures::{SinkExt, StreamExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let msg = msg.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await else { return };
                let _ = ws.next().await; // solver_subscribe
                // Give the loop a block or two so it has snapshots to value against.
                tokio::time::sleep(Duration::from_secs(4)).await;
                let _ = ws.send(tokio_tungstenite::tungstenite::Message::Text(msg.into())).await;
                while ws.next().await.is_some() {}
            });
        }
    });
    format!("ws://{addr}")
}

#[tokio::test]
#[ignore = "needs anvil + network; run with --ignored"]
async fn e2e_runner_values_an_atlas_auction_without_flashblocks() {
    // No crash: the price model must reproduce the real oracle, and the auction's median moves it.
    let h = Harness::new("atlas", false).await;
    let p = ProviderBuilder::new().connect_http(h.anvil.endpoint_url());
    let proxy = IFeedProxy::new(ETH_USD_PROXY, &p);
    let aggregator = proxy.aggregator().call().await.unwrap();
    let answer = proxy.latestRoundData().call().await.unwrap()._1;
    let median = answer.into_raw() * U256::from(85) / U256::from(100); // ETH -15%
    let deadline = p.get_block_number().await.unwrap() + 100;
    let msg = serde_json::json!({ "jsonrpc": "2.0", "method": "solver_subscription", "params": { "result": {
        "auction_id": "e2e-auction",
        "partial_user_operation": {
            "chainId": "0x2105", "control": "0xa5E1a36938769cbd5a26f5e19D8FCB379f597c83",
            "deadline": format!("{deadline:#x}"), "maxFeePerGas": "0x3b9aca00",
            "userOpHash": format!("{:#x}", B256::repeat_byte(0x11)),
            "hints": { "aggregator": format!("{aggregator:#x}"), "medianPrice": format!("{median:#x}") }
        }
    }}})
    .to_string();
    let feed = mock_auction_feed(msg).await;
    let cfg = h.config(false, "", &format!("[flashblocks]\nenabled = false\n\n[atlas]\nobserve = true\nws = \"{feed}\"\nmax_concurrent = 2\n"));
    let journal = h.run(&cfg, false, 120, |j| j.contains("\"kind\":\"atlas_auction\"")).await;
    let auctions = rows(&journal, "atlas_auction");
    assert_eq!(auctions.len(), 1, "the auction was valued: {journal:.0}");
    let a = &auctions[0];
    assert_eq!(a["auction_id"], "e2e-auction");
    assert!(a["liquidatable"].as_u64().unwrap() > 0, "a 15% ETH drop makes WETH/USDC positions liquidatable: {a}");
    for k in ["queued_ms", "value_ms", "bundle_ms"] {
        assert!(!a[k].is_null(), "{k} journaled: {a}");
    }
    // The oracle logs subscription is up (the loop reached the model with a WebSocket endpoint).
    assert!(!journal.contains("Atlas path off"));
}

#[tokio::test]
#[ignore = "needs anvil + network; run with --ignored"]
async fn e2e_runner_block_path_bids_against_the_seeded_competition() {
    let h = Harness::new("seed", true).await;
    // Complete liquidation rows of someone else (gross USD, block time, the tx's `to`) seed the
    // competition model at start; the block path then bids against it (competitor_wei is set).
    let now = crate::gates::unix_secs();
    let rows_text: String = (0..5u8)
        .map(|i| {
            serde_json::json!({ "kind": "liquidation", "block": 1, "timestamp": now - 60, "tx": format!("{:#x}", B256::repeat_byte(i + 1)), "tx_index": 0,
                "market": "WETH/USDC", "borrower": format!("{:#x}", Address::repeat_byte(i + 1)), "liquidator": format!("{:#x}", Address::repeat_byte(0xee)),
                "eoa": format!("{:#x}", Address::repeat_byte(0xef)), "priority_fee_gwei": 0.5, "fee_usd": 0.1, "repaid": 1000.0, "seized_value": 1100.0,
                "gross_bonus": 100.0, "gross_usd": 100.0, "tx_to": format!("{:#x}", Address::repeat_byte(0xee)), "bad_debt": 0.0, "outcome": "untracked",
                "our_first_block": null, "our_candidate_block": null, "delay_blocks": null, "first_hf": null, "prev_block_hf": null,
                "our_plan_block": null, "our_plan_net_usd": null, "our_plan_error": null })
            .to_string()
                + "\n"
        })
        .collect();
    std::fs::write(h.dir.join("journal.jsonl"), rows_text).unwrap();
    let cfg = h.config(false, "bid_min_samples = 3", "");
    // The fork drops a request now and then (a plan that errors is retried next block).
    let journal = h.run(&cfg, false, 120, |j| j.contains("\"action\":\"send\"")).await;
    let plans = rows(&journal, "plan");
    let send = plans.iter().find(|p| p["decision"]["action"] == "send").unwrap_or_else(|| panic!("an approved plan: {plans:?}"));
    assert!(!send["decision"]["competitor_wei"].is_null(), "the seeded competition is in the bid: {send}");
}
