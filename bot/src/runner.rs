//! The watch loop as a library: `Runner` connects to the configured endpoints, builds the Morpho
//! engine and runs the block path plus the Atlas auction path. A binary only parses its CLI, loads
//! the signer before the async runtime exists (`load_signer_pre_runtime`) and calls
//! `Runner::build(..).run()`; the diagnostic commands (`scan`, `plan`, `value-auction`) are methods
//! on the same `Runner`.

use std::{
    collections::HashMap,
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::state::{AccountOverride, StateOverride},
};
use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::sync::watch;
use tracing::{info, warn};

use alloy::sol;

use crate::{
    actor::{self, ExecHandle},
    bid::WinningBids,
    atlas, atlas_submit, atlas_venue, config, dex, executor, flash_venue, funding, gates, heads, live, loss, morpho, oracle, oracle_logs, planner, protocol, risk, rpc, sim, v3router, venue,
    config::Config,
    engine::JournalRow,
    heads::Head,
    morpho::{Morpho, MorphoEngine, PositionView, evaluate_position, math, onchain::MarketMeta},
    planner::{PlanCtx, Planner},
    router::Kyber,
    policy::{AuctionBidPolicy, PriorityBidPolicy},
    ledger::Ledger,
    sim::{LiquidatorCode, Simulator},
};

/// Loads the live signer from `exec.keystore`. Call it BEFORE the async runtime exists: the
/// password may come from an env var that is removed after reading, and mutating the environment is
/// only sound while single-threaded.
pub fn load_signer_pre_runtime(cfg: &Config) -> Result<alloy::signers::local::PrivateKeySigner> {
    let ks = cfg.exec.keystore.clone().context("live mode needs exec.keystore")?;
    executor::load_signer(&ks, &cfg.exec.keystore_password_env)
}

/// What `Runner::build` needs besides the endpoints in the config.
pub struct RunnerInputs {
    pub cfg: Config,
    /// The live signer (`load_signer_pre_runtime`); None = shadow mode.
    pub signer: Option<alloy::signers::local::PrivateKeySigner>,
    /// `run` replays from this past block (needs an RPC with historical state).
    pub from_block: Option<u64>,
    /// `run` returns once this turns true (tests; None = run until an error).
    pub shutdown: Option<tokio::sync::watch::Receiver<bool>>,
}

/// The connected bot: RPC pools, the Morpho engine and, in live mode, the signer and its broadcast
/// targets.
pub struct Runner<P> {
    engine: MorphoEngine<P>,
    pool: rpc::RpcPool,
    ws: Vec<(String, String)>,
    live: Option<(alloy::signers::local::PrivateKeySigner, Vec<(String, String)>)>,
    /// Valuation endpoints of the Atlas path (`[atlas] rpc`), when its own.
    atlas_pool: Option<(P, rpc::RpcPool)>,
    from_block: Option<u64>,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    auction: Arc<dyn AuctionBidPolicy>,
    priority: Arc<dyn PriorityBidPolicy>,
}

impl Runner<()> {
    /// Connects to the configured endpoints and loads the markets. `auction` and `priority` decide
    /// the bids (`policy::defaults` for the `[atlas]` margin and the `[exec]` bid curve).
    pub async fn build(inputs: RunnerInputs, auction: impl AuctionBidPolicy, priority: impl PriorityBidPolicy) -> Result<Runner<impl Provider + Clone + 'static>> {
        let RunnerInputs { cfg, signer, from_block, shutdown } = inputs;
        let resolved = cfg.resolve_rpc()?;
        info!(
            http = ?resolved.http.iter().map(|e| format!("{}(p{})", e.name, e.priority)).collect::<Vec<_>>(),
            ws = ?resolved.ws.iter().map(|w| w.0.as_str()).collect::<Vec<_>>(),
            "rpc endpoints (urls hidden)"
        );
        // Raw txs go to every RPC endpoint plus the extra broadcast URLs (e.g. the sequencer).
        // A local fork (every endpoint on loopback) never broadcasts anywhere else: the extra URLs
        // (default: the Base sequencer) would put test txs on mainnet.
        let local_only = resolved.http.iter().all(|e| is_loopback_url(&e.url));
        if local_only && !cfg.exec.extra_broadcast.is_empty() {
            warn!("all rpc endpoints are local: exec.extra_broadcast ignored");
        }
        let extra: &[String] = if local_only { &[] } else { &cfg.exec.extra_broadcast };
        let broadcast: Vec<(String, String)> = resolved
            .http
            .iter()
            .map(|e| (e.name.clone(), e.url.clone()))
            .chain(extra.iter().enumerate().map(|(i, u)| (format!("extra{i}"), u.clone())))
            .collect();
        // Denim removes Flashblocks: the same-block path is not part of the runner (tag legacy-final
        // of the old bot has it).
        anyhow::ensure!(!cfg.flashblocks.enabled, "[flashblocks] enabled: the same-block path was removed (Base Denim drops Flashblocks); set it to false");
        // Atlas valuations on their own endpoints, in strict preference order, when configured.
        let atlas_specs: Vec<rpc::EndpointSpec> = cfg
            .atlas
            .rpc
            .iter()
            .map(|n| resolved.http.iter().find(|e| &e.name == n).with_context(|| format!("[atlas] rpc names {n:?}, not a configured [[rpc]] endpoint")))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .enumerate()
            .map(|(i, e)| rpc::EndpointSpec { name: e.name.clone(), url: e.url.clone(), priority: i as u32, skip_methods: e.skip_methods.clone(), max_log_blocks: e.max_log_blocks })
            .collect();
        // The oracle logs WebSocket: `[atlas] logs_ws` first, then the head endpoints.
        let mut ws = resolved.ws.clone();
        if !cfg.atlas.logs_ws.is_empty() {
            let i = ws.iter().position(|w| w.0 == cfg.atlas.logs_ws).with_context(|| format!("[atlas] logs_ws = {:?}: no [[rpc]] endpoint of that name has a ws url", cfg.atlas.logs_ws))?;
            ws.swap(0, i);
        }
        let pool = rpc::RpcPool::new(resolved.http)?;
        let provider = ProviderBuilder::new().connect_client(alloy::rpc::client::RpcClient::new(pool.clone(), false));
        let atlas_pool = if atlas_specs.is_empty() {
            None
        } else {
            info!(rpc = ?atlas_specs.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), "atlas valuation endpoints");
            let p = rpc::RpcPool::new(atlas_specs)?;
            Some((ProviderBuilder::new().connect_client(alloy::rpc::client::RpcClient::new(p.clone(), false)), p))
        };
        let morpho = Morpho::from_config(&cfg);
        let engine = MorphoEngine::new(provider, cfg, morpho).await?;
        // Profits, gas trade-offs and bids are in USD: a market whose loan token has no price model
        // would be silently mispriced, so it is refused here.
        let unpriced: Vec<String> = engine.metas.iter().filter(|m| morpho::loan_usd_model(engine.cfg.chain_id, m.params.loanToken, 1.0).is_none()).map(|m| format!("{} (loan {})", m.name, m.loan_symbol)).collect();
        anyhow::ensure!(unpriced.is_empty(), "markets with a loan token the bot cannot price in USD (Base USD stablecoins and WETH, by address): {}", unpriced.join(", "));
        Ok(Runner { engine, pool, ws, live: signer.map(|s| (s, broadcast)), atlas_pool, from_block, shutdown, auction: Arc::new(auction), priority: Arc::new(priority) })
    }
}

impl<P: Provider + Clone + Send + Sync + 'static> Runner<P> {
    /// The watch loop: every block (and every SVR auction) until an error. Shadow mode unless the
    /// inputs carried a signer.
    pub async fn run(mut self) -> Result<()> {
        watch(&mut self.engine, self.from_block, self.pool, self.ws, self.live, self.atlas_pool, self.auction, self.priority, self.shutdown).await
    }

    /// One-shot risk summary of every tracked position, checked against the Morpho API.
    #[doc(hidden)]
    pub async fn scan(&mut self, top: usize) -> Result<()> {
        scan(&mut self.engine, top).await
    }

    /// Plans and simulates one position (diagnostic).
    #[doc(hidden)]
    pub async fn plan(&mut self, market: &str, user: Address, drop_bps: u64, at_block: Option<u64>, pending: bool) -> Result<()> {
        plan_one(&mut self.engine, market, user, drop_bps, at_block, pending).await
    }

    /// Replays one market's Atlas auction valuation at a past block (diagnostic).
    #[doc(hidden)]
    pub async fn value_auction(&mut self, market: &str, drop_bps: u64, at_block: u64, users: &[Address], use_kyber: bool) -> Result<()> {
        value_auction_at(&mut self.engine, market, drop_bps, at_block, users, use_kyber, self.auction.clone(), self.priority.clone()).await
    }
}

/// http://127.0.0.1:8545, http://localhost:..., ws://[::1]... (anvil forks).
fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |x| x.1);
    let host = if host.starts_with('[') { host.split(']').next().unwrap_or("").trim_start_matches('[') } else { host.split(':').next().unwrap_or("") };
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Builds the direct router and discovers paths for every tracked market.
async fn build_v3<P: Provider>(engine: &MorphoEngine<P>) -> Result<v3router::V3Router> {
    let rc = &engine.cfg.router;
    let mut v3 = v3router::V3Router::new(rc.factories.clone(), rc.connectors.clone(), &rc.quoter_artifact, rc.max_paths, rc.grid_points)?;
    if rc.factories.is_empty() {
        warn!("no [[router.factories]] configured: direct router disabled, aggregator only");
    }
    // One route set per (collateral, debt) pair any tracked market may be liquidated on.
    for m in &engine.metas {
        for pair in protocol::LendingProtocol::pairs(&engine.protocol, m) {
            if v3.routes.contains_key(&pair) {
                continue;
            }
            let labels = v3.discover(&engine.provider, pair).await?.labels();
            info!(market = %m.name, legs = labels.len(), ?labels, "direct routes discovered");
        }
    }
    Ok(v3)
}

/// Seconds per block over the 10 blocks before `h` (the one-off commands; watch measures it
/// continuously, BlockClock).
async fn recent_block_time<P: Provider>(p: &P, h: &Head) -> f64 {
    match heads::http_head(p, BlockNumberOrTag::Number(h.number.saturating_sub(10))).await {
        Ok(b) if h.number > b.number && h.timestamp >= b.timestamp => (h.timestamp - b.timestamp) as f64 / (h.number - b.number) as f64,
        _ => 2.0,
    }
}

/// The liquidation contract the planner encodes for and simulates (`exec.venue`).
enum Venue {
    /// MorphoLiquidator: Morpho funds the repayment inside its liquidate callback.
    Callback(Simulator),
    /// FlashLiquidator: the repayment is flash-borrowed first (Morpho flash loan or V4).
    Flash(flash_venue::FlashLiquidatorVenue<Morpho>),
}

impl venue::LiquidationVenue<Morpho> for Venue {
    fn target(&self) -> Address {
        match self {
            Venue::Callback(v) => venue::LiquidationVenue::<Morpho>::target(v),
            Venue::Flash(v) => v.target(),
        }
    }
    fn packed_routes(&self) -> bool {
        match self {
            Venue::Callback(v) => venue::LiquidationVenue::<Morpho>::packed_routes(v),
            Venue::Flash(v) => v.packed_routes(),
        }
    }
    fn swap_recipient(&self) -> Address {
        match self {
            Venue::Callback(v) => venue::LiquidationVenue::<Morpho>::swap_recipient(v),
            Venue::Flash(v) => v.swap_recipient(),
        }
    }
    fn encode(&self, m: &MarketMeta, user: Address, pair: protocol::Pair, seize: U256, repay: U256, route: &dex::ExitRoute, min_profit: U256, deadline_block: u64) -> executor::Payload {
        match self {
            Venue::Callback(v) => v.encode(m, user, pair, seize, repay, route, min_profit, deadline_block),
            Venue::Flash(v) => v.encode(m, user, pair, seize, repay, route, min_profit, deadline_block),
        }
    }
    async fn simulate<P: Provider>(&self, p: &P, calldata: alloy::primitives::Bytes, block: alloy::eips::BlockId, overrides: &StateOverride, at: Option<venue::SimAt>, fee: Option<venue::SimFee>, probe: Option<Address>) -> Result<venue::SimResult> {
        match self {
            Venue::Callback(v) => venue::LiquidationVenue::<Morpho>::simulate(v, p, calldata, block, overrides, at, fee, probe).await,
            Venue::Flash(v) => v.simulate(p, calldata, block, overrides, at, fee, probe).await,
        }
    }
    async fn probe_gas<P: Provider>(&self, p: &P, calldata: alloy::primitives::Bytes, block: alloy::eips::BlockId, overrides: &StateOverride, limits: &[u64]) -> Vec<(u64, bool)> {
        match self {
            Venue::Callback(v) => venue::LiquidationVenue::<Morpho>::probe_gas(v, p, calldata, block, overrides, limits).await,
            Venue::Flash(v) => v.probe_gas(p, calldata, block, overrides, limits).await,
        }
    }
}

/// Exit router clients + the liquidation venue. Without a deployed contract, it is simulated by
/// running its constructor in an eth_call and injecting the result (for MorphoLiquidator, with the
/// standing Morpho allowance on every loan token, from the discovered storage slot).
async fn build_planner_parts<P: Provider>(engine: &MorphoEngine<P>) -> Result<(Kyber, Venue)> {
    let cfg = &engine.cfg;
    let kyber = Kyber::new(engine.http.clone(), &cfg.exec.kyber_api, &cfg.exec.kyber_client_id);
    if cfg.exec.venue == "flash" {
        return Ok((kyber, Venue::Flash(build_flash_venue(engine).await?)));
    }
    let code = match cfg.exec.liquidator {
        None => Some(LiquidatorCode::build(&engine.provider, &cfg.exec.liquidator_artifact, &[engine.protocol.address, cfg.exec.executor, cfg.exec.executor, cfg.exec.executor], cfg.exec.executor).await?),
        Some(_) => None,
    };
    let mut sim = Simulator::new(cfg.exec.liquidator, cfg.exec.executor, code, cfg.exec.allowed_targets.clone());
    match cfg.exec.liquidator {
        None => inject_morpho_approvals(engine, &mut sim).await?,
        // Deployed: the owner must have called approveMorpho(token, true).
        Some(liq) => {
            for token in loan_tokens(engine) {
                if !sim::has_standing_allowance(&engine.provider, token, liq, engine.protocol.address).await? {
                    warn!(%token, "liquidator has no standing Morpho allowance: call approveMorpho(token, true) as owner");
                }
            }
        }
    }
    Ok((kyber, Venue::Callback(sim)))
}

/// Every distinct loan token of the tracked markets.
fn loan_tokens<P: Provider>(engine: &MorphoEngine<P>) -> Vec<Address> {
    let mut loans: Vec<Address> = engine.metas.iter().map(|m| m.params.loanToken).collect();
    loans.sort();
    loans.dedup();
    loans
}

/// An injected MorphoLiquidator also needs the standing Morpho allowance the owner sets on the real
/// one (`approveMorpho`): found per loan token by probing its allowance storage slot.
async fn inject_morpho_approvals<P: Provider>(engine: &MorphoEngine<P>, sim: &mut Simulator) -> Result<()> {
    for token in loan_tokens(engine) {
        let key = sim::discover_allowance_key(&engine.provider, token, sim.liquidator, engine.protocol.address).await?;
        sim.add_approval(token, key);
    }
    Ok(())
}

/// Atlas's gas surcharge on solver ops, read from the escrow (`getAtlasSurchargeRate`, scale 1e4);
/// the configured `atlas.gas_surcharge` when Atlas is off or the read fails.
async fn atlas_surcharge<P: Provider>(engine: &MorphoEngine<P>) -> f64 {
    let fallback = engine.cfg.atlas.gas_surcharge;
    if !engine.cfg.atlas.observe {
        return fallback;
    }
    use alloy::sol_types::SolCall;
    let tx = alloy::rpc::types::TransactionRequest::default().to(engine.cfg.atlas.address).input(IAtlasRates::getAtlasSurchargeRateCall {}.abi_encode().into());
    match engine.provider.call(tx).await.ok().and_then(|out| IAtlasRates::getAtlasSurchargeRateCall::abi_decode_returns(&out).ok()) {
        Some(rate) => {
            let s = math::u256_f64(rate) / 1e4;
            info!(surcharge = s, "atlas: gas surcharge read from the escrow");
            s
        }
        None => {
            warn!(fallback, "atlas: could not read the gas surcharge; using atlas.gas_surcharge");
            fallback
        }
    }
}

