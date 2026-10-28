//! dashboard.html = template.html + one JSON blob from the SQLite history (last DATA_DAYS days).
//! All presentation lives in the template (KPI cards, linked charts, global filters, drill-down,
//! expandable detail rows): filters re-render every view in the browser, no server needed.
//! Rows are compact arrays (keys documented per field below); times are unix seconds.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use super::{civil_from_days, now};
use crate::config::Config;

const TEMPLATE: &str = include_str!("template.html");
/// History shipped to the page (the longest time filter).
const DATA_DAYS: f64 = 90.0;
/// Display fallback for blocks no longer in `blocks`: Base makes a block every 2 s until Denim
/// (~2026-11, 200 ms blocks); per-block rows give exact times while they are kept.
const BLOCK_SECS: f64 = 2.0;
/// Display time zone for the page and its daily buckets, in seconds east of UTC: env
/// `BOT_DASH_TZ_HOURS` (e.g. "8" or "-5.5"), UTC when unset or unparsable.
fn tz_offset() -> f64 {
    std::env::var("BOT_DASH_TZ_HOURS").ok().and_then(|h| h.trim().parse::<f64>().ok()).filter(|h| h.abs() <= 14.0).unwrap_or(0.0) * 3600.0
}

/// Block time: exact while the per-block row exists, else extrapolated from the newest block.
struct Clock {
    ref_block: i64,
    ref_ts: f64,
}

impl Clock {
    fn new(db: &Connection) -> Result<Self> {
        let r: Option<(i64, f64)> = db.query_row("SELECT block, ts FROM blocks ORDER BY block DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let (ref_block, ref_ts) = r.unwrap_or((0, now()));
        Ok(Self { ref_block, ref_ts })
    }
    fn ts(&self, db: &Connection, block: i64) -> f64 {
        db.query_row("SELECT ts FROM blocks WHERE block = ?1", params![block], |r| r.get(0)).unwrap_or_else(|_| self.estimate(block))
    }
    fn estimate(&self, block: i64) -> f64 {
        self.ref_ts + (block - self.ref_block) as f64 * BLOCK_SECS
    }
    /// First block at or after `ts` (estimated).
    fn block_at(&self, ts: f64) -> i64 {
        self.ref_block - ((self.ref_ts - ts) / BLOCK_SECS) as i64
    }
}

pub fn page(db: &Connection, cfg: &Config) -> Result<String> {
    let clock = Clock::new(db)?;
    let since = now() - DATA_DAYS * 86_400.0;
    let eth_usd: f64 = db.query_row("SELECT eth_usd FROM atlas_auctions WHERE eth_usd > 0 ORDER BY head DESC LIMIT 1", [], |r| r.get(0)).optional()?.unwrap_or(cfg.exec.eth_usd_fallback);
    let data = json!({
        "meta": meta(db, cfg, eth_usd)?,
        "hours": hours(db, since)?,
        "events": events(db, since)?,
        "liq": liquidations(db, &clock, since)?,
        "auctions": auctions(db, &clock, since, eth_usd)?,
        "auction_days": auction_days(db, &clock, since)?,
        "paper": paper(db, &clock, since, cfg)?,
        "exits": exits(db, &clock, since)?,
        "rpc": rpc(db, &clock)?,
    });
    Ok(TEMPLATE.replace("/*__DATA__*/null", &data.to_string().replace("</", "<\\/")))
}

fn opt_f64(db: &Connection, sql: &str, p: impl rusqlite::Params) -> Option<f64> {
    db.query_row(sql, p, |r| r.get::<_, Option<f64>>(0)).ok().flatten()
}

fn pctl(mut v: Vec<i64>, p: f64) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[((v.len() - 1) as f64 * p).round() as usize])
}

