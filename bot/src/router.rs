//! Exit routing. v1: KyberSwap aggregator API (covers Uni V3/V4, Aerodrome/Aero, Pancake,
//! Curve, prop AMMs...). Two calls: /routes for a quote, /route/build for calldata.

use std::time::Duration;

use alloy::primitives::{Address, Bytes, U256};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub struct Kyber {
    http: reqwest::Client,
    base: String,
    client_id: String,
    /// Source ids excluded when a build fails on an RFQ/PMM route (re-quote over pools only).
    rfq_exclude: String,
    /// Time budget for one /routes or /route/build call; past it the call is abandoned.
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Quote {
    pub amount_out: U256,
    /// Kyber's own gas estimate for the swap (None if the response had none). Measured 2026-10-19:
    /// real execution inside our liquidation ran ~1.6x this (KYBER_GAS_SAFETY).
    pub gas: Option<u64>,
    /// Opaque route summary, passed back to /route/build.
    summary: Value,
    /// The request, kept so a build failure on an RFQ/PMM route can re-quote over pools only.
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    gas_price_wei: Option<u128>,
}

#[derive(Clone, Debug)]
pub struct BuiltSwap {
    pub router: Address,
    pub calldata: Bytes,
    /// The route was rebuilt without RFQ/PMM sources after the first build failed.
    pub fell_back: bool,
}

/// A Kyber call abandoned past its time budget (`kyber_timeout_ms`).
#[derive(Debug)]
struct KyberTimeout(u128);

impl std::fmt::Display for KyberTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "kyber timeout ({} ms)", self.0)
    }
}

impl std::error::Error for KyberTimeout {}

/// The liquidity sources (`exchange` ids, e.g. "uniswap-v3", "pmm-19") a route summary uses; None
/// when the summary carries no readable route.
fn route_sources(summary: &Value) -> Option<Vec<&str>> {
    let hops: Vec<&str> = summary["route"].as_array()?.iter().filter_map(Value::as_array).flatten().filter_map(|s| s["exchange"].as_str()).collect();
    (!hops.is_empty()).then_some(hops)
}

/// The error text of a failed RFQ/PMM fill: a signed maker quote that expired or was declined ("pmm
/// RFQ failed"), or the build endpoint refusing the route (HTTP 403 Forbidden). Only consulted when
/// the route itself is unreadable.
fn is_rfq_build_error(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}").to_lowercase();
    s.contains("pmm") || s.contains("rfq") || s.contains("forbidden") || s.contains("403")
}

/// Whether a failed build is worth one pool-only re-quote: only when the route used one of the
/// excluded (RFQ/PMM) sources — excluding them from a route that has none yields the same route, so
/// an unrelated failure (a WAF 403, a bad request) costs no extra round trip on the auction's critical
/// path. A timeout already spent the budget: no retry. An unreadable route falls back to the error
/// text.
fn worth_pool_only_retry(summary: &Value, exclude: &str, e: &anyhow::Error) -> bool {
    if exclude.is_empty() || e.downcast_ref::<KyberTimeout>().is_some() {
        return false;
    }
    match route_sources(summary) {
        Some(srcs) => srcs.iter().any(|s| exclude.split(',').any(|x| x.trim().eq_ignore_ascii_case(s))),
        None => is_rfq_build_error(e),
    }
}

/// Sends the request and parses JSON, keeping status + body head in the error for diagnosis.
async fn read_json(req: reqwest::RequestBuilder) -> Result<Value> {
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    serde_json::from_str(&text).with_context(|| format!("kyber http {status}: {}", text.chars().take(200).collect::<String>()))
}

/// Gas prices (wei) Kyber is asked to price route gas at: its default (the live price, best output)
/// and a forcing ~1000 gwei penalty that collapses the route to a single PMM/RFQ or pool path (a
/// little less output for far less gas). The forcing value is deliberate, not the real fee: measured
/// 2026-11-08, 100 gwei was too weak to collapse a split $50k route at the real ~15 gwei auction
/// price (whether it collapsed depended mostly on transient `pmm-19` RFQ depth, hence the earlier
/// "identical routes" observation), while 1000 gwei reliably returned 1 path / ~0.28M gas that fits
/// the Atlas 6M cap. The live-price rung wins when it fits the budget; the forcing rung is the
/// guaranteed-fits fallback. Two rungs, not more: a big quote takes Kyber 0.4-1.4 s and parallel
/// bursts get throttled (2026-10-19: 4 plans x 9 quotes all fell back).
const KYBER_GAS_LADDER: [Option<u128>; 2] = [None, Some(1_000_000_000_000)];
/// Real gas of a Kyber swap inside our liquidation vs Kyber's estimate (2026-10-19: 12.67M real
/// for a route Kyber put at ~8M).
pub const KYBER_GAS_SAFETY: f64 = 1.6;