/// The Atlas solver venue (`[atlas.solver] simulate`): the solver and its MorphoLiquidator are
/// injected unless deployed, and Atlas is replaced by its stand-in in every simulation.
async fn build_atlas_solver<P: Provider>(engine: &MorphoEngine<P>) -> Result<Option<atlas_venue::AtlasSolver>> {
    let c = &engine.cfg.atlas.solver;
    if !c.simulate {
        return Ok(None);
    }
    let (e, p, morpho) = (&engine.cfg.exec, &engine.provider, engine.protocol.address);
    let atlas = engine.cfg.atlas.address;
    let signer = c.signer.unwrap_or(e.executor);
    let solver = c.address.unwrap_or(atlas_venue::SIM_SOLVER);
    let solver_code = match c.address {
        None => Some(LiquidatorCode::build(p, &c.artifact, &[atlas, c.weth, signer, signer, signer], signer).await?),
        Some(_) => None,
    };
    let liq_code = match c.liquidator {
        None => Some(LiquidatorCode::build(p, &e.liquidator_artifact, &[morpho, solver, solver, solver], solver).await?),
        Some(_) => None,
    };
    let mut liq = Simulator::new(c.liquidator, solver, liq_code, e.allowed_targets.clone());
    if c.liquidator.is_none() {
        inject_morpho_approvals(engine, &mut liq).await?;
    }
    let art: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&c.stub_artifact).with_context(|| format!("reading {} (run `forge build` in contracts/)", c.stub_artifact))?)?;
    let stub: alloy::primitives::Bytes = art["deployedBytecode"]["object"].as_str().context("atlas stub: no deployedBytecode")?.parse()?;
    info!(%solver, %signer, "atlas: solver operations are simulated (never signed or sent)");
    Ok(Some(atlas_venue::AtlasSolver::new(atlas, c.address, signer, c.weth, c.bid_router, c.bid_fee, liq, solver_code, stub)))
}

/// FlashLiquidator over Morpho: the repayment is flash-borrowed (exec.flash_funding) and Morpho's
/// `liquidate` is called without callback data. Morpho itself must be an allowed protocol.
async fn build_flash_venue<P: Provider>(engine: &MorphoEngine<P>) -> Result<flash_venue::FlashLiquidatorVenue<Morpho>> {
    let e = &engine.cfg.exec;
    let morpho = engine.protocol.address;
    let pool_manager = e.pool_manager.unwrap_or(funding::BASE_POOL_MANAGER);
    let source: Box<dyn funding::FlashSource> = match e.flash_funding.as_str() {
        "v4" => Box::new(funding::UniswapV4 { pool_manager }),
        _ => Box::new(funding::MorphoBlue { address: morpho }),
    };
    // The contract repays exactly what it borrowed, so a source with a fee would revert every time.
    anyhow::ensure!(source.fee(U256::from(1_000_000_000_000_000_000u128)).is_zero(), "flash source {} charges a fee: FlashLiquidator only supports 0-fee sources", source.name());
    let code = match e.flash_liquidator {
        None => Some(LiquidatorCode::build(&engine.provider, &e.flash_liquidator_artifact, &[morpho, pool_manager, e.executor, e.executor, e.executor], e.executor).await?),
        Some(_) => None,
    };
    if let Some(c) = &code {
        anyhow::ensure!(c.has_slot("allowedProtocol"), "{}: no allowedProtocol in storageLayout (rebuild contracts/)", e.flash_liquidator_artifact);
    }
    info!(funding = source.name(), source = %source.holder(), headroom_bps = e.flash_headroom_bps, "venue: FlashLiquidator");
    Ok(flash_venue::FlashLiquidatorVenue::new(engine.protocol.clone(), e.flash_liquidator, e.executor, source, e.flash_headroom_bps, code, vec![morpho], e.allowed_targets.clone()))
}

async fn plan_one<P: Provider>(engine: &mut MorphoEngine<P>, market: &str, user: Address, drop_bps: u64, at_block: Option<u64>, pending: bool) -> Result<()> {
    let mi = engine.market_by_name(market).with_context(|| format!("market {market} not in config"))?;
    let tag = at_block.map_or(BlockNumberOrTag::Latest, BlockNumberOrTag::Number);
    anyhow::ensure!(!(pending && at_block.is_some()), "--pending and --at-block are exclusive");
    if at_block.is_some() && engine.cfg.router.mode != "direct" {
        bail!("--at-block needs router.mode = \"direct\": aggregators can only quote the current state");
    }
    let h = heads::http_head(&engine.provider, tag).await?;
    let bt = recent_block_time(&engine.provider, &h).await;
    engine.reload(&[(mi, user)], h.number.into()).await?;
    let Some(&(shares, collateral)) = engine.positions.get(&(mi, user)) else { bail!("{user} has no debt in {market}") };
    let snaps = engine.snapshots(h.number.into(), h.timestamp + engine.cfg.lookahead_secs(bt)).await?;
    let eth_usd = engine.eth_usd(&snaps);
    let mut snap = snaps[mi].clone().context("snapshot failed")?;
    let meta = &engine.metas[mi];

    let mut overrides = StateOverride::default();
    if drop_bps > 0 {
        snap.price = snap.price * U256::from(10_000 - drop_bps) / U256::from(10_000u64);
        overrides.insert(meta.params.oracle, AccountOverride { code: Some(sim::constant_price_oracle(snap.price)), ..Default::default() });
    }
    let view = evaluate_position(&engine.protocol, meta, &snap, mi, user, shares, collateral);
    println!(
        "{} {:#x} | HF {:.4} | debt {:.2} {} | collateral {:.6} {} | oracle price drop {} bps",
        meta.name,
        user,
        view.hf,
        view.debt,
        meta.loan_symbol,
        math::units(collateral, meta.collateral_decimals),
        meta.collateral_symbol,
        drop_bps
    );
    if !view.health.is_liquidatable() {
        println!("not liquidatable (try a larger --price-drop-bps)");
        return Ok(());
    }
    println!(
        "max seize {:.6} {} | oracle-price bonus {:.2} {}",
        math::units(view.max_seize, meta.collateral_decimals),
        meta.collateral_symbol,
        view.bonus,
        meta.loan_symbol
    );
    let (kyber, simulator) = build_planner_parts(engine).await?;
    let v3 = build_v3(engine).await?;
    let meta = &engine.metas[mi];
    let planner = Planner { protocol: &engine.protocol, aggregator: &kyber, venue: &simulator, cfg: &engine.cfg.exec, v3: Some(&v3), mode: &engine.cfg.router.mode };
    let deadline_block = h.number + gates::deadline_blocks(engine.cfg.exec.deadline_secs, bt);
    let sim_time = h.timestamp + engine.cfg.lookahead_secs(bt);
    let ctx = PlanCtx { meta, snap: &snap, view: &view, block: h.number, base_fee_wei: h.base_fee, eth_usd, overrides, deadline_block, sim_time, pending, price_probe: None };
    let plan = planner.plan(&engine.provider, &ctx).await?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}

async fn scan<P: Provider>(engine: &mut MorphoEngine<P>, top: usize) -> Result<()> {
    let h = heads::http_head(&engine.provider, BlockNumberOrTag::Latest).await?;
    let t0 = Instant::now();
    engine.refresh_borrowers(h.number.into()).await?;
    let load_ms = t0.elapsed().as_millis() as u64;
    let t1 = Instant::now();
    let snaps = engine.snapshots(h.number.into(), h.timestamp).await?;
    let views = engine.evaluate(&snaps);
    info!(block = h.number, positions = views.len(), load_ms, per_block_ms = t1.elapsed().as_millis() as u64, "evaluated");
    if let Err(err) = live::write_snapshot(&engine.cfg.dashboard, &engine.metas, &snaps, &engine.positions, &views, h.number, h.timestamp) {
        warn!(error = %err, "live snapshot write failed");
    }

    for (mi, meta) in engine.metas.iter().enumerate() {
        let Some(snap) = &snaps[mi] else {
            warn!(market = %meta.name, "snapshot failed");
            continue;
        };
        let mv: Vec<&PositionView> = views.iter().filter(|v| v.market == mi).collect();
        let debt: f64 = mv.iter().map(|v| v.debt).sum();
        let liq = mv.iter().filter(|v| v.health.is_liquidatable()).count();
        let near = |h: f64| mv.iter().filter(|v| v.hf < h).count();
        println!(
            "\n== {} | price {:.6} {}/{} | borrowers {} | debt {:.0} | liquidatable {} | HF<1.02 {} | HF<1.05 {} | HF<1.10 {}",
            meta.name,
            price_human(snap.price, meta),
            meta.loan_symbol,
            meta.collateral_symbol,
            mv.len(),
            debt,
            liq,
            near(1.02),
            near(1.05),
            near(1.10)
        );
        for v in mv.iter().take(top) {
            let api = engine.api_hf.get(&(mi, v.user)).map(|h| format!("{h:.4}")).unwrap_or("-".into());
            let liq = if v.health.is_liquidatable() {
                format!(
                    "  LIQUIDATABLE seize {:.6} {} est bonus {:.2} {}",
                    math::units(v.max_seize, meta.collateral_decimals),
                    meta.collateral_symbol,
                    v.bonus,
                    meta.loan_symbol
                )
            } else {
                String::new()
            };
            println!("   {:#x}  HF {:.4} (api {:>7})  debt {:>12.2}{liq}", v.user, v.hf, api, v.debt);
        }
    }

    // Validation: the one-round-trip snapshot (IRM rate from the previous block's state) vs exact.
    let prev = engine.snapshots((h.number - 1).into(), h.timestamp).await?;
    let fast = engine.snapshots_fast(h.number.into(), h.timestamp, &prev).await?;
    let worst = snaps
        .iter()
        .zip(&fast)
        .filter_map(|(a, b)| Some((a.as_ref()?, b.as_ref()?)))
        .map(|(a, b)| {
            let (x, y) = (math::u256_f64(a.totals.total_borrow_assets), math::u256_f64(b.totals.total_borrow_assets));
            (x - y).abs() / x.max(1.0)
        })
        .fold(0.0f64, f64::max);
    println!("\nfast snapshot vs exact: max relative diff in accrued borrow totals {worst:.3e}");

    // Validation: our HF vs the API's (API can lag a few minutes, so only large gaps matter).
    let mut diffs: Vec<f64> = views
        .iter()
        .filter_map(|v| engine.api_hf.get(&(v.market, v.user)).filter(|h| h.is_finite() && **h < 5.0).map(|a| (v.hf - a).abs() / a))
        .collect();
    if !diffs.is_empty() {
        diffs.sort_by(f64::total_cmp);
        let n = diffs.len();
        println!("\nHF vs Morpho API: n={n} median rel diff {:.6} p99 {:.6} max {:.6}", diffs[n / 2], diffs[n * 99 / 100], diffs[n - 1]);
    }
    Ok(())
}