/// Status, freshness, wallet, gas, config flags.
fn meta(db: &Connection, cfg: &Config, eth_usd: f64) -> Result<Value> {
    let t = now();
    let last: Option<(i64, f64)> = db.query_row("SELECT block, seen FROM blocks ORDER BY block DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let mut q = db.prepare("SELECT head_age_ms, fetch_ms, decide_ms FROM blocks WHERE seen >= ?1")?;
    let (mut a, mut f, mut d) = (vec![], vec![], vec![]);
    for r in q.query_map(params![t - 3600.0], |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?)))? {
        let (x, y, z) = r?;
        a.extend(x);
        f.extend(y);
        d.extend(z);
    }
    let mut ws = db.prepare("SELECT ts, eth FROM wallet WHERE ts >= ?1 ORDER BY ts")?;
    let wallet: Vec<(f64, f64)> = ws.query_map(params![t - 30.0 * 86_400.0], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let utc_midnight = (t / 86_400.0).floor() * 86_400.0;
    let gas_eth = opt_f64(db, "SELECT sum(o.gas_cost_eth) FROM outcomes o JOIN blocks b ON b.block = o.block WHERE b.ts >= ?1", params![utc_midnight]).unwrap_or(0.0);
    let (sent, ok, failed): (i64, i64, i64) = db.query_row(
        "SELECT (SELECT count(*) FROM sends), (SELECT count(*) FROM outcomes WHERE success = 1), (SELECT count(*) FROM outcomes WHERE result != 'included' OR success = 0)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(json!({
        "generated_at": t,
        "tz_offset": tz_offset(),
        "last_block": last.map(|(b, s)| json!({ "block": b, "seen": s })),
        "run_start": opt_f64(db, "SELECT max(ts) FROM runs", []),
        "last_hour": { "age50": pctl(a.clone(), 0.5), "age90": pctl(a, 0.9), "fetch50": pctl(f.clone(), 0.5), "fetch90": pctl(f, 0.9), "decide50": pctl(d.clone(), 0.5), "decide90": pctl(d, 0.9) },
        "wallet": wallet.iter().map(|(ts, e)| json!([ts, e])).collect::<Vec<_>>(),
        "eth_usd": eth_usd,
        "gas_today_usd": gas_eth * eth_usd,
        "gas_budget_usd": cfg.exec.daily_gas_budget_usd,
        "sends": { "sent": sent, "ok": ok, "failed": failed },
        "mode": cfg.exec.mode,
        "fb_send": cfg.flashblocks.send,
        "atlas_observe": cfg.atlas.observe,
        "kill_switch": std::path::Path::new(&cfg.exec.kill_switch_file).exists(),
        "markets": cfg.morpho.markets.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        "db_bytes": std::fs::metadata(&cfg.dashboard.db_path).map(|m| m.len()).unwrap_or(0),
    }))
}

