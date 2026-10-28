//! Exit routing. v1: KyberSwap aggregator API (covers Uni V3/V4, Aerodrome/Aero, Pancake,
//! Curve, prop AMMs...). Two calls: /routes for a quote, /route/build for calldata.

use alloy::primitives::{Address, Bytes, U256};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub struct Kyber {
    http: reqwest::Client,
    base: String,
    client_id: String,
}

#[derive(Clone, Debug)]
pub struct Quote {
    pub amount_out: U256,
    /// Kyber's own gas estimate for the swap (None if the response had none). Measured 2026-10-19:
    /// real execution inside our liquidation ran ~1.6x this (KYBER_GAS_SAFETY).
    pub gas: Option<u64>,
    /// Opaque route summary, passed back to /route/build.
    summary: Value,
}

#[derive(Clone, Debug)]
pub struct BuiltSwap {
    pub router: Address,
    pub calldata: Bytes,
}

/// Sends the request and parses JSON, keeping status + body head in the error for diagnosis.
async fn read_json(req: reqwest::RequestBuilder) -> Result<Value> {
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    serde_json::from_str(&text).with_context(|| format!("kyber http {status}: {}", text.chars().take(200).collect::<String>()))
}

/// Gas prices (wei) Kyber is asked to price route gas at: its default (the live price) and 100 gwei,
/// which trades a little output for far fewer split paths. Two rungs, not more: a big quote takes
/// Kyber 0.4-1.4 s and parallel bursts get throttled (2026-10-19: 4 plans x 9 quotes all fell back).
const KYBER_GAS_LADDER: [Option<u128>; 2] = [None, Some(100_000_000_000)];
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
    quotes.into_iter().filter(|q| q.real_gas().is_none_or(|g| g <= gas_budget)).max_by_key(|q| q.amount_out)
}

fn parse_u256(v: &Value) -> Option<U256> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(U256::from),
        _ => None,
    }
}

impl Kyber {
    pub fn new(http: reqwest::Client, base: &str, client_id: &str) -> Self {
        Self { http, base: base.trim_end_matches('/').to_string(), client_id: client_id.to_string() }
    }

    /// One route. `gas_price_wei` (None = Kyber's default, the live price) is what Kyber charges each
    /// unit of route gas when it compares splits: a high value buys fewer, cheaper paths.
    pub async fn quote(&self, token_in: Address, token_out: Address, amount_in: U256, gas_price_wei: Option<u128>) -> Result<Quote> {
        let mut query = vec![("tokenIn", format!("{token_in:#x}")), ("tokenOut", format!("{token_out:#x}")), ("amountIn", amount_in.to_string())];
        if let Some(g) = gas_price_wei {
            query.push(("gasPrice", g.to_string()));
        }
        let req = self.http.get(format!("{}/routes", self.base)).query(&query).header("x-client-id", &self.client_id);
        let resp = read_json(req).await?;
        if resp["code"].as_i64() != Some(0) {
            bail!("kyber routes: {}", resp["message"]);
        }
        let summary = resp["data"]["routeSummary"].clone();
        let amount_out = parse_u256(&summary["amountOut"]).context("kyber: no amountOut")?;
        let gas = parse_u256(&summary["gas"]).map(|g| g.saturating_to());
        if gas.is_none() {
            tracing::warn!("kyber route has no gas estimate; the gas budget falls back to simulation");
        }
        Ok(Quote { amount_out, gas, summary })
    }

    /// Best route whose swap fits `gas_budget` (real gas, after KYBER_GAS_SAFETY). Kyber's default
    /// routing on Base, where gas is nearly free, splits big trades over 10-20 paths: a large
    /// alt-coin -> USDC quote came back as 11 paths / 15.6M gas, too heavy for one tx (cap 16.7M,
    /// our max_gas 8M) or an Atlas solver op (6M). Re-quoting with a gas penalty
    /// (KYBER_GAS_LADDER) collapses it, e.g. 1 path / 0.47M gas for 0.24% less out; all rungs are
    /// quoted in parallel and the best one that fits wins.
    pub async fn quote_within(&self, token_in: Address, token_out: Address, amount_in: U256, gas_budget: u64) -> Result<Quote> {
        let rungs = futures::future::join_all(KYBER_GAS_LADDER.iter().map(|g| self.quote(token_in, token_out, amount_in, *g))).await;
        let mut last_err = None;
        let ok: Vec<Quote> = rungs
            .into_iter()
            .filter_map(|r| r.map_err(|e| last_err = Some(e)).ok())
            .collect();
        if ok.is_empty() {
            return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("kyber: no quote")));
        }
        pick_within_budget(ok, gas_budget).with_context(|| format!("kyber: no route within {gas_budget} gas"))
    }

    /// Calldata for the quoted route; `sender` is the contract that holds the tokens and calls the router.
    pub async fn build(&self, quote: &Quote, sender: Address, recipient: Address, slippage_bps: u32) -> Result<BuiltSwap> {
        let body = json!({
            "routeSummary": quote.summary,
            "sender": format!("{sender:#x}"),
            "recipient": format!("{recipient:#x}"),
            "slippageTolerance": slippage_bps,
        });
        let req = self.http.post(format!("{}/route/build", self.base)).header("x-client-id", &self.client_id).json(&body);
        let resp = read_json(req).await?;
        if resp["code"].as_i64() != Some(0) {
            bail!("kyber build: {}", resp["message"]);
        }
        let d = &resp["data"];
        Ok(BuiltSwap {
            router: d["routerAddress"].as_str().context("kyber: no router")?.parse()?,
            calldata: d["data"].as_str().context("kyber: no data")?.parse()?,
        })
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

    fn amount_out(&self, quote: &Quote) -> U256 {
        quote.amount_out
    }

    fn swap_gas(&self, quote: &Quote) -> Option<u64> {
        quote.real_gas()
    }

    async fn build(&self, quote: &Quote, sender: Address, recipient: Address, slippage_bps: u32) -> Result<crate::dex::SwapCall> {
        let b = Kyber::build(self, quote, sender, recipient, slippage_bps).await?;
        Ok(crate::dex::SwapCall { target: b.router, calldata: b.calldata })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(out: u64, gas: u64) -> Quote {
        Quote { amount_out: U256::from(out), gas: Some(gas), summary: Value::Null }
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
        let unknown = Quote { amount_out: U256::from(1u8), gas: None, summary: Value::Null };
        assert!(pick_within_budget(vec![unknown], 1).is_some());
    }
}
