//! Live monitor of the positions closest to liquidation.
//!
//! `watch` calls [`write_snapshot`] every block: the `live_top` lowest-HF positions plus a per-market
//! summary go to `[dashboard] live_path` (written to a temp file, then renamed, so a reader never
//! sees half a file). `bot live` ([`serve`]) is a separate process serving a page and that file on
//! 127.0.0.1 only; it can be restarted or broken without touching the bot. Nothing is persisted
//! beyond the one file: the history that matters (watch rows, liquidations) is in the journal.

use std::collections::HashMap;

use alloy::primitives::{Address, U256};
use anyhow::{Context, Result};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tracing::{info, warn};

use crate::{
    config::DashboardConfig,
    morpho::PositionView,
    morpho::{
        math,
        onchain::{MarketMeta, MarketSnapshot},
    },
};

const PAGE: &str = include_str!("live.html");

/// Collateral price in loan units per whole collateral token (the oracle price is scaled by 1e36
/// and by the decimals difference).
fn price_human(price: U256, meta: &MarketMeta) -> f64 {
    math::u256_f64(price) / 1e36 * 10f64.powi(meta.collateral_decimals as i32 - meta.loan_decimals as i32)
}

/// `views` sorted by HF, lowest first (Engine::evaluate). Positions are public on-chain data; the
/// file holds no keys or endpoint URLs.
pub fn write_snapshot(
    cfg: &DashboardConfig,
    metas: &[MarketMeta],
    snaps: &[Option<MarketSnapshot>],
    positions: &HashMap<(usize, Address), (U256, U256)>,
    views: &[PositionView],
    block: u64,
    block_ts: u64,
) -> Result<()> {
    let markets: Vec<serde_json::Value> = metas
        .iter()
        .enumerate()
        .map(|(mi, m)| {
            let mv = views.iter().filter(|v| v.market == mi);
            let (mut n, mut liq, mut b102, mut b105, mut near_debt) = (0usize, 0usize, 0usize, 0usize, 0.0f64);
            for v in mv {
                n += 1;
                liq += usize::from(v.health.is_liquidatable());
                b102 += usize::from(v.hf < 1.02);
                if v.hf < 1.05 {
                    b105 += 1;
                    near_debt += v.debt;
                }
            }
            json!({ "name": m.name, "price": snaps.get(mi).and_then(|s| s.as_ref()).map(|s| price_human(s.price, m)),
                    "positions": n, "liquidatable": liq, "below_1_02": b102, "below_1_05": b105, "debt_below_1_05": near_debt })
        })
        .collect();
    let top: Vec<serde_json::Value> = views
        .iter()
        .filter(|v| v.hf.is_finite())
        .take(cfg.live_top)
        .map(|v| {
            let m = &metas[v.market];
            let price = snaps.get(v.market).and_then(|s| s.as_ref()).map(|s| price_human(s.price, m));
            let collateral = positions.get(&(v.market, v.user)).map(|p| math::units(p.1, m.collateral_decimals));
            // HF is linear in the collateral price: it reaches 1 at price / hf.
            json!({ "market": m.name, "borrower": v.user, "hf": v.hf, "debt": v.debt, "collateral": collateral, "price": price,
                    "liq_price": price.map(|p| p / v.hf), "drop_to_liq_pct": (1.0 - 1.0 / v.hf) * 100.0,
                    "liquidatable": v.health.is_liquidatable(), "bonus": v.bonus })
        })
        .collect();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let body = json!({ "block": block, "block_ts": block_ts, "written": now, "markets": markets, "top": top }).to_string();
    let tmp = format!("{}.tmp", cfg.live_path);
    std::fs::write(&tmp, body).with_context(|| format!("writing {tmp}"))?;
    std::fs::rename(&tmp, &cfg.live_path).with_context(|| format!("renaming to {}", cfg.live_path))?;
    Ok(())
}

/// `bot live`, on 127.0.0.1 only: GET / = the dashboard (with its live section), GET /live = the
/// standalone live page, GET /live.json = the latest snapshot.
pub async fn serve(cfg: &DashboardConfig) -> Result<()> {
    let addr = format!("127.0.0.1:{}", cfg.live_port);
    let listener = TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    info!(%addr, file = %cfg.live_path, "live monitor: open http://localhost:{} in a browser", cfg.live_port);
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "live monitor: accept failed");
                continue;
            }
        };
        let path = cfg.live_path.clone();
        let html = cfg.html_path.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let n = match tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf)).await {
                Ok(Ok(n)) => n,
                _ => return,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let target = req.split_whitespace().nth(1).unwrap_or("/");
            let (status, ctype, body) = match target.split('?').next().unwrap_or("/") {
                // The dashboard (rewritten every minute by `bot dashboard`), whose "Live positions" section
                // polls /live.json; until a dashboard exists, the standalone page.
                "/" | "/index.html" => match tokio::fs::read(&html).await {
                    Ok(b) => ("200 OK", "text/html; charset=utf-8", b),
                    Err(_) => ("200 OK", "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
                },
                "/live" => ("200 OK", "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
                "/live.json" => match tokio::fs::read(&path).await {
                    Ok(b) => ("200 OK", "application/json", b),
                    Err(_) => ("503 Service Unavailable", "application/json", br#"{"error":"no snapshot yet (is watch running?)"}"#.to_vec()),
                },
                _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
            };
            let head = format!("HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n", body.len());
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body).await;
        });
    }
}
