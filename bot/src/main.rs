//! Liquidation bot — entry point (scan / watch / dashboard / live / loss / value-auction). The
//! framework is the `liquidation_framework` library (see lib.rs, and `runner` for the watch loop);
//! this binary runs it for Morpho Blue on Base.

use alloy::primitives::Address;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use liquidation_framework::{
    config::Config,
    dash, live, loss,
    runner::{Runner, RunnerInputs, load_signer_pre_runtime},
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
    // Live: get the key BEFORE the async runtime exists (see load_signer_pre_runtime).
    let live_signer = match &cli.cmd {
        Cmd::Watch { live: true, .. } => Some(load_signer_pre_runtime(&cfg)?),
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
    let from_block = match cli.cmd {
        Cmd::Watch { from_block, .. } => from_block,
        _ => None,
    };
    let mut runner = Runner::build(RunnerInputs { cfg, signer: live_signer, from_block }).await?;
    match cli.cmd {
        Cmd::Scan { top } => runner.scan(top).await,
        Cmd::Watch { .. } => runner.run().await,
        Cmd::Plan { market, user, price_drop_bps, at_block, pending } => runner.plan(&market, user, price_drop_bps, at_block, pending).await,
        Cmd::ValueAuction { market, price_drop_bps, at_block, users, kyber } => runner.value_auction(&market, price_drop_bps, at_block, &users, kyber).await,
        Cmd::Report => unreachable!("handled before connecting"),
        Cmd::Dashboard => unreachable!("handled before connecting"),
        Cmd::Live => unreachable!("handled before connecting"),
    }
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

