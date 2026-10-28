//! Morpho GraphQL API: borrower discovery (bootstrap only; the chain is the source of truth).

use std::collections::HashSet;

use alloy::primitives::{Address, B256};
use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::protocol::BorrowerInfo;

/// All borrowers of a market. The API caps `skip` at 10,000, so pages are walked with a
/// cursor on borrowShares (descending) instead; duplicates at page edges are dropped.
pub async fn fetch_borrowers(http: &reqwest::Client, api_url: &str, chain_id: u64, market: B256) -> Result<Vec<BorrowerInfo>> {
    const PAGE: usize = 1000;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let cursor_filter = cursor.as_ref().map(|c| format!(r#", borrowShares_lte: "{c}""#)).unwrap_or_default();
        let query = format!(
            r#"{{ marketPositions(first: {PAGE}, orderBy: BorrowShares, orderDirection: Desc, where: {{ chainId_in: [{chain_id}], marketUniqueKey_in: ["{market:#x}"], borrowShares_gte: "1"{cursor_filter} }}) {{ items {{ healthFactor user {{ address }} state {{ borrowShares }} }} }} }}"#
        );
        let resp: Value = http.post(api_url).json(&json!({ "query": query })).send().await?.json().await?;
        if let Some(err) = resp.get("errors") {
            anyhow::bail!("morpho api error: {err}");
        }
        let items = resp["data"]["marketPositions"]["items"].as_array().context("unexpected api response")?;
        let mut new_in_page = 0;
        for it in items {
            let Some(addr) = it["user"]["address"].as_str().and_then(|s| s.parse::<Address>().ok()) else { continue };
            if seen.insert(addr) {
                new_in_page += 1;
                out.push(BorrowerInfo { user: addr, api_health_factor: it["healthFactor"].as_f64() });
            }
        }
        if items.len() < PAGE || new_in_page == 0 {
            break;
        }
        // BigInt comes back as a JSON string when large and as a number when small.
        cursor = items.last().and_then(|it| match &it["state"]["borrowShares"] {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => n.as_u64().map(|v| v.to_string()),
            _ => None,
        });
        if cursor.is_none() {
            break;
        }
    }
    Ok(out)
}
