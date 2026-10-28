//! `bot dashboard`: durable, queryable history + a local status page.
//!
//! One pass per call (scripts/dashboard_loop.sh runs it every minute, as its own process):
//!  1. ingest what the journal and the run log gained since the last pass into SQLite
//!     (`dashboard.db_path`): per-block timings (kept `raw_block_days`), hourly aggregates,
//!     warnings, restarts, RPC stats and usage, liquidations, Atlas auctions, sends/outcomes.
//!     Everything except per-block rows is kept forever; the raw files are never modified;
//!  2. look up, on `dashboard.rpc`, who won each journaled liquidation (Atlas metacall bids) and
//!     the hot wallet balance;
//!  3. render `dashboard.html_path` (render.rs) and, once a day, copy the database and the journal
//!     to `dashboard.backup_dir`.
//!
//! It only reads the bot's files and never touches its RPC pool. URLs (API keys) are never
//! stored or printed: endpoints appear by name only.

mod exits;
mod render;

use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    time::Duration,
};

use alloy::{
    primitives::{Address, B256, Bytes, address},
    sol,
    sol_types::{SolCall, SolEvent},
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::config::Config;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- ts = block timestamp (arrival - head_age), seen = when the bot had processed it.
CREATE TABLE IF NOT EXISTS blocks (block INTEGER PRIMARY KEY, ts REAL NOT NULL, seen REAL NOT NULL,
    fetch_ms INTEGER, decide_ms INTEGER, head_age_ms INTEGER, planned INTEGER);
CREATE TABLE IF NOT EXISTS hour_stats (hour INTEGER PRIMARY KEY, blocks INTEGER, missing INTEGER,
    head_age_p50 INTEGER, head_age_p90 INTEGER, fetch_p50 INTEGER, fetch_p90 INTEGER, decide_p50 INTEGER, decide_p90 INTEGER);
CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY, ts REAL NOT NULL, level TEXT, kind TEXT, endpoint TEXT, message TEXT);
CREATE INDEX IF NOT EXISTS events_ts ON events (ts);
CREATE TABLE IF NOT EXISTS runs (ts REAL PRIMARY KEY);
CREATE TABLE IF NOT EXISTS rpc_stats (ts REAL, endpoint TEXT, requests INTEGER, failovers INTEGER, lagging INTEGER,
    hedges INTEGER, ewma_ms INTEGER, cooling INTEGER, PRIMARY KEY (ts, endpoint));
CREATE TABLE IF NOT EXISTS rpc_usage (block INTEGER, uptime_s INTEGER, pool TEXT, endpoint TEXT, method TEXT, n INTEGER,
    PRIMARY KEY (block, pool, endpoint, method));
CREATE TABLE IF NOT EXISTS liquidations (tx TEXT, borrower TEXT, block INTEGER, market TEXT, liquidator TEXT, eoa TEXT,
    gross_usd REAL, repaid REAL, seized_value REAL, bad_debt REAL, fee_usd REAL, outcome TEXT, json TEXT, PRIMARY KEY (tx, borrower));
CREATE INDEX IF NOT EXISTS liquidations_block ON liquidations (block);
CREATE TABLE IF NOT EXISTS atlas_auctions (auction_id TEXT PRIMARY KEY, user_op_hash TEXT, head INTEGER, deadline INTEGER,
    aggregator TEXT, liquidatable INTEGER, bonus_usd REAL, bundle_net_usd REAL, net_usd REAL, bid_usd REAL, bid_basis TEXT,
    eth_usd REAL, bundle_ms INTEGER, value_ms INTEGER, queued_ms INTEGER, skipped TEXT, json TEXT);
CREATE INDEX IF NOT EXISTS atlas_auctions_uoh ON atlas_auctions (user_op_hash);
CREATE INDEX IF NOT EXISTS atlas_auctions_head ON atlas_auctions (head);
-- On-chain facts about journaled liquidation txs (dashboard.rpc lookups).
CREATE TABLE IF NOT EXISTS liq_txs (tx TEXT PRIMARY KEY, block INTEGER, is_atlas INTEGER, user_op_hash TEXT, winner TEXT,
    win_bid_eth REAL, bids TEXT);
CREATE INDEX IF NOT EXISTS liq_txs_uoh ON liq_txs (user_op_hash);
-- Verbatim tx + receipt of every journaled liquidation tx (exits.rs decodes from here).
CREATE TABLE IF NOT EXISTS liq_raw (tx TEXT PRIMARY KEY, block INTEGER, tx_json TEXT, receipt_json TEXT, fetched REAL);
-- Liquidation txs the RPC would not serve (error or null), retried with backoff, then given up on.
CREATE TABLE IF NOT EXISTS liq_fetch_fail (tx TEXT PRIMARY KEY, tries INTEGER, last REAL, error TEXT);
-- Stored txs exits::parse could not read (per decoder version, so a new decoder retries them).
CREATE TABLE IF NOT EXISTS liq_unparsed (tx TEXT PRIMARY KEY, version INTEGER);
CREATE TABLE IF NOT EXISTS liq_exits (tx TEXT PRIMARY KEY, block INTEGER, actor TEXT, eoa TEXT, via TEXT, n_liq INTEGER, flash TEXT,
    exit_kind TEXT, route TEXT, venues TEXT, collateral TEXT, seized REAL, sold REAL, kept REAL, carried REAL, loan_in REAL,
    oracle_usd REAL, slippage_bps REAL, unknown TEXT, gas_used INTEGER, gas_price_gwei REAL, priority_gwei REAL, version INTEGER);
