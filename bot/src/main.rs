//! Liquidation bot — entry point (scan / watch / dashboard / live / loss / value-auction). The
//! framework is the `liquidation_framework` library (see lib.rs); this binary runs it for Morpho
//! Blue on Base.

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
use clap::{Parser, Subcommand};
use serde_json::json;
use tokio::sync::watch;
use tracing::{info, warn};

use alloy::sol;

use liquidation_framework::{
    actor::{self, ExecHandle},
    bid::WinningBids,
    atlas, atlas_submit, atlas_venue, config, dash, dex, executor, flash_venue, flashblocks, funding, gates, heads, live, loss, morpho, oracle, planner, protocol, risk, rpc, sim, v3router, venue,
    config::Config,
    engine::JournalRow,
    heads::Head,
    morpho::{Morpho, MorphoEngine, PositionView, evaluate_position, math, onchain::MarketMeta},
    planner::{PlanCtx, Planner, PriceProbe},
    router::Kyber,
    sim::{LiquidatorCode, Simulator},
};

#[derive(Parser)]
#[command(about = "Liquidation bot (Morpho Blue on Base): shadow mode by default; `watch --live` sends")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-shot: load all tracked positions, print risk summary and validate HF against the Morpho API.
    Scan {
        /// Rows to show per market.
        #[arg(long, default_value_t = 5)]
        top: usize,
    },
    /// Shadow mode: track every new block, plan + simulate candidates, journal everything. Sends nothing.
    Watch {
        /// Replay from this past block (needs an RPC with historical state). Planning is skipped
        /// while replaying because exit quotes are only available at current prices.
        #[arg(long)]
        from_block: Option<u64>,
        /// Actually send transactions. Also requires exec.mode = "live", a deployed liquidator and
        /// a keystore; a pre-flight check refuses to start if anything is off.
        #[arg(long)]
        live: bool,
    },
    /// Plan and simulate one position. --price-drop-bps mocks the market oracle lower ("what if").
    Plan {
        #[arg(long)]
        market: String,
        #[arg(long)]
        user: Address,
        #[arg(long, default_value_t = 0)]
        price_drop_bps: u64,
        /// Plan as of this past block (needs an archive RPC): the sim runs at the next block, as
        /// in watch mode. Quotes come from the direct router only (aggregators quote "now").
        #[arg(long)]
        at_block: Option<u64>,
        /// Quote and simulate on the "pending" flashblock state. Diagnostic only: the live paths
        /// pin the snapshot block, since "pending" calls often run on a stale block.
        #[arg(long)]
        pending: bool,
    },
    /// Replay the Atlas auction valuation (bundle quote + per-position plans, as in watch) of one
    /// market at a past block, at its oracle price minus --price-drop-bps. Needs an archive RPC.
    ValueAuction {
        #[arg(long)]
        market: String,
        #[arg(long)]
        price_drop_bps: u64,
        #[arg(long)]
        at_block: u64,
        /// Borrowers to value (comma-separated). Default: the Morpho API's current borrowers,
        /// which no longer lists the ones liquidated since.
        #[arg(long, value_delimiter = ',')]
        users: Vec<Address>,
        /// Also quote Kyber and plan in "best" mode. Kyber quotes the current market, so this only
        /// makes sense with --at-block at (or right behind) the chain head.
        #[arg(long)]
        kyber: bool,
    },
    /// Loss analysis over the journal: who liquidated what, and how early we saw it.
    Report,
    /// One dashboard pass: ingest the journal + run log into SQLite, look up Atlas winners and the
    /// wallet balance on dashboard.rpc, render dashboard.html (scripts/dashboard_loop.sh).
    Dashboard,
    /// Serves the dashboard on 127.0.0.1:[dashboard] live_port, with a live section of the
    /// positions closest to liquidation (the snapshot `watch` writes every block). Read-only, no RPC.
    Live,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    if let Cmd::Report = cli.cmd {
        return report(&cfg.journal_path);
    }
    // Live: get the key BEFORE the async runtime exists. The password may come from an env var that
    // is removed after reading, and mutating the environment is only sound while single-threaded.
    let live_signer = match &cli.cmd {
        Cmd::Watch { live: true, .. } => {
            let ks = cfg.exec.keystore.clone().context("live mode needs exec.keystore")?;
            Some(executor::load_signer(&ks, &cfg.exec.keystore_password_env)?)
        }
        _ => None,
    };
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(cli, cfg, live_signer))
}