/// [hour, blocks, missing, age50, age90, fetch50, fetch90, decide50, decide90]
fn hours(db: &Connection, since: f64) -> Result<Vec<Value>> {
    let mut q = db.prepare("SELECT hour, blocks, missing, head_age_p50, head_age_p90, fetch_p50, fetch_p90, decide_p50, decide_p90 FROM hour_stats WHERE hour >= ?1 ORDER BY hour")?;
    let rows = q.query_map(params![since], |r| {
        Ok(json!([r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, Option<i64>>(6)?, r.get::<_, Option<i64>>(7)?, r.get::<_, Option<i64>>(8)?]))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// hourly: [hour, kind, endpoint, n]; recent (newest 400): [ts, level, kind, endpoint, message]
fn events(db: &Connection, since: f64) -> Result<Value> {
    let mut q = db.prepare("SELECT CAST(ts / 3600 AS INTEGER) * 3600, kind, coalesce(endpoint, ''), count(*) FROM events WHERE ts >= ?1 GROUP BY 1, 2, 3 ORDER BY 1")?;
    let hourly: Vec<Value> = q.query_map(params![since], |r| Ok(json!([r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?])))?.collect::<rusqlite::Result<_>>()?;
    let mut q = db.prepare("SELECT ts, level, kind, coalesce(endpoint, ''), message FROM events WHERE ts >= ?1 ORDER BY ts DESC LIMIT 400")?;
    let recent: Vec<Value> = q
        .query_map(params![since], |r| Ok(json!([r.get::<_, f64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?])))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(json!({ "hourly": hourly, "recent": recent }))
}

/// [ts, market, gross_usd, liquidator, outcome, via ("atlas" | "direct" | null = not looked up), tx, borrower]
fn liquidations(db: &Connection, clock: &Clock, since: f64) -> Result<Vec<Value>> {
    let mut q = db.prepare(
        "SELECT l.block, l.market, l.gross_usd, l.liquidator, l.outcome, t.is_atlas, l.tx, l.borrower
         FROM liquidations l LEFT JOIN liq_txs t ON t.tx = l.tx WHERE l.block >= ?1 ORDER BY l.block DESC",
    )?;
    let rows: Vec<(i64, Option<String>, Option<f64>, Option<String>, Option<String>, Option<bool>, Option<String>, Option<String>)> =
        q.query_map(params![clock.block_at(since)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .map(|(b, m, g, l, o, a, tx, who)| json!([clock.ts(db, b), m, g.unwrap_or(0.0), l, o, a.map(|a| if a { "atlas" } else { "direct" }), tx, who]))
        .collect())
}

/// Auctions with liquidatable positions, one per report (the sibling auction id of a matched one
/// dropped). Objects: the detail drawer shows bids and positions.
fn auctions(db: &Connection, clock: &Clock, since: f64, eth_usd_now: f64) -> Result<Vec<Value>> {
    let mut q = db.prepare(
        "SELECT a.auction_id, a.head, a.liquidatable, a.bonus_usd, a.bundle_net_usd, a.net_usd, a.bid_usd, a.bid_basis, a.eth_usd, a.bundle_ms, a.json,
                t.winner, t.win_bid_eth, t.tx, t.bids, (SELECT sum(gross_usd) FROM liquidations l WHERE l.tx = t.tx)
         FROM atlas_auctions a LEFT JOIN liq_txs t ON t.user_op_hash = a.user_op_hash
         WHERE a.liquidatable > 0 AND a.head >= ?1 ORDER BY a.head DESC",
    )?;
    let mut rows: Vec<Value> = vec![];
    let mut matched: std::collections::HashSet<(i64, i64)> = Default::default();
    let raw: Vec<_> = q
        .query_map(params![clock.block_at(since)], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                r.get::<_, Option<f64>>(4)?,
                r.get::<_, Option<f64>>(5)?,
                r.get::<_, Option<f64>>(6)?,
                r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                r.get::<_, Option<f64>>(8)?,
                r.get::<_, Option<i64>>(9)?,
                r.get::<_, String>(10)?,
                (r.get::<_, Option<String>>(11)?, r.get::<_, Option<f64>>(12)?, r.get::<_, Option<String>>(13)?, r.get::<_, Option<String>>(14)?, r.get::<_, Option<f64>>(15)?),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for r in &raw {
        if r.11.0.is_some() {
            matched.insert((r.1, r.3 as i64));
        }
    }
    for (id, head, n, bonus, bundle, net, bid, basis, eth, ms, js, (winner, win_eth, tx, bids, gross)) in raw {
        if winner.is_none() && matched.contains(&(head, bonus as i64)) {
            continue;
        }
        let j: Value = serde_json::from_str(&js).unwrap_or_default();
        let markets: Vec<Value> = j["markets"].as_array().into_iter().flatten().filter(|m| m["liquidatable"].as_i64().unwrap_or(0) > 0).cloned().collect();
        let eth = eth.unwrap_or(eth_usd_now);
        // Rows from before bundle valuation have no bundle value: their "net" is the per-position
        // sum (overstated).
        let ours = if basis == "bundle_net" { bundle } else { net };
        let bids: Vec<Value> = bids.and_then(|b| serde_json::from_str::<Vec<Value>>(&b).ok()).unwrap_or_default().into_iter().map(|b| json!({ "solver": b["solver"], "usd": b["bid_eth"].as_f64().unwrap_or(0.0) * eth })).collect();
        rows.push(json!({
            "id": id, "t": clock.ts(db, head), "n": n, "bonus": bonus, "gross": gross, "ours": ours, "basis": basis, "bid": bid,
            "win": win_eth.map(|w| w * eth), "winner": winner, "tx": tx, "ms": ms, "bids": bids,
            "markets": markets.iter().map(|m| json!({ "market": m["market"], "n": m["liquidatable"], "bonus": m["bonus_usd"], "bundle": m["bundle"] })).collect::<Vec<_>>(),
            "positions": j["positions"],
        }));
    }
    Ok(rows)
}

/// Paper trading of the solver: every auction whose on-chain winner was decoded, against bidding our
/// bundle net x (1 - m) for each atlas.paper_margins m. Won = our bid above the winning one (in ETH,
/// at the auction's ETH price); in time = our valuation done within atlas.paper_deadline_ms of the
/// auction. P&L of a win = bundle net - our bid (what our own exit would have left).
fn paper(db: &Connection, clock: &Clock, since: f64, cfg: &Config) -> Result<Value> {
    let mut q = db.prepare(
        "SELECT a.head, a.bundle_net_usd, a.eth_usd, a.value_ms, a.json, t.win_bid_eth, t.winner
         FROM atlas_auctions a JOIN liq_txs t ON t.user_op_hash = a.user_op_hash
         WHERE a.liquidatable > 0 AND a.bid_basis = 'bundle_net' AND t.win_bid_eth IS NOT NULL AND a.head >= ?1 ORDER BY a.head",
    )?;
    let rows: Vec<(i64, f64, f64, Option<i64>, String, f64, Option<String>)> = q
        .query_map(params![clock.block_at(since)], |r| Ok((r.get(0)?, r.get::<_, Option<f64>>(1)?.unwrap_or(0.0), r.get::<_, Option<f64>>(2)?.unwrap_or(0.0), r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let deadline = cfg.atlas.paper_deadline_ms as i64;
    let mut margins = vec![];
    for &m in &cfg.atlas.paper_margins {
        let (mut wins, mut pnl, mut wins_t, mut pnl_t, mut won_usd) = (0, 0.0, 0, 0.0, 0.0);
        for (_, net, eth, ms, _, win_eth, _) in &rows {
            let bid = net * (1.0 - m);
            if *eth <= 0.0 || bid / eth <= *win_eth {
                continue;
            }
            wins += 1;
            pnl += net - bid;
            won_usd += win_eth * eth;
            if ms.is_some_and(|ms| ms <= deadline) {
                wins_t += 1;
                pnl_t += net - bid;
            }
        }
        margins.push(json!({ "m": m, "wins": wins, "pnl": pnl, "wins_t": wins_t, "pnl_t": pnl_t, "won_usd": won_usd }));
    }
    // Per market and per day at the first margin of 1% or more (the realistic bid).
    let m1 = cfg.atlas.paper_margins.iter().copied().find(|m| *m >= 0.01).unwrap_or(0.01);
    let mut markets: std::collections::BTreeMap<String, (i64, i64, f64, f64, f64)> = Default::default();
    let mut days: std::collections::BTreeMap<i64, (i64, i64, f64)> = Default::default();
    for (head, net, eth, ms, js, win_eth, _) in &rows {
        let j: Value = serde_json::from_str(js).unwrap_or_default();
        let name = j["markets"].as_array().into_iter().flatten().filter(|m| m["liquidatable"].as_i64().unwrap_or(0) > 0).filter_map(|m| m["market"].as_str()).collect::<Vec<_>>().join("+");
        let win_usd = win_eth * eth;
        let won = *eth > 0.0 && net * (1.0 - m1) / eth > *win_eth && ms.is_some_and(|ms| ms <= deadline);
        let e = markets.entry(name).or_default();
        e.0 += 1;
        e.1 += i64::from(won);
        e.2 += if won { net * m1 } else { 0.0 };
        e.3 += net;
        e.4 += win_usd;
        let day = ((clock.ts(db, *head) + tz_offset()) / 86_400.0).floor() as i64;
        let d = days.entry(day).or_default();
        d.0 += 1;
        d.1 += i64::from(won);
        d.2 += if won { net * m1 } else { 0.0 };
    }
    let fast = rows.iter().filter(|r| r.3.is_some_and(|ms| ms <= deadline)).count();
    Ok(json!({
        "auctions": rows.len(), "in_time": fast, "deadline_ms": deadline, "m1": m1, "margins": margins,
        "markets": markets.iter().map(|(k, v)| json!({ "market": k, "n": v.0, "wins": v.1, "pnl": v.2, "ours": v.3, "win_usd": v.4 })).collect::<Vec<_>>(),
        "days": days.iter().map(|(d, v)| json!([d, v.0, v.1, v.2])).collect::<Vec<_>>(),
        "first_head": rows.first().map(|r| r.0),
    }))
}

/// [day (Taipei, days since epoch), valued, with liquidatable, skipped]
fn auction_days(db: &Connection, clock: &Clock, since: f64) -> Result<Vec<Value>> {
    let mut q = db.prepare(
        "SELECT CAST((?2 + (head - ?3) * ?4 + ?5) / 86400 AS INTEGER) d, count(*), sum(liquidatable > 0), sum(skipped IS NOT NULL)
         FROM atlas_auctions WHERE head >= ?1 GROUP BY d ORDER BY d",
    )?;
    let rows = q.query_map(params![clock.block_at(since), clock.ref_ts, clock.ref_block, BLOCK_SECS, tz_offset()], |r| Ok(json!([r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?])))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Decoded entry/exit per liquidation tx (objects; venues/unknown parsed for the drawer).
fn exits(db: &Connection, clock: &Clock, since: f64) -> Result<Vec<Value>> {
    let mut q = db.prepare(
        "SELECT e.block, e.tx, e.actor, e.eoa, e.via, e.n_liq, e.flash, e.exit_kind, e.route, e.venues, e.collateral, e.seized, e.sold, e.kept, e.carried, e.loan_in,
                e.oracle_usd, e.slippage_bps, e.unknown, e.gas_used, e.gas_price_gwei, e.priority_gwei,
                (SELECT sum(gross_usd) FROM liquidations l WHERE l.tx = e.tx), (SELECT group_concat(DISTINCT market) FROM liquidations l WHERE l.tx = e.tx)
         FROM liq_exits e WHERE e.block >= ?1 ORDER BY e.block DESC",
    )?;
    let rows = q.query_map(params![clock.block_at(since)], |r| {
        let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
        Ok(json!({
            "t": clock.ts(db, r.get::<_, i64>(0)?), "tx": r.get::<_, String>(1)?, "actor": r.get::<_, Option<String>>(2)?, "eoa": r.get::<_, Option<String>>(3)?,
            "via": r.get::<_, String>(4)?, "n": r.get::<_, i64>(5)?, "flash": r.get::<_, Option<String>>(6)?, "kind": r.get::<_, String>(7)?, "route": r.get::<_, Option<String>>(8)?,
            "venues": parse(r.get(9)?), "collateral": r.get::<_, Option<String>>(10)?, "seized": r.get::<_, Option<f64>>(11)?, "sold": r.get::<_, Option<f64>>(12)?,
            "kept": r.get::<_, Option<f64>>(13)?, "carried": r.get::<_, Option<f64>>(14)?, "loan_in": r.get::<_, Option<f64>>(15)?, "oracle_usd": r.get::<_, Option<f64>>(16)?,
            "slippage": r.get::<_, Option<f64>>(17)?, "unknown": parse(r.get(18)?), "gas": r.get::<_, i64>(19)?, "gas_price": r.get::<_, Option<f64>>(20)?,
            "prio": r.get::<_, Option<f64>>(21)?, "gross": r.get::<_, Option<f64>>(22)?,
            "markets": r.get::<_, Option<String>>(23)?.map(|m| m.split(',').map(String::from).collect::<Vec<_>>()).unwrap_or_default(),
        }))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Free-plan price list (estimates from the providers' published plans; check them, plans change):
/// unit name, monthly allowance (None = not published), cost per method (default last).
fn plan(endpoint: &str, days_in_month: f64) -> Option<(&'static str, Option<f64>, &'static [(&'static str, f64)])> {
    match endpoint {
        "alchemy" => Some((
            "CU",
            Some(30_000_000.0),
            &[("eth_call", 26.0), ("eth_getLogs", 60.0), ("eth_simulateV1", 40.0), ("eth_getBlockByNumber", 16.0), ("eth_blockNumber", 10.0), ("eth_getBalance", 19.0), ("eth_getTransactionReceipt", 15.0), ("eth_sendRawTransaction", 40.0), ("*", 26.0)],
        )),
        // 3M credits per day.
        "infura" => Some(("credits", Some(3_000_000.0 * days_in_month), &[("eth_getLogs", 255.0), ("eth_simulateV1", 300.0), ("*", 80.0)])),
        "ankr" => Some(("credits", Some(200_000_000.0), &[("*", 200.0)])),
        // 210M CU per 30 days.
        "drpc" => Some(("CU", Some(210_000_000.0 * days_in_month / 30.0), &[("*", 20.0)])),
        _ => None,
    }
}

fn cost(table: &[(&str, f64)], method: &str) -> f64 {
    table.iter().find(|(m, _)| *m == method).or_else(|| table.iter().find(|(m, _)| *m == "*")).map_or(0.0, |x| x.1)
}

/// Month-to-date usage per endpoint (vs free plan) + hourly requests (7 days) + latest stats.
fn rpc(db: &Connection, clock: &Clock) -> Result<Value> {
    let t = now();
    // Month boundaries in UTC (providers reset on UTC calendar months).
    let (y, m, _) = civil_from_days((t / 86_400.0).floor() as i64);
    let month_start = super::days_from_civil(y, m, 1) as f64 * 86_400.0;
    let next = if m == 12 { super::days_from_civil(y + 1, 1, 1) } else { super::days_from_civil(y, m + 1, 1) } as f64 * 86_400.0;
    let days_in_month = (next - month_start) / 86_400.0;
    let elapsed = ((t - month_start) / (next - month_start)).clamp(1e-6, 1.0);

    // rpc_usage holds cumulative counts per run; turn them into per-snapshot deltas.
    let mut q = db.prepare("SELECT block, uptime_s, pool, endpoint, method, n FROM rpc_usage ORDER BY block")?;
    let mut prev: HashMap<(String, String, String), (i64, i64)> = HashMap::new();
    let mut month: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut hourly: HashMap<(i64, String), f64> = HashMap::new();
    for r in q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?)))? {
        let (block, up, pool, ep, method, n) = r?;
        let key = (pool, ep.clone(), method.clone());
        let delta = match prev.get(&key) {
            Some(&(pu, pn)) if up >= pu && n >= pn => n - pn,
            _ => n, // first snapshot of a run
        };
        prev.insert(key, (up, n));
        let ts = clock.estimate(block);
        if ts >= month_start {
            *month.entry(ep.clone()).or_default().entry(method).or_default() += delta as f64;
        }
        if ts >= t - 7.0 * 86_400.0 {
            *hourly.entry(((ts / 3600.0).floor() as i64 * 3600, ep)).or_default() += delta as f64;
        }
    }
    let mut endpoints: Vec<Value> = month
        .iter()
        .map(|(ep, methods)| {
            let reqs: f64 = methods.values().sum();
            let mut top: Vec<(&String, &f64)> = methods.iter().collect();
            top.sort_by(|a, b| b.1.total_cmp(a.1));
            let top: Vec<Value> = top.iter().take(5).map(|(m, n)| json!([m, n])).collect();
            match plan(ep, days_in_month) {
                Some((unit, limit, prices)) => {
                    let used: f64 = methods.iter().map(|(m, n)| n * cost(prices, m)).sum();
                    json!({ "name": ep, "reqs": reqs, "unit": unit, "used": used, "limit": limit, "proj": used / elapsed, "top": top })
                }
                None => json!({ "name": ep, "reqs": reqs, "unit": null, "used": null, "limit": null, "proj_reqs": reqs / elapsed, "top": top }),
            }
        })
        .collect();
    endpoints.sort_by(|a, b| b["reqs"].as_f64().unwrap_or(0.0).total_cmp(&a["reqs"].as_f64().unwrap_or(0.0)));
    let mut hourly: Vec<Value> = hourly.into_iter().map(|((h, ep), n)| json!([h, ep, n])).collect();
    hourly.sort_by_key(|v| v[0].as_i64().unwrap_or(0));
    let latest: Option<f64> = db.query_row("SELECT max(ts) FROM rpc_stats", [], |r| r.get(0))?;
    let stats: Vec<Value> = match latest {
        Some(ts) => {
            let mut q = db.prepare("SELECT endpoint, requests, failovers, lagging, hedges, ewma_ms, cooling FROM rpc_stats WHERE ts = ?1 ORDER BY requests DESC")?;
            q.query_map(params![ts], |r| Ok(json!([r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, bool>(6)?])))?.collect::<rusqlite::Result<_>>()?
        }
        None => vec![],
    };
    Ok(json!({ "month_start": month_start, "elapsed": elapsed, "endpoints": endpoints, "hourly": hourly, "stats_ts": latest, "stats": stats }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_prices() {
        let (unit, limit, p) = plan("alchemy", 31.0).unwrap();
        assert_eq!((unit, limit), ("CU", Some(30_000_000.0)));
        assert_eq!(cost(p, "eth_call"), 26.0);
        assert_eq!(cost(p, "eth_getCode"), 26.0, "default");
        assert_eq!(plan("infura", 31.0).unwrap().1, Some(93_000_000.0));
        assert!(plan("tenderly-public", 31.0).is_none());
    }

    #[test]
    fn template_has_the_data_slot() {
        assert!(TEMPLATE.contains("/*__DATA__*/null"), "render replaces this placeholder");
    }
}