CREATE INDEX IF NOT EXISTS liq_exits_block ON liq_exits (block);
CREATE TABLE IF NOT EXISTS pool_meta (pool TEXT PRIMARY KEY, protocol TEXT, factory TEXT, token0 TEXT, token1 TEXT);
CREATE TABLE IF NOT EXISTS token_meta (token TEXT PRIMARY KEY, symbol TEXT, decimals INTEGER);
CREATE TABLE IF NOT EXISTS sends (tx TEXT PRIMARY KEY, block INTEGER, path TEXT, json TEXT);
CREATE TABLE IF NOT EXISTS outcomes (tx TEXT PRIMARY KEY, block INTEGER, result TEXT, success INTEGER, gas_cost_eth REAL, json TEXT);
CREATE TABLE IF NOT EXISTS wallet (ts REAL PRIMARY KEY, address TEXT, eth REAL);
-- Every other journal row (fb_oracle, fb_plan, fb_race, plan, watch, ...), verbatim.
CREATE TABLE IF NOT EXISTS journal_other (id INTEGER PRIMARY KEY, kind TEXT, block INTEGER, json TEXT);
CREATE INDEX IF NOT EXISTS journal_other_kind ON journal_other (kind, block);
";

/// Liquidation txs fetched per pass (2 RPC calls each, spaced out): a crash day's backlog
/// drains over a few minutes without bursting the endpoint.
const LOOKUPS_PER_PASS: usize = 40;
/// A tx the RPC does not serve is retried FETCH_RETRY_SECS x tries later, at most FETCH_TRIES times.
const FETCH_TRIES: i64 = 8;
const FETCH_RETRY_SECS: f64 = 300.0;
const WALLET_EVERY_SECS: f64 = 600.0;
const BACKUPS_KEPT: usize = 14;

pub async fn run(cfg: &Config) -> Result<()> {
    let d = &cfg.dashboard;
    if let Some(dir) = std::path::Path::new(&d.db_path).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut db = Connection::open(&d.db_path).with_context(|| format!("opening {}", d.db_path))?;
    db.busy_timeout(Duration::from_secs(10))?;
    db.execute_batch(SCHEMA)?;

    let (lines, first_new_ts) = ingest_log(&mut db, &d.log_path)?;
    refresh_hour_stats(&db, first_new_ts)?;
    let rows = ingest_journal(&mut db, &cfg.journal_path)?;
    let cutoff = now() - d.raw_block_days as f64 * 86_400.0;
    db.execute("DELETE FROM blocks WHERE seen < ?1", params![cutoff])?;

    match rpc_url(cfg)? {
        Some(url) => {
            let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
            if let Err(e) = fetch_liq_raw(&db, &http, &url).await {
                warn!(error = format!("{e:#}"), "dashboard: liquidation tx fetch failed");
            }
            if let Err(e) = analyze_liq_txs(&db, &http, &url, cfg).await {
                warn!(error = format!("{e:#}"), "dashboard: liquidation decoding failed");
            }
            if let Err(e) = record_wallet(&db, &http, &url, cfg.exec.executor).await {
                warn!(error = format!("{e:#}"), "dashboard: wallet balance failed");
            }
        }
        None => {}
    }

    let html = render::page(&db, cfg)?;
    let tmp = format!("{}.tmp", d.html_path);
    std::fs::write(&tmp, html)?;
    std::fs::rename(&tmp, &d.html_path)?;
    if let Some(dir) = &d.backup_dir {
        if let Err(e) = backup(&db, dir, &cfg.journal_path) {
            warn!(error = format!("{e:#}"), "dashboard: backup failed");
        }
    }
    info!(log_lines = lines, journal_rows = rows, html = %d.html_path, "dashboard updated");
    Ok(())
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// The configured dashboard endpoint's URL (from .env; never logged).
fn rpc_url(cfg: &Config) -> Result<Option<String>> {
    let Some(name) = &cfg.dashboard.rpc else { return Ok(None) };
    let resolved = cfg.resolve_rpc()?;
    let url = resolved.http.into_iter().find(|e| &e.name == name).map(|e| e.url);
    if url.is_none() {
        warn!(endpoint = %name, "dashboard.rpc names no usable [[rpc]] endpoint; lookups off");
    }
    Ok(url)
}

// ---------------------------------------------------------------- incremental file reading

/// New complete lines of `path` since the offset saved under `key` (restarts from 0 when the
/// file was replaced or truncated). The offset only advances past whole lines.
fn read_new(db: &Connection, path: &str, key: &str) -> Result<(String, u64)> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((String::new(), 0)),
        Err(e) => return Err(e).with_context(|| format!("opening {path}")),
    };
    let len = f.metadata()?.len();
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&f.metadata()?);
    #[cfg(not(unix))]
    let ino = 0u64;
    let saved: Option<String> = db.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).optional()?;
    let (mut off, saved_ino) = saved.and_then(|v| v.split_once(':').map(|(a, b)| (a.parse().unwrap_or(0), b.parse().unwrap_or(0)))).unwrap_or((0u64, ino));
    if saved_ino != ino || off > len {
        off = 0;
    }
    f.seek(SeekFrom::Start(off))?;
    let mut buf = Vec::with_capacity((len - off) as usize);
    f.read_to_end(&mut buf)?;
    let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    buf.truncate(end);
    let new_off = off + end as u64;
    // Becomes the real offset only when the ingest transaction commits (commit_offset).
    db.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![format!("{key}.pending"), format!("{new_off}:{ino}")])?;
    Ok((String::from_utf8_lossy(&buf).into_owned(), new_off))
}