/// Next block to process when live. Sources, whichever fires first:
/// - a WebSocket head newer than `last`;
/// - the flashblock stream saying a block newer than `last` is sealed: the head is fetched over
///   HTTP right away, retried every 40 ms while the RPC nodes catch up (they trail the stream by
///   ~60-360 ms). Block data always comes from RPC, never from the stream;
/// - HTTP polling as the fallback (every `poll_ms` without other sources, else every 2.5 s).
async fn wait_head<P: Provider>(ws: &mut Option<watch::Receiver<Option<Head>>>, p: &P, last: u64, poll_ms: u64) -> Result<Head> {
    let fresh = |rx: &watch::Receiver<Option<Head>>| rx.borrow().filter(|h| h.number > last);
    let fallback = Duration::from_millis(if ws.is_some() { 2_500 } else { poll_ms });
    loop {
        if let Some(h) = ws.as_ref().and_then(fresh) {
            return Ok(h);
        }
        let ws_changed = async {
            match ws.as_mut() {
                Some(rx) => {
                    if rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = ws_changed => {}
            _ = tokio::time::sleep(fallback) => {
                let h = heads::http_head(p, BlockNumberOrTag::Latest).await?;
                if h.number > last {
                    return Ok(h);
                }
            }
        }
    }
}

sol! {
    interface ILiquidatorView {
        function executor() external view returns (address);
        function paused() external view returns (bool);
        function PROFIT_RECEIVER() external view returns (address);
        function allowedTarget(address) external view returns (bool);
        function allowedProtocol(address) external view returns (bool);
        function MORPHO() external view returns (address);
        function POOL_MANAGER() external view returns (address);
    }
    interface IAtlasRates {
        function getAtlasSurchargeRate() external view returns (uint256);
    }
}

/// `[atlas.solver] submit` in live mode: the solver operations' signer, after checking that the
/// deployed solver is run by it and that it has atlETH bonded. None in shadow mode or without
/// `submit`.
async fn build_submitter<P: Provider>(engine: &MorphoEngine<P>, signer: Option<&alloy::signers::local::PrivateKeySigner>) -> Result<Option<atlas_submit::Submitter>> {
    use alloy::sol_types::SolCall;
    let c = &engine.cfg.atlas.solver;
    let Some(signer) = signer.filter(|_| c.submit) else { return Ok(None) };
    let (p, atlas, control) = (&engine.provider, engine.cfg.atlas.address, engine.cfg.atlas.control);
    let solver = c.address.context("atlas.solver.submit needs the deployed solver address")?;
    anyhow::ensure!(c.signer.is_none_or(|s| s == signer.address()), "atlas.solver.signer must be the hot wallet (it signs the solver ops)");
    let req = alloy::rpc::types::TransactionRequest::default().to(solver).input(alloy::primitives::Bytes::from(ILiquidatorView::executorCall {}.abi_encode()).into());
    let run_by = ILiquidatorView::executorCall::abi_decode_returns(&p.call(req).await.context("solver executor()")?)?;
    anyhow::ensure!(run_by == signer.address(), "solver {solver} is run by {run_by}, not the hot wallet {}", signer.address());
    let bonded = atlas_submit::bonded(p, atlas, signer.address()).await?;
    let bonded_eth = math::units(bonded, 18);
    anyhow::ensure!(bonded_eth >= c.min_bond_eth, "{bonded_eth} atlETH bonded by {} < atlas.solver.min_bond_eth {} (Atlas.depositAndBond, see docs)", signer.address(), c.min_bond_eth);
    let domain = atlas_submit::read_domain(p, atlas).await?;
    let gas_limit = atlas_submit::solver_gas_limit(p, control).await?;
    warn!(%solver, signer = %signer.address(), bonded_eth, gas_limit, version = ?domain.version, "ATLAS LIVE: profitable solver operations WILL be signed and submitted");
    Ok(Some(atlas_submit::Submitter::new(signer.clone(), domain, atlas, control, c.endpoint.clone(), engine.http.clone(), gas_limit)))
}

/// Live pre-flight: refuses to send unless the deployed contract, the hot wallet and the
/// standing Morpho allowances are all consistent. Returns the contract's profit receiver.
async fn preflight_live<P: Provider>(engine: &MorphoEngine<P>, signer_addr: Address) -> Result<Address> {
    let cfg = &engine.cfg.exec;
    anyhow::ensure!(cfg.mode == "live", "exec.mode is {:?}, not \"live\"", cfg.mode);
    let flash = cfg.venue == "flash";
    let liq = if flash {
        cfg.flash_liquidator.context("live mode with venue = \"flash\" needs exec.flash_liquidator (deployed contract address)")?
    } else {
        cfg.liquidator.context("live mode needs exec.liquidator (deployed contract address)")?
    };
    let code = engine.provider.get_code_at(liq).await?;
    anyhow::ensure!(!code.is_empty(), "no contract at exec.liquidator {liq}");
    let call = |data: Vec<u8>| alloy::rpc::types::TransactionRequest::default().to(liq).input(data.into());
    use alloy::sol_types::SolCall;
    let exec_out = engine.provider.call(call(ILiquidatorView::executorCall {}.abi_encode())).await?;
    let on_chain_exec = ILiquidatorView::executorCall::abi_decode_returns(&exec_out)?;
    anyhow::ensure!(on_chain_exec == signer_addr, "contract executor {on_chain_exec} != keystore address {signer_addr}");
    anyhow::ensure!(cfg.executor == signer_addr, "exec.executor {} != keystore address {signer_addr}", cfg.executor);
    let paused = ILiquidatorView::pausedCall::abi_decode_returns(&engine.provider.call(call(ILiquidatorView::pausedCall {}.abi_encode())).await?)?;
    anyhow::ensure!(!paused, "liquidator is paused (owner must unpause)");
    let receiver = ILiquidatorView::PROFIT_RECEIVERCall::abi_decode_returns(&engine.provider.call(call(ILiquidatorView::PROFIT_RECEIVERCall {}.abi_encode())).await?)?;
    anyhow::ensure!(receiver != signer_addr, "PROFIT_RECEIVER must be a cold wallet, not the hot executor");
    if flash {
        // FlashLiquidator calls Morpho's liquidate itself: Morpho must be an allowed protocol, and
        // the funding source the config picks must be the one the contract was deployed with.
        let morpho = engine.protocol.address;
        let read = |data: Vec<u8>| engine.provider.call(call(data));
        let ok = ILiquidatorView::allowedProtocolCall::abi_decode_returns(&read(ILiquidatorView::allowedProtocolCall(morpho).abi_encode()).await?)?;
        anyhow::ensure!(ok, "Morpho {morpho} is not an allowed protocol of the flash liquidator (owner: setAllowedProtocol)");
        let source = if cfg.flash_funding == "v4" {
            (ILiquidatorView::POOL_MANAGERCall::abi_decode_returns(&read(ILiquidatorView::POOL_MANAGERCall {}.abi_encode()).await?)?, cfg.pool_manager.unwrap_or(funding::BASE_POOL_MANAGER))
        } else {
            (ILiquidatorView::MORPHOCall::abi_decode_returns(&read(ILiquidatorView::MORPHOCall {}.abi_encode()).await?)?, morpho)
        };
        anyhow::ensure!(source.0 == source.1 && source.0 != Address::ZERO, "flash liquidator's {} funding source is {}, config expects {}", cfg.flash_funding, source.0, source.1);
    } else {
        for m in &engine.metas {
            anyhow::ensure!(
                sim::has_standing_allowance(&engine.provider, m.params.loanToken, liq, engine.protocol.address).await?,
                "no standing Morpho allowance for loan token {} (owner: approveMorpho(token, true))",
                m.params.loanToken
            );
        }
    }
    let bal = engine.provider.get_balance(signer_addr).await?;
    anyhow::ensure!(math::units(bal, 18) >= cfg.min_wallet_eth, "hot wallet balance {} ETH < min {}", math::units(bal, 18), cfg.min_wallet_eth);
    // At least one tx at its worst case, else every send would be refused (`[exec] max_tx_cost_eth`).
    anyhow::ensure!(math::units(bal, 18) >= cfg.max_tx_cost_eth, "hot wallet balance {} ETH < exec.max_tx_cost_eth {}: no tx could be sent", math::units(bal, 18), cfg.max_tx_cost_eth);
    info!(%liq, executor = %signer_addr, %receiver, balance_eth = math::units(bal, 18), "LIVE pre-flight passed");
    Ok(receiver)
}

/// Planner inputs, shared with background tasks (Atlas auction valuations).
struct PlannerParts {
    protocol: Morpho,
    kyber: Kyber,
    venue: Venue,
    /// The Atlas solver venue, when `[atlas.solver] simulate`.
    solver: Option<atlas_venue::AtlasSolver>,
    /// Atlas's gas surcharge on solver ops (share, e.g. 0.1).
    atlas_surcharge: f64,
    v3: v3router::V3Router,
    exec: config::ExecConfig,
    /// What winners of past liquidations paid (the risk gate bids against it; see risk.rs).
    bids: std::sync::Mutex<WinningBids>,
    /// Signs and submits solver operations (live + `[atlas.solver] submit`).
    submitter: Option<atlas_submit::Submitter>,
    /// The day's money (ledger.rs): failed-gas spend and the reservations of txs in flight, the
    /// Atlas bond baseline, the block path's PnL. Persisted in live mode.
    ledger: std::sync::Mutex<Ledger>,
    /// An Atlas op was accepted: read the bond again soon.
    bond_dirty: std::sync::atomic::AtomicBool,
    /// `[atlas.solver] atlas_bond_budget_usd`.
    atlas_bond_budget_usd: f64,
    /// `[atlas.solver] min_bond_eth`: below it no op is submitted.
    atlas_min_bond_eth: f64,
    /// The bidding policies (see policy.rs).
    auction: Arc<dyn AuctionBidPolicy>,
    priority: Arc<dyn PriorityBidPolicy>,
}

impl PlannerParts {
    fn planner<'a>(&'a self, mode: &'a str) -> Planner<'a, Morpho, Venue, Kyber> {
        Planner { protocol: &self.protocol, aggregator: &self.kyber, venue: &self.venue, cfg: &self.exec, v3: Some(&self.v3), mode }
    }

    /// The competition model's priority per USD of gross for `market` now (None = too few winners
    /// observed).
    fn competition(&self, market: &str, now_unix: u64) -> Option<f64> {
        let e = &self.exec;
        self.bids.lock().unwrap().ratio_quantile(market, now_unix, e.bid_quantile, e.bid_min_samples)
    }

    /// A liquidation someone else won: its priority fee per gas against the gross it took. Atlas
    /// metacalls are left out (their priority fee is the bundler's, the race was the auction), and
    /// so are our own liquidations.
    fn observe_winner(&self, row: &loss::LiquidationRow, ours: Address, atlas: Address) {
        if let Some((at, prio_wei, gross_usd)) = row.winning_bid(ours, atlas) {
            self.bids.lock().unwrap().observe(&row.market, at, prio_wei, gross_usd);
        }
    }
}

/// Seeds the competition model from the liquidations already in the journal (the last
/// `JOURNAL_SEED_BYTES` of it), so a restart does not forget the competition.
fn seed_winning_bids(parts: &PlannerParts, journal_path: &str, ours: Address, atlas: Address) {
    const JOURNAL_SEED_BYTES: u64 = 64 << 20;
    use std::io::{BufRead, Seek};
    let Ok(mut f) = std::fs::File::open(journal_path) else { return };
    let len = f.metadata().map_or(0, |m| m.len());
    if f.seek(std::io::SeekFrom::Start(len.saturating_sub(JOURNAL_SEED_BYTES))).is_err() {
        return;
    }
    // Only rows with what the model needs (gross USD, block time, the tx's `to`): rows written
    // before those fields existed are skipped, not read as zeros.
    let (mut n, mut skipped) = (0usize, 0usize);
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        if !line.contains("\"kind\":\"liquidation\"") {
            continue;
        }
        match serde_json::from_str::<loss::LiquidationRow>(&line) {
            Ok(row) if row.gross_usd.is_some() && row.timestamp > 0 && row.tx_to.is_some() => {
                parts.observe_winner(&row, ours, atlas);
                n += 1;
            }
            _ => skipped += 1,
        }
    }
    info!(liquidations = n, skipped, "competition model seeded from the journal");
    if n < parts.exec.bid_min_samples {
        warn!(liquidations = n, min_samples = parts.exec.bid_min_samples, "competition model is cold: block-path bids use the profit share until enough liquidations are seen");
    }
}

/// (aggregator, median) -> (first seen, its valuation, computed once): see `AtlasState::auction_cache`.
type AuctionCache = HashMap<(Address, U256), (Instant, Arc<tokio::sync::OnceCell<serde_json::Value>>)>;

/// The background borrower refresh's result: (market, its borrowers or the error) per market.
type BorrowerRefresh = Vec<(usize, Result<Vec<protocol::BorrowerInfo>>)>;

/// What the watch loop handles between and during blocks: SVR auctions and oracle log events.
#[derive(Debug)]
pub enum SideEvent {
    Auction(atlas::Auction),
    Oracle(oracle_logs::OracleLogEvent),
}

/// Capacity of the side-event channel (auctions + oracle logs); a full channel drops the event and
/// journals `atlas_dropped` once a minute.
const SIDE_CHANNEL: usize = 1024;

/// How many recent heads the block-time estimate and the block -> timestamp table keep.
const BLOCK_CLOCK_HEADS: usize = 50;
const BLOCK_TS_KEEP: u64 = 600;

/// The Atlas bond is read every this many heads (and at once after an op was accepted, via
/// `bond_dirty`). Head-counted, so post-Denim (200 ms blocks) this is ~100 s, not the old ~100 s at
/// 2 s blocks x 50 — kept near 100 s here to avoid an extra bond read every few seconds on the Atlas
/// RPC. `bond_dirty` still forces an immediate read whenever it actually matters.
const BOND_CHECK_HEADS: u64 = 500;

/// Whether a head must fetch fresh market snapshots rather than reuse the previous ones (the A1
/// snapshot throttle). Refetch with no snapshot yet, on an oracle update, when a market log touched
/// a position or a liquidation happened, or when the last snapshot is older than
/// `snapshot_max_age`; otherwise the prices are still current and the snapshot is reused.
fn must_refetch_snapshots(has_prev: bool, oracle_updated: bool, touched_any: bool, fresh_enough: bool) -> bool {
    !has_prev || oracle_updated || touched_any || !fresh_enough
}

/// The chain's block time from the last BLOCK_CLOCK_HEADS heads: follows a block-time change (Base
/// Denim: 2 s -> 0.2 s) within that many blocks, no setting to switch.
#[derive(Debug, Default)]
struct BlockClock {
    heads: std::collections::VecDeque<(u64, u64)>,
}

impl BlockClock {
    fn push(&mut self, number: u64, timestamp: u64) {
        if self.heads.back().is_some_and(|(n, _)| *n >= number) {
            return;
        }
        self.heads.push_back((number, timestamp));
        while self.heads.len() > BLOCK_CLOCK_HEADS {
            self.heads.pop_front();
        }
    }

    /// Seconds per block (2.0 until two heads are known).
    fn block_time(&self) -> f64 {
        match (self.heads.front(), self.heads.back()) {
            (Some((n0, t0)), Some((n1, t1))) if n1 > n0 && t1 >= t0 => (t1 - t0) as f64 / (n1 - n0) as f64,
            _ => 2.0,
        }
    }

    /// How late (ms) head `(number, timestamp)` reached us, at wall-clock `now_ms`. Block
    /// timestamps are only second-granular, and post-Denim five 200 ms blocks share one second, so
    /// `now - timestamp*1000` jitters 0..1000 ms. Instead we place the head within its second: the
    /// first block seen with this timestamp sits on the second boundary, each later one `block_time`
    /// after it. Pre-Denim (2 s blocks, one per timestamp) the index is always 0, so this reduces to
    /// the old `now - timestamp*1000`. `number` must already be pushed.
    fn head_age_ms(&self, number: u64, timestamp: u64, now_ms: u64) -> i64 {
        let first = self.heads.iter().filter(|(_, t)| *t == timestamp).map(|(n, _)| *n).min().unwrap_or(number);
        let idx = number.saturating_sub(first);
        let spacing_ms = (self.block_time() * 1000.0).max(1.0);
        let theoretical_ms = timestamp as i64 * 1000 + (idx as f64 * spacing_ms) as i64;
        now_ms as i64 - theoretical_ms
    }
}

/// The Atlas auction path: the price model the auctions are valued with (kept current by the oracle
/// logs, cross-checked against each block's snapshot) and the valuation resources.
struct AtlasState<P> {
    /// Endpoints for valuations (`[atlas] rpc`, else the main pool).
    provider: P,
    /// The valuation pool when it is its own (for the usage report).
    pool: Option<rpc::RpcPool>,
    model: oracle::PriceModel,
    logs: oracle_logs::OracleLogs,
    /// block -> timestamp of recent heads, for oracle logs that carry none.
    block_ts: HashMap<u64, u64>,
    /// Oracle events whose block timestamp is not known yet.
    held: Vec<oracle_logs::OracleLogEvent>,
    /// Highest block an applied oracle event belongs to.
    max_update_block: u64,
    /// Consecutive cross-checks that found the model behind the chain (missed oracle events).
    missed_streak: u32,
    /// Cross-check every market on the next block, whatever max_update_block says (the logs
    /// reconnected, a log was removed, a secondary had no pending answer).
    resync_all: bool,
    svr_cutoff_secs: u64,
    /// Journal rows finished by background tasks (the receiver lives in `watch`, which writes them
    /// via drain_journal so lines never interleave with its own).
    journal_tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// Permits for auction valuations running at once (atlas.max_concurrent).
    valuations: Arc<tokio::sync::Semaphore>,
    /// One valuation per report: Chainlink opens two auctions for each update (same aggregator and
    /// median, different ids and userOpHashes; only one ends on-chain). The sibling waited behind the
    /// first for the only permit and was valued again, 0.3-10 s late (10/24). (aggregator, median) ->
    /// (first seen, its valuation), kept AUCTION_SHARE_SECS.
    auction_cache: Arc<std::sync::Mutex<AuctionCache>>,
    /// Auctions the feed task dropped on a full channel (journaled as they grow).
    dropped: Arc<std::sync::atomic::AtomicU64>,
    dropped_journaled: u64,
}

impl<P: Provider + Clone + 'static> AtlasState<P> {
    /// An oracle log event: applied now if its block's timestamp is known, else held for its head.
    fn on_oracle_log(&mut self, ev: oracle_logs::OracleLogEvent) {
        use oracle_logs::OracleLogEvent as E;
        match ev {
            E::Reconnected => self.resync_all = true,
            E::Removed { aggregator } => {
                warn!(%aggregator, "oracle log removed by a reorg; resyncing the model");
                self.model.invalidate(aggregator);
                self.resync_all = true;
            }
            E::CutoffChanged { aggregator, cutoff_secs, at } => {
                // Takes effect immediately: `due` reads the new value on the next head. Applied
                // without waiting for the block timestamp (the cutoff is not timed on it).
                if self.model.set_cutoff(aggregator, cutoff_secs as u64) {
                    info!(%aggregator, cutoff_secs, block = at.block, "ORACLE SVR cutoff changed");
                    let _ = self.journal_tx.send(json!({ "kind": "oracle_cutoff", "block": at.block, "aggregator": aggregator, "cutoff_secs": cutoff_secs }).to_string());
                }
            }
            E::Transmission { at, .. } | E::Secondary { at, .. } => {
                let ts = if at.block_ts > 0 { Some(at.block_ts) } else { self.block_ts.get(&at.block).copied() };
                match ts {
                    Some(ts) => self.apply_oracle_log(ev, ts),
                    None => self.held.push(ev),
                }
            }
        }
    }

    fn apply_oracle_log(&mut self, ev: oracle_logs::OracleLogEvent, ts: u64) {
        use oracle_logs::OracleLogEvent as E;
        let (applied, path, aggregator, at) = match ev {
            E::Transmission { aggregator, round, answer, at } => (Some(self.model.apply_transmission(aggregator, round, answer, ts)), oracle::Path::Primary, aggregator, at),
            E::Secondary { aggregator, round, at } => (self.model.apply_secondary_round(aggregator, round, ts), oracle::Path::Secondary, aggregator, at),
            _ => return,
        };
        self.max_update_block = self.max_update_block.max(at.block);
        let outcome = match &applied {
            Some(oracle::Applied::Moved(m)) => format!("moved {}", m.len()),
            Some(oracle::Applied::Deferred(_)) => "deferred".into(),
            Some(oracle::Applied::Rejected(why)) => format!("rejected: {why}"),
            None => {
                // A secondary with no pending answer of its round: the transmit was missed.
                self.resync_all = true;
                "no pending answer".into()
            }
        };
        info!(%aggregator, block = at.block, path = ?path, outcome, "ORACLE log");
        let _ = self.journal_tx.send(json!({ "kind": "oracle_log", "block": at.block, "tx": at.tx, "aggregator": aggregator, "path": path, "outcome": outcome }).to_string());
    }

    /// Per block: held logs of this block or earlier, SVR primaries past their cutoff, and the
    /// cross-check of the PREVIOUS block's snapshot against the model (one block later, so that the
    /// logs of that block have arrived; a mismatch is a missed event: resynced and counted).
    async fn on_head(&mut self, head: &Head, prev: Option<(u64, &[Option<morpho::onchain::MarketSnapshot>])>) {
        self.block_ts.insert(head.number, head.timestamp);
        self.block_ts.retain(|b, _| b + BLOCK_TS_KEEP > head.number);
        for ev in std::mem::take(&mut self.held) {
            let Some(at) = ev.pos() else { continue };
            if at.block > head.number {
                self.held.push(ev);
                continue;
            }
            let ts = match self.block_ts.get(&at.block) {
                Some(ts) => *ts,
                None => match heads::http_head(&self.provider, BlockNumberOrTag::Number(at.block)).await {
                    Ok(h) => h.timestamp,
                    Err(e) => {
                        warn!(block = at.block, error = %e, "no timestamp for an oracle log's block; dropped (resync catches up)");
                        self.resync_all = true;
                        continue;
                    }
                },
            };
            self.apply_oracle_log(ev, ts);
        }
        for (aggregator, answer) in self.model.due(head.timestamp, self.svr_cutoff_secs) {
            let r = self.model.apply(aggregator, answer, oracle::Path::Expired, head.timestamp);
            // In effect from this head on: the previous block's snapshot still has the old price, so
            // its cross-check must wait a block like a log of this block would.
            if matches!(&r, oracle::Applied::Moved(m) if !m.is_empty()) {
                self.max_update_block = self.max_update_block.max(head.number);
            }
            info!(%aggregator, block = head.number, outcome = ?r, "ORACLE SVR primary past its cutoff");
        }
        let Some((prev_block, snaps)) = prev else { return };
        if !(self.resync_all || self.max_update_block <= prev_block) {
            return;
        }
        let before = self.model.aggregators();
        let mut missed = 0;
        for (mi, s) in snaps.iter().enumerate() {
            if let Some(s) = s.as_ref().filter(|_| self.model.is_modelled(mi))
                && self.model.price(mi) != Some(s.price)
            {
                missed += 1;
                self.model.resync(&self.provider, mi, s.price, prev_block.into()).await;
            }
        }
        if missed > 0 && !self.resync_all {
            self.missed_streak += 1;
            warn!(block = prev_block, markets = missed, streak = self.missed_streak, "oracle logs missed an update");
            if self.missed_streak >= 3 {
                self.logs.reconnect();
                self.missed_streak = 0;
            }
        } else if missed == 0 {
            self.missed_streak = 0;
        }
        self.resync_all = false;
        let after = self.model.aggregators();
        if after != before {
            self.logs.set_addresses(after);
        }
    }

    /// Did an oracle update (a log, or an SVR primary past its cutoff) land at a block after
    /// `last_block`? The snapshot throttle refetches when so, so a price move is never missed.
    fn pending_update(&self, last_block: u64) -> bool {
        self.resync_all || self.max_update_block > last_block
    }

    /// Journals the auctions the feed task dropped since the last call.
    fn journal_dropped(&mut self, block: u64) {
        let n = self.dropped.load(std::sync::atomic::Ordering::Relaxed);
        if n > self.dropped_journaled {
            warn!(dropped = n - self.dropped_journaled, "atlas_dropped: side event channel was full");
            let _ = self.journal_tx.send(json!({ "kind": "atlas_dropped", "block": block, "dropped": n - self.dropped_journaled, "total": n }).to_string());
            self.dropped_journaled = n;
        }
    }
}