impl Quote {
    /// Estimated real gas of the swap; None if Kyber gave no estimate.
    pub fn real_gas(&self) -> Option<u64> {
        self.gas.map(|g| (g as f64 * KYBER_GAS_SAFETY) as u64)
    }
}

/// Highest-output quote whose estimated real gas fits the budget. A quote without an estimate
/// passes (callers that can simulate check the real gas; bundles count it as the whole budget):
/// if Kyber ever drops the field, that keeps Kyber working instead of silently rejecting it.
fn pick_within_budget(quotes: Vec<Quote>, gas_budget: u64) -> Option<Quote> {
    within_budget(quotes, gas_budget).into_iter().max_by_key(|q| q.amount_out)
}

/// The quotes whose estimated real gas fits the budget (see `pick_within_budget` for the no-estimate
/// rule). `quotes_within` hands them all back so the caller can score each by value net of gas.
fn within_budget(quotes: Vec<Quote>, gas_budget: u64) -> Vec<Quote> {
    quotes.into_iter().filter(|q| q.real_gas().is_none_or(|g| g <= gas_budget)).collect()
}

fn parse_u256(v: &Value) -> Option<U256> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(U256::from),
        _ => None,
    }
}

impl Kyber {
    pub fn new(http: reqwest::Client, base: &str, client_id: &str, rfq_exclude: &str, timeout_ms: u64) -> Self {
        Self { http, base: base.trim_end_matches('/').to_string(), client_id: client_id.to_string(), rfq_exclude: rfq_exclude.to_string(), timeout: Duration::from_millis(timeout_ms) }
    }

    /// `read_json` under the Kyber time budget: a slow call (up to 12 s seen 2026-11-13) is abandoned
    /// so the valuation can fall back to the direct route or skip instead of waiting.
    async fn fetch(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        match tokio::time::timeout(self.timeout, read_json(req)).await {
            Ok(r) => r,
            Err(_) => Err(KyberTimeout(self.timeout.as_millis()).into()),
        }
    }

    /// One route. `gas_price_wei` (None = Kyber's default, the live price) is what Kyber charges each
    /// unit of route gas when it compares splits: a high value buys fewer, cheaper paths. `exclude`
    /// (comma-separated source ids, empty = none) drops liquidity sources, e.g. RFQ/PMM on a retry.
    pub async fn quote(&self, token_in: Address, token_out: Address, amount_in: U256, gas_price_wei: Option<u128>, exclude: &str) -> Result<Quote> {
        // gasInclude=true asks for gas-aware route selection and populates routeSummary.gas/gasUsd
        // (harmless at the default price; needed for the gasPrice penalty to bite).
        let mut query = vec![
            ("tokenIn", format!("{token_in:#x}")),
            ("tokenOut", format!("{token_out:#x}")),
            ("amountIn", amount_in.to_string()),
            ("gasInclude", "true".to_string()),
        ];
        if let Some(g) = gas_price_wei {
            query.push(("gasPrice", g.to_string()));
        }
        if !exclude.is_empty() {
            query.push(("excludedSources", exclude.to_string()));
        }
        let req = self.http.get(format!("{}/routes", self.base)).query(&query).header("x-client-id", &self.client_id);
        let resp = self.fetch(req).await?;
        if resp["code"].as_i64() != Some(0) {
            bail!("kyber routes: {}", resp["message"]);
        }
        let summary = resp["data"]["routeSummary"].clone();
        let amount_out = parse_u256(&summary["amountOut"]).context("kyber: no amountOut")?;
        let gas = parse_u256(&summary["gas"]).map(|g| g.saturating_to());
        if gas.is_none() {
            tracing::warn!("kyber route has no gas estimate; the gas budget falls back to simulation");
        }
        Ok(Quote { amount_out, gas, summary, token_in, token_out, amount_in, gas_price_wei })
    }

    /// Best route whose swap fits `gas_budget` (real gas, after KYBER_GAS_SAFETY). Kyber's default
    /// routing on Base, where gas is nearly free, splits big trades over 10-20 paths: a large
    /// alt-coin -> USDC quote came back as 11 paths / 15.6M gas, too heavy for one tx (cap 16.7M,
    /// our max_gas 8M) or an Atlas solver op (6M). Re-quoting with a gas penalty
    /// (KYBER_GAS_LADDER) collapses it, e.g. 1 path / 0.47M gas for 0.24% less out; all rungs are
    /// quoted in parallel and the best one that fits wins.
    pub async fn quote_within(&self, token_in: Address, token_out: Address, amount_in: U256, gas_budget: u64) -> Result<Quote> {
        let ok = self.quote_rungs(token_in, token_out, amount_in).await?;
        pick_within_budget(ok, gas_budget).with_context(|| format!("kyber: no route within {gas_budget} gas"))
    }