/// Commits the offset read_new prepared, inside the ingest transaction.
fn commit_offset(tx: &rusqlite::Transaction, key: &str) -> Result<()> {
    let pending: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = ?1", params![format!("{key}.pending")], |r| r.get(0)).optional()?;
    if let Some(v) = pending {
        tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![key, v])?;
    }
    Ok(())
}

// ---------------------------------------------------------------- run log

/// Unix seconds of a log timestamp ("2026-10-19T10:58:29.569583Z").
pub(crate) fn parse_ts(s: &str) -> Option<f64> {
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let frac = s.get(19..).unwrap_or("").trim_end_matches('Z');
    let frac: f64 = if frac.starts_with('.') { format!("0{frac}").parse().ok()? } else { 0.0 };
    Some((days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se) as f64 + frac)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// (year, month, day) of a day number from days_from_civil.
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `key=value` from a tracing line (value up to the next space; quotes stripped).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!(" {key}=");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    if let Some(q) = rest.strip_prefix('"') {
        return q.find('"').map(|e| &q[..e]);
    }
    Some(rest.split(' ').next().unwrap_or(rest))
}

fn num(line: &str, key: &str) -> Option<i64> {
    field(line, key)?.parse().ok()
}

/// Drops anything URL-like (endpoint URLs carry API keys).
fn scrub(s: &str) -> String {
    s.split(' ').map(|w| if w.contains("://") { "<url>" } else { w }).collect::<Vec<_>>().join(" ")
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            for c in it.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// What a WARN/ERROR line is about (for counting).
fn event_kind(msg: &str) -> &'static str {
    const KINDS: [(&str, &str); 14] = [
        ("receipt not visible", "metacall_receipt_timeout"),
        ("metacall reverted", "metacall_reverted"),
        ("head fetch failed", "head_fetch_failed"),
        ("fetch failed", "fetch_failed"),
        ("position reload failed", "reload_failed"),
        ("rpc failover", "rpc_failover"),
        ("rpc method unsupported", "rpc_method_learned"),
        ("flashblocks stream", "fb_stream_reconnect"),
        ("ws head stream", "ws_reconnect"),
        ("atlas auction feed", "atlas_feed_reconnect"),
        ("ATLAS auction skipped", "atlas_skipped"),
        ("LIVE MODE", "live_start"),
        ("HALTED", "live_halted"),
        ("planning failed", "planning_failed"),
    ];
    KINDS.iter().find(|(k, _)| msg.contains(k)).map_or("other", |(_, v)| v)
}

/// One `source stats` endpoint entry: "ankr: req 297 failover 0 (lag 3) hedge 12 ewma 747ms COOLING".
fn parse_rpc_stat(entry: &str) -> Option<(String, [i64; 5], bool)> {
    let (name, rest) = entry.split_once(": ")?;
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let after = |k: &str| toks.iter().position(|t| *t == k).and_then(|i| toks.get(i + 1)).map(|v| v.trim_end_matches("ms").trim_end_matches(')'));
    let get = |k: &str| after(k).and_then(|v| v.parse::<i64>().ok());
    let lag = toks.iter().position(|t| *t == "(lag").and_then(|i| toks.get(i + 1)).and_then(|v| v.trim_end_matches(')').parse().ok());
    Some((name.trim_matches('"').to_string(), [get("req")?, get("failover").unwrap_or(0), lag.unwrap_or(0), get("hedge").unwrap_or(0), get("ewma").unwrap_or(0)], rest.contains("COOLING")))
}

/// Ingests new log lines; returns (lines, earliest block time ingested) for hour_stats.
fn ingest_log(db: &mut Connection, path: &str) -> Result<(usize, Option<f64>)> {
    let (text, _) = read_new(db, path, "log_offset")?;
    let tx = db.transaction()?;
    let mut first: Option<f64> = None;
    let mut n = 0;
    {
        let mut ins_block = tx.prepare("INSERT OR REPLACE INTO blocks (block, ts, seen, fetch_ms, decide_ms, head_age_ms, planned) VALUES (?1,?2,?3,?4,?5,?6,?7)")?;
        let mut ins_event = tx.prepare("INSERT INTO events (ts, level, kind, endpoint, message) VALUES (?1,?2,?3,?4,?5)")?;
        let mut ins_stat = tx.prepare("INSERT OR REPLACE INTO rpc_stats (ts, endpoint, requests, failovers, lagging, hedges, ewma_ms, cooling) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)")?;
        for raw in text.lines() {
            n += 1;
            let line = strip_ansi(raw);
            let Some((ts_s, rest)) = line.split_once(' ') else { continue };
            let Some(ts) = parse_ts(ts_s) else { continue };
            let rest = rest.trim_start();
            let (level, msg) = rest.split_once(' ').unwrap_or((rest, ""));
            if level == "INFO" {
                if msg.starts_with("bot: block block=") {
                    let (Some(b), Some(age)) = (num(&line, "block"), num(&line, "head_age_ms")) else { continue };
                    let bts = ts - age as f64 / 1000.0;
                    first = Some(first.map_or(bts, |f: f64| f.min(bts)));
                    ins_block.execute(params![b, bts, ts, num(&line, "fetch_ms"), num(&line, "decide_ms"), age, num(&line, "planned")])?;
                } else if msg.contains("rpc endpoints (urls hidden)") {
                    tx.execute("INSERT OR IGNORE INTO runs (ts) VALUES (?1)", params![ts])?;
                } else if msg.contains("source stats") {
                    if let Some(list) = msg.split_once("rpc=[").map(|x| x.1.trim_end_matches(']')) {
                        for entry in list.split("\", \"") {
                            if let Some((name, v, cooling)) = parse_rpc_stat(entry.trim_matches('"')) {
                                ins_stat.execute(params![ts, name, v[0], v[1], v[2], v[3], v[4], cooling])?;
                            }
                        }
                    }
                }
            } else if level == "WARN" || level == "ERROR" {
                let body = msg.split_once(": ").map_or(msg, |x| x.1);
                ins_event.execute(params![ts, level, event_kind(body), field(&line, "endpoint"), scrub(body).chars().take(300).collect::<String>()])?;
            }
        }
    }
    commit_offset(&tx, "log_offset")?;
    tx.commit()?;
    Ok((n, first))
}