/// Writes the journal rows background tasks have finished.
fn drain_journal(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>, journal: &mut std::fs::File) -> Result<()> {
    while let Ok(line) = rx.try_recv() {
        writeln!(journal, "{line}")?;
    }
    Ok(())
}

/// One side event: an SVR auction is valued in the background (on the previous block's snapshots,
/// the newest the loop has), an oracle log updates the price model.
fn on_side<P: Provider + Clone + 'static>(
    engine: &MorphoEngine<P>,
    st: &mut AtlasState<P>,
    ev: SideEvent,
    ctx: Option<(&[Option<morpho::onchain::MarketSnapshot>], &Head, f64, f64)>,
    parts: &Arc<PlannerParts>,
) {
    match ev {
        SideEvent::Oracle(ev) => st.on_oracle_log(ev),
        SideEvent::Auction(a) => match ctx {
            Some((snaps, head, eth_usd, block_time)) => on_auction(engine, st, a, snaps, head, eth_usd, block_time, parts),
            None => info!(auction = %a.id, "ATLAS auction before the first block; skipped"),
        },
    }
}

/// Longest an auction valuation may wait for a permit before it is skipped: SVR auctions last ~2 s,
/// and 10 s (5 blocks) still values the state the auction saw.
const ATLAS_MAX_QUEUE_MS: u64 = 10_000;

/// One market of an SVR auction: positions that become liquidatable at the auction price.
struct AuctionMarket {
    meta: MarketMeta,
    /// Snapshot with the auction price.
    snap: morpho::onchain::MarketSnapshot,
    old_price: U256,
    /// Largest bonus first.
    liq: Vec<PositionView>,
}

/// Solver phase 0: value one SVR auction as if we were bidding, journal it, never submit.
/// The price comes from the auction hint (the report about to be written). Which positions it
/// makes liquidatable is evaluated here, in memory; the RPC work runs in a background task so a
/// big auction (dozens of positions, several seconds) never stalls the block path.
#[allow(clippy::too_many_arguments)]
fn on_auction<P: Provider + Clone + 'static>(
    engine: &MorphoEngine<P>,
    st: &AtlasState<P>,
    a: atlas::Auction,
    snaps: &[Option<morpho::onchain::MarketSnapshot>],
    head: &Head,
    eth_usd: f64,
    block_time: f64,
    parts: &Arc<PlannerParts>,
) {
    let markets: Vec<AuctionMarket> = st
        .model
        .preview(a.aggregator, a.median)
        .into_iter()
        .filter_map(|(mi, price)| snaps.get(mi).and_then(|s| s.as_ref()).map(|base| auction_market(engine, mi, base, price)))
        .collect();
    let sim_time = head.timestamp + engine.cfg.lookahead_secs(block_time);
    let atlas = engine.cfg.atlas.clone();
    let (provider, parts, journal, permits, head) = (st.provider.clone(), parts.clone(), st.journal_tx.clone(), st.valuations.clone(), *head);
    // The sibling auction of the same report shares the first one's valuation.
    let shared = {
        let mut cache = st.auction_cache.lock().unwrap();
        cache.retain(|_, (t, _)| t.elapsed() < Duration::from_secs(AUCTION_SHARE_SECS));
        cache.entry((a.aggregator, a.median)).or_insert_with(|| (Instant::now(), Arc::new(tokio::sync::OnceCell::new()))).1.clone()
    };
    // Valuations queue behind each other (atlas.max_concurrent, taken inside value_auction by the
    // one that values) instead of piling onto the RPC pool the block path needs; the wait is
    // journaled as queued_ms.
    tokio::spawn(value_auction(provider, parts, a, markets, head, eth_usd, sim_time, atlas, true, journal, Some(permits), Some(shared)));
}

/// How long a report's valuation is shared with its sibling auction: they arrive milliseconds
/// apart; longer would hand a later report of the same feed with an unchanged median a stale one.
const AUCTION_SHARE_SECS: u64 = 5;

/// The positions of market `mi` (snapshot `base`) that `price` makes liquidatable.
fn auction_market<P: Provider>(engine: &MorphoEngine<P>, mi: usize, base: &morpho::onchain::MarketSnapshot, price: U256) -> AuctionMarket {
    let mut snap = base.clone();
    let old_price = snap.price;
    snap.price = price;
    let mut liq: Vec<PositionView> = engine
        .evaluate_market(mi, &snap)
        .into_iter()
        .take_while(|v| v.health.is_liquidatable())
        .filter(|v| v.max_seize > U256::ZERO && v.bonus >= engine.cfg.min_bonus)
        .collect();
    liq.sort_by(|x, y| y.bonus.total_cmp(&x.bonus));
    AuctionMarket { meta: engine.metas[mi].clone(), snap, old_price, liq }
}