    /// Every rung whose swap fits `gas_budget`, for a caller that scores each by value net of gas (an
    /// Atlas bundle weighs gas at the auction's ~15 gwei) rather than taking the most output: the
    /// forcing rung (fewer paths, far less gas) can win on net even when it gives up a little output.
    /// Errs only when a rung quoted but none fit.
    pub async fn quotes_within(&self, token_in: Address, token_out: Address, amount_in: U256, gas_budget: u64) -> Result<Vec<Quote>> {
        let ok = self.quote_rungs(token_in, token_out, amount_in).await?;
        let within = within_budget(ok, gas_budget);
        if within.is_empty() {
            bail!("kyber: no route within {gas_budget} gas");
        }
        Ok(within)
    }

    /// Quotes every rung of `KYBER_GAS_LADDER` in parallel; errs only if they all failed.
    async fn quote_rungs(&self, token_in: Address, token_out: Address, amount_in: U256) -> Result<Vec<Quote>> {
        let rungs = futures::future::join_all(KYBER_GAS_LADDER.iter().map(|g| self.quote(token_in, token_out, amount_in, *g, ""))).await;
        let mut last_err = None;
        let ok: Vec<Quote> = rungs.into_iter().filter_map(|r| r.map_err(|e| last_err = Some(e)).ok()).collect();
        if ok.is_empty() {
            return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("kyber: no quote")));
        }
        Ok(ok)
    }

    /// Calldata for the quoted route; `sender` is the contract that holds the tokens and calls the
    /// router. If a route that uses an RFQ/PMM source fails to build (expired or declined maker
    /// quote, a 403), re-quote the same trade over pools only (`rfq_exclude`) and build that, so one
    /// flaky maker does not lose the auction (see `worth_pool_only_retry`). The fallback route is
    /// flagged (`fell_back`) for the journal; its output and gas are not the original quote's, so the
    /// simulation that gates the send is what prices it.
    pub async fn build(&self, quote: &Quote, sender: Address, recipient: Address, slippage_bps: u32) -> Result<BuiltSwap> {
        match self.build_summary(&quote.summary, sender, recipient, slippage_bps).await {
            Ok((router, calldata)) => Ok(BuiltSwap { router, calldata, fell_back: false }),
            Err(e) if worth_pool_only_retry(&quote.summary, &self.rfq_exclude, &e) => {
                tracing::warn!(error = %e, exclude = %self.rfq_exclude, "kyber build failed on an RFQ/PMM route; re-quoting over pools only");
                let q = self.quote(quote.token_in, quote.token_out, quote.amount_in, quote.gas_price_wei, &self.rfq_exclude).await?;
                let (router, calldata) = self.build_summary(&q.summary, sender, recipient, slippage_bps).await?;
                Ok(BuiltSwap { router, calldata, fell_back: true })
            }
            Err(e) => Err(e),
        }
    }

    async fn build_summary(&self, summary: &Value, sender: Address, recipient: Address, slippage_bps: u32) -> Result<(Address, Bytes)> {
        let body = json!({
            "routeSummary": summary,
            "sender": format!("{sender:#x}"),
            "recipient": format!("{recipient:#x}"),
            "slippageTolerance": slippage_bps,
        });
        let req = self.http.post(format!("{}/route/build", self.base)).header("x-client-id", &self.client_id).json(&body);
        let resp = self.fetch(req).await?;
        if resp["code"].as_i64() != Some(0) {
            bail!("kyber build: {}", resp["message"]);
        }
        let d = &resp["data"];
        Ok((
            d["routerAddress"].as_str().context("kyber: no router")?.parse()?,
            d["data"].as_str().context("kyber: no data")?.parse()?,
        ))
    }
}

/// KyberSwap as the planner's aggregator. Inherent methods take precedence over trait methods, so
/// these delegate to Kyber's own.
impl crate::dex::Dex for Kyber {
    type Quote = Quote;