fn percentile(v: &mut [i64], p: f64) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[((v.len() - 1) as f64 * p).round() as usize])
}

/// Recomputes the hourly aggregates of every hour from `from` (block time) on.
fn refresh_hour_stats(db: &Connection, from: Option<f64>) -> Result<()> {
    let Some(from) = from else { return Ok(()) };
    let start = (from / 3600.0).floor() as i64 * 3600;
    let mut q = db.prepare("SELECT block, ts, fetch_ms, decide_ms, head_age_ms FROM blocks WHERE ts >= ?1 ORDER BY block")?;
    // A gap counts in the hour of the block after it, also when it straddles an hour boundary
    // (seeded with the last block before `start`); not when the bot was restarted in between.
    let mut prev: Option<(i64, f64)> = db.query_row("SELECT block, ts FROM blocks WHERE ts < ?1 ORDER BY block DESC LIMIT 1", params![start as f64], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let runs: Vec<f64> = db.prepare("SELECT ts FROM runs")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    // hour -> (blocks, fetch_ms, decide_ms, head_age_ms, missing)
    let mut hours: HashMap<i64, (i64, Vec<i64>, Vec<i64>, Vec<i64>, i64)> = HashMap::new();
    let rows = q.query_map(params![start as f64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?)))?;
    for row in rows {
        let (b, ts, f, d, a) = row?;
        let h = hours.entry((ts / 3600.0).floor() as i64 * 3600).or_default();
        h.0 += 1;
        h.1.extend(f);
        h.2.extend(d);
        h.3.extend(a);
        if let Some((pb, pts)) = prev {
            if !runs.iter().any(|r| *r > pts && *r <= ts) {
                h.4 += (b - pb - 1).max(0);
            }
        }
        prev = Some((b, ts));
    }
    for (hour, (blocks, mut f, mut d, mut a, missing)) in hours {
        db.execute(
            "INSERT OR REPLACE INTO hour_stats (hour, blocks, missing, head_age_p50, head_age_p90, fetch_p50, fetch_p90, decide_p50, decide_p90) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![hour, blocks, missing, percentile(&mut a, 0.5), percentile(&mut a, 0.9), percentile(&mut f, 0.5), percentile(&mut f, 0.9), percentile(&mut d, 0.5), percentile(&mut d, 0.9)],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------- journal

fn ingest_journal(db: &mut Connection, path: &str) -> Result<usize> {
    let (text, _) = read_new(db, path, "journal_offset")?;
    let tx = db.transaction()?;
    let mut n = 0;
    for line in text.lines() {
        let Ok(r) = serde_json::from_str::<Value>(line) else { continue };
        n += 1;
        let s = |k: &str| r[k].as_str().map(str::to_string);
        let f = |k: &str| r[k].as_f64();
        let i = |k: &str| r[k].as_i64();
        match r["kind"].as_str().unwrap_or("") {
            "liquidation" => {
                tx.execute(
                    "INSERT OR IGNORE INTO liquidations (tx, borrower, block, market, liquidator, eoa, gross_usd, repaid, seized_value, bad_debt, fee_usd, outcome, json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![s("tx"), s("borrower"), i("block"), s("market"), s("liquidator"), s("eoa"), f("gross_bonus"), f("repaid"), f("seized_value"), f("bad_debt"), f("fee_usd"), s("outcome"), line],
                )?;
            }
            "atlas_auction" => {
                tx.execute(
                    "INSERT OR IGNORE INTO atlas_auctions (auction_id, user_op_hash, head, deadline, aggregator, liquidatable, bonus_usd, bundle_net_usd, net_usd, bid_usd, bid_basis, eth_usd, bundle_ms, value_ms, queued_ms, skipped, json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                    params![
                        s("auction_id"), s("user_op_hash").map(|h| h.to_lowercase()), i("head"), i("deadline"), s("aggregator"), i("liquidatable"), f("bonus_usd"), f("bundle_net_usd"), f("net_usd"), f("bid_usd"),
                        s("bid_basis").or_else(|| Some("per_position_sum".into())), f("eth_usd"), i("bundle_ms"), i("value_ms"), i("queued_ms"), s("skipped"), line
                    ],
                )?;
            }
            "rpc_usage" => {
                for u in r["usage"].as_array().into_iter().flatten() {
                    tx.execute(
                        "INSERT OR IGNORE INTO rpc_usage (block, uptime_s, pool, endpoint, method, n) VALUES (?1,?2,?3,?4,?5,?6)",
                        params![i("block"), i("uptime_s"), u["pool"].as_str(), u["endpoint"].as_str(), u["method"].as_str(), u["n"].as_i64()],
                    )?;
                }
            }
            "sent" => {
                tx.execute("INSERT OR IGNORE INTO sends (tx, block, path, json) VALUES (?1,?2,?3,?4)", params![r["tx"]["hash"].as_str(), i("block"), s("path"), line])?;
            }
            "outcome" => {
                let o = &r["outcome"];
                let wei = o["gas_used"].as_f64().unwrap_or(0.0) * o["effective_gas_price"].as_f64().unwrap_or(0.0) + o["l1_fee_wei"].as_f64().unwrap_or(0.0);
                tx.execute(
                    "INSERT OR IGNORE INTO outcomes (tx, block, result, success, gas_cost_eth, json) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![r["tx"]["hash"].as_str(), i("block"), o["result"].as_str(), o["success"].as_bool(), wei / 1e18, line],
                )?;
            }
            kind => {
                tx.execute("INSERT INTO journal_other (kind, block, json) VALUES (?1,?2,?3)", params![kind, i("block"), line])?;
            }
        }
    }
    commit_offset(&tx, "journal_offset")?;
    tx.commit()?;
    Ok(n)
}

// ---------------------------------------------------------------- RPC lookups

sol! {
    struct UserOperation { address from; address to; uint256 value; uint256 gas; uint256 maxFeePerGas; uint256 nonce; uint256 deadline; address dapp; address control; uint32 callConfig; uint32 dappGasLimit; uint32 solverGasLimit; uint24 bundlerSurchargeRate; address sessionKey; bytes data; bytes signature; }
    struct SolverOperation { address from; address to; uint256 value; uint256 gas; uint256 maxFeePerGas; uint256 deadline; address solver; address control; bytes32 userOpHash; address bidToken; uint256 bidAmount; bytes data; bytes signature; }
    struct DAppOperation { address from; address to; uint256 nonce; uint256 deadline; address control; address bundler; bytes32 userOpHash; bytes32 callChainHash; bytes signature; }
    function metacall(UserOperation userOp, SolverOperation[] solverOps, DAppOperation dAppOp, address gasRefundBeneficiary);
    event SolverTxResult(address indexed solverTo, address indexed solverFrom, address indexed dAppControl, address bidToken, uint256 bidAmount, bool executed, bool success, uint256 result);
}

/// Atlas v1.6.4 on Base.
const ATLAS: Address = address!("583dcFef0D240DC80753F0F0B26513feE27D9B77");

async fn rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> Result<Value> {
    let resp: Value = http.post(url).json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })).send().await.map_err(|_| anyhow::anyhow!("{method}: transport error"))?.json().await?;
    if let Some(e) = resp.get("error") {
        anyhow::bail!("{method}: {}", e["message"].as_str().unwrap_or("error"));
    }
    Ok(resp["result"].clone())
}