#[allow(clippy::too_many_arguments)]
async fn value_auction_at<P: Provider + Clone + 'static>(engine: &mut MorphoEngine<P>, market: &str, drop_bps: u64, at_block: u64, users: &[Address], use_kyber: bool, auction: Arc<dyn AuctionBidPolicy>, priority: Arc<dyn PriorityBidPolicy>) -> Result<()> {
    let mi = engine.market_by_name(market).with_context(|| format!("market {market} not in config"))?;
    anyhow::ensure!(drop_bps < 10_000, "--price-drop-bps must be < 10000");
    let h = heads::http_head(&engine.provider, BlockNumberOrTag::Number(at_block)).await?;
    let bt = recent_block_time(&engine.provider, &h).await;
    if users.is_empty() {
        engine.refresh_borrowers(h.number.into()).await?;
    } else {
        engine.reload(&users.iter().map(|u| (mi, *u)).collect::<Vec<_>>(), h.number.into()).await?;
    }
    let sim_time = h.timestamp + engine.cfg.lookahead_secs(bt);
    let snaps = engine.snapshots(h.number.into(), sim_time).await?;
    let eth_usd = engine.eth_usd(&snaps);
    let base = snaps[mi].clone().context("snapshot failed")?;
    let price = base.price * U256::from(10_000 - drop_bps) / U256::from(10_000u64);
    let m = auction_market(engine, mi, &base, price);
    let (kyber, simulator) = build_planner_parts(engine).await?;
    let parts = Arc::new(PlannerParts { protocol: engine.protocol.clone(), kyber, venue: simulator, solver: build_atlas_solver(engine).await?, atlas_surcharge: atlas_surcharge(engine).await, v3: build_v3(engine).await?, exec: engine.cfg.exec.clone(), bids: std::sync::Mutex::new(WinningBids::new(engine.cfg.exec.bid_window_secs)), submitter: None, ledger: std::sync::Mutex::new(Ledger::in_memory()), bond_dirty: Default::default(), atlas_bond_budget_usd: engine.cfg.atlas.solver.atlas_bond_budget_usd, atlas_min_bond_eth: engine.cfg.atlas.solver.min_bond_eth, auction, priority });
    let a = atlas::Auction {
        id: format!("replay-{market}-{at_block}"),
        aggregator: Address::ZERO,
        median: U256::ZERO,
        deadline: h.number + 100,
        max_fee_per_gas: 0,
        user_op_hash: alloy::primitives::B256::ZERO,
        seen: Instant::now(),
        seen_unix_ms: gates::unix_ms(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    value_auction(engine.provider.clone(), parts, a, vec![m], h, eth_usd, sim_time, engine.cfg.atlas.clone(), use_kyber, tx, None, None).await;
    let row: serde_json::Value = serde_json::from_str(&rx.recv().await.context("no valuation row")?)?;
    println!("{}", serde_json::to_string_pretty(&row)?);
    Ok(())
}

/// Background half of on_auction. Per market, first the bid: one quote for the whole bundle
/// (what a solver op liquidating every position at once nets; bid = that x (1 - margin)), its
/// swap held to what the op's liquidations leave of atlas.solver_gas_limit. Then the execution
/// check: the largest positions planned + simulated one by one. Everything runs on the
/// snapshot's block (`head`) as the next block at the accrual time, like the block path.
/// `live`: also use Kyber, which quotes the current market only (off in `value-auction` replays
/// of past blocks). The winning on-chain bid is matched offline by the userOpHash (atlas_match).
#[allow(clippy::too_many_arguments)]
async fn value_auction<P: Provider>(
    provider: P,
    parts: Arc<PlannerParts>,
    a: atlas::Auction,
    markets: Vec<AuctionMarket>,
    head: Head,
    eth_usd: f64,
    sim_time: u64,
    atlas: config::AtlasConfig,
    live: bool,
    journal: tokio::sync::mpsc::UnboundedSender<String>,
    permits: Option<Arc<tokio::sync::Semaphore>>,
    shared: Option<Arc<tokio::sync::OnceCell<serde_json::Value>>>,
) {
    // Whoever runs the valuation takes the permit (atlas.max_concurrent): the sibling of a report
    // that is still queued waits on the same cell instead of valuing outside the limit.
    let compute = || async {
        let _permit = match &permits {
            Some(s) => match s.clone().acquire_owned().await {
                Ok(p) => Some(p),
                Err(_) => return json!({ "skipped": "shutdown", "first_auction": a.id }),
            },
            None => None,
        };
        let queued_ms = a.seen.elapsed().as_millis() as u64;
        // Queued behind other valuations for too long (a crash burst): the auction is long over and
        // `head` may be pruned on the pending endpoints. Journal what it held, spend no RPC on it.
        if queued_ms > ATLAS_MAX_QUEUE_MS {
            let n_liq: usize = markets.iter().map(|m| m.liq.len()).sum();
            let bonus: f64 = markets.iter().flat_map(|m| &m.liq).map(|v| v.bonus).sum();
            return json!({ "skipped": "queued_too_long", "queued_ms": queued_ms, "liquidatable": n_liq, "bonus_usd": bonus, "first_auction": a.id });
        }
        let mut v = valuation(&provider, &parts, &a, &markets, head, eth_usd, sim_time, &atlas, live).await;
        v["queued_ms"] = json!(queued_ms);
        v
    };
    let mut v = match &shared {
        Some(cell) => cell.get_or_init(compute).await.clone(),
        None => compute().await,
    };
    v["shared"] = json!(v["first_auction"] != json!(a.id));
    if let Some(skip) = v["skipped"].as_str() {
        warn!(auction = %a.id, queued_ms = %v["queued_ms"], liquidatable = %v["liquidatable"], bonus_usd = %v["bonus_usd"], skip, "ATLAS auction skipped: queued too long");
        let row = json!({ "kind": "atlas_auction", "auction_id": a.id, "user_op_hash": a.user_op_hash, "aggregator": a.aggregator, "deadline": a.deadline, "head": head.number,
                          "skipped": skip, "queued_ms": v["queued_ms"], "liquidatable": v["liquidatable"], "bonus_usd": v["bonus_usd"], "eth_usd": eth_usd, "shared": v["shared"] });
        let _ = journal.send(row.to_string());
        return;
    }
    let queued_ms = v["queued_ms"].as_u64().unwrap_or(0);
    let bundle_net = v["bundle_net_usd"].as_f64().unwrap_or(0.0);
    // The journaled bid: the policy's bid on the bundle net (default: x (1 - atlas.margin)); the
    // dashboard's paper table tries the atlas.paper_margins ladder around it. None = no bid.
    let bid_usd = parts.auction.bid_usd(None, bundle_net, eth_usd);
    // From this auction's arrival to the end of the valuation proper (before any solver-op
    // simulation, see valuation()).
    let since_valued = v["valued_unix_ms"].as_u64().map_or(0, |t| gates::unix_ms().saturating_sub(t));
    let value_ms = (a.seen.elapsed().as_millis() as u64).saturating_sub(since_valued);
    info!(auction = %a.id, aggregator = %a.aggregator, deadline = a.deadline, liquidatable = %v["liquidatable"], bonus_usd = %v["bonus_usd"], bundle_net_usd = bundle_net, net_usd = %v["net_usd"], bid_usd = bid_usd.unwrap_or(0.0), bundle_ms = %v["bundle_ms"], value_ms, shared = %v["shared"], "ATLAS auction valued (observe only)");
    let mut row = json!({ "kind": "atlas_auction", "auction_id": a.id, "user_op_hash": a.user_op_hash, "aggregator": a.aggregator, "median": a.median.to_string(),
                          "deadline": a.deadline, "head": head.number, "max_fee_gwei": a.max_fee_per_gas as f64 / 1e9, "bid_usd": bid_usd, "bid_eth": bid_usd.map(|b| b / eth_usd.max(1.0)),
                          "eth_usd": eth_usd, "bid_basis": "bundle_net", "bid_margin": atlas.margin, "queued_ms": queued_ms, "value_ms": value_ms });
    for k in ["markets", "positions", "liquidatable", "bonus_usd", "bundle_net_usd", "net_usd", "bundle_ms", "shared"] {
        row[k] = v[k].clone();
    }
    let _ = journal.send(row.to_string());
}

/// The valuation proper (see value_auction): per-market rows, positions and totals, timed from
/// when `a` (the first auction of its report) arrived.
#[allow(clippy::too_many_arguments)]
/// A simulated solver op that would pay: what `submit_solver_op` needs to sign and submit it.
struct OpCandidate {
    /// Index of its market in the auction.
    market: usize,
    payload: executor::Payload,
    bid_wei: U256,
    profit: U256,
    gas_used: u64,
    /// Profit after the bid, in USD: how the auction's markets are ranked for its one op.
    profit_usd: f64,
}

/// The auction's top position as a solver operation (`[atlas.solver] simulate`): its direct-router
/// plan, wrapped into `atlasSolverCall` with a bid of the plan's net x (1 - margin), simulated against
/// the Atlas stand-in. Returns the journal row and, when the op succeeds with a profit after the
/// bid, the candidate for submission (`valuation` submits the auction's best one). None: solver
/// venue off, or no direct-router plan to wrap (an aggregator route is built for the direct
/// liquidator's address).
#[allow(clippy::too_many_arguments)]
async fn solver_op<P: Provider>(
    provider: &P,
    parts: &PlannerParts,
    mi: usize,
    m: &AuctionMarket,
    bundle: Option<&planner::BundleQuote>,
    top: Option<&Result<planner::Plan>>,
    overrides: &StateOverride,
    block: u64,
    sim_time: u64,
    deadline: u64,
    min_bonus_usd: f64,
    atlas_gas_usd: f64,
    eth_usd: f64,
) -> Option<(serde_json::Value, Option<OpCandidate>)> {
    let (Some(solver), Venue::Callback(_)) = (&parts.solver, &parts.venue) else { return None };
    // Below the bonus floor: do not value a solver op for this market.
    let bonus: f64 = m.liq.iter().map(|v| v.bonus).sum();
    if bonus < min_bonus_usd {
        return Some((json!({ "ok": false, "skipped": "min_bonus", "bonus_usd": bonus }), None));
    }
    // A batch op over the whole bundle when it has an executable plan; otherwise the single top plan.
    if let Some(b) = bundle
        && let Some(plan) = &b.plan
    {
        return Some(batch_op(provider, parts, mi, m, b, plan, solver, overrides, block, sim_time, deadline, atlas_gas_usd, eth_usd).await);
    }

    let Some(Ok(plan)) = top else { return None };
    let payload = plan.payload.as_ref().filter(|_| plan.router == "direct")?;
    // Net of the op's gas as Atlas charges it (see valuation()).
    let net = plan.profit_usd.unwrap_or(0.0) - plan.gas_used.unwrap_or(0) as f64 * atlas_gas_usd;
    let Some(bid_usd) = parts.auction.bid_usd(Some(&m.meta.name), net.max(0.0), eth_usd) else {
        return Some((json!({ "bid_basis": "single_est", "ok": false, "skipped": "policy" }), None));
    };
    let bid_wei = U256::from((bid_usd / eth_usd.max(1.0) * 1e18) as u128);
    let venue = atlas_venue::AtlasBid { solver, bid_wei };
    let calldata = solver.calldata(m.meta.params.loanToken, &payload.calldata, bid_wei, U256::ZERO);
    let at = Some(venue::SimAt { number: block + 1, time: sim_time });
    let r = venue::LiquidationVenue::<Morpho>::simulate(&venue, provider, calldata, alloy::eips::BlockId::number(block), overrides, at, None, None).await;
    Some(match r {
        Ok(s) => {
            let net_after_bid = math::units(s.profit, m.meta.loan_decimals);
            let loan_usd = protocol::LendingProtocol::loan_usd(&parts.protocol, &m.meta, &m.snap, m.meta.params.loanToken, eth_usd).unwrap_or(0.0);
            let cand = (!s.profit.is_zero()).then(|| OpCandidate { market: mi, payload: payload.clone(), bid_wei, profit: s.profit, gas_used: s.gas_used, profit_usd: net_after_bid * loan_usd });
            (json!({ "bid_basis": "single_est", "bid_usd": bid_usd, "ok": true, "net_after_bid": net_after_bid, "gas_used": s.gas_used }), cand)
        }
        Err(e) => (json!({ "bid_basis": "single_est", "bid_usd": bid_usd, "ok": false, "error": e.to_string().chars().take(200).collect::<String>() }), None),
    })
}

/// A batch solver op over a bundle's whole plan: the bid comes from the estimate (gross minus the
/// whole metacall gas Atlas charges, decision 3), and the simulation only gates the send (its
/// profit, gas and the gap to the estimate are journaled to recalibrate the gas constants).
#[allow(clippy::too_many_arguments)]
async fn batch_op<P: Provider>(
    provider: &P,
    parts: &PlannerParts,
    mi: usize,
    m: &AuctionMarket,
    bundle: &planner::BundleQuote,
    plan: &planner::BundlePlan,
    solver: &atlas_venue::AtlasSolver,
    overrides: &StateOverride,
    block: u64,
    sim_time: u64,
    deadline: u64,
    atlas_gas_usd: f64,
    eth_usd: f64,
) -> (serde_json::Value, Option<OpCandidate>) {
    let loan = m.meta.params.loanToken;
    let bid_basis = bundle.gross_usd - (plan.gas_est + atlas_venue::METACALL_FIXED_GAS) as f64 * atlas_gas_usd;
    let row = |extra: serde_json::Value| {
        let mut base = json!({ "bid_basis": "batch_est", "positions": plan.items.len(), "router": plan.router, "bid_basis_usd": bid_basis, "est_gas": plan.gas_est });
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        base
    };
    // Gas floor: the estimated metacall gas already costs more than the gross -> do not bid.
    if bid_basis <= 0.0 {
        return (row(json!({ "ok": false, "skipped": "gas_floor" })), None);
    }
    let Some(bid_usd) = parts.auction.bid_usd(Some(&m.meta.name), bid_basis, eth_usd) else {
        return (row(json!({ "ok": false, "skipped": "policy" })), None);
    };
    let bid_wei = U256::from((bid_usd / eth_usd.max(1.0) * 1e18) as u128);
    let inner = solver.batch_inner(&m.meta, plan, deadline);
    let calldata = solver.calldata(loan, &inner, bid_wei, U256::ZERO);
    let venue = atlas_venue::AtlasBid { solver, bid_wei };
    let at = Some(venue::SimAt { number: block + 1, time: sim_time });
    let r = venue::LiquidationVenue::<Morpho>::simulate(&venue, provider, calldata, alloy::eips::BlockId::number(block), overrides, at, None, None).await;
    match r {
        Ok(s) => {
            let net_after_bid = math::units(s.profit, m.meta.loan_decimals);
            let loan_usd = protocol::LendingProtocol::loan_usd(&parts.protocol, &m.meta, &m.snap, loan, eth_usd).unwrap_or(0.0);
            // Candidate when the op nets positive after the bid; the minProfit (profit x keep) that
            // protects it against state drift is patched in at submit, as for the single-position op.
            let cand = (!s.profit.is_zero()).then(|| OpCandidate {
                market: mi,
                payload: executor::Payload { to: solver.solver, calldata: inner, min_profit: atlas_venue::SOLVER_MIN_PROFIT },
                bid_wei,
                profit: s.profit,
                gas_used: s.gas_used,
                profit_usd: net_after_bid * loan_usd,
            });
            (row(json!({ "bid_usd": bid_usd, "ok": true, "net_after_bid": net_after_bid, "gas_used": s.gas_used })), cand)
        }
        Err(e) => (row(json!({ "bid_usd": bid_usd, "ok": false, "error": e.to_string().chars().take(200).collect::<String>() })), None),
    }
}

/// Signs and submits the simulated solver op `c` for auction `a` (see solver_op): minProfit after
/// the bid = the simulated net x min_profit_keep, gas = simulated x gas_limit_buffer (within the
/// DappControl's limit), the auction's own max fee and deadline. Refused when the kill switch is
/// on, when this EOA already bid for that block, or when the op's worst-case gas charge to the
/// bond, on top of the bond's shrinkage today, does not fit `[atlas.solver] atlas_bond_budget_usd`.
/// Returns whether the endpoint accepted it, and the journal row saying what happened. The block's one-op slot is held only by an
/// accepted op: a refusal after claiming it gives it back for the auction's next candidate.
async fn submit_solver_op(sub: &atlas_submit::Submitter, parts: &PlannerParts, solver: &atlas_venue::AtlasSolver, a: &atlas::Auction, m: &AuctionMarket, c: &OpCandidate, eth_usd: f64) -> (bool, serde_json::Value) {
    let e = &parts.exec;
    let refused = |why: String| (false, json!({ "ok": false, "error": why }));
    if gates::kill_switch_on(&e.kill_switch_file) {
        return refused("kill switch file present".into());
    }
    let gas = ((c.gas_used as f64 * e.gas_limit_buffer) as u64).min(sub.gas_limit);
    // What Atlas may take from the bond for this op: all of its gas at the auction's max fee plus
    // the escrow's surcharge (also when the op fails).
    let worst_usd = gas as f64 * a.max_fee_per_gas as f64 * (1.0 + parts.atlas_surcharge) / 1e18 * eth_usd;
    // The bond's shrinkage today plus this op's worst case must fit `[atlas.solver] atlas_bond_budget_usd`.
    let (lost, bond) = {
        let l = parts.ledger.lock().unwrap();
        (l.atlas_loss_usd(eth_usd), l.bond_wei())
    };
    if let Some(b) = bond.filter(|b| math::units(*b, 18) < parts.atlas_min_bond_eth) {
        return refused(format!("atlETH bond {} < atlas.solver.min_bond_eth {}", math::units(b, 18), parts.atlas_min_bond_eth));
    }
    if lost + worst_usd > parts.atlas_bond_budget_usd {
        return refused(format!("atlas bond budget: lost {lost:.4} USD today + this op's worst case {worst_usd:.4} > {}", parts.atlas_bond_budget_usd));
    }
    if !sub.claim(a.deadline) {
        return refused("this EOA already bid for that block (one op per block per EOA)".into());
    }
    let min_profit = c.profit * U256::from((e.min_profit_keep * 1e4) as u64) / U256::from(10_000u64);
    let data = solver.op_data(m.meta.params.loanToken, &c.payload.calldata, c.bid_wei, min_profit);
    let signed = match sub.op(solver.solver, a.user_op_hash, a.deadline, a.max_fee_per_gas, gas, c.bid_wei, data).and_then(|op| atlas_submit::sign(op, &sub.domain, &sub.signer)) {
        Ok(x) => x,
        Err(err) => {
            sub.unclaim(a.deadline);
            return refused(err.to_string());
        }
    };
    match atlas_submit::submit(&sub.http, &sub.endpoint, &a.id, &signed).await {
        Ok(r) => {
            // Whether Atlas ran it is not observed here: the bond is read again soon (ledger.rs).
            parts.bond_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            warn!(auction = %a.id, market = %m.meta.name, bid_eth = math::units(c.bid_wei, 18), gas, "ATLAS solver op SUBMITTED");
            (true, json!({ "ok": true, "bid_wei": c.bid_wei.to_string(), "gas": gas, "min_profit": min_profit.to_string(), "answer": r }))
        }
        Err(err) => {
            sub.unclaim(a.deadline);
            warn!(auction = %a.id, error = %err, "ATLAS solver op submission failed");
            refused(err.to_string())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn valuation<P: Provider>(
    provider: &P,
    parts: &PlannerParts,
    a: &atlas::Auction,
    markets: &[AuctionMarket],
    head: Head,
    eth_usd: f64,
    sim_time: u64,
    atlas: &config::AtlasConfig,
    live: bool,
) -> serde_json::Value {
    // Execution checks: the largest position also tries Kyber ("best": quote + build + sim proves
    // the aggregator route runs in our contract), the rest only the direct router: Kyber throttles
    // bursts, and the bundle quote already priced the Kyber exit.
    let planner = parts.planner("direct");
    let planner_top = parts.planner(if live { "best" } else { "direct" });
    // The exits are weighed at what Atlas charges a solver op's gas (the auction's max fee plus the
    // escrow surcharge), where gas dominates, so a lower-gas exit can beat a higher-output one.
    let auction_gas_wei = (a.max_fee_per_gas as f64 * (1.0 + parts.atlas_surcharge)) as u128;
    let valued = futures::future::join_all(markets.iter().map(|m| async {
        let mut overrides = StateOverride::default();
        overrides.insert(m.meta.params.oracle, AccountOverride { code: Some(sim::constant_price_oracle(m.snap.price)), ..Default::default() });
        // One solver op holds only so many liquidations: bundle the largest that fit (liq is sorted
        // by bonus) instead of pricing an op that cannot exist.
        let fit = &m.liq[..m.liq.len().min(atlas_venue::bundle_capacity(atlas.solver_gas_limit, atlas.max_bundle_positions))];
        let bundle = match fit.is_empty() {
            true => None,
            false => Some(planner.quote_bundle(provider, &m.meta, &m.snap, fit, head.number, &overrides, head.base_fee, eth_usd, atlas_venue::bundle_swap_budget(atlas.solver_gas_limit, fit.len()), atlas_venue::ATLAS_BUNDLE_GAS, live, atlas.flash_headroom_bps, Some(auction_gas_wei)).await),
        };
        let bundle_ms = a.seen.elapsed().as_millis() as u64;
        let ctxs: Vec<PlanCtx<Morpho>> = m
            .liq
            .iter()
            .take(atlas.max_plans)
            .map(|v| PlanCtx { meta: &m.meta, snap: &m.snap, view: v, block: head.number, base_fee_wei: head.base_fee, eth_usd, overrides: overrides.clone(), deadline_block: a.deadline, sim_time, pending: false, price_probe: None })
            .collect();
        let plans = futures::future::join_all(ctxs.iter().enumerate().map(|(i, c)| if i == 0 { planner_top.plan(provider, c) } else { planner.plan(provider, c) })).await;
        (bundle, bundle_ms, overrides, ctxs.iter().map(|c| c.view).zip(plans).map(|(v, p)| (v.clone(), p)).collect::<Vec<_>>())
    }))
    .await;
    // The valuation (what paper trading times against the auction deadline) is complete here; the
    // optional solver-op simulations below are diagnostics and must not count towards it. Wall-clock,
    // because a shared valuation is timed against each sibling auction's own arrival.
    let valued_unix_ms = gates::unix_ms();
    // What Atlas charges a solver op for its gas: the metacall's gas price (the auction's
    // maxFeePerGas) plus the escrow's surcharge, per gas unit, in USD.
    let atlas_gas_usd = a.max_fee_per_gas as f64 * (1.0 + parts.atlas_surcharge) / 1e18 * eth_usd;
    let ops = futures::future::join_all(markets.iter().zip(&valued).enumerate().map(|(mi, (m, (bundle, _, ov, plans)))| solver_op(provider, parts, mi, m, bundle.as_ref(), plans.first().map(|(_, p)| p), ov, head.number, sim_time, a.deadline, atlas.min_bonus_usd, atlas_gas_usd, eth_usd))).await;
    let (mut solvers, cands): (Vec<Option<serde_json::Value>>, Vec<Option<OpCandidate>>) = ops.into_iter().map(|o| o.map_or((None, None), |(row, c)| (Some(row), c))).unzip();
    // Atlas takes one op per block from an EOA: the auction's candidates are tried most profitable
    // first (live + `submit`), regardless of which market's simulation finished first, until one
    // is accepted; a refused one (budget, endpoint) leaves the slot to the next.
    if let (Some(sub), Some(solver)) = (&parts.submitter, &parts.solver) {
        let mut order: Vec<&OpCandidate> = cands.iter().flatten().collect();
        order.sort_by(|x, y| y.profit_usd.total_cmp(&x.profit_usd));
        let mut accepted: Option<&str> = None;
        for c in order {
            let submitted = match accepted {
                Some(name) => json!({ "ok": false, "error": format!("the op of {name} was submitted for this auction (one op per block per EOA)") }),
                None => {
                    let (ok, row) = submit_solver_op(sub, parts, solver, a, &markets[c.market], c, eth_usd).await;
                    if ok {
                        accepted = Some(&markets[c.market].meta.name);
                    }
                    row
                }
            };
            if let Some(Some(row)) = solvers.get_mut(c.market) {
                row["submitted"] = submitted;
            }
        }
    }

    let (mut rows, mut positions) = (vec![], vec![]);
    let (mut n_liq, mut bonus_sum, mut net_sum, mut bundle_net, mut bundle_ms_max) = (0usize, 0.0f64, 0.0f64, 0.0f64, 0u64);
    for ((m, (bundle, bundle_ms, _, plans)), solver) in markets.iter().zip(valued).zip(solvers) {
        let bonus: f64 = m.liq.iter().map(|v| v.bonus).sum();
        n_liq += m.liq.len();
        bonus_sum += bonus;
        // The bid basis ("batch_est"): the bundle's exit gain minus the whole metacall gas Atlas
        // charges (the solver op's estimate plus the fixed metacall overhead), at the auction's gas
        // price, so a bid never spends what the solver owes Atlas.
        let atlas_net = bundle.as_ref().and_then(|b| b.net_usd.map(|_| b.gross_usd - (b.gas_units + atlas_venue::METACALL_FIXED_GAS) as f64 * atlas_gas_usd));
        bundle_net += atlas_net.unwrap_or(0.0).max(0.0);
        if bundle.is_some() {
            bundle_ms_max = bundle_ms_max.max(bundle_ms);
        }
        rows.push(json!({ "market": m.meta.name, "price_old": m.old_price.to_string(), "price_new": m.snap.price.to_string(), "liquidatable": m.liq.len(), "bonus_usd": bonus, "bundle": bundle, "atlas_net_usd": atlas_net, "bundle_ms": bundle_ms, "solver": solver }));
        for (v, r) in plans {
            let (router, net, err, quote_ms, sim_ms) = match r {
                Ok(p) => (p.router, p.net_usd, p.error, p.quote_ms, p.sim_ms),
                Err(e) => (String::new(), None, Some(e.to_string()), 0, 0),
            };
            net_sum += net.unwrap_or(0.0).max(0.0);
            positions.push(json!({ "market": m.meta.name, "user": v.user, "hf": v.hf, "bonus_usd": v.bonus, "router": router, "net_usd": net, "error": err, "quote_ms": quote_ms, "sim_ms": sim_ms }));
        }
    }
    json!({ "markets": rows, "positions": positions, "liquidatable": n_liq, "bonus_usd": bonus_sum, "bundle_net_usd": bundle_net, "net_usd": net_sum, "bundle_ms": bundle_ms_max, "valued_unix_ms": valued_unix_ms, "first_auction": a.id })
}

/// The risk gate's bid, checked at its own priority. The plan was simulated at the base priority;
/// pools that read tx.gasprice (priority-fee taxes, dynamic fees) pay out less at a higher bid, so
/// the plan is simulated again at the bid and the bid is kept only if it still clears min_net_usd
/// there; else the base priority goes. Returns (priority, simulated profit in loan units); None =
/// the plan carries no simulated profit, so nothing may be sent (minProfit would be 0).
async fn checked_priority<P: Provider>(parts: &PlannerParts, provider: &P, c: &PlanCtx<'_, Morpho>, plan: &planner::Plan, bid: u128) -> Option<(u128, f64)> {
    let base = parts.exec.priority_fee_wei;
    let (Some(payload), Some(profit), Some(gas)) = (&plan.payload, plan.sim_profit.filter(|p| *p > 0.0), plan.gas_used) else { return None };
    if bid <= base {
        return Some((base, profit));
    }
    let fee = venue::SimFee { max_fee_per_gas: c.base_fee_wei * 2 + bid, max_priority_fee_per_gas: bid };
    let r = venue::LiquidationVenue::<Morpho>::simulate(&parts.venue, provider, payload.calldata.clone(), c.block_id(), &c.overrides, c.sim_at(), Some(fee), None).await;
    // The same USD price the plan was scored with.
    let loan_usd = protocol::LendingProtocol::loan_usd(&parts.protocol, c.meta, c.snap, c.view.pair.debt, c.eth_usd).unwrap_or(0.0);
    let at_bid = r.as_ref().ok().map(|s| {
        let p = math::units(s.profit, c.meta.loan_decimals);
        let cost = gas as f64 * (c.base_fee_wei + bid) as f64 / 1e18 * c.eth_usd + plan.l1_fee_usd.unwrap_or(0.0);
        (p, p * loan_usd - cost)
    });
    Some(match at_bid {
        Some((p, net)) if net >= parts.exec.min_net_usd && p > 0.0 => (bid, p.min(profit)),
        other => {
            info!(bid_gwei = bid as f64 / 1e9, net_at_bid = ?other.map(|x| x.1), error = ?r.err().map(|e| e.to_string()), "the bid does not pay at its own priority; base bid");
            (base, profit)
        }
    })
}

/// A plan the risk gate approved, with the bid to send it at.
struct Approved {
    index: usize,
    state: &'static str,
    plan: planner::Plan,
    priority_fee_wei: u128,
    /// The competitor's predicted priority fee (None = no model yet).
    competitor_wei: Option<u128>,
}

/// What became of an approved plan.
enum SendOutcome {
    Sent(executor::Inflight),
    /// Not sent, by a spending limit: journaled as a `refused` row.
    Refused(serde_json::Value),
    /// Not sent for a transient reason (in flight, cooldown, ...): logged only.
    Skipped,
}

/// The most a tx may cost if it reverts at its whole gas limit (`[exec] max_tx_cost_eth`), and the
/// priority that fits it: `gas_limit x (2 x base fee + priority) + L1 fee <= cap`. None = even the
/// base priority does not fit, or the cut bid no longer beats the competitor (a tx sent to lose).
fn fit_tx_cost(gas_limit: u64, base_fee_wei: u128, priority_wei: u128, l1_fee_wei: u128, cap_wei: u128, floor_wei: u128, competitor_wei: Option<u128>) -> Option<u128> {
    let worst = |p: u128| gas_limit as u128 * (base_fee_wei * 2 + p) + l1_fee_wei;
    if worst(priority_wei) <= cap_wei {
        return Some(priority_wei);
    }
    let fit = (cap_wei.saturating_sub(l1_fee_wei) / (gas_limit.max(1) as u128)).checked_sub(base_fee_wei * 2)?;
    (fit >= floor_wei && competitor_wei.is_none_or(|c| fit > c)).then_some(fit)
}

/// Live send of an approved plan (skips at max_inflight, when this position or another one in the
/// same market is in flight, or when the wallet cannot cover the worst-case gas). The bid is the
/// risk gate's, checked at its own priority first, then held to `[exec] max_tx_cost_eth`.
async fn send_approved<P: Provider>(e: &ExecHandle, parts: &PlannerParts, provider: &P, c: &PlanCtx<'_, Morpho>, a: &Approved, path: &str) -> SendOutcome {
    let (plan, bid, competitor_wei) = (&a.plan, a.priority_fee_wei, a.competitor_wei);
    let (meta, v, cfg) = (c.meta, c.view, &parts.exec);
    if !e.can_send() || e.is_inflight(tx_key(v)) {
        return SendOutcome::Skipped;
    }
    // Plans are simulated on a state without the txs in flight: one in the same market sells the
    // same collateral through the same pools first, so this plan's price would no longer hold.
    if e.is_group_inflight(v.market as u64) {
        info!(market = %meta.name, user = %v.user, path, "a liquidation in this market is in flight; not sending another on a stale price");
        return SendOutcome::Skipped;
    }
    let (Some(payload), Some(gas)) = (plan.payload.clone(), plan.gas_used) else { return SendOutcome::Skipped };
    let Some((checked_wei, profit)) = checked_priority(parts, provider, c, plan, bid).await else { return SendOutcome::Skipped };
    let (base_fee_wei, block, deadline_block) = (c.base_fee_wei, c.block, c.deadline_block);
    let min_profit = U256::from((profit * cfg.min_profit_keep * 10f64.powi(meta.loan_decimals as i32)) as u128);
    let gas_limit = (gas as f64 * cfg.gas_limit_buffer) as u64;
    let l1_fee_wei = (plan.l1_fee_usd.unwrap_or(0.0) / c.eth_usd.max(1.0) * 1e18) as u128;
    let cap_wei = (cfg.max_tx_cost_eth * 1e18) as u128;
    let refused = |reason: &str, worst_wei: u128| {
        info!(market = %meta.name, user = %v.user, path, reason, worst_eth = worst_wei as f64 / 1e18, "not sending");
        SendOutcome::Refused(json!({ "kind": "refused", "block": block, "market": meta.name, "user": v.user, "path": path, "reason": reason,
                                     "worst_eth": worst_wei as f64 / 1e18, "cap_eth": cfg.max_tx_cost_eth, "bid_wei": checked_wei.to_string() }))
    };
    let Some(priority_fee_wei) = fit_tx_cost(gas_limit, base_fee_wei, checked_wei, l1_fee_wei, cap_wei, cfg.priority_fee_wei, competitor_wei) else {
        return refused("tx_cost_cap", gas_limit as u128 * (base_fee_wei * 2 + checked_wei) + l1_fee_wei);
    };
    if priority_fee_wei < checked_wei {
        info!(market = %meta.name, user = %v.user, bid_gwei = checked_wei as f64 / 1e9, cut_gwei = priority_fee_wei as f64 / 1e9, "bid cut to fit exec.max_tx_cost_eth");
    }
    let worst_wei = gas_limit as u128 * (base_fee_wei * 2 + priority_fee_wei) + l1_fee_wei;
    // The background reading minus what the txs in flight may still burn; a live read only before
    // the first reading.
    let spendable = match e.spendable() {
        Some(b) => Ok(b),
        None => provider.get_balance(e.address()).await.map_err(anyhow::Error::from),
    };
    match spendable {
        Ok(b) if b < U256::from(worst_wei) => {
            warn!(balance_wei = %b, worst_wei, "hot wallet cannot cover worst-case gas; not sending");
            return refused("wallet", worst_wei);
        }
        Err(err) => {
            warn!(error = %err, "balance check failed; not sending");
            return SendOutcome::Skipped;
        }
        _ => {}
    }
    // The daily failed-gas budget holds this tx's worst case (all of its gas limit reverted, plus
    // the L1 fee) while it is in flight: sends running at once cannot each fit the cap alone and
    // overrun it together. Released in on_settled.
    let key = tx_key(v);
    let worst_usd = worst_wei as f64 / 1e18 * c.eth_usd;
    let now = gates::unix_secs();
    // A reservation already held for this position is refused, not replaced.
    let reserved = parts.ledger.lock().unwrap().failed.reserve(key, now, worst_usd, cfg.daily_gas_budget_usd);
    if let Err(why) = reserved {
        return match why {
            gates::Refusal::Budget { .. } => refused("failed_gas_budget", worst_wei),
            _ => SendOutcome::Skipped,
        };
    }
    // One liquidation per market in flight, enforced by the executor task itself: the check above
    // and this send are not atomic.
    let req = executor::SendRequest { payload, min_profit, gas_limit, base_fee_wei, priority_fee_wei, key, exclusive_group: true, block, deadline_block };
    match e.send(req).await {
        Ok(inf) => {
            info!(hash = %inf.hash, block, deadline_block, market = %meta.name, user = %v.user, path, "LIVE tx SENT");
            SendOutcome::Sent(inf)
        }
        Err(err) => {
            parts.ledger.lock().unwrap().failed.release(key);
            // The in-flight rules or the cooldown: expected, not a send failure.
            match err.downcast_ref::<executor::SendRefused>() {
                Some(why) => info!(market = %meta.name, user = %v.user, path, reason = %why, "not sending (executor)"),
                None => warn!(error = %err, "LIVE send failed"),
            }
            SendOutcome::Skipped
        }
    }
}

/// The executor key of a liquidation: the market is the group (one tx per market in flight), the
/// borrower the item.
fn tx_key(v: &PositionView) -> executor::TxKey {
    executor::TxKey::of_address(v.market as u64, v.user)
}

/// Plans the candidates in `ready` (each yields its index and plan result) and sends the approved
/// ones as soon as each is ready, WITHOUT pausing the planning of the others; several sends run at
/// once (their bid re-simulations in parallel; the executor task broadcasts them in turn). One per
/// market per batch: a second plan in a market already being sent waits for the next batch, priced
/// on a state that includes the first. `on_plan` journals a plan and returns it if approved;
/// `ctx_of(i, plan_state)` gives the context it was planned on.
///
/// `side` = auctions and oracle logs that must not wait for this batch: each one is handled by
/// `on_side` the moment it arrives, ahead of the batch, whose planning resumes afterwards.
#[allow(clippy::too_many_arguments)]
async fn plan_and_send<'c, P, S, R, F, C, G>(
    mut ready: S,
    exec: Option<&ExecHandle>,
    parts: &PlannerParts,
    provider: &P,
    path: &str,
    journal: &mut std::fs::File,
    mut on_plan: F,
    ctx_of: C,
    mut side: Option<&mut tokio::sync::mpsc::Receiver<SideEvent>>,
    mut on_side: G,
) -> Result<()>
where
    P: Provider,
    S: futures::Stream<Item = R> + Unpin,
    F: FnMut(R, &mut std::fs::File) -> Result<Option<Approved>>,
    C: Fn(usize, &'static str) -> &'c PlanCtx<'c, Morpho>,
    G: AsyncFnMut(SideEvent, &mut std::fs::File) -> Result<()>,
{
    use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
    let mut sending: FuturesUnordered<futures::future::BoxFuture<'_, SendOutcome>> = FuturesUnordered::new();
    let mut claimed: std::collections::HashSet<usize> = Default::default();
    let mut planning = true;
    while planning || !sending.is_empty() {
        tokio::select! {
            biased;
            ev = async { side.as_mut().expect("guarded").recv().await }, if side.is_some() => match ev {
                Some(ev) => on_side(ev, journal).await?,
                None => side = None,
            },
            next = ready.next(), if planning => match next {
                Some(r) => {
                    if let (Some(a), Some(e)) = (on_plan(r, journal)?, exec) {
                        let c = ctx_of(a.index, a.state);
                        if claimed.insert(c.view.market) {
                            sending.push(async move { send_approved(e, parts, provider, c, &a, path).await }.boxed());
                        } else {
                            info!(market = %c.meta.name, user = %c.view.user, path, "another liquidation in this market is being sent; this one waits for a fresh plan");
                        }
                    }
                }
                None => planning = false,
            },
            Some(out) = sending.next(), if !sending.is_empty() => match out {
                SendOutcome::Sent(inf) => writeln!(journal, "{}", json!({ "kind": "sent", "block": inf.sent_block, "path": path, "tx": inf }))?,
                SendOutcome::Refused(row) => writeln!(journal, "{row}")?,
                SendOutcome::Skipped => {}
            }
        }
    }
    Ok(())
}

/// A tx the executor task settled: journaled, its worst-case reservation released, and a revert's
/// real cost charged to the failed-gas budget. Returns (tx hash, its gas + L1 cost in USD) of an
/// included tx, for the PnL (see settle_pnl).
fn on_settled(s: actor::Settled, block: u64, now_unix: u64, eth_usd: f64, cooldown_secs: u64, ledger: &mut Ledger, journal: &mut std::fs::File) -> Result<Option<(alloy::primitives::B256, f64)>> {
    let actor::Settled { inflight, outcome, cooldown_started } = s;
    ledger.failed.release(inflight.key);
    let mut included = None;
    if let executor::Outcome::Included { success, gas_used, effective_gas_price, l1_fee_wei, .. } = &outcome {
        let cost = (*gas_used as f64 * *effective_gas_price as f64 + *l1_fee_wei as f64) / 1e18 * eth_usd;
        if !success {
            ledger.failed.charge(now_unix, cost);
        }
        included = Some((inflight.hash, cost));
    }
    info!(hash = %inflight.hash, ?outcome, "LIVE tx outcome");
    writeln!(journal, "{}", json!({ "kind": "outcome", "block": block, "tx": inflight, "outcome": outcome }))?;
    if cooldown_started {
        warn!(cooldown_s = cooldown_secs, "LIVE sending paused after a burst of failed txs; resumes by itself");
    }
    Ok(included)
}

/// USD paid to `receiver` in the receipt of `hash`: ERC20 Transfers of the tracked markets' loan
/// tokens (`loan_tokens`: token -> decimals), priced by the loan-token model. Other tokens (a
/// connector leftover) are logged and not counted.
async fn paid_to<P: Provider>(p: &P, chain_id: u64, hash: alloy::primitives::B256, receiver: Address, loan_tokens: &HashMap<Address, u8>, eth_usd: f64) -> Result<f64> {
    alloy::sol! { event Transfer(address indexed from, address indexed to, uint256 value); }
    use alloy::sol_types::SolEvent;
    let r = p.get_transaction_receipt(hash).await?.with_context(|| format!("no receipt for {hash}"))?;
    let mut usd = 0.0;
    for log in r.inner.logs() {
        let Ok(t) = Transfer::decode_log(&log.inner) else { continue };
        if t.to != receiver {
            continue;
        }
        let token = log.address();
        match (loan_tokens.get(&token), morpho::loan_usd_model(chain_id, token, eth_usd)) {
            (Some(decimals), Some(px)) => usd += math::units(t.value, *decimals) * px,
            _ => info!(%token, value = %t.value, %hash, "profit paid in a token without a price model; not counted in the PnL"),
        }
    }
    Ok(usd)
}

#[allow(clippy::too_many_arguments)]
async fn watch<P: Provider + Clone + Send + Sync + 'static>(
    engine: &mut MorphoEngine<P>,
    from_block: Option<u64>,
    pool: rpc::RpcPool,
    ws_endpoints: Vec<(String, String)>,
    live: Option<(alloy::signers::local::PrivateKeySigner, Vec<(String, String)>)>,
    atlas_pool: Option<(P, rpc::RpcPool)>,
    auction: Arc<dyn AuctionBidPolicy>,
    priority: Arc<dyn PriorityBidPolicy>,
    mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<()> {
    if let Some(dir) = std::path::Path::new(&engine.cfg.journal_path).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut journal = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&engine.cfg.journal_path)
        .with_context(|| format!("opening journal {}", engine.cfg.journal_path))?;
    let (kyber, simulator) = build_planner_parts(engine).await?;
    let v3 = build_v3(engine).await?;
    // Live: the day's spend and PnL survive restarts (an unreadable ledger refuses live mode).
    let ledger = match &live {
        Some((signer, _)) => {
            let path = crate::ledger::ledger_path(&engine.cfg.exec.ledger_file, &engine.cfg.exec.nonce_path(signer.address()), signer.address());
            let l = Ledger::load(path.clone()).context("live mode needs a readable ledger")?;
            info!(ledger = %path.display(), "ledger loaded");
            l
        }
        None => Ledger::in_memory(),
    };
    let parts = Arc::new(PlannerParts { protocol: engine.protocol.clone(), kyber, venue: simulator, solver: build_atlas_solver(engine).await?, atlas_surcharge: atlas_surcharge(engine).await, v3, exec: engine.cfg.exec.clone(), bids: std::sync::Mutex::new(WinningBids::new(engine.cfg.exec.bid_window_secs)), submitter: build_submitter(engine, live.as_ref().map(|l| &l.0)).await?, ledger: std::sync::Mutex::new(ledger), bond_dirty: Default::default(), atlas_bond_budget_usd: engine.cfg.atlas.solver.atlas_bond_budget_usd, atlas_min_bond_eth: engine.cfg.atlas.solver.min_bond_eth, auction, priority });

    // Watchlist rows are journaled only when a position enters it or its HF moves > 0.2%.
    let mut last_logged_hf: HashMap<(usize, Address), f64> = HashMap::new();
    // Candidate -> block it was last planned at.
    let mut last_planned: HashMap<(usize, Address), u64> = HashMap::new();
    let mut tracker = loss::LossTracker::default();
    // Live sending (None = shadow). The key never leaves the keystore decrypt. The executor runs
    // as its own task (onchain_core::actor): sends go to it as messages, it polls receipts on every
    // block announced on `blocks_tx`, and settled txs come back on `settled`.
    let (blocks_tx, blocks_rx) = watch::channel(0u64);
    let mut profit_receiver = None;
    let (exec, mut settled) = match live {
        None => (None, None),
        Some((signer, broadcast)) => {
            profit_receiver = Some(preflight_live(engine, signer.address()).await?);
            let b = executor::Broadcaster::new(executor::Broadcaster::client()?, broadcast);
            b.spawn_keepalive(executor::KEEPALIVE_EVERY);
            let nonce_file = engine.cfg.exec.nonce_path(signer.address());
            info!(nonce_file = %nonce_file.display(), "shared hot-wallet nonce file");
            let mut e = executor::Executor::new(engine.provider.clone(), signer, engine.cfg.chain_id, b, engine.cfg.exec.failure_policy())
                .with_shared_nonce(nonce_file)
                .with_max_inflight(engine.cfg.exec.max_inflight);
            e.sync_nonce().await?;
            e.spawn_nonce_watch(executor::NONCE_WATCH_EVERY);
            warn!(address = %e.address, "LIVE MODE: approved plans WILL be sent");
            let (h, s) = actor::spawn(e, blocks_rx);
            (Some(h), Some(s))
        }
    };
    // Background borrower refresh (see the end of the loop).
    let mut refresh_job: Option<tokio::task::JoinHandle<BorrowerRefresh>> = None;
    // Observed block time (for deadline_secs -> blocks and the accrual lookahead).
    let mut clock = BlockClock::default();
    let tip = heads::http_head(&engine.provider, BlockNumberOrTag::Latest).await?;
    let mut known_tip = tip.number;
    if let Ok(back) = heads::http_head(&engine.provider, BlockNumberOrTag::Number(tip.number.saturating_sub(10))).await {
        clock.push(back.number, back.timestamp);
    }
    clock.push(tip.number, tip.timestamp);
    let mut last_block = from_block.map(|b| b - 1).unwrap_or(known_tip);
    engine.refresh_borrowers(last_block.into()).await?;
    let mut last_refresh = Instant::now();
    let ws_names: Vec<String> = ws_endpoints.iter().map(|w| w.0.clone()).collect();
    let logs_ws = ws_endpoints.first().cloned();
    let mut ws = (!ws_endpoints.is_empty()).then(|| heads::spawn_ws_many(ws_endpoints));
    // Which source delivered each processed block first (WS endpoint index, or HTTP polling).
    let mut head_wins: HashMap<usize, u64> = HashMap::new();
    let mut prev_snaps: Option<Vec<Option<morpho::onchain::MarketSnapshot>>> = None;
    // Snapshot throttle (A1): skip the per-head market `eth_call` (the bulk of egress) when nothing
    // changed. `last_snap_at` is when snapshots were really fetched; `prev_fresh` is whether
    // `prev_snaps` came from a real fetch (only those may feed the Atlas cross-check).
    let mut last_snap_at: Option<Instant> = None;
    let mut prev_fresh = false;
    let snapshot_max_age = Duration::from_millis(engine.cfg.exec.snapshot_max_age_ms);
    let market_ids = engine.market_ids();
    let liquidator = venue::LiquidationVenue::<Morpho>::target(&parts.venue);
    seed_winning_bids(&parts, &engine.cfg.journal_path, liquidator, engine.cfg.atlas.address);
    if exec.is_some() {
        warn!(from_block = last_block + 1, ws = ws.is_some(), %liquidator, max_inflight = engine.cfg.exec.max_inflight, "LIVE mode: watching blocks, approved liquidations WILL be sent");
    } else {
        info!(from_block = last_block + 1, ws = ws.is_some(), %liquidator, "shadow mode: watching blocks, nothing will be sent");
    }

    // Rows finished by background tasks (auction valuations, oracle logs), written by this loop.
    let (journal_tx, mut journal_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // The Atlas path: SVR auctions and the oracle logs that keep the price model current, on one
    // channel the loop serves while waiting for a head, while fetching, and while planning.
    let (side_tx, side_rx) = tokio::sync::mpsc::channel::<SideEvent>(SIDE_CHANNEL);
    let mut side_rx = Some(side_rx);
    let mut atlas_st: Option<AtlasState<P>> = None;
    if engine.cfg.atlas.observe && from_block.is_none() {
        let model = oracle::PriceModel::load(&engine.provider, &engine.metas, alloy::eips::BlockId::latest()).await;
        let (name, url) = logs_ws.context("[atlas] observe needs a WebSocket endpoint ([[rpc]] ws) for the oracle logs")?;
        let logs = oracle_logs::spawn(name, url, model.aggregators(), side_tx.clone());
        let dropped = atlas::spawn(engine.cfg.atlas.ws.clone(), engine.cfg.chain_id, engine.cfg.atlas.control, side_tx.clone());
        let (provider, own_pool) = match atlas_pool {
            Some((p, pool)) => (p, Some(pool)),
            None => (engine.provider.clone(), None),
        };
        info!(own_pool = own_pool.is_some(), "atlas: valuing SVR auctions; price model from the oracle logs");
        atlas_st = Some(AtlasState {
            provider,
            pool: own_pool,
            model,
            logs,
            block_ts: HashMap::new(),
            held: vec![],
            max_update_block: 0,
            missed_streak: 0,
            resync_all: false,
            svr_cutoff_secs: engine.cfg.atlas.svr_cutoff_secs,
            journal_tx: journal_tx.clone(),
            valuations: Arc::new(tokio::sync::Semaphore::new(engine.cfg.atlas.max_concurrent.max(1))),
            auction_cache: Arc::default(),
            dropped,
            dropped_journaled: 0,
        });
    }
    drop(side_tx);
    let run_started = Instant::now();
    // Context a side event needs: the latest processed head, ETH price, block time.
    let mut last_ctx: Option<(Head, f64, f64)> = None;
    let mut last_minute = Instant::now();
    let mut last_usage = Instant::now();
    let mut head_ms: Vec<u64> = Vec::new();
    let mut skipped_heads = 0u64;
    let mut heads_since_bond = BOND_CHECK_HEADS;
    let loan_decimals: HashMap<Address, u8> = engine.metas.iter().map(|m| (m.params.loanToken, m.loan_decimals)).collect();

    loop {
        if shutdown.as_ref().is_some_and(|s| *s.borrow()) {
            info!("shutdown requested; watch loop ends");
            parts.ledger.lock().unwrap().save_or_warn();
            return Ok(());
        }
        // Live: newest head. Replaying: step one block at a time so nothing is skipped.
        let replaying = known_tip > last_block + 10;
        let head = if replaying {
            heads::http_head(&engine.provider, BlockNumberOrTag::Number(last_block + 1)).await
        } else {
            // While waiting for the next block, handle auctions and oracle logs.
            loop {
                let stop = async {
                    match shutdown.as_mut() {
                        Some(s) => {
                            let _ = s.wait_for(|v| *v).await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                let ev = tokio::select! {
                    _ = stop => {
                        info!("shutdown requested; watch loop ends");
                        parts.ledger.lock().unwrap().save_or_warn();
                        return Ok(());
                    }
                    ev = async { side_rx.as_mut().expect("guarded").recv().await }, if side_rx.is_some() => Err(ev),
                    h = wait_head(&mut ws, &engine.provider, last_block, engine.cfg.poll_ms) => Ok(h),
                };
                match ev {
                    Ok(h) => break h,
                    Err(Some(ev)) => {
                        if let Some(st) = atlas_st.as_mut() {
                            let ctx = prev_snaps.as_deref().zip(last_ctx.as_ref()).map(|(s, (h, eth, bt))| (s, h, *eth, *bt));
                            on_side(engine, st, ev, ctx, &parts);
                            drain_journal(&mut journal_rx, &mut journal)?;
                        }
                    }
                    Err(None) => {
                        warn!("side event tasks ended; Atlas path off");
                        side_rx = None;
                    }
                }
            }
        };
        let h = match head {
            Ok(h) => h,
            Err(e) => {
                warn!(error = format!("{e:#}"), "head fetch failed");
                tokio::time::sleep(Duration::from_millis(engine.cfg.poll_ms)).await;
                continue;
            }
        };
        known_tip = known_tip.max(h.number);
        let number = h.number;
        // The executor task checks its txs in flight on this block, alongside the fetch below.
        blocks_tx.send_replace(number);
        if !replaying {
            clock.push(number, h.timestamp);
        }
        let block_time = clock.block_time();
        let deadline_block = number + gates::deadline_blocks(engine.cfg.exec.deadline_secs, block_time);
        let replan_blocks = engine.cfg.exec.replan_secs.map_or(engine.cfg.exec.replan_blocks, |s| (s / block_time).ceil().max(1.0) as u64);
        let t0 = Instant::now();

        let gap = number - last_block;
        if gap > 1 && !replaying {
            skipped_heads += gap - 1;
        }
        if gap > engine.cfg.max_log_range && !replaying {
            // Too far behind for one getLogs: liquidations in the gap are not analysed.
            warn!(gap, "fell behind; full reload");
            engine.positions.clear();
            engine.refresh_borrowers(number.into()).await?;
            last_refresh = Instant::now();
            last_block = number - 1;
            prev_snaps = None;
        }
        // The Atlas price model: held oracle logs, SVR cutoffs, and the previous block's cross-check.
        // Only a genuinely fetched previous snapshot may feed the cross-check (a reused one shares
        // the model's prices, so a missed log would not show up — see the snapshot throttle below).
        if let Some(st) = atlas_st.as_mut() {
            let cross = if prev_fresh { prev_snaps.as_deref().map(|s| (last_block, s)) } else { None };
            st.on_head(&h, cross).await;
        }

        // 1. Events since the last block, then the market snapshots. Side events arriving meanwhile
        //    are handled right away instead of queueing behind the block path; `drive!` runs a
        //    future while servicing them. Interest is accrued to ~the next block so interest-only
        //    crossings show up early.
        let accrue_to = h.timestamp + engine.cfg.lookahead_secs(block_time);
        let from_block_logs = last_block + 1;
        macro_rules! drive {
            ($fut:expr) => {{
                let fut = $fut;
                tokio::pin!(fut);
                loop {
                    let (Some(st), Some(rx)) = (atlas_st.as_mut(), side_rx.as_mut()) else { break fut.await };
                    tokio::select! {
                        r = &mut fut => break r,
                        Some(ev) = rx.recv() => {
                            let ctx = prev_snaps.as_deref().zip(last_ctx.as_ref()).map(|(s, (lh, eth, bt))| (s, lh, *eth, *bt));
                            on_side(engine, st, ev, ctx, &parts);
                            drain_journal(&mut journal_rx, &mut journal)?;
                        }
                    }
                }
            }};
        }
        let touched = drive!(engine.touched(&market_ids, from_block_logs, number));
        // Snapshot throttle (A1): the per-head market snapshot `eth_call` is ~85% of the VM's
        // egress, so skip it when nothing moved — no oracle update, no market-touching log, and the
        // last real snapshot is younger than `snapshot_max_age`. Reuse the previous snapshot then
        // (its prices are still current; interest drift over < max_age is negligible for the HF).
        let oracle_updated = atlas_st.as_ref().is_some_and(|st| st.pending_update(last_block));
        let fresh_enough = last_snap_at.is_some_and(|t| t.elapsed() < snapshot_max_age);
        let touched_any = touched.as_ref().map(|t| !t.is_empty()).unwrap_or(true);
        let fetched_this_head = must_refetch_snapshots(prev_snaps.is_some(), oracle_updated, touched_any, fresh_enough);
        let snaps = if fetched_this_head {
            let s = drive!(async {
                match &prev_snaps {
                    Some(prev) => engine.snapshots_fast(number.into(), accrue_to, prev).await,
                    None => engine.snapshots(number.into(), accrue_to).await,
                }
            });
            if s.is_ok() {
                last_snap_at = Some(Instant::now());
            }
            s
        } else {
            // Reuse: prev_snaps is Some (checked above via `prev_snaps.is_none()`).
            Ok(prev_snaps.clone().unwrap_or_default())
        };
        prev_fresh = fetched_this_head;
        let fetch_ms = t0.elapsed().as_millis() as u64;
        let (touched, snaps) = match (touched, snaps) {
            (Ok(t), Ok(s)) => (t, s),
            // Do not advance last_block: the same range is retried next loop.
            (Err(e), _) | (_, Err(e)) => {
                warn!(block = number, error = format!("{e:#}"), "fetch failed");
                continue;
            }
        };

        // 2. Re-read only the positions those events touched (often none).
        let t1 = Instant::now();
        let (touched, liquidations) = match engine.apply_touched(touched, number).await {
            Ok(x) => x,
            Err(e) => {
                warn!(block = number, error = format!("{e:#}"), "position reload failed");
                continue;
            }
        };
        let reload_ms = t1.elapsed().as_millis() as u64;
        last_block = number;
        let eth_usd = engine.eth_usd(&snaps);

        // Live: the txs the executor task has settled since the last block, including any in this
        // block (its receipt poll ran alongside the fetch; waited for briefly if it is still going).
        if let Some(e) = exec.as_ref() {
            e.wait_polled(number, actor::RECEIPT_WAIT).await;
        }
        let mut included_txs = vec![];
        if let Some(rx) = settled.as_mut() {
            while let Ok(s) = rx.try_recv() {
                if let Some(t) = on_settled(s, number, h.timestamp, eth_usd, engine.cfg.exec.failure_cooldown_secs, &mut parts.ledger.lock().unwrap(), &mut journal)? {
                    included_txs.push(t);
                }
            }
        }

        // 3. In-memory HF for every position (pure CPU).
        let t2 = Instant::now();
        let views = engine.evaluate(&snaps);
        let eval_us = t2.elapsed().as_micros() as u64;
        // Head received -> all candidates known: the number that matters for racing.
        let decide_ms = h.seen_at.elapsed().as_millis() as u64;
        // Live monitor snapshot (bot live): after the timing above, so it never delays a race.
        if let Err(err) = live::write_snapshot(&engine.cfg.dashboard, &engine.metas, &snaps, &engine.positions, &views, h.number, h.timestamp) {
            warn!(error = %err, "live snapshot write failed");
        }

        // 4. Loss analysis, part 1: classify every real liquidation against what we had seen
        //    before it (pure memory; must happen before on_views updates the detections). The
        //    receipt lookups for who-paid-what run AFTER planning/sending (part 2), so a burst of
        //    liquidations never delays our own reaction.
        let mut liq_rows: Vec<loss::LiquidationRow> = liquidations
            .iter()
            .map(|(mi, ev)| {
                let ever = engine.ever_tracked.contains(&(*mi, ev.borrower));
                let (meta, snap) = (&engine.metas[*mi], snaps[*mi].as_ref());
                let mut row = tracker.classify(*mi, ev, meta, snap, ever, None, eth_usd);
                row.timestamp = h.timestamp;
                row.gross_usd = snap.and_then(|s| protocol::LendingProtocol::loan_usd(&engine.protocol, meta, s, meta.params.loanToken, eth_usd)).map(|u| row.gross_bonus * u);
                row
            })
            .collect();
        tracker.on_views(number, &views, engine.cfg.min_bonus);

        // 5. Journal candidates / watchlist; collect candidates due for (re)planning.
        let mut to_plan: Vec<&PositionView> = Vec::new();
        for v in &views {
            let meta = &engine.metas[v.market];
            let key = (v.market, v.user);
            let kind = if v.health.is_liquidatable() && v.max_seize > U256::ZERO && v.bonus >= engine.cfg.min_bonus {
                if !replaying && last_planned.get(&key).is_none_or(|b| number >= b + replan_blocks) {
                    to_plan.push(v);
                }
                "candidate"
            } else if v.hf < engine.cfg.watch_hf {
                if last_logged_hf.get(&key).is_some_and(|p| (p - v.hf).abs() / p < 0.002) {
                    continue;
                }
                "watch"
            } else {
                break; // views are sorted by HF
            };
            last_logged_hf.insert(key, v.hf);
            let row = JournalRow {
                kind,
                block: number,
                timestamp: h.timestamp,
                market: &meta.name,
                user: v.user,
                hf: v.hf,
                debt: v.debt,
                seize: v.max_seize.to_string(),
                repay: v.quote.map(|q| q.repaid_assets.to_string()).unwrap_or_default(),
                bonus: v.bonus,
            };
            writeln!(journal, "{}", serde_json::to_string(&row)?)?;
        }

        // 6. Plan + simulate the most valuable candidates (shadow: result is only journaled), all
        //    at once; each is handled (journaled, sent) as soon as its plan is ready, so the best
        //    one never waits for a slow aggregator quote of another.
        to_plan.sort_by(|a, b| b.bonus.total_cmp(&a.bonus));
        let planner = parts.planner(&engine.cfg.router.mode);
        let planned = to_plan.len().min(engine.cfg.exec.max_plans_per_block);
        let ctxs: Vec<PlanCtx<Morpho>> = to_plan
            .into_iter()
            .take(planned)
            .map(|v| {
                let meta = &engine.metas[v.market];
                let snap = snaps[v.market].as_ref().expect("evaluated markets have snapshots");
                last_planned.insert((v.market, v.user), number);
                PlanCtx { meta, snap, view: v, block: number, base_fee_wei: h.base_fee, eth_usd, overrides: StateOverride::default(), deadline_block, sim_time: accrue_to, pending: false, price_probe: None }
            })
            .collect();
        let ready: futures::stream::FuturesUnordered<_> = ctxs
            .iter()
            .enumerate()
            .map(|(i, ctx)| {
                let (planner, provider) = (&planner, &engine.provider);
                async move { (i, planner.plan(provider, ctx).await) }
            })
            .collect();
        let on_plan = |(i, res): (usize, Result<planner::Plan>), journal: &mut std::fs::File| -> Result<_> {
            let ctx = &ctxs[i];
            let (meta, v) = (ctx.meta, ctx.view);
            match res {
                Ok(plan) => {
                    tracker.on_plan((v.market, v.user), number, plan.net_usd, plan.error.clone());
                    let kill = std::path::Path::new(&engine.cfg.exec.kill_switch_file).exists();
                    let decision = risk::decide(&engine.cfg.exec, &plan, eth_usd, h.base_fee, kill, parts.priority.bid(&meta.name, &plan, parts.competition(&meta.name, h.timestamp), eth_usd));
                    info!(
                        block = number, market = %meta.name, user = %v.user, hf = v.hf,
                        quoted_gross = plan.quoted_gross, net_usd = ?plan.net_usd, error = ?plan.error, decision = ?decision,
                        "PLAN"
                    );
                    let row = json!({ "kind": "plan", "block": number, "timestamp": h.timestamp, "market": meta.name, "user": v.user, "hf": v.hf, "plan": plan, "decision": decision });
                    writeln!(journal, "{row}")?;
                    // Live: send what the risk gate approved (up to exec.max_inflight at once).
                    if let risk::Decision::Send { priority_fee_wei, competitor_wei, .. } = &decision {
                        return Ok(Some(Approved { index: i, state: "block", plan, priority_fee_wei: *priority_fee_wei, competitor_wei: *competitor_wei }));
                    }
                }
                Err(e) => warn!(error = %e, "planning failed"),
            }
            Ok(None)
        };
        // Side events arriving while this batch is planned are handled at once (on the previous
        // block's snapshots, like those seen during the fetch above), not after the batch.
        let batch = {
            let (st, journal_rx) = (&mut atlas_st, &mut journal_rx);
            let (prev_snaps, last_ctx, parts) = (&prev_snaps, &last_ctx, &parts);
            let engine = &*engine;
            let on_side = async move |ev: SideEvent, journal: &mut std::fs::File| -> Result<()> {
                if let Some(st) = st.as_mut() {
                    let ctx = prev_snaps.as_deref().zip(last_ctx.as_ref()).map(|(s, (lh, eth, bt))| (s, lh, *eth, *bt));
                    on_side(engine, st, ev, ctx, parts);
                    drain_journal(journal_rx, journal)?;
                }
                Ok(())
            };
            plan_and_send(ready, exec.as_ref(), parts, &engine.provider, "block", &mut journal, on_plan, |i, _| &ctxs[i], side_rx.as_mut(), on_side).await
        };
        batch?;
        // The plans borrow this block's snapshots and the engine: release them before both move on.
        drop(ctxs);
        last_ctx = Some((h, eth_usd, block_time));
        drain_journal(&mut journal_rx, &mut journal)?;
        prev_snaps = Some(snaps);

        // 4b. Loss analysis, part 2: sender / fee lookups for this block's liquidations, in parallel.
        if !liq_rows.is_empty() {
            let costs = futures::future::join_all(liq_rows.iter().map(|r| morpho::events::tx_cost(&engine.provider, r.tx))).await;
            for (row, cost) in liq_rows.iter_mut().zip(costs) {
                match cost {
                    Ok((eoa, to, prio, fee)) => {
                        row.apply_tx(loss::TxInfo { eoa, to, priority_fee_wei: prio, fee_wei: fee }, eth_usd);
                        parts.observe_winner(row, liquidator, engine.cfg.atlas.address);
                    }
                    Err(e) => warn!(tx = %row.tx, error = %e, "receipt lookup failed"),
                }
                info!(
                    block = row.block, market = %row.market, borrower = %row.borrower, liquidator = %row.liquidator,
                    gross = row.gross_bonus, outcome = %row.outcome, delay = ?row.delay_blocks, "LIQUIDATION observed"
                );
                writeln!(journal, "{}", serde_json::to_string(&*row)?)?;
            }
        }

        // Live money (ledger.rs): the settled txs' PnL, the Atlas bond, and the daily loss stop.
        if let Some(receiver) = profit_receiver {
            let mut changed = !included_txs.is_empty();
            for (hash, cost) in included_txs {
                let income = match paid_to(&engine.provider, engine.cfg.chain_id, hash, receiver, &loan_decimals, eth_usd).await {
                    Ok(usd) => usd,
                    Err(e) => {
                        warn!(%hash, error = %e, "receipt lookup for the PnL failed; income counted as 0");
                        0.0
                    }
                };
                parts.ledger.lock().unwrap().record_block_tx(h.timestamp, income, cost);
            }
            heads_since_bond += 1;
            let bond_due = parts.bond_dirty.swap(false, std::sync::atomic::Ordering::Relaxed) || heads_since_bond >= BOND_CHECK_HEADS;
            if let (Some(sub), true) = (parts.submitter.as_ref(), bond_due) {
                heads_since_bond = 0;
                match atlas_submit::bonded(&engine.provider, sub.atlas, sub.signer.address()).await {
                    Ok(b) => {
                        parts.ledger.lock().unwrap().observe_bond(h.timestamp, b);
                        changed = true;
                        if math::units(b, 18) < engine.cfg.atlas.solver.min_bond_eth {
                            warn!(bonded_eth = math::units(b, 18), min = engine.cfg.atlas.solver.min_bond_eth, "HALTED: atlETH bond below atlas.solver.min_bond_eth; no solver op is submitted until it is topped up");
                        }
                    }
                    Err(e) => warn!(error = %e, "bond read failed"),
                }
            }
            let pnl = parts.ledger.lock().unwrap().pnl_usd(h.timestamp, eth_usd);
            let limit = engine.cfg.exec.daily_loss_limit_usd;
            if pnl < -limit && !gates::kill_switch_on(&engine.cfg.exec.kill_switch_file) {
                let why = format!("daily loss {pnl:.2} USD past exec.daily_loss_limit_usd {limit}");
                match crate::ledger::write_kill_switch(&engine.cfg.exec.kill_switch_file, &why) {
                    Ok(()) => warn!(reason = %why, file = %engine.cfg.exec.kill_switch_file, "kill switch written: every send is refused until a person removes it"),
                    Err(e) => warn!(error = %e, "could not write the kill switch"),
                }
                writeln!(journal, "{}", json!({ "kind": "kill_switch", "block": number, "reason": why, "pnl_usd": pnl }))?;
            }
            if changed {
                parts.ledger.lock().unwrap().save_or_warn();
            }
        }

        // Borrower list refresh is a slow full API walk (events already catch new borrowers, so
        // it is a safety net): it runs as a background task, and only merging its result (loading
        // the few new positions) happens here, between blocks.
        if refresh_job.as_ref().is_some_and(|j| j.is_finished()) {
            match refresh_job.take().unwrap().await {
                Ok(lists) => match engine.merge_borrowers(lists, number.into()).await {
                    Ok(n) => {
                        info!(new = n, "background borrower refresh merged");
                        // The interval counts from completion, so slow walks never run back to back.
                        last_refresh = Instant::now();
                    }
                    Err(e) => warn!(error = %e, "borrower merge failed"),
                },
                Err(e) => warn!(error = %e, "borrower refresh task failed"),
            }
        }
        if !replaying && refresh_job.is_none() && last_refresh.elapsed() > Duration::from_secs(engine.cfg.borrower_refresh_secs) {
            refresh_job = Some(tokio::spawn(engine.borrower_fetch_job()));
            last_refresh = Instant::now();
            info!("background borrower refresh started");
        }
        // Wall-clock age of the block when we finished deciding (needs a synced clock, e.g. chrony).
        // Block-number-derived so it stays meaningful under 200 ms blocks (see BlockClock::head_age_ms).
        let head_age_ms = clock.head_age_ms(number, h.timestamp, gates::unix_ms());
        info!(
            block = number, tracked = views.len(), touched, planned, fetch_ms, reload_ms, eval_us, decide_ms, head_age_ms,
            "block"
        );
        head_ms.push(h.seen_at.elapsed().as_millis() as u64);
        *head_wins.entry(h.source).or_default() += 1;
        if last_minute.elapsed() >= Duration::from_secs(60) {
            last_minute = Instant::now();
            let wins: Vec<String> = head_wins
                .iter()
                .map(|(i, n)| format!("{}={n}", if *i == heads::SOURCE_FLASHBLOCK { "flashblock" } else { ws_names.get(*i).map(String::as_str).unwrap_or("http-poll") }))
                .collect();
            let rpc: Vec<String> = pool
                .report()
                .iter()
                .map(|e| format!("{}: req {} failover {} (lag {}) hedge {} ewma {:.0}ms{}", e.name, e.requests, e.failovers, e.lagging, e.hedges, e.ewma_ms, if e.cooling { " COOLING" } else { "" }))
                .collect();
            head_ms.sort_unstable();
            let head_ms_p50 = head_ms.get(head_ms.len() / 2).copied();
            info!(?wins, ?rpc, ?head_ms_p50, heads = head_ms.len(), skipped_heads, block_time, "source stats");
            head_ms.clear();
            skipped_heads = 0;
            if let Some(st) = atlas_st.as_mut() {
                st.journal_dropped(number);
                drain_journal(&mut journal_rx, &mut journal)?;
            }
        }
        // Quota and egress accounting: cumulative attempts and request bytes per (endpoint, method),
        // journaled every ~10 min.
        if last_usage.elapsed() >= Duration::from_secs(600) {
            last_usage = Instant::now();
            let row = |pool_name: &str, (e, m, n, b): (String, String, u64, u64)| json!({"pool": pool_name, "endpoint": e, "method": m, "n": n, "bytes_out": b});
            let mut usage: Vec<serde_json::Value> = pool.method_bytes_report().into_iter().map(|r| row("main", r)).collect();
            if let Some(p) = atlas_st.as_ref().and_then(|st| st.pool.as_ref()) {
                usage.extend(p.method_bytes_report().into_iter().map(|r| row("atlas", r)));
            }
            info!(uptime_s = run_started.elapsed().as_secs(), entries = usage.len(), "rpc method usage journaled");
            writeln!(journal, "{}", json!({ "kind": "rpc_usage", "block": number, "uptime_s": run_started.elapsed().as_secs(), "usage": usage }))?;
        }
    }
}


fn price_human(price: U256, meta: &MarketMeta) -> f64 {
    // price is loan-per-collateral scaled by 1e36 and by the decimals difference.
    math::u256_f64(price) / 1e36 * 10f64.powi(meta.collateral_decimals as i32 - meta.loan_decimals as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An SVR primary past its cutoff is in effect from the head that applies it: the previous
    /// block (still the old price on-chain) is not a missed update, and the next block agrees.
    /// Found in a shadow run (2026-11-02): two false "missed" warnings and a block valued at the old
    /// price after the resync.
    #[tokio::test]
    async fn an_expired_svr_primary_is_not_a_missed_update() {
        use alloy::{primitives::address, transports::mock::Asserter};
        let eth = address!("05c84a58FE042275b37db038bAAcD15F410c7bB0");
        let e24 = U256::from(10u64).pow(U256::from(24u64));
        let model = oracle::PriceModel::for_test(vec![(Address::repeat_byte(1), eth, U256::from(228_360_000_000u64), true)], vec![Some((e24, vec![0], vec![]))]);
        let (journal_tx, _journal_rx) = tokio::sync::mpsc::unbounded_channel();
        // No RPC answers queued: a resync (the false alarm) would fail and be counted.
        let provider = ProviderBuilder::new().connect_mocked_client(Asserter::new());
        let mut st = AtlasState {
            provider,
            pool: None,
            model,
            logs: oracle_logs::OracleLogs::detached(),
            block_ts: HashMap::new(),
            held: vec![],
            max_update_block: 0,
            missed_streak: 0,
            resync_all: false,
            svr_cutoff_secs: 10,
            journal_tx,
            valuations: Arc::new(tokio::sync::Semaphore::new(1)),
            auction_cache: Arc::default(),
            dropped: Arc::default(),
            dropped_journaled: 0,
        };
        let old = st.model.price(0).unwrap();
        let new_answer = U256::from(228_747_000_000u64);
        let new = st.model.preview(eth, new_answer)[0].1;
        assert_ne!(old, new);
        // The primary lands in block 95 (ts 1000): deferred for the SVR auction.
        assert!(matches!(st.model.apply(eth, new_answer, oracle::Path::Primary, 1000), oracle::Applied::Deferred(_)));
        st.max_update_block = 95;
        let snap = |price| Some(morpho::onchain::MarketSnapshot { price, totals: Default::default(), raw: Default::default() });
        let head = |number, timestamp| Head { number, timestamp, base_fee: 0, seen_at: Instant::now(), source: 0 };
        // Head 100 (ts 1010): not past the cutoff yet; block 99 has the old price.
        st.on_head(&head(100, 1010), Some((99, &[snap(old)]))).await;
        assert_eq!((st.model.price(0), st.missed_streak), (Some(old), 0));
        // Head 101 (ts 1012): past the cutoff, applied; block 100 still has the old price.
        st.on_head(&head(101, 1012), Some((100, &[snap(old)]))).await;
        assert_eq!((st.model.price(0), st.missed_streak), (Some(new), 0), "block 100 is not a missed update");
        // Head 102: block 101 has the new price, as the model.
        st.on_head(&head(102, 1014), Some((101, &[snap(new)]))).await;
        assert_eq!((st.model.price(0), st.missed_streak), (Some(new), 0));
    }

    #[test]
    fn loopback_urls_are_recognized() {
        use super::is_loopback_url;
        assert!(is_loopback_url("http://127.0.0.1:8545"));
        assert!(is_loopback_url("ws://localhost:8545"));
        assert!(is_loopback_url("http://[::1]:8545/"));
        assert!(!is_loopback_url("https://mainnet-sequencer.base.org"));
        assert!(!is_loopback_url("https://127.0.0.1.evil.example/x"));
    }

    #[test]
    fn the_tx_cost_cap_cuts_the_bid_or_refuses_a_losing_send() {
        const GWEI: u128 = 1_000_000_000;
        let cap = 2_000_000_000_000_000u128; // 0.002 ETH
        let base = GWEI / 100; // 0.01 gwei
        let floor = GWEI / 1000;
        // A 3.8M-gas plan at 5 gwei would cost ~0.019 ETH: the most that fits is ~0.5 gwei, below a
        // 3 gwei competitor, so the tx would be sent to lose: refused.
        assert_eq!(fit_tx_cost(3_800_000, base, 5 * GWEI, 0, cap, floor, Some(3 * GWEI)), None);
        // Without a competitor model the bid is cut to what fits (worst case exactly within the cap).
        let cut = fit_tx_cost(3_800_000, base, 5 * GWEI, 0, cap, floor, None).unwrap();
        assert!(cut < 5 * GWEI && 3_800_000 * (2 * base + cut) <= cap, "{cut}");
        // The L1 fee counts too.
        let with_l1 = fit_tx_cost(3_800_000, base, 5 * GWEI, cap / 2, cap, floor, None).unwrap();
        assert!(with_l1 < cut);
        // A small plan fits as is.
        assert_eq!(fit_tx_cost(300_000, base, GWEI, 0, cap, floor, Some(GWEI / 2)), Some(GWEI));
        // The base fee alone past the cap: nothing fits.
        assert_eq!(fit_tx_cost(3_800_000, cap, GWEI, 0, cap, floor, None), None);
    }

    #[test]
    fn block_clock_follows_a_block_time_change_within_its_window() {
        let mut c = BlockClock::default();
        assert_eq!(c.block_time(), 2.0, "nothing measured yet");
        // 2 s blocks (Base before Denim)...
        for n in 0..50u64 {
            c.push(1000 + n, 10_000 + 2 * n);
        }
        assert_eq!(c.block_time(), 2.0);
        // ...then 5 blocks a second: within one window the estimate is the new block time. Timestamps
        // are whole seconds, so 50 heads span 10 s.
        let (n0, t0) = (1050u64, 10_100u64);
        for k in 0..50u64 {
            c.push(n0 + k, t0 + k / 5);
        }
        assert!((c.block_time() - 0.2).abs() < 0.05, "{}", c.block_time());
        // An old or repeated head changes nothing.
        c.push(1000, 1);
        assert!((c.block_time() - 0.2).abs() < 0.05);
    }

    #[test]
    fn snapshot_throttle_reuses_only_on_a_quiet_fresh_head() {
        // Quiet head with a fresh snapshot: reuse (no refetch).
        assert!(!must_refetch_snapshots(true, false, false, true));
        // Any one of: no prev, an oracle update, a touched market, or a stale snapshot forces a refetch.
        assert!(must_refetch_snapshots(false, false, false, true), "no snapshot yet");
        assert!(must_refetch_snapshots(true, true, false, true), "oracle updated");
        assert!(must_refetch_snapshots(true, false, true, true), "a market was touched");
        assert!(must_refetch_snapshots(true, false, false, false), "snapshot too old");
    }

    #[test]
    fn head_age_places_blocks_within_their_second() {
        // Five 200 ms blocks share each timestamp; span a whole number of seconds so the measured
        // block_time is exactly 0.2 (blocks 2000..=2025 over seconds 10_000..=10_005).
        let mut c = BlockClock::default();
        for k in 0..=25u64 {
            c.push(2000 + k, 10_000 + k / 5);
        }
        assert!((c.block_time() - 0.2).abs() < 1e-9, "{}", c.block_time());
        // First block of the second 10_004 is #2020 (k=20). now = 10_004.300 s.
        let now_ms = 10_004_300u64;
        // #2020 is on the boundary: ~300 ms late. #2021 is +200 ms theoretical: ~100 ms late.
        assert!((c.head_age_ms(2020, 10_004, now_ms) - 300).abs() <= 15, "{}", c.head_age_ms(2020, 10_004, now_ms));
        assert!((c.head_age_ms(2021, 10_004, now_ms) - 100).abs() <= 15, "{}", c.head_age_ms(2021, 10_004, now_ms));

        // Pre-Denim 2 s blocks, one per timestamp: reduces to now - timestamp*1000.
        let mut c2 = BlockClock::default();
        for n in 0..50u64 {
            c2.push(1000 + n, 10_000 + 2 * n);
        }
        assert_eq!(c2.head_age_ms(1040, 10_080, 10_080_590), 590);
    }
}