async fn run(cli: Cli, cfg: Config, live_signer: Option<alloy::signers::local::PrivateKeySigner>) -> Result<()> {
    if let Cmd::Dashboard = cli.cmd {
        return dash::run(&cfg).await;
    }
    if let Cmd::Live = cli.cmd {
        return live::serve(&cfg.dashboard).await;
    }
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
    // Same-block mode: a separate pool of the endpoints that serve the "pending" flashblock state.
    let fb_specs: Vec<rpc::EndpointSpec> = if cfg.flashblocks.enabled {
        cfg.flashblocks
            .pending_rpc
            .iter()
            .filter_map(|n| resolved.http.iter().find(|e| &e.name == n))
            .enumerate()
            // Strict preference order (not load-spread): a quote and its simulation must read the
            // same node's pending state, which differs between nodes by up to a flashblock.
            .map(|(i, e)| rpc::EndpointSpec { name: e.name.clone(), url: e.url.clone(), priority: i as u32, skip_methods: e.skip_methods.clone(), max_log_blocks: e.max_log_blocks })
            .collect()
    } else {
        vec![]
    };
    let pool = rpc::RpcPool::new(resolved.http)?;
    let provider = ProviderBuilder::new().connect_client(alloy::rpc::client::RpcClient::new(pool.clone(), false));
    let fb_provider = if cfg.flashblocks.enabled {
        anyhow::ensure!(!fb_specs.is_empty(), "flashblocks.pending_rpc names no configured [[rpc]] endpoint");
        info!(pending_rpc = ?fb_specs.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), "flashblocks mode enabled");
        let fb_pool = rpc::RpcPool::with_timeout(fb_specs, Duration::from_millis(1500))?;
        Some((ProviderBuilder::new().connect_client(alloy::rpc::client::RpcClient::new(fb_pool.clone(), false)), fb_pool))
    } else {
        None
    };
    let morpho = Morpho::from_config(&cfg);
    let mut engine = MorphoEngine::new(provider, cfg, morpho).await?;
    // Profits, gas trade-offs and bids are in USD: a market whose loan token has no price model
    // would be silently mispriced, so it is refused here.
    let unpriced: Vec<String> = engine.metas.iter().filter(|m| morpho::loan_usd_model(engine.cfg.chain_id, m.params.loanToken, 1.0).is_none()).map(|m| format!("{} (loan {})", m.name, m.loan_symbol)).collect();
    anyhow::ensure!(unpriced.is_empty(), "markets with a loan token the bot cannot price in USD (Base USD stablecoins and WETH, by address): {}", unpriced.join(", "));
    match cli.cmd {
        Cmd::Scan { top } => scan(&mut engine, top).await,
        Cmd::Watch { from_block, .. } => watch(&mut engine, from_block, pool, resolved.ws, live_signer.map(|s| (s, broadcast)), fb_provider).await,
        Cmd::Plan { market, user, price_drop_bps, at_block, pending } => plan_one(&mut engine, &market, user, price_drop_bps, at_block, pending).await,
        Cmd::ValueAuction { market, price_drop_bps, at_block, users, kyber } => value_auction_at(&mut engine, &market, price_drop_bps, at_block, &users, kyber).await,
        Cmd::Report => unreachable!("handled before connecting"),
        Cmd::Dashboard => unreachable!("handled before connecting"),
        Cmd::Live => unreachable!("handled before connecting"),
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

fn report(journal_path: &str) -> Result<()> {
    let text = std::fs::read_to_string(journal_path).with_context(|| format!("reading {journal_path}"))?;
    let rows: Vec<loss::LiquidationRow> = text
        .lines()
        .filter(|l| l.contains(r#""kind":"liquidation""#))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    loss::report(&rows);
    Ok(())
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

/// Block time assumed until enough blocks were seen to measure it (one-off CLI paths use it too).
const DEFAULT_BLOCK_TIME: f64 = 2.0;

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
    let tx = alloy::rpc::types::TransactionRequest::default().to(engine.cfg.flashblocks.atlas).input(IAtlasRates::getAtlasSurchargeRateCall {}.abi_encode().into());
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
    let atlas = engine.cfg.flashblocks.atlas;
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
    engine.reload(&[(mi, user)], h.number.into()).await?;
    let Some(&(shares, collateral)) = engine.positions.get(&(mi, user)) else { bail!("{user} has no debt in {market}") };
    let snaps = engine.snapshots(h.number.into(), h.timestamp + engine.cfg.lookahead_secs(DEFAULT_BLOCK_TIME)).await?;
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
    let deadline_block = h.number + gates::deadline_blocks(engine.cfg.exec.deadline_secs, DEFAULT_BLOCK_TIME);
    let sim_time = h.timestamp + engine.cfg.lookahead_secs(DEFAULT_BLOCK_TIME);
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
async fn wait_head<P: Provider>(
    ws: &mut Option<watch::Receiver<Option<Head>>>,
    sealed: Option<&mut watch::Receiver<u64>>,
    p: &P,
    last: u64,
    poll_ms: u64,
) -> Result<Head> {
    let fresh = |rx: &watch::Receiver<Option<Head>>| rx.borrow().filter(|h| h.number > last);
    let fallback = Duration::from_millis(if ws.is_some() || sealed.is_some() { 2_500 } else { poll_ms });
    let mut sealed = sealed;
    loop {
        if let Some(h) = ws.as_ref().and_then(fresh) {
            return Ok(h);
        }
        if sealed.as_ref().is_some_and(|s| *s.borrow() > last) {
            let h = heads::http_head(p, BlockNumberOrTag::Latest).await?;
            if h.number > last {
                return Ok(Head { source: heads::SOURCE_FLASHBLOCK, ..h });
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
            continue;
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
        let sealed_changed = async {
            match sealed.as_mut() {
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
            _ = sealed_changed => {}
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
    let (p, atlas, control) = (&engine.provider, engine.cfg.flashblocks.atlas, engine.cfg.atlas.control);
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
/// standing Morpho allowances are all consistent.
async fn preflight_live<P: Provider>(engine: &MorphoEngine<P>, signer_addr: Address) -> Result<()> {
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
    info!(%liq, executor = %signer_addr, %receiver, balance_eth = math::units(bal, 18), "LIVE pre-flight passed");
    Ok(())
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
    /// Gas burned by reverted txs today, plus the worst case of the txs in flight; shared by the
    /// block path and the flashblock events it handles while planning.
    budget: std::sync::Mutex<risk::DailyBudget>,
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
    let mut n = 0usize;
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        if !line.contains("\"kind\":\"liquidation\"") {
            continue;
        }
        if let Ok(row) = serde_json::from_str::<loss::LiquidationRow>(&line) {
            parts.observe_winner(&row, ours, atlas);
            n += 1;
        }
    }
    info!(liquidations = n, "competition model seeded from the journal");
}

/// (aggregator, median) -> (first seen, its valuation, computed once): see `FbState::auction_cache`.
type AuctionCache = HashMap<(Address, U256), (Instant, Arc<tokio::sync::OnceCell<serde_json::Value>>)>;

/// The background borrower refresh's result: (market, its borrowers or the error) per market.
type BorrowerRefresh = Vec<(usize, Result<Vec<protocol::BorrowerInfo>>)>;

/// Same-block (Flashblocks) mode state.
struct FbState<P> {
    /// Pool of endpoints for same-block quotes and sims (pending state + the snapshot's block, see
    /// on_oracle_update).
    provider: P,
    pool: rpc::RpcPool,
    stream: flashblocks::Stream,
    model: oracle::PriceModel,
    /// market -> (block, flashblock index, when seen) of its latest oracle update.
    last_oracle: HashMap<usize, (u64, u64, Instant)>,
    /// (market, user) -> (block, ms from oracle update seen to plan ready, simulated net).
    planned: HashMap<(usize, Address), (u64, u64, Option<f64>)>,
    /// Highest block an applied update belongs to (the model runs ahead of sealed blocks).
    max_update_block: u64,
    updates: u64,
    started: Instant,
    /// Journal rows finished by background tasks (the receiver lives in `watch`, which writes
    /// them via drain_journal so lines never interleave with its own, even after fb is gone).
    journal_tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// Permits for auction valuations running at once (atlas.max_concurrent).
    valuations: Arc<tokio::sync::Semaphore>,
    /// One valuation per report: Chainlink opens two auctions for each update (same aggregator and
    /// median, different ids and userOpHashes; only one ends on-chain). The sibling waited behind the
    /// first for the only permit and was valued again, 0.3-10 s late (10/24). (aggregator, median) ->
    /// (first seen, its valuation), kept AUCTION_SHARE_SECS.
    auction_cache: Arc<std::sync::Mutex<AuctionCache>>,
}

/// Writes the journal rows background tasks have finished.
fn drain_journal(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>, journal: &mut std::fs::File) -> Result<()> {
    while let Ok(line) = rx.try_recv() {
        writeln!(journal, "{line}")?;
    }
    Ok(())
}

/// One event from the flashblock stream: an oracle update, or a block opening, at which SVR
/// primaries that no auction carried become what Morpho reads once older than the cutoff.
#[allow(clippy::too_many_arguments)]
async fn on_fb_event<P: Provider + Clone + 'static>(
    engine: &MorphoEngine<P>,
    fb: &mut FbState<P>,
    ev: flashblocks::FbEvent,
    snaps: &[Option<morpho::onchain::MarketSnapshot>],
    head: &Head,
    eth_usd: f64,
    block_time: f64,
    parts: &Arc<PlannerParts>,
    exec: Option<&ExecHandle>,
    journal: &mut std::fs::File,
) -> Result<()> {
    match ev {
        flashblocks::FbEvent::Oracle(u) => on_oracle_update(engine, fb, u, snaps, head, eth_usd, block_time, parts, exec, journal).await?,
        flashblocks::FbEvent::Opened { block, timestamp, seen } => {
            for (aggregator, answer) in fb.model.due(timestamp, engine.cfg.flashblocks.svr_cutoff_secs) {
                let u = flashblocks::OracleUpdate { aggregator, answer, path: flashblocks::Path::Expired, block, block_ts: timestamp, index: 0, tx: alloy::primitives::B256::ZERO, seen };
                on_oracle_update(engine, fb, u, snaps, head, eth_usd, block_time, parts, exec, journal).await?;
            }
        }
        flashblocks::FbEvent::Auction(a) => on_auction(engine, fb, a, snaps, head, eth_usd, parts),
    }
    Ok(())
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
fn on_auction<P: Provider + Clone + 'static>(
    engine: &MorphoEngine<P>,
    fb: &FbState<P>,
    a: atlas::Auction,
    snaps: &[Option<morpho::onchain::MarketSnapshot>],
    head: &Head,
    eth_usd: f64,
    parts: &Arc<PlannerParts>,
) {
    let markets: Vec<AuctionMarket> = fb
        .model
        .preview(a.aggregator, a.median)
        .into_iter()
        .filter_map(|(mi, price)| snaps.get(mi).and_then(|s| s.as_ref()).map(|base| auction_market(engine, mi, base, price)))
        .collect();
    let sim_time = head.timestamp + engine.cfg.lookahead_secs(DEFAULT_BLOCK_TIME);
    let atlas = engine.cfg.atlas.clone();
    let (provider, parts, journal, permits, head) = (fb.provider.clone(), parts.clone(), fb.journal_tx.clone(), fb.valuations.clone(), *head);
    // The sibling auction of the same report shares the first one's valuation.
    let shared = {
        let mut cache = fb.auction_cache.lock().unwrap();
        cache.retain(|_, (t, _)| t.elapsed() < Duration::from_secs(AUCTION_SHARE_SECS));
        cache.entry((a.aggregator, a.median)).or_insert_with(|| (Instant::now(), Arc::new(tokio::sync::OnceCell::new()))).1.clone()
    };
    // Valuations queue behind each other (atlas.max_concurrent, taken inside value_auction by the
    // one that values) instead of piling onto the RPC pool the same-block path needs; the wait is
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

async fn value_auction_at<P: Provider + Clone + 'static>(engine: &mut MorphoEngine<P>, market: &str, drop_bps: u64, at_block: u64, users: &[Address], use_kyber: bool) -> Result<()> {
    let mi = engine.market_by_name(market).with_context(|| format!("market {market} not in config"))?;
    anyhow::ensure!(drop_bps < 10_000, "--price-drop-bps must be < 10000");
    let h = heads::http_head(&engine.provider, BlockNumberOrTag::Number(at_block)).await?;
    if users.is_empty() {
        engine.refresh_borrowers(h.number.into()).await?;
    } else {
        engine.reload(&users.iter().map(|u| (mi, *u)).collect::<Vec<_>>(), h.number.into()).await?;
    }
    let sim_time = h.timestamp + engine.cfg.lookahead_secs(DEFAULT_BLOCK_TIME);
    let snaps = engine.snapshots(h.number.into(), sim_time).await?;
    let eth_usd = engine.eth_usd(&snaps);
    let base = snaps[mi].clone().context("snapshot failed")?;
    let price = base.price * U256::from(10_000 - drop_bps) / U256::from(10_000u64);
    let m = auction_market(engine, mi, &base, price);
    let (kyber, simulator) = build_planner_parts(engine).await?;
    let parts = Arc::new(PlannerParts { protocol: engine.protocol.clone(), kyber, venue: simulator, solver: build_atlas_solver(engine).await?, atlas_surcharge: atlas_surcharge(engine).await, v3: build_v3(engine).await?, exec: engine.cfg.exec.clone(), bids: std::sync::Mutex::new(WinningBids::new(engine.cfg.exec.bid_window_secs)), submitter: None, budget: Default::default() });
    let a = atlas::Auction {
        id: format!("replay-{market}-{at_block}"),
        aggregator: Address::ZERO,
        median: U256::ZERO,
        deadline: h.number + 100,
        max_fee_per_gas: 0,
        user_op_hash: alloy::primitives::B256::ZERO,
        seen: Instant::now(),
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
    // The journaled bid: bundle net x (1 - atlas.margin); the dashboard's paper table tries the
    // atlas.paper_margins ladder around it.
    let bid_usd = bundle_net * (1.0 - atlas.margin);
    // From this auction's arrival to the end of the valuation proper (before any solver-op
    // simulation, see valuation()).
    let since_valued = v["valued_unix_ms"].as_u64().map_or(0, |t| gates::unix_ms().saturating_sub(t));
    let value_ms = (a.seen.elapsed().as_millis() as u64).saturating_sub(since_valued);
    info!(auction = %a.id, aggregator = %a.aggregator, deadline = a.deadline, liquidatable = %v["liquidatable"], bonus_usd = %v["bonus_usd"], bundle_net_usd = bundle_net, net_usd = %v["net_usd"], bid_usd, bundle_ms = %v["bundle_ms"], value_ms, shared = %v["shared"], "ATLAS auction valued (observe only)");
    let mut row = json!({ "kind": "atlas_auction", "auction_id": a.id, "user_op_hash": a.user_op_hash, "aggregator": a.aggregator, "median": a.median.to_string(),
                          "deadline": a.deadline, "head": head.number, "max_fee_gwei": a.max_fee_per_gas as f64 / 1e9, "bid_usd": bid_usd, "bid_eth": bid_usd / eth_usd.max(1.0),
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
    top: Option<&Result<planner::Plan>>,
    overrides: &StateOverride,
    block: u64,
    sim_time: u64,
    atlas_gas_usd: f64,
    eth_usd: f64,
    margin: f64,
) -> Option<(serde_json::Value, Option<OpCandidate>)> {
    let (Some(solver), Venue::Callback(_), Some(Ok(plan))) = (&parts.solver, &parts.venue, top) else { return None };
    let payload = plan.payload.as_ref().filter(|_| plan.router == "direct")?;
    // Net of the op's gas as Atlas charges it (see valuation()).
    let net = plan.profit_usd.unwrap_or(0.0) - plan.gas_used.unwrap_or(0) as f64 * atlas_gas_usd;
    let bid_usd = net.max(0.0) * (1.0 - margin);
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
            (json!({ "bid_usd": bid_usd, "ok": true, "net_after_bid": net_after_bid, "gas_used": s.gas_used }), cand)
        }
        Err(e) => (json!({ "bid_usd": bid_usd, "ok": false, "error": e.to_string().chars().take(200).collect::<String>() }), None),
    })
}

/// Signs and submits the simulated solver op `c` for auction `a` (see solver_op): minProfit after
/// the bid = the simulated net x min_profit_keep, gas = simulated x gas_limit_buffer (within the
/// DappControl's limit), the auction's own max fee and deadline. Refused when the kill switch is
/// on, when this EOA already bid for that block, or when the op's worst-case gas charge to the
/// bond does not fit the daily budget (`Submitter::budget`). Returns whether the endpoint accepted
/// it, and the journal row saying what happened. The block's one-op slot is held only by an
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
    let now = gates::unix_secs();
    if !sub.budget.lock().unwrap().allows(now, worst_usd, e.daily_gas_budget_usd) {
        return refused(format!("daily gas budget: the op's worst-case bond charge {worst_usd:.4} USD does not fit {}", e.daily_gas_budget_usd));
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
            // Charged once accepted: whether Atlas ran it (and charged the bond) is not observed
            // here, so every accepted op counts at its worst case.
            sub.budget.lock().unwrap().charge(now, worst_usd);
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
    let valued = futures::future::join_all(markets.iter().map(|m| async {
        let mut overrides = StateOverride::default();
        overrides.insert(m.meta.params.oracle, AccountOverride { code: Some(sim::constant_price_oracle(m.snap.price)), ..Default::default() });
        // One solver op holds only so many liquidations: bundle the largest that fit (liq is sorted
        // by bonus) instead of pricing an op that cannot exist.
        let fit = &m.liq[..m.liq.len().min(atlas_venue::bundle_capacity(atlas.solver_gas_limit))];
        let bundle = match fit.is_empty() {
            true => None,
            false => Some(planner.quote_bundle(provider, &m.meta, &m.snap, fit, head.number, &overrides, head.base_fee, eth_usd, atlas_venue::bundle_swap_budget(atlas.solver_gas_limit, fit.len()), atlas_venue::ATLAS_BUNDLE_GAS, live).await),
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
    let ops = futures::future::join_all(markets.iter().zip(&valued).enumerate().map(|(mi, (m, (_, _, ov, plans)))| solver_op(provider, parts, mi, m, plans.first().map(|(_, p)| p), ov, head.number, sim_time, atlas_gas_usd, eth_usd, atlas.margin))).await;
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
        // The bid basis: the bundle's exit gain minus its whole gas as Atlas charges it (not the
        // planner's own gas price), so a bid never spends what the solver owes Atlas.
        let atlas_net = bundle.as_ref().and_then(|b| b.net_usd.map(|_| b.gross_usd - b.gas_units as f64 * atlas_gas_usd));
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

/// A Chainlink update seen in a flashblock: new price -> re-evaluate that market in memory ->
/// plan the liquidatable positions on the snapshot block with the new price pinned (direct router:
/// aggregators quote too slowly) -> journal, and send when `flashblocks.send` and live.
#[allow(clippy::too_many_arguments)]
async fn on_oracle_update<P: Provider + Clone>(
    engine: &MorphoEngine<P>,
    fb: &mut FbState<P>,
    u: flashblocks::OracleUpdate,
    snaps: &[Option<morpho::onchain::MarketSnapshot>],
    head: &Head,
    eth_usd: f64,
    block_time: f64,
    parts: &PlannerParts,
    exec: Option<&ExecHandle>,
    journal: &mut std::fs::File,
) -> Result<()> {
    let queue_ms = u.seen.elapsed().as_millis() as u64;
    fb.updates += 1;
    fb.max_update_block = fb.max_update_block.max(u.block);
    let moved = match fb.model.apply(u.aggregator, u.answer, u.path, u.block_ts) {
        oracle::Applied::Moved(m) => m,
        // SVR public transmit: not what Morpho reads yet. Journal what it will do once the Atlas
        // metacall (typically next block) puts the same answer in effect; plan nothing now.
        oracle::Applied::Deferred(preview) => {
            for (mi, price) in preview {
                let Some(base) = snaps.get(mi).and_then(|s| s.as_ref()) else { continue };
                let mut snap = base.clone();
                snap.price = price;
                let n = engine.evaluate_market(mi, &snap).iter().take_while(|v| v.health.is_liquidatable()).count();
                let meta = &engine.metas[mi];
                info!(market = %meta.name, block = u.block, flashblock = u.index, liquidatable_once_in_effect = n, "FLASHBLOCK SVR primary seen; waiting for its Atlas metacall");
                writeln!(journal, "{}", json!({ "kind": "fb_oracle", "path": "primary_svr_pending", "block": u.block, "flashblock": u.index, "market": meta.name, "tx": u.tx, "price_new": price.to_string(), "liquidatable_once_in_effect": n, "queue_ms": queue_ms }))?;
            }
            return Ok(());
        }
        oracle::Applied::Rejected(why) => {
            warn!(aggregator = %u.aggregator, tx = %u.tx, why, "flashblock oracle update rejected");
            return Ok(());
        }
    };
    if moved.is_empty() {
        return Ok(());
    }
    // An SVR secondary is in effect only if its metacall succeeds, and the auction winner's own
    // liquidations come with it. We do not wait for the metacall's receipt to learn either: the
    // RPCs serve it only once the block is sealed (2026-10-20, 12 metacalls: never within 800 ms,
    // 1.0-1.5 s for half, 3-4+ s for the rest; ordinary txs ~70 ms), long after the leftovers are
    // gone, and waiting stalled the main loop. Instead the fresh "pending" state answers both: it
    // already holds the metacall's effects, so it is simulated WITHOUT the price override (a failed
    // metacall leaves the old price; a borrower the winner liquidated is healthy). The sim also
    // reads the oracle in that same state (PriceProbe): until the node has applied the metacall
    // (it trails the stream by ~50-100 ms) the plan counts as stale and is retried briefly. Only a
    // fresh, probed pending plan may send (see below).
    let planner = parts.planner(&engine.cfg.flashblocks.router);
    let deadline_block = u.block + gates::deadline_blocks(engine.cfg.exec.deadline_secs, block_time);
    for (mi, price) in moved {
        fb.last_oracle.insert(mi, (u.block, u.index, u.seen));
        let meta = &engine.metas[mi];
        let Some(base) = snaps.get(mi).and_then(|s| s.as_ref()) else { continue };
        let mut snap = base.clone();
        let old = snap.price;
        snap.price = price;
        let t_eval = Instant::now();
        let views = engine.evaluate_market(mi, &snap);
        let eval_us = t_eval.elapsed().as_micros() as u64;
        let mut cands: Vec<&PositionView> = views
            .iter()
            .take_while(|v| v.health.is_liquidatable())
            .filter(|v| v.max_seize > U256::ZERO && v.bonus >= engine.cfg.min_bonus)
            .filter(|v| fb.planned.get(&(mi, v.user)).is_none_or(|p| p.0 != u.block))
            .collect();
        cands.sort_by(|a, b| b.bonus.total_cmp(&a.bonus));
        let change_bps = (math::u256_f64(price) / math::u256_f64(old).max(1.0) - 1.0) * 1e4;
        info!(market = %meta.name, block = u.block, flashblock = u.index, path = ?u.path, change_bps, candidates = cands.len(), queue_ms, eval_us, "FLASHBLOCK oracle update");
        writeln!(
            journal,
            "{}",
            json!({ "kind": "fb_oracle", "path": u.path, "block": u.block, "flashblock": u.index, "market": meta.name, "tx": u.tx, "price_old": old.to_string(), "price_new": price.to_string(), "change_bps": change_bps, "candidates": cands.len(), "queue_ms": queue_ms, "eval_us": eval_us })
        )?;
        // Each candidate is planned twice, in parallel:
        //  - on "pending", the block being built: it includes what is already in it (the
        //    metacall's own swaps on our pools, earlier liquidations), but nodes often serve it
        //    1-5 blocks stale, and on a stale base Morpho accrues less interest than our model.
        //    No price override: the state must hold the update itself, and the oracle is read in
        //    that same state (PriceProbe) to prove it does (a transmit that reverted, or an SVR
        //    cutoff one block away, shows up as the old price instead of being papered over);
        //  - on the block our snapshot came from (`head`), as the next block at the accrual time,
        //    with the new price pinned by a state override: exactly the state the candidate was
        //    computed from, minus the in-block changes.
        // The pending plan wins when its sim provably ran on the in-progress block with the update
        // in effect, whatever it says (a revert there is real in-block information); else the
        // snapshot plan, which may only send for a public transmit (may_send).
        let mut overrides = StateOverride::default();
        overrides.insert(meta.params.oracle, AccountOverride { code: Some(sim::constant_price_oracle(price)), ..Default::default() });
        let sim_time = head.timestamp + engine.cfg.lookahead_secs(block_time);
        let ctxs: Vec<(PlanCtx<Morpho>, PlanCtx<Morpho>)> = cands
            .into_iter()
            .take(engine.cfg.flashblocks.max_plans)
            .map(|v| {
                let at_head = PlanCtx { meta, snap: &snap, view: v, block: head.number, base_fee_wei: head.base_fee, eth_usd, overrides: overrides.clone(), deadline_block, sim_time, pending: false, price_probe: None };
                let probe = Some(PriceProbe { oracle: meta.params.oracle, old, new: price });
                let on_pending = PlanCtx { block: u.block - 1, pending: true, overrides: StateOverride::default(), price_probe: probe, ..at_head };
                (at_head, on_pending)
            })
            .collect();
        // All candidates are planned at once and each is handled (journaled, sent) the moment its
        // plan is ready: the best one never waits for the slowest. Both plans of a candidate run at
        // once; a fresh pending plan is used as soon as it is ready (the snapshot plan is dropped).
        let provider = fb.provider.clone();
        let ready: futures::stream::FuturesUnordered<_> = ctxs
            .iter()
            .enumerate()
            .map(|(i, (h, p))| {
                let (planner, provider) = (&planner, &provider);
                async move {
                    let at_head = planner.plan(provider, h);
                    // The pending RPCs trail the flashblock stream by ~50-100 ms: while the probe
                    // says the node has not applied the update yet, ask again (briefly).
                    let on_pending = async {
                        let mut tries = 1;
                        loop {
                            let r = planner.plan(provider, p).await;
                            let behind = matches!(&r, Ok(plan) if plan.stale_price || !p.is_fresh(plan));
                            if !(behind && tries < SECONDARY_PENDING_TRIES && u.seen.elapsed() < SECONDARY_PENDING_WINDOW) {
                                break r;
                            }
                            tries += 1;
                            tokio::time::sleep(SECONDARY_PENDING_RETRY).await;
                        }
                    };
                    tokio::pin!(at_head, on_pending);
                    let mut head_done = None;
                    let rp = loop {
                        tokio::select! {
                            r = &mut on_pending => break r,
                            r = &mut at_head, if head_done.is_none() => head_done = Some(r),
                        }
                    };
                    let chosen = match rp {
                        Ok(plan) if p.is_fresh(&plan) && !plan.stale_price => (Ok(plan), "pending"),
                        _ => (match head_done {
                            Some(r) => r,
                            None => at_head.await,
                        }, "snapshot"),
                    };
                    (i, chosen, u.seen.elapsed().as_millis() as u64)
                }
            })
            .collect();
        let planned = &mut fb.planned;
        let on_plan = |(i, (res, state), ready_ms): (usize, (Result<planner::Plan>, &'static str), u64), journal: &mut std::fs::File| -> Result<_> {
            let v = ctxs[i].0.view;
            match res {
                Ok(plan) => {
                    planned.insert((mi, v.user), (u.block, ready_ms, plan.net_usd));
                    let kill = std::path::Path::new(&engine.cfg.exec.kill_switch_file).exists();
                    let decision = risk::decide(&engine.cfg.exec, &mut parts.budget.lock().unwrap(), &plan, eth_usd, head.base_fee, head.timestamp, kill, parts.competition(&meta.name, head.timestamp));
                    let send_ok = may_send(u.path, state);
                    info!(market = %meta.name, user = %v.user, hf = v.hf, block = u.block, flashblock = u.index, state, ready_ms, net_usd = ?plan.net_usd, error = ?plan.error, decision = ?decision, "FLASHBLOCK PLAN");
                    writeln!(
                        journal,
                        "{}",
                        json!({ "kind": "fb_plan", "block": u.block, "flashblock": u.index, "market": meta.name, "user": v.user, "hf": v.hf, "state": state, "send_ok": send_ok, "ready_ms": ready_ms, "plan": plan, "decision": decision })
                    )?;
                    if let (true, risk::Decision::Send { priority_fee_wei, .. }) = (engine.cfg.flashblocks.send && send_ok, &decision) {
                        return Ok(Some((i, state, plan, *priority_fee_wei)));
                    }
                }
                Err(e) => warn!(error = %e, market = %meta.name, user = %v.user, "flashblock planning failed"),
            }
            Ok(None)
        };
        let ctx_of = |i: usize, state: &'static str| if state == "pending" { &ctxs[i].1 } else { &ctxs[i].0 };
        plan_and_send(ready, exec, parts, &provider, "flashblock", journal, on_plan, ctx_of, None, no_side).await?;
    }
    Ok(())
}

/// `plan_and_send` without side events (the same-block path itself).
async fn no_side(_: flashblocks::FbEvent, _: &mut std::fs::File) -> Result<()> {
    Ok(())
}

/// Pending plans whose state is behind (stale block or old oracle price) are retried up to this
/// many times in total, SECONDARY_PENDING_RETRY apart, while the update is younger than
/// SECONDARY_PENDING_WINDOW (later the leftovers are usually gone anyway).
const SECONDARY_PENDING_TRIES: u32 = 4;
const SECONDARY_PENDING_RETRY: Duration = Duration::from_millis(80);
const SECONDARY_PENDING_WINDOW: Duration = Duration::from_millis(600);

/// May a same-block plan be sent? A public transmit (Primary) is in effect once included, so its
/// snapshot plan (new price pinned) may send too. An SVR secondary or an expired SVR primary only
/// from the fresh pending state with the price proved there: whether the metacall succeeded (and
/// which borrowers it liquidated), or whether the cutoff has passed, is visible only there.
fn may_send(path: flashblocks::Path, state: &str) -> bool {
    path == flashblocks::Path::Primary || state == "pending"
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

/// Live send of an approved plan (skips at max_inflight, when this position or another one in the
/// same market is in flight, or when the wallet cannot cover the worst-case gas). `bid` = the risk
/// gate's priority, checked at its own priority first. `path` = "block" or "flashblock".
async fn send_approved<P: Provider>(e: &ExecHandle, parts: &PlannerParts, provider: &P, c: &PlanCtx<'_, Morpho>, plan: &planner::Plan, bid: u128, path: &str) -> Option<executor::Inflight> {
    let (meta, v, cfg) = (c.meta, c.view, &parts.exec);
    if !e.can_send() || e.is_inflight(tx_key(v)) {
        return None;
    }
    // Plans are simulated on a state without the txs in flight: one in the same market sells the
    // same collateral through the same pools first, so this plan's price would no longer hold.
    if e.is_group_inflight(v.market as u64) {
        info!(market = %meta.name, user = %v.user, path, "a liquidation in this market is in flight; not sending another on a stale price");
        return None;
    }
    let (Some(payload), Some(gas)) = (plan.payload.clone(), plan.gas_used) else { return None };
    let (priority_fee_wei, profit) = checked_priority(parts, provider, c, plan, bid).await?;
    let (base_fee_wei, block, deadline_block) = (c.base_fee_wei, c.block, c.deadline_block);
    let min_profit = U256::from((profit * cfg.min_profit_keep * 10f64.powi(meta.loan_decimals as i32)) as u128);
    let gas_limit = (gas as f64 * cfg.gas_limit_buffer) as u64;
    let worst_wei = gas_limit as u128 * (base_fee_wei * 2 + priority_fee_wei);
    // The background reading minus what the txs in flight may still burn; a live read only before
    // the first reading.
    let spendable = match e.spendable() {
        Some(b) => Ok(b),
        None => provider.get_balance(e.address()).await.map_err(anyhow::Error::from),
    };
    match spendable {
        Ok(b) if b < U256::from(worst_wei) => {
            warn!(balance_wei = %b, worst_wei, "hot wallet cannot cover worst-case gas; not sending");
            return None;
        }
        Err(err) => {
            warn!(error = %err, "balance check failed; not sending");
            return None;
        }
        _ => {}
    }
    // The daily budget holds this tx's worst case (all of its gas limit reverted, plus the L1 fee)
    // while it is in flight: sends running at once cannot each fit the cap alone and overrun it
    // together. Released in on_settled.
    let key = tx_key(v);
    let worst_usd = worst_wei as f64 / 1e18 * c.eth_usd + plan.l1_fee_usd.unwrap_or(0.0);
    let now = gates::unix_secs();
    // A reservation already held for this position (the other path is sending it) is refused, not
    // replaced.
    let reserved = parts.budget.lock().unwrap().reserve(key, now, worst_usd, cfg.daily_gas_budget_usd);
    if let Err(why) = reserved {
        info!(market = %meta.name, user = %v.user, worst_usd, path, reason = %why, "not sending");
        return None;
    }
    // One liquidation per market in flight, enforced by the executor task itself: the check above
    // and this send are not atomic, and the block path and flashblock events send concurrently.
    let req = executor::SendRequest { payload, min_profit, gas_limit, base_fee_wei, priority_fee_wei, key, exclusive_group: true, block, deadline_block };
    match e.send(req).await {
        Ok(inf) => {
            info!(hash = %inf.hash, block, deadline_block, market = %meta.name, user = %v.user, path, "LIVE tx SENT");
            Some(inf)
        }
        Err(err) => {
            parts.budget.lock().unwrap().release(key);
            // The in-flight rules (the other path won the race for this market) or the cooldown:
            // expected, not a send failure.
            match err.downcast_ref::<executor::SendRefused>() {
                Some(why) => info!(market = %meta.name, user = %v.user, path, reason = %why, "not sending (executor)"),
                None => warn!(error = %err, "LIVE send failed"),
            }
            None
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
/// on a state that includes the first. `on_plan` journals a plan and returns the bid to send it at
/// (None = not sent); `ctx_of(i, plan_state)` gives the context it was planned on.
///
/// `side` = flashblock events that must not wait for this batch (the block path passes the
/// stream): each one is handled by `on_side` the moment it arrives, ahead of the batch, whose
/// planning resumes afterwards.
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
    mut side: Option<&mut tokio::sync::mpsc::Receiver<flashblocks::FbEvent>>,
    mut on_side: G,
) -> Result<()>
where
    P: Provider,
    S: futures::Stream<Item = R> + Unpin,
    F: FnMut(R, &mut std::fs::File) -> Result<Option<(usize, &'static str, planner::Plan, u128)>>,
    C: Fn(usize, &'static str) -> &'c PlanCtx<'c, Morpho>,
    G: AsyncFnMut(flashblocks::FbEvent, &mut std::fs::File) -> Result<()>,
{
    use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
    let mut sending: FuturesUnordered<futures::future::BoxFuture<'_, Option<executor::Inflight>>> = FuturesUnordered::new();
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
                    if let (Some((i, state, plan, bid)), Some(e)) = (on_plan(r, journal)?, exec) {
                        let c = ctx_of(i, state);
                        if claimed.insert(c.view.market) {
                            sending.push(async move { send_approved(e, parts, provider, c, &plan, bid, path).await }.boxed());
                        } else {
                            info!(market = %c.meta.name, user = %c.view.user, path, "another liquidation in this market is being sent; this one waits for a fresh plan");
                        }
                    }
                }
                None => planning = false,
            },
            Some(inf) = sending.next(), if !sending.is_empty() => {
                if let Some(inf) = inf {
                    writeln!(journal, "{}", json!({ "kind": "sent", "block": inf.sent_block, "path": path, "tx": inf }))?;
                }
            }
        }
    }
    Ok(())
}

/// A tx the executor task settled: journaled, its worst-case reservation released, and a revert's
/// real cost charged to the daily budget (a success paid for itself).
fn on_settled(s: actor::Settled, block: u64, now_unix: u64, eth_usd: f64, cooldown_secs: u64, budget: &mut risk::DailyBudget, journal: &mut std::fs::File) -> Result<()> {
    let actor::Settled { inflight, outcome, cooldown_started } = s;
    budget.release(inflight.key);
    if let executor::Outcome::Included { success: false, gas_used, effective_gas_price, l1_fee_wei, .. } = &outcome {
        let cost = (*gas_used as f64 * *effective_gas_price as f64 + *l1_fee_wei as f64) / 1e18 * eth_usd;
        budget.charge(now_unix, cost);
    }
    info!(hash = %inflight.hash, ?outcome, "LIVE tx outcome");
    writeln!(journal, "{}", json!({ "kind": "outcome", "block": block, "tx": inflight, "outcome": outcome }))?;
    if cooldown_started {
        warn!(cooldown_s = cooldown_secs, "LIVE sending paused after a burst of failed txs; resumes by itself");
    }
    Ok(())
}

async fn watch<P: Provider + Clone + Send + Sync + 'static>(
    engine: &mut MorphoEngine<P>,
    from_block: Option<u64>,
    pool: rpc::RpcPool,
    ws_endpoints: Vec<(String, String)>,
    live: Option<(alloy::signers::local::PrivateKeySigner, Vec<(String, String)>)>,
    fb_provider: Option<(P, rpc::RpcPool)>,
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
    let parts = Arc::new(PlannerParts { protocol: engine.protocol.clone(), kyber, venue: simulator, solver: build_atlas_solver(engine).await?, atlas_surcharge: atlas_surcharge(engine).await, v3, exec: engine.cfg.exec.clone(), bids: std::sync::Mutex::new(WinningBids::new(engine.cfg.exec.bid_window_secs)), submitter: build_submitter(engine, live.as_ref().map(|l| &l.0)).await?, budget: Default::default() });

    // Watchlist rows are journaled only when a position enters it or its HF moves > 0.2%.
    let mut last_logged_hf: HashMap<(usize, Address), f64> = HashMap::new();
    // Candidate -> block it was last planned at.
    let mut last_planned: HashMap<(usize, Address), u64> = HashMap::new();
    let mut tracker = loss::LossTracker::default();
    // Live sending (None = shadow). The key never leaves the keystore decrypt. The executor runs
    // as its own task (onchain_core::actor): sends go to it as messages, it polls receipts on every
    // block announced on `blocks_tx`, and settled txs come back on `settled`.
    let (blocks_tx, blocks_rx) = watch::channel(0u64);
    let (exec, mut settled) = match live {
        None => (None, None),
        Some((signer, broadcast)) => {
            preflight_live(engine, signer.address()).await?;
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
    // Observed block time (for deadline_secs -> blocks): anchor on the first live block.
    let mut bt_anchor: Option<(u64, u64)> = None;
    let mut known_tip = heads::http_head(&engine.provider, BlockNumberOrTag::Latest).await?.number;
    let mut last_block = from_block.map(|b| b - 1).unwrap_or(known_tip);
    engine.refresh_borrowers(last_block.into()).await?;
    let mut last_refresh = Instant::now();
    let ws_names: Vec<String> = ws_endpoints.iter().map(|w| w.0.clone()).collect();
    let mut ws = (!ws_endpoints.is_empty()).then(|| heads::spawn_ws_many(ws_endpoints));
    // Which source delivered each processed block first (WS endpoint index, or HTTP polling).
    let mut head_wins: HashMap<usize, u64> = HashMap::new();
    let mut prev_snaps: Option<Vec<Option<morpho::onchain::MarketSnapshot>>> = None;
    let market_ids = engine.market_ids();
    let liquidator = venue::LiquidationVenue::<Morpho>::target(&parts.venue);
    seed_winning_bids(&parts, &engine.cfg.journal_path, liquidator, engine.cfg.flashblocks.atlas);
    if exec.is_some() {
        warn!(from_block = last_block + 1, ws = ws.is_some(), %liquidator, max_inflight = engine.cfg.exec.max_inflight, "LIVE mode: watching blocks, approved liquidations WILL be sent");
    } else {
        info!(from_block = last_block + 1, ws = ws.is_some(), %liquidator, "shadow mode: watching blocks, nothing will be sent");
    }

    // Rows finished by background tasks (auction valuations), written by this loop.
    let (journal_tx, mut journal_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // Same-block mode: local oracle prices + the flashblock stream (live replay only).
    let mut fb: Option<FbState<P>> = match fb_provider {
        Some((provider, fb_pool)) if from_block.is_none() => {
            let model = oracle::PriceModel::load(&engine.provider, &engine.metas, alloy::eips::BlockId::latest()).await;
            let stream = flashblocks::spawn(engine.cfg.flashblocks.ws.clone(), model.watched(), engine.cfg.flashblocks.atlas);
            if engine.cfg.atlas.observe {
                atlas::spawn(engine.cfg.atlas.ws.clone(), engine.cfg.chain_id, engine.cfg.atlas.control, stream.sender.clone());
                info!("atlas solver phase 0: valuing SVR auctions, never bidding");
            }
            info!(send = engine.cfg.flashblocks.send && exec.is_some(), "flashblocks mode: reacting to oracle updates inside the block");
            Some(FbState { provider, pool: fb_pool, stream, model, last_oracle: HashMap::new(), planned: HashMap::new(), max_update_block: 0, updates: 0, started: Instant::now(), journal_tx: journal_tx.clone(), valuations: Arc::new(tokio::sync::Semaphore::new(engine.cfg.atlas.max_concurrent.max(1))), auction_cache: Arc::default() })
        }
        _ => None,
    };
    let run_started = Instant::now();
    // Context a mid-block oracle update needs: the latest processed head, ETH price, block time.
    let mut last_ctx: Option<(Head, f64, f64)> = None;

    loop {
        // Live: newest head. Replaying: step one block at a time so nothing is skipped.
        let replaying = known_tip > last_block + 10;
        let head = if replaying {
            heads::http_head(&engine.provider, BlockNumberOrTag::Number(last_block + 1)).await
        } else {
            // While waiting for the next block, handle oracle updates seen in flashblocks.
            loop {
                enum Ev {
                    Head(Result<Head>),
                    Oracle(Option<flashblocks::FbEvent>),
                }
                let ev = match fb.as_mut() {
                    Some(f) => tokio::select! {
                        u = f.stream.updates.recv() => Ev::Oracle(u),
                        h = wait_head(&mut ws, Some(&mut f.stream.sealed), &engine.provider, last_block, engine.cfg.poll_ms) => Ev::Head(h),
                    },
                    None => Ev::Head(wait_head(&mut ws, None, &engine.provider, last_block, engine.cfg.poll_ms).await),
                };
                match ev {
                    Ev::Head(h) => break h,
                    Ev::Oracle(Some(u)) => {
                        if let (Some(f), Some(snaps), Some((h, eth_usd, bt))) = (fb.as_mut(), prev_snaps.as_ref(), last_ctx.as_ref()) {
                            on_fb_event(engine, f, u, snaps, h, *eth_usd, *bt, &parts, exec.as_ref(), &mut journal).await?;
                            drain_journal(&mut journal_rx, &mut journal)?;
                        }
                    }
                    Ev::Oracle(None) => {
                        warn!("flashblocks stream task ended; same-block mode off");
                        fb = None;
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
        if !replaying && bt_anchor.is_none() {
            bt_anchor = Some((number, h.timestamp));
        }
        let block_time = match bt_anchor {
            Some((n0, t0)) if number >= n0 + 10 && h.timestamp > t0 => (h.timestamp - t0) as f64 / (number - n0) as f64,
            _ => DEFAULT_BLOCK_TIME,
        };
        let deadline_block = number + gates::deadline_blocks(engine.cfg.exec.deadline_secs, block_time);
        let t0 = Instant::now();

        let gap = number - last_block;
        if gap > engine.cfg.max_log_range && !replaying {
            // Too far behind for one getLogs: liquidations in the gap are not analysed.
            warn!(gap, "fell behind; full reload");
            engine.positions.clear();
            engine.refresh_borrowers(number.into()).await?;
            last_refresh = Instant::now();
            last_block = number - 1;
            prev_snaps = None;
        }

        // 1. Events since the last block and fresh market snapshots, fetched in parallel.
        //    Interest is accrued to ~the next block so interest-only crossings show up early.
        let accrue_to = h.timestamp + engine.cfg.lookahead_secs(block_time);
        let from_block_logs = last_block + 1;
        // The fetch is most of a block's processing time (~270 ms without alchemy): oracle updates
        // seen meanwhile are handled right away instead of queueing behind the block path (seen:
        // 165 ms queue delay in the 2026-10-18 shadow run). Scoped so the future (and its borrows
        // of engine / prev_snaps) is gone before the block path mutates them.
        let (touched, snaps) = {
            let fetch = async {
                tokio::join!(
                    engine.touched(&market_ids, from_block_logs, number),
                    async {
                        match &prev_snaps {
                            Some(prev) => engine.snapshots_fast(number.into(), accrue_to, prev).await,
                            None => engine.snapshots(number.into(), accrue_to).await,
                        }
                    }
                )
            };
            tokio::pin!(fetch);
            loop {
                let Some(f) = fb.as_mut() else { break fetch.await };
                tokio::select! {
                    r = &mut fetch => break r,
                    Some(u) = f.stream.updates.recv() => {
                        if let (Some(prev), Some((lh, eth, bt))) = (prev_snaps.as_ref(), last_ctx.as_ref()) {
                            on_fb_event(engine, f, u, prev, lh, *eth, *bt, &parts, exec.as_ref(), &mut journal).await?;
                            drain_journal(&mut journal_rx, &mut journal)?;
                        }
                    }
                }
            }
        };
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
        if let Some(rx) = settled.as_mut() {
            while let Ok(s) = rx.try_recv() {
                on_settled(s, number, h.timestamp, eth_usd, engine.cfg.exec.failure_cooldown_secs, &mut parts.budget.lock().unwrap(), &mut journal)?;
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
                if !replaying && last_planned.get(&key).is_none_or(|b| number >= b + engine.cfg.exec.replan_blocks) {
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
                    let decision = risk::decide(&engine.cfg.exec, &mut parts.budget.lock().unwrap(), &plan, eth_usd, h.base_fee, h.timestamp, kill, parts.competition(&meta.name, h.timestamp));
                    info!(
                        block = number, market = %meta.name, user = %v.user, hf = v.hf,
                        quoted_gross = plan.quoted_gross, net_usd = ?plan.net_usd, error = ?plan.error, decision = ?decision,
                        "PLAN"
                    );
                    let row = json!({ "kind": "plan", "block": number, "timestamp": h.timestamp, "market": meta.name, "user": v.user, "hf": v.hf, "plan": plan, "decision": decision });
                    writeln!(journal, "{row}")?;
                    // Live: send what the risk gate approved (up to exec.max_inflight at once).
                    if let risk::Decision::Send { priority_fee_wei, .. } = &decision {
                        return Ok(Some((i, "block", plan, *priority_fee_wei)));
                    }
                }
                Err(e) => warn!(error = %e, "planning failed"),
            }
            Ok(None)
        };
        // Oracle updates seen while this batch is planned are handled at once (on the previous
        // block's snapshots, like those seen during the fetch above), not after the batch: the
        // stream's receiver is lent to plan_and_send for the duration.
        let mut fb_events = fb.as_mut().map(|f| std::mem::replace(&mut f.stream.updates, tokio::sync::mpsc::channel(1).1));
        let batch = {
            let (fb, journal_rx) = (&mut fb, &mut journal_rx);
            let (prev_snaps, last_ctx, parts, exec) = (&prev_snaps, &last_ctx, &parts, exec.as_ref());
            let engine = &*engine;
            let on_side = async move |ev: flashblocks::FbEvent, journal: &mut std::fs::File| -> Result<()> {
                if let (Some(f), Some(prev), Some((lh, eth, bt))) = (fb.as_mut(), prev_snaps.as_ref(), last_ctx.as_ref()) {
                    on_fb_event(engine, f, ev, prev, lh, *eth, *bt, parts, exec, journal).await?;
                    drain_journal(journal_rx, journal)?;
                }
                Ok(())
            };
            plan_and_send(ready, exec, parts, &engine.provider, "block", &mut journal, on_plan, |i, _| &ctxs[i], fb_events.as_mut(), on_side).await
        };
        if let (Some(f), Some(rx)) = (fb.as_mut(), fb_events) {
            f.stream.updates = rx;
        }
        batch?;
        // The plans borrow this block's snapshots and the engine: release them before both move on.
        drop(ctxs);
        last_ctx = Some((h, eth_usd, block_time));
        if let Some(f) = fb.as_mut() {
            // The model runs ahead of sealed blocks; once they have caught up it must equal the
            // chain (else an update was missed or reverted, or an aggregator was swapped).
            if f.max_update_block <= number {
                for (mi, s) in snaps.iter().enumerate() {
                    if let Some(s) = s.as_ref().filter(|_| f.model.is_modelled(mi))
                        && f.model.price(mi) != Some(s.price)
                    {
                        f.model.resync(&engine.provider, mi, s.price, number.into()).await;
                    }
                }
            }
            f.planned.retain(|_, p| p.0 + 30 > number);
        }
        drain_journal(&mut journal_rx, &mut journal)?;
        prev_snaps = Some(snaps);

        // 4b. Loss analysis, part 2: sender / fee lookups for this block's liquidations, in parallel.
        if !liq_rows.is_empty() {
            let costs = futures::future::join_all(liq_rows.iter().map(|r| morpho::events::tx_cost(&engine.provider, r.tx))).await;
            for (row, cost) in liq_rows.iter_mut().zip(costs) {
                match cost {
                    Ok((eoa, to, prio, fee)) => {
                        row.apply_tx(loss::TxInfo { eoa, to, priority_fee_wei: prio, fee_wei: fee }, eth_usd);
                        parts.observe_winner(row, liquidator, engine.cfg.flashblocks.atlas);
                    }
                    Err(e) => warn!(tx = %row.tx, error = %e, "receipt lookup failed"),
                }
                info!(
                    block = row.block, market = %row.market, borrower = %row.borrower, liquidator = %row.liquidator,
                    gross = row.gross_bonus, outcome = %row.outcome, delay = ?row.delay_blocks, "LIQUIDATION observed"
                );
                writeln!(journal, "{}", serde_json::to_string(&*row)?)?;
                // Same-block race: which flashblock the winner landed in vs when we were ready.
                if let (Some(f), Some(mi)) = (fb.as_ref(), engine.market_by_name(&row.market)) {
                    let winner = f.stream.seen.lock().unwrap().get(&row.tx).copied();
                    let oracle = f.last_oracle.get(&mi).filter(|o| o.0 == row.block).copied();
                    let ours = f.planned.get(&(mi, row.borrower)).filter(|p| p.0 == row.block).copied();
                    let winner_after_oracle_ms = match (winner, oracle) {
                        (Some(w), Some(o)) => Some(w.at.saturating_duration_since(o.2).as_millis() as i64),
                        _ => None,
                    };
                    let verdict = match (oracle, ours, winner_after_oracle_ms) {
                        (None, _, _) => "no_oracle_update_this_block",
                        (Some(_), None, _) => "not_planned",
                        (Some(_), Some(p), Some(w)) if (p.1 as i64) < w => "ready_before_winner_flashblock",
                        (Some(_), Some(_), Some(_)) => "ready_after_winner_flashblock",
                        (Some(_), Some(_), None) => "winner_not_in_stream",
                    };
                    info!(market = %row.market, borrower = %row.borrower, winner_flashblock = ?winner.map(|w| w.index), oracle_flashblock = ?oracle.map(|o| o.1), our_ready_ms = ?ours.map(|p| p.1), ?winner_after_oracle_ms, verdict, "FLASHBLOCK race");
                    writeln!(
                        journal,
                        "{}",
                        json!({ "kind": "fb_race", "block": row.block, "market": row.market, "borrower": row.borrower, "tx": row.tx,
                                "winner_flashblock": winner.map(|w| w.index), "winner_block_in_stream": winner.map(|w| w.block),
                                "oracle_flashblock": oracle.map(|o| o.1), "winner_after_oracle_ms": winner_after_oracle_ms,
                                "our_ready_after_oracle_ms": ours.map(|p| p.1), "our_net_usd": ours.and_then(|p| p.2), "verdict": verdict })
                    )?;
                }
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
        let head_age_ms = gates::unix_ms() as i64 - (h.timestamp as i64) * 1000;
        info!(
            block = number, tracked = views.len(), touched, planned, fetch_ms, reload_ms, eval_us, decide_ms, head_age_ms,
            "block"
        );
        *head_wins.entry(h.source).or_default() += 1;
        if number % 30 == 0 {
            let wins: Vec<String> = head_wins
                .iter()
                .map(|(i, n)| format!("{}={n}", if *i == heads::SOURCE_FLASHBLOCK { "flashblock" } else { ws_names.get(*i).map(String::as_str).unwrap_or("http-poll") }))
                .collect();
            let rpc: Vec<String> = pool
                .report()
                .iter()
                .map(|e| format!("{}: req {} failover {} (lag {}) hedge {} ewma {:.0}ms{}", e.name, e.requests, e.failovers, e.lagging, e.hedges, e.ewma_ms, if e.cooling { " COOLING" } else { "" }))
                .collect();
            info!(?wins, ?rpc, "source stats");
            if let Some(f) = fb.as_ref() {
                let age_ms = f.stream.last_msg.lock().unwrap().map(|t| t.elapsed().as_millis() as u64);
                let seen = f.stream.seen.lock().unwrap().len();
                info!(updates = f.updates, last_msg_age_ms = ?age_ms, txs_tracked = seen, "flashblocks stats");
                // Never connected yet is only a problem once startup is well over.
                if age_ms.map_or(f.started.elapsed() > Duration::from_secs(30), |a| a > 5_000) {
                    warn!("flashblocks stream silent for >5 s: same-block mode is blind until it reconnects");
                }
            }
        }
        // Quota accounting: cumulative attempts per (endpoint, method), journaled every ~10 min.
        if number % 300 == 0 {
            let mut usage: Vec<serde_json::Value> = pool.method_report().into_iter().map(|(e, m, n)| json!({"pool": "main", "endpoint": e, "method": m, "n": n})).collect();
            if let Some(f) = fb.as_ref() {
                usage.extend(f.pool.method_report().into_iter().map(|(e, m, n)| json!({"pool": "pending", "endpoint": e, "method": m, "n": n})));
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
    use super::may_send;

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
    fn secondary_sends_only_from_fresh_pending_state() {
        // SVR secondary: only the pending state knows whether the metacall worked and whom it took.
        use crate::flashblocks::Path;
        assert!(may_send(Path::Secondary, "pending"));
        assert!(!may_send(Path::Secondary, "snapshot"));
        // An expired SVR primary takes effect only past the cutoff: proven in the pending state only.
        assert!(may_send(Path::Expired, "pending"));
        assert!(!may_send(Path::Expired, "snapshot"));
        // A public transmit is in effect once included.
        assert!(may_send(Path::Primary, "pending"));
        assert!(may_send(Path::Primary, "snapshot"));
    }
}