/// Bids and winner of one Atlas metacall: (userOpHash, winner, winning bid in ETH, all bids).
fn decode_metacall(input: &Bytes, receipt: &Value) -> Option<(B256, Option<Address>, Option<f64>, Vec<(Address, f64)>)> {
    let mc = metacallCall::abi_decode(input).ok()?;
    let uoh = mc.solverOps.first()?.userOpHash;
    let bids = mc.solverOps.iter().map(|s| (s.solver, s.bidAmount.to_string().parse::<f64>().unwrap_or(0.0) / 1e18)).collect();
    let mut winner = None;
    for l in receipt["logs"].as_array().into_iter().flatten() {
        let t: Vec<B256> = l["topics"].as_array().into_iter().flatten().filter_map(|x| x.as_str()?.parse().ok()).collect();
        if t.first() != Some(&SolverTxResult::SIGNATURE_HASH) {
            continue;
        }
        let data: Bytes = l["data"].as_str().unwrap_or("0x").parse().ok()?;
        if let Ok(e) = SolverTxResult::decode_raw_log(t.iter().copied(), &data) {
            if e.success {
                winner = Some((e.solverTo, e.bidAmount.to_string().parse::<f64>().unwrap_or(0.0) / 1e18));
                break;
            }
        }
    }
    Some((uoh, winner.map(|w| w.0), winner.map(|w| w.1), bids))
}