    fn name(&self) -> &'static str {
        "kyber"
    }

    async fn quote_within(&self, token_in: Address, token_out: Address, amount_in: U256, gas_budget: u64) -> Result<Quote> {
        Kyber::quote_within(self, token_in, token_out, amount_in, gas_budget).await
    }

    async fn quotes_within(&self, token_in: Address, token_out: Address, amount_in: U256, gas_budget: u64) -> Result<Vec<Quote>> {
        Kyber::quotes_within(self, token_in, token_out, amount_in, gas_budget).await
    }

    fn rung_gas_price_wei(&self, quote: &Quote) -> Option<u128> {
        quote.gas_price_wei
    }

    fn amount_out(&self, quote: &Quote) -> U256 {
        quote.amount_out
    }

    fn swap_gas(&self, quote: &Quote) -> Option<u64> {
        quote.real_gas()
    }

    async fn build(&self, quote: &Quote, sender: Address, recipient: Address, slippage_bps: u32) -> Result<crate::dex::SwapCall> {
        let b = Kyber::build(self, quote, sender, recipient, slippage_bps).await?;
        Ok(crate::dex::SwapCall { target: b.router, calldata: b.calldata, note: b.fell_back.then_some("no-rfq") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(out: u64, gas: u64) -> Quote {
        Quote { amount_out: U256::from(out), gas: Some(gas), summary: Value::Null, token_in: Address::ZERO, token_out: Address::ZERO, amount_in: U256::ZERO, gas_price_wei: None }
    }

    #[test]
    fn rfq_build_errors_trigger_the_pool_only_fallback() {
        assert!(is_rfq_build_error(&anyhow::anyhow!("kyber build: pmm RFQ failed")));
        assert!(is_rfq_build_error(&anyhow::anyhow!("kyber http 403 Forbidden: ...")));
        assert!(is_rfq_build_error(&anyhow::anyhow!("RFQ quote expired")));
        // A generic failure is not retried over a different route.
        assert!(!is_rfq_build_error(&anyhow::anyhow!("kyber build: insufficient liquidity")));
        assert!(!is_rfq_build_error(&anyhow::anyhow!("connection reset")));
    }

    #[test]
    fn the_pool_only_retry_follows_the_routes_sources_not_the_error_text() {
        let excl = "pmm-19,bebop";
        let route = |ex: &[&str]| json!({ "route": [ex.iter().map(|e| json!({ "exchange": e })).collect::<Vec<_>>()] });
        let with_pmm = route(&["uniswap-v3", "pmm-19"]);
        let pools = route(&["uniswap-v3", "aerodrome"]);
        let forbidden = anyhow::anyhow!("kyber http 403 Forbidden: ...");
        // The route used a maker: any build failure is worth a pool-only route.
        assert!(worth_pool_only_retry(&with_pmm, excl, &anyhow::anyhow!("kyber build: route expired")));
        assert!(worth_pool_only_retry(&route(&["BEBOP"]), excl, &forbidden), "case-insensitive ids");
        // A pools-only route would re-quote to the same route: a 403 (e.g. a WAF) is not retried.
        assert!(!worth_pool_only_retry(&pools, excl, &forbidden));
        // A timeout already spent the budget.
        assert!(!worth_pool_only_retry(&with_pmm, excl, &anyhow::Error::new(KyberTimeout(700))));
        // No exclusion configured: never.
        assert!(!worth_pool_only_retry(&with_pmm, "", &forbidden));
        // An unreadable route falls back to the error text.
        assert!(worth_pool_only_retry(&Value::Null, excl, &anyhow::anyhow!("kyber build: pmm RFQ failed")));
        assert!(!worth_pool_only_retry(&Value::Null, excl, &anyhow::anyhow!("kyber build: insufficient liquidity")));
    }

    #[test]
    fn budget_picks_best_route_that_fits() {
        // Real quotes for one large sale at default / 1 gwei / 100 gwei: 15.6M, 11.1M, 0.47M gas.
        let ladder = || vec![q(162_258, 15_566_630), q(162_249, 11_120_140), q(161_882, 467_166)];
        // max_gas 8M: only the single-path route fits after the 1.6x safety.
        assert_eq!(pick_within_budget(ladder(), 8_000_000).unwrap().amount_out, U256::from(161_882u64));
        // A loose budget keeps the best output.
        assert_eq!(pick_within_budget(ladder(), 30_000_000).unwrap().amount_out, U256::from(162_258u64));
        // Nothing fits.
        assert!(pick_within_budget(ladder(), 500_000).is_none());
        // No estimate: passes (the simulation or the bundle's full budget accounts for it).
        let unknown = Quote { amount_out: U256::from(1u8), gas: None, summary: Value::Null, token_in: Address::ZERO, token_out: Address::ZERO, amount_in: U256::ZERO, gas_price_wei: None };
        assert!(pick_within_budget(vec![unknown], 1).is_some());
    }

    #[test]
    fn within_budget_keeps_every_fitting_rung_for_net_scoring() {
        // Same three rungs: at 8M only the single-path rung fits, at 30M all three do. quotes_within
        // hands them ALL back (not just the highest-output one) so the caller can pick by net gas.
        let ladder = || vec![q(162_258, 15_566_630), q(162_249, 11_120_140), q(161_882, 467_166)];
        let tight: Vec<u64> = within_budget(ladder(), 8_000_000).iter().map(|q| q.amount_out.to()).collect();
        assert_eq!(tight, vec![161_882u64], "only the single-path rung fits 8M after the 1.6x safety");
        assert_eq!(within_budget(ladder(), 30_000_000).len(), 3, "a loose budget keeps all rungs for the caller to weigh");
        assert!(within_budget(ladder(), 500_000).is_empty(), "nothing fits");
    }
}