/// Tx + receipt of each journaled liquidation tx not fetched yet (oldest first, LOOKUPS_PER_PASS
/// per pass), stored verbatim in liq_raw. A tx the RPC does not serve is set aside with backoff
/// (liq_fetch_fail) so it cannot hold up the ones behind it; 3 failures in a row end the pass
/// (the endpoint itself is likely down).
async fn fetch_liq_raw(db: &Connection, http: &reqwest::Client, url: &str) -> Result<()> {
    let todo: Vec<(String, i64)> = {
        let mut q = db.prepare(
            "SELECT DISTINCT l.tx, l.block FROM liquidations l LEFT JOIN liq_raw r ON r.tx = l.tx LEFT JOIN liq_fetch_fail f ON f.tx = l.tx
             WHERE r.tx IS NULL AND l.tx IS NOT NULL AND (f.tx IS NULL OR (f.tries < ?2 AND f.last + ?3 * f.tries < ?4)) ORDER BY l.block LIMIT ?1",
        )?;
        q.query_map(params![LOOKUPS_PER_PASS as i64, FETCH_TRIES, FETCH_RETRY_SECS, now()], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let mut failed_in_row = 0;
    for (hash, block) in todo {
        let tx = rpc(http, url, "eth_getTransactionByHash", json!([hash])).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let receipt = rpc(http, url, "eth_getTransactionReceipt", json!([hash])).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        match (tx, receipt) {
            (Ok(tx), Ok(receipt)) if !tx.is_null() && !receipt.is_null() => {
                failed_in_row = 0;
                db.execute("INSERT OR REPLACE INTO liq_raw (tx, block, tx_json, receipt_json, fetched) VALUES (?1,?2,?3,?4,?5)", params![hash, block, tx.to_string(), receipt.to_string(), now()])?;
                db.execute("DELETE FROM liq_fetch_fail WHERE tx = ?1", params![hash])?;
            }
            (tx, receipt) => {
                let why = match (tx, receipt) {
                    (Err(e), _) | (_, Err(e)) => scrub(&e.to_string()),
                    _ => "not served (null)".to_string(),
                };
                db.execute(
                    "INSERT INTO liq_fetch_fail (tx, tries, last, error) VALUES (?1, 1, ?2, ?3)
                     ON CONFLICT(tx) DO UPDATE SET tries = tries + 1, last = ?2, error = ?3",
                    params![hash, now(), why.chars().take(200).collect::<String>()],
                )?;
                failed_in_row += 1;
                if failed_in_row >= 3 {
                    bail!("3 liquidation tx lookups failed in a row (last: {why})");
                }
            }
        }
    }
    Ok(())
}

/// One eth_call; None on revert / empty.
async fn call(http: &reqwest::Client, url: &str, to: Address, selector: &str) -> Option<Bytes> {
    let r = rpc(http, url, "eth_call", json!([{ "to": to, "data": selector }, "latest"])).await.ok()?;
    let b: Bytes = r.as_str()?.parse().ok()?;
    (!b.is_empty()).then_some(b)
}

/// Cached factory/token metadata for what a tx's swaps and transfers touch (looked up once each).
async fn ensure_meta(db: &Connection, http: &reqwest::Client, url: &str, cfg: &Config, raw: &exits::Raw) -> Result<(HashMap<Address, exits::PoolMeta>, HashMap<Address, exits::TokenMeta>)> {
    let (pools, tokens) = exits::needs(raw);
    let factories: HashMap<Address, String> = cfg.router.factories.iter().map(|f| (f.address, f.name.clone())).chain(exits::EXTRA_FACTORIES.iter().map(|(a, n)| (*a, n.to_string())))
        .chain(cfg.dashboard.factories.iter().map(|f| (f.address, f.name.clone())))
        .collect();
    let mut pool_meta = HashMap::new();
    for p in pools {
        let key = format!("{p:#x}");
        let cached: Option<String> = db.query_row("SELECT protocol FROM pool_meta WHERE pool = ?1", params![key], |r| r.get(0)).optional()?;
        let protocol = match cached {
            Some(p) => p,
            None => {
                // factory() / token0() / token1()
                let factory = call(http, url, p, "0xc45a0155").await.filter(|b| b.len() >= 32).map(|b| Address::from_slice(&b[12..32]));
                let t0 = call(http, url, p, "0x0dfe1681").await.filter(|b| b.len() >= 32).map(|b| Address::from_slice(&b[12..32]));
                let t1 = call(http, url, p, "0xd21220a7").await.filter(|b| b.len() >= 32).map(|b| Address::from_slice(&b[12..32]));
                let protocol = factory.and_then(|f| factories.get(&f).cloned()).unwrap_or_default();
                if factory.is_none() {
                    // Not a pool with factory() (or the lookup failed): not cached, asked again later.
                    pool_meta.insert(p, exits::PoolMeta { protocol });
                    continue;
                }
                db.execute(
                    "INSERT OR REPLACE INTO pool_meta (pool, protocol, factory, token0, token1) VALUES (?1,?2,?3,?4,?5)",
                    params![key, protocol, factory.map(|f| format!("{f:#x}")), t0.map(|t| format!("{t:#x}")), t1.map(|t| format!("{t:#x}"))],
                )?;
                protocol
            }
        };
        pool_meta.insert(p, exits::PoolMeta { protocol });
    }
    let mut token_meta = HashMap::new();
    for t in tokens {
        let key = format!("{t:#x}");
        let cached: Option<(String, i64)> = db.query_row("SELECT symbol, decimals FROM token_meta WHERE token = ?1", params![key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let (symbol, decimals) = match cached {
            Some(c) => c,
            None => {
                // symbol() (string, or bytes32 on old tokens) / decimals()
                let symbol = call(http, url, t, "0x95d89b41")
                    .await
                    .map(|b| {
                        let s = if b.len() >= 96 { String::from_utf8_lossy(&b[64..64 + (b[63] as usize).min(32)]).into_owned() } else { String::from_utf8_lossy(&b).into_owned() };
                        s.trim_matches(char::from(0)).to_string()
                    })
                    .unwrap_or_else(|| key[..8].to_string());
                let decimals = call(http, url, t, "0x313ce567").await.filter(|b| b.len() >= 32).map(|b| b[31] as i64);
                // A failed decimals() lookup (RPC error) must not be cached as 18 for good.
                if let Some(d) = decimals {
                    db.execute("INSERT OR REPLACE INTO token_meta (token, symbol, decimals) VALUES (?1,?2,?3)", params![key, symbol, d])?;
                }
                (symbol, decimals.unwrap_or(18))
            }
        };
        token_meta.insert(t, exits::TokenMeta { symbol, decimals: decimals as u8 });
    }
    Ok((pool_meta, token_meta))
}

/// Decodes stored txs that have no current-version analysis: Atlas bids/winner (liq_txs) and the
/// entry/exit path (liq_exits). Only metadata lookups use RPC.
async fn analyze_liq_txs(db: &Connection, http: &reqwest::Client, url: &str, cfg: &Config) -> Result<()> {
    let todo: Vec<(String, i64, String, String)> = {
        let mut q = db.prepare(
            "SELECT r.tx, r.block, r.tx_json, r.receipt_json FROM liq_raw r LEFT JOIN liq_exits e ON e.tx = r.tx
             LEFT JOIN liq_unparsed u ON u.tx = r.tx AND u.version >= ?1
             WHERE (e.tx IS NULL OR e.version < ?1) AND u.tx IS NULL ORDER BY r.block LIMIT 200",
        )?;
        q.query_map(params![exits::DECODER_VERSION], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let contracts: Vec<(Address, String, String)> = cfg.dashboard.contracts.iter().map(|c| (c.address, c.name.clone(), c.kind.clone())).collect();
    for (hash, block, tx_json, rc_json) in todo {
        let (tx, rc): (Value, Value) = (serde_json::from_str(&tx_json)?, serde_json::from_str(&rc_json)?);
        let to: Option<Address> = tx["to"].as_str().and_then(|s| s.parse().ok());
        // Atlas: bids and winner.
        if to == Some(ATLAS) {
            let input: Bytes = tx["input"].as_str().unwrap_or("0x").parse().unwrap_or_default();
            match decode_metacall(&input, &rc) {
                Some((uoh, winner, bid, bids)) => {
                    let bids = json!(bids.iter().map(|(s, b)| json!({ "solver": s, "bid_eth": b })).collect::<Vec<_>>()).to_string();
                    db.execute(
                        "INSERT OR REPLACE INTO liq_txs (tx, block, is_atlas, user_op_hash, winner, win_bid_eth, bids) VALUES (?1,?2,1,?3,?4,?5,?6)",
                        params![hash, block, format!("{uoh:#x}"), winner.map(|w| format!("{w:#x}")), bid, bids],
                    )?;
                }
                None => {
                    db.execute("INSERT OR REPLACE INTO liq_txs (tx, block, is_atlas) VALUES (?1, ?2, 1)", params![hash, block])?;
                }
            }
        } else {
            db.execute("INSERT OR REPLACE INTO liq_txs (tx, block, is_atlas) VALUES (?1, ?2, 0)", params![hash, block])?;
        }
        // Entry / exit path.
        let Some(raw) = exits::parse(&tx, &rc) else {
            // Recorded so it is not picked again every pass (it would crowd out newer txs).
            db.execute("INSERT OR REPLACE INTO liq_unparsed (tx, version) VALUES (?1, ?2)", params![hash, exits::DECODER_VERSION])?;
            continue;
        };
        let (pools, tokens) = ensure_meta(db, http, url, cfg, &raw).await?;
        let rows: Vec<(String, Option<f64>)> = {
            let mut q = db.prepare("SELECT market, sum(seized_value) FROM liquidations WHERE tx = ?1 GROUP BY market")?;
            q.query_map(params![hash], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<f64>>(1)?)))?.filter_map(|r| r.ok()).collect()
        };
        let markets: Vec<String> = rows.iter().map(|(m, _)| m.clone()).collect();
        let seized_usd: HashMap<String, f64> = rows.iter().filter_map(|(m, v)| Some((m.split('/').next()?.to_string(), (*v)?))).collect();
        let e = exits::decode(&raw, ATLAS, &pools, &tokens, &markets, &seized_usd, &contracts);
        let fmt = |a: Option<Address>| a.map(|a| format!("{a:#x}"));
        db.execute(
            "INSERT OR REPLACE INTO liq_exits (tx, block, actor, eoa, via, n_liq, flash, exit_kind, route, venues, collateral, seized, sold, kept, carried, loan_in, oracle_usd, slippage_bps, unknown, gas_used, gas_price_gwei, priority_gwei, version)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
            params![
                hash, block, fmt(e.actor), fmt(e.eoa), e.via, e.n_liq as i64, e.flash.join(","), e.kind, e.route, e.venues_json(), e.collateral.join(","), e.seized, e.sold, e.kept, e.carried, e.loan_in,
                e.oracle_usd, e.slippage_bps, json!(e.unknown_counterparties).to_string(), e.gas_used as i64, e.gas_price_gwei, e.priority_gwei, exits::DECODER_VERSION
            ],
        )?;
    }
    Ok(())
}

async fn record_wallet(db: &Connection, http: &reqwest::Client, url: &str, addr: Address) -> Result<()> {
    let last: Option<f64> = db.query_row("SELECT max(ts) FROM wallet", [], |r| r.get(0))?;
    if last.is_some_and(|t| now() - t < WALLET_EVERY_SECS) {
        return Ok(());
    }
    let bal = rpc(http, url, "eth_getBalance", json!([addr, "latest"])).await?;
    let wei = u128::from_str_radix(bal.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).unwrap_or(0);
    db.execute("INSERT OR REPLACE INTO wallet (ts, address, eth) VALUES (?1, ?2, ?3)", params![now(), format!("{addr:#x}"), wei as f64 / 1e18])?;
    Ok(())
}

// ---------------------------------------------------------------- backup

/// Once a (UTC) day: a consistent copy of the database (VACUUM INTO) and the journal, keeping
/// the last BACKUPS_KEPT of each.
fn backup(db: &Connection, dir: &str, journal: &str) -> Result<()> {
    let day = (now() / 86_400.0).floor() as i64;
    let (y, m, d) = civil_from_days(day);
    let tag = format!("{y:04}{m:02}{d:02}");
    let done: Option<String> = db.query_row("SELECT value FROM meta WHERE key = 'backup_day'", [], |r| r.get(0)).optional()?;
    if done.as_deref() == Some(tag.as_str()) {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    let db_copy = format!("{dir}/metrics-{tag}.db");
    let _ = std::fs::remove_file(&db_copy);
    db.execute("VACUUM INTO ?1", params![db_copy])?;
    std::fs::copy(journal, format!("{dir}/journal-{tag}.jsonl")).with_context(|| format!("copying {journal}"))?;
    for prefix in ["metrics-", "journal-"] {
        let mut files: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(prefix)).collect();
        files.sort();
        for old in files.iter().rev().skip(BACKUPS_KEPT) {
            let _ = std::fs::remove_file(format!("{dir}/{old}"));
        }
    }
    db.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('backup_day', ?1)", params![tag])?;
    info!(dir, "dashboard backup written");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip() {
        // Block 52785758 has timestamp 1792360863 (journal watch row): 2026-10-18T22:01:03Z.
        assert_eq!(parse_ts("2026-10-18T22:01:03Z"), Some(1_792_360_863.0));
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_ts("2026-10-19T10:58:29.5Z"), Some(1_792_407_509.5));
        for z in [-1000, 0, 20_000, 20_745] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn log_line_fields() {
        let l = "2026-10-19T10:58:29.569583Z  INFO bot: block block=52809081 tracked=31171 touched=0 planned=0 fetch_ms=339 reload_ms=0 eval_us=33885 decide_ms=373 head_age_ms=569";
        assert_eq!(num(l, "block"), Some(52_809_081));
        assert_eq!(num(l, "head_age_ms"), Some(569));
        assert_eq!(num(l, "decide_ms"), Some(373));
        let w = r#"2026-10-19T10:31:38Z  WARN bot::rpc: rpc failover endpoint=tenderly-public why="rate limit exceeded" cooldown_s=2"#;
        assert_eq!(field(w, "endpoint"), Some("tenderly-public"));
        assert_eq!(field(w, "why"), Some("rate limit exceeded"));
        assert_eq!(event_kind("head fetch failed error=x"), "head_fetch_failed");
        assert_eq!(event_kind("fetch failed block=1"), "fetch_failed");
        assert_eq!(scrub("transport error https://base.example/v2/KEY rest"), "transport error <url> rest");
        assert_eq!(strip_ansi("\u{1b}[2m2026\u{1b}[0m x"), "2026 x");
    }

    #[test]
    fn rpc_stat_entries() {
        let (n, v, c) = parse_rpc_stat("ankr: req 297 failover 4 (lag 3) hedge 12 ewma 747ms").unwrap();
        assert_eq!((n.as_str(), v, c), ("ankr", [297, 4, 3, 12, 747], false));
        let (n, v, c) = parse_rpc_stat("tenderly-public: req 2474 failover 9 hedge 26 ewma 274ms COOLING").unwrap();
        assert_eq!((n.as_str(), v, c), ("tenderly-public", [2474, 9, 0, 26, 274], true));
    }

    #[test]
    fn ingest_is_incremental_and_whole_line() {
        let dir = std::env::temp_dir().join(format!("dash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("run.log");
        let a = "2026-10-19T10:00:00.500Z  INFO bot: block block=100 tracked=1 touched=0 planned=0 fetch_ms=300 reload_ms=0 eval_us=1 decide_ms=310 head_age_ms=500\n";
        let b = "2026-10-19T10:00:02.400Z  INFO bot: block block=101 tracked=1 touched=0 planned=0 fetch_ms=320 reload_ms=0 eval_us=1 decide_ms=330 head_age_ms=400\n";
        std::fs::write(&log, format!("{a}{}", &b[..40])).unwrap(); // second line half-written
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        let p = log.to_str().unwrap();
        assert_eq!(ingest_log(&mut db, p).unwrap().0, 1);
        std::fs::write(&log, format!("{a}{b}")).unwrap();
        assert_eq!(ingest_log(&mut db, p).unwrap().0, 1, "only the completed line is new");
        let (n, ts): (i64, f64) = db.query_row("SELECT count(*), min(ts) FROM blocks", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(n, 2);
        assert_eq!(ts, parse_ts("2026-10-19T10:00:00Z").unwrap(), "block time = seen - head_age");
        std::fs::write(&log, a).unwrap(); // replaced by a shorter file: start over
        assert_eq!(ingest_log(&mut db, p).unwrap().0, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missed_blocks_count_across_hours_but_not_across_restarts() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        let add = |b: i64, ts: f64| db.execute("INSERT INTO blocks (block, ts, seen) VALUES (?1, ?2, ?2)", params![b, ts]).unwrap();
        // Hour 0: blocks 1-2; hour 1: 6-7 (3, 4, 5 missed across the boundary); a restart, then 20.
        add(1, 3590.0);
        add(2, 3592.0);
        add(6, 3602.0);
        add(7, 3604.0);
        db.execute("INSERT INTO runs (ts) VALUES (3700.0)", []).unwrap();
        add(20, 3730.0);
        refresh_hour_stats(&db, Some(0.0)).unwrap();
        let missing = |h: i64| db.query_row("SELECT missing FROM hour_stats WHERE hour = ?1", params![h], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(missing(0), 0);
        assert_eq!(missing(3600), 3, "the gap straddling the hour boundary; the restart gap is not counted");
        // A later refresh that starts mid-way is seeded with the block before it.
        refresh_hour_stats(&db, Some(3601.0)).unwrap();
        assert_eq!(missing(3600), 3);
    }
}
