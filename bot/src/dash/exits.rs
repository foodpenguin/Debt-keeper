//! How other liquidators got in and out, from raw receipts (no trace API needed).
//!
//! Every liquidation tx in the journal is fetched once (tx + receipt, kept verbatim in `liq_raw`)
//! and decoded from that copy, so a better decoder (DECODER_VERSION) re-runs on all history with no
//! RPC. Per tx it records:
//!  - entry: Atlas metacall or direct call, liquidations batched, flash loans (Morpho, Aave,
//!    Balancer), gas used, gas price, priority fee;
//!  - exit: venues in log order (pool + protocol, singletons like the Uniswap v4 PoolManager,
//!    known routers / aggregators / RFQ settlement), Aave carry (supply collateral + borrow instead of
//!    selling), collateral kept, and the realized exit price vs the oracle value of
//!    what was sold (slippage).
//!
//! Token flows come from Transfer logs: Morpho's net outflow per token is the seized collateral,
//! its net inflow the repaid loan token. The "actor" is Morpho's Liquidate caller (plus tx.from).

use std::collections::{BTreeSet, HashMap};

use alloy::primitives::{Address, B256, U256, address, keccak256};
use serde::Serialize;
use serde_json::{Value, json};

/// Bump to re-decode every stored receipt with the current code.
pub const DECODER_VERSION: i64 = 2;

pub const MORPHO: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
const AAVE_POOL: Address = address!("A238Dd80C259a72e81d7e4664a9801593F98d1c5");
const BALANCER_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
const V4_POOL_MANAGER: Address = address!("498581fF718922c3f8e6A244956aF099B2652b2b");

/// Contracts recognized by address (as published by each project on Base), plus the ones in
/// [[dashboard.contracts]] (venues that migrate, e.g. Aerodrome to Aero, belong there). A wrong or
/// missing entry only changes a label; unknown counterparties are listed per tx for extending this.
const KNOWN: [(Address, &str, &str); 7] = [
    (address!("6131B5fae19EA4f9D964eAc0408E4408b66337b5"), "kyber", "aggregator"),
    (address!("111111125421cA6dc452d289314280a0f8842A65"), "1inch", "aggregator"),
    (address!("19cEeAd7105607Cd444F5ad10dd51356436095a1"), "odos", "aggregator"),
    (address!("6A000F20005980200259B80c5102003040001068"), "velora", "aggregator"),
    (address!("0000000000001fF3684f28c67538d4D072C22734"), "0x", "aggregator"),
    (address!("bbbbbBB520d69a9775E85b458C58c648259FAD5F"), "bebop", "rfq"),
    (address!("6fF5693b99212Da76ad316178A184AB56D299b43"), "uniswap-router", "router"),
];

/// Pool factories beyond [router.factories] and [[dashboard.factories]] (from the config by name).
pub const EXTRA_FACTORIES: [(Address, &str); 1] = [(address!("8909Dc15e40173Ff4699343b6eB8132c65e18eC6"), "uniV2")];

fn topic(sig: &str) -> B256 {
    keccak256(sig.as_bytes())
}

/// Swap-like events: signature -> protocol family. The emitter is the pool, except for the v4
/// PoolManager and the Balancer vault (singletons).
fn swap_family(t: &B256) -> Option<&'static str> {
    const SIGS: [(&str, &str); 7] = [
        ("Swap(address,address,int256,int256,uint160,uint128,int24)", "v3"),
        ("Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128)", "pancake-v3"),
        ("Swap(address,address,uint256,uint256,uint256,uint256)", "v2"),
        ("Swap(address,uint256,uint256,uint256,uint256,address)", "uniV2"),
        ("Swap(bytes32,address,int128,int128,uint160,uint128,int24,uint24)", "uniV4"),
        ("Swap(bytes32,address,address,uint256,uint256)", "balancer"),
        ("TokenExchange(address,int128,uint256,int128,uint256)", "curve"),
    ];
    SIGS.iter().find(|(s, _)| topic(s) == *t).map(|(_, f)| *f)
}

#[derive(Debug, Clone)]
pub struct Log {
    pub addr: Address,
    pub topics: Vec<B256>,
    pub data: Vec<u8>,
}

/// The parts of a tx + receipt the decoder uses.
#[derive(Debug, Clone)]
pub struct Raw {
    pub from: Address,
    pub to: Option<Address>,
    pub gas_used: u64,
    pub gas_price: u128,
    pub priority: Option<u128>,
    pub logs: Vec<Log>,
}

fn hex_u128(v: &Value) -> Option<u128> {
    u128::from_str_radix(v.as_str()?.trim_start_matches("0x"), 16).ok()
}

pub fn parse(tx: &Value, rc: &Value) -> Option<Raw> {
    let logs = rc["logs"]
        .as_array()?
        .iter()
        .filter_map(|l| {
            Some(Log {
                addr: l["address"].as_str()?.parse().ok()?,
                topics: l["topics"].as_array()?.iter().filter_map(|t| t.as_str()?.parse().ok()).collect(),
                data: alloy::hex::decode(l["data"].as_str().unwrap_or("0x")).ok()?,
            })
        })
        .collect();
    Some(Raw {
        from: tx["from"].as_str()?.parse().ok()?,
        to: tx["to"].as_str().and_then(|s| s.parse().ok()),
        gas_used: hex_u128(&rc["gasUsed"])? as u64,
        gas_price: hex_u128(&rc["effectiveGasPrice"]).unwrap_or(0),
        priority: hex_u128(&tx["maxPriorityFeePerGas"]),
        logs,
    })
}

fn word(data: &[u8], i: usize) -> U256 {
    data.get(i * 32..i * 32 + 32).map_or(U256::ZERO, U256::from_be_slice)
}

fn topic_addr(t: &B256) -> Address {
    Address::from_word(*t)
}

#[derive(Debug, Clone, Default)]
pub struct PoolMeta {
    pub protocol: String,
}

#[derive(Debug, Clone)]
pub struct TokenMeta {
    pub symbol: String,
    pub decimals: u8,
}

/// Pools (swap emitters that are not singletons) and tokens (Transfer emitters) a raw tx needs
/// metadata for.
pub fn needs(raw: &Raw) -> (BTreeSet<Address>, BTreeSet<Address>) {
    let transfer = topic("Transfer(address,address,uint256)");
    let mut pools = BTreeSet::new();
    let mut tokens = BTreeSet::new();
    for l in &raw.logs {
        match l.topics.first() {
            Some(t) if *t == transfer && l.topics.len() == 3 => {
                tokens.insert(l.addr);
            }
            Some(t) if swap_family(t).is_some() && l.addr != V4_POOL_MANAGER && l.addr != BALANCER_VAULT => {
                pools.insert(l.addr);
            }
            _ => {}
        }
    }
    (pools, tokens)
}

#[derive(Debug, Clone, Serialize)]
pub struct Venue {
    pub protocol: String,
    pub kind: String,
    pub address: Address,
    pub token_in: Vec<String>,
    pub token_out: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Exit {
    pub actor: Option<Address>,
    pub eoa: Option<Address>,
    pub via: String,
    pub n_liq: usize,
    pub flash: Vec<String>,
    pub kind: String,
    pub route: String,
    pub venues: Vec<Venue>,
    pub collateral: Vec<String>,
    /// Collateral amounts in token units, summed over collateral tokens of the same value scale
    /// only through `oracle_usd` (the USD value of what was sold).
    pub seized: f64,
    pub sold: f64,
    pub kept: f64,
    pub carried: f64,
    /// Loan tokens (USD stablecoins in our markets) received from exit venues, token units.
    pub loan_in: f64,
    pub oracle_usd: Option<f64>,
    pub slippage_bps: Option<f64>,
    /// Counterparties of the actor's token flows that no venue rule explains (for extending KNOWN).
    pub unknown_counterparties: Vec<Address>,
    pub gas_used: u64,
    pub gas_price_gwei: f64,
    pub priority_gwei: Option<f64>,
}

fn units(x: U256, decimals: u8) -> f64 {
    x.to_string().parse::<f64>().unwrap_or(0.0) / 10f64.powi(decimals as i32)
}

/// Decodes one tx. `markets`: the journal's markets liquidated in it ("WETH/USDC": collateral /
/// loan symbols); `seized_usd`: oracle value seized per collateral symbol (from those rows).
/// `contracts` = (address, name, kind) labels on top of KNOWN.
pub fn decode(raw: &Raw, atlas: Address, pools: &HashMap<Address, PoolMeta>, tokens: &HashMap<Address, TokenMeta>, markets: &[String], seized_usd: &HashMap<String, f64>, contracts: &[(Address, String, String)]) -> Exit {
    let transfer = topic("Transfer(address,address,uint256)");
    let liquidate = crate::morpho::events::LIQUIDATE;
    let sym = |t: &Address| tokens.get(t).map_or_else(|| format!("{t:#x}")[..8].to_string(), |m| m.symbol.clone());
    let dec = |t: &Address| tokens.get(t).map_or(18, |m| m.decimals);

    let mut e = Exit {
        via: if raw.to == Some(atlas) { "atlas" } else { "direct" }.into(),
        gas_used: raw.gas_used,
        gas_price_gwei: raw.gas_price as f64 / 1e9,
        priority_gwei: raw.priority.map(|p| p as f64 / 1e9),
        eoa: Some(raw.from),
        ..Default::default()
    };
    // Entry: liquidations and their caller.
    let mut callers: HashMap<Address, usize> = HashMap::new();
    for l in raw.logs.iter().filter(|l| l.addr == MORPHO && l.topics.first() == Some(&liquidate) && l.topics.len() == 4) {
        *callers.entry(topic_addr(&l.topics[2])).or_default() += 1;
        e.n_liq += 1;
    }
    e.actor = callers.iter().max_by_key(|(_, n)| **n).map(|(a, _)| *a);
    let actors: BTreeSet<Address> = callers.keys().copied().chain([raw.from]).collect();

    // Flash loans and Aave carry.
    let (morpho_flash, aave_flash, bal_flash) = (topic("FlashLoan(address,address,uint256)"), topic("FlashLoan(address,address,address,uint256,uint8,uint256,uint16)"), topic("FlashLoan(address,address,uint256,uint256)"));
    let (aave_supply, aave_borrow) = (topic("Supply(address,address,address,uint256,uint16)"), topic("Borrow(address,address,address,uint256,uint8,uint256,uint16)"));
    let mut supplied: HashMap<Address, U256> = HashMap::new();
    let mut flashed: HashMap<Address, U256> = HashMap::new();
    let (mut aave_borrowed, mut morpho_borrowed, mut morpho_resupply) = (false, false, false);
    for l in &raw.logs {
        let Some(t0) = l.topics.first() else { continue };
        match (l.addr, *t0) {
            (MORPHO, t) if t == morpho_flash => {
                e.flash.push("morpho".into());
                if let Some(tok) = l.topics.get(2) {
                    *flashed.entry(topic_addr(tok)).or_default() += word(&l.data, 0);
                }
            }
            (AAVE_POOL, t) if t == aave_flash => e.flash.push("aave".into()),
            (BALANCER_VAULT, t) if t == bal_flash => e.flash.push("balancer".into()),
            (AAVE_POOL, t) if t == aave_supply && l.topics.len() >= 2 => *supplied.entry(topic_addr(&l.topics[1])).or_default() += word(&l.data, 1),
            (AAVE_POOL, t) if t == aave_borrow => aave_borrowed = true,
            // A liquidator borrowing from / supplying collateral to Morpho in the same tx keeps the
            // seized collateral as its own position.
            (MORPHO, t) if t == crate::morpho::events::BORROW => morpho_borrowed = true,
            (MORPHO, t) if t == crate::morpho::events::SUPPLY_COLLATERAL => morpho_resupply = true,
            _ => {}
        }
    }
    let borrowed = aave_borrowed || morpho_borrowed;

    // Token flows.
    struct T {
        token: Address,
        from: Address,
        to: Address,
        amount: U256,
    }
    let transfers: Vec<T> = raw
        .logs
        .iter()
        .filter(|l| l.topics.first() == Some(&transfer) && l.topics.len() == 3)
        .map(|l| T { token: l.addr, from: topic_addr(&l.topics[1]), to: topic_addr(&l.topics[2]), amount: word(&l.data, 0) })
        .collect();
    let mut morpho_flow: HashMap<Address, (U256, U256)> = HashMap::new(); // (out to actors, in from actors)
    for t in &transfers {
        if t.from == MORPHO && actors.contains(&t.to) {
            morpho_flow.entry(t.token).or_default().0 += t.amount;
        }
        if t.to == MORPHO && actors.contains(&t.from) {
            morpho_flow.entry(t.token).or_default().1 += t.amount;
        }
    }
    // Which tokens are collateral / loan: by symbol from the journal's markets; without them,
    // by net Morpho flow (wrong when collateral goes back into Morpho, hence the markets).
    let coll_syms: BTreeSet<&str> = markets.iter().filter_map(|m| m.split('/').next()).collect();
    let loan_syms: BTreeSet<&str> = markets.iter().filter_map(|m| m.split('/').nth(1)).collect();
    let tokens_seen: BTreeSet<Address> = morpho_flow.keys().copied().collect();
    let (coll_tokens, loans): (BTreeSet<Address>, BTreeSet<Address>) = if coll_syms.is_empty() {
        (tokens_seen.iter().copied().filter(|t| morpho_flow[t].0 > morpho_flow[t].1).collect(), tokens_seen.iter().copied().filter(|t| morpho_flow[t].1 > morpho_flow[t].0).collect())
    } else {
        (tokens_seen.iter().copied().filter(|t| coll_syms.contains(sym(t).as_str())).collect(), tokens_seen.iter().copied().filter(|t| loan_syms.contains(sym(t).as_str())).collect())
    };
    // Seized = what Morpho sent the actor, minus flash-loaned amounts of the same token.
    let collaterals: Vec<(Address, U256, U256)> = coll_tokens
        .iter()
        .map(|t| {
            let (out, back) = morpho_flow[t];
            let seized = out.saturating_sub(flashed.get(t).copied().unwrap_or_default());
            // Sent back to Morpho as the actor's own collateral (SupplyCollateral), not repaid.
            let resupplied = if morpho_resupply { back.min(seized) } else { U256::ZERO };
            (*t, seized, resupplied)
        })
        .filter(|(_, s, _)| !s.is_zero())
        .collect();

    // Venues, in log order: swap emitters (and singletons) plus known contracts in the flows.
    let mut venues: Vec<Venue> = vec![];
    let mut venue_addrs: BTreeSet<Address> = BTreeSet::new();
    let flows_of = |a: Address| {
        let ins: BTreeSet<String> = transfers.iter().filter(|t| t.to == a).map(|t| sym(&t.token)).collect();
        let outs: BTreeSet<String> = transfers.iter().filter(|t| t.from == a).map(|t| sym(&t.token)).collect();
        (ins.into_iter().collect::<Vec<_>>(), outs.into_iter().collect::<Vec<_>>())
    };
    for l in &raw.logs {
        let Some(fam) = l.topics.first().and_then(swap_family) else { continue };
        if !venue_addrs.insert(l.addr) {
            continue;
        }
        let protocol = match pools.get(&l.addr) {
            Some(m) if !m.protocol.is_empty() => m.protocol.clone(),
            _ => fam.to_string(),
        };
        let (token_in, token_out) = flows_of(l.addr);
        venues.push(Venue { protocol, kind: "dex".into(), address: l.addr, token_in, token_out });
    }
    let known = KNOWN.iter().map(|(a, n, k)| (*a, *n, *k)).chain(contracts.iter().map(|(a, n, k)| (*a, n.as_str(), k.as_str())));
    for (addr, name, kind) in known {
        if venue_addrs.contains(&addr) {
            continue;
        }
        if raw.to == Some(addr) || transfers.iter().any(|t| t.from == addr || t.to == addr) {
            venue_addrs.insert(addr);
            let (token_in, token_out) = flows_of(addr);
            venues.push(Venue { protocol: name.into(), kind: kind.into(), address: addr, token_in, token_out });
        }
    }

    // Collateral: seized, sold (sent by the actor to anyone else), carried into Aave, kept.
    let mut sold_usd = 0.0;
    let mut priced = true;
    let mut carry_venues: BTreeSet<&str> = BTreeSet::new();
    for (tok, seized, resupplied) in &collaterals {
        let d = dec(tok);
        let sold_raw: U256 = transfers.iter().filter(|t| t.token == *tok && actors.contains(&t.from) && !actors.contains(&t.to) && t.to != MORPHO).fold(U256::ZERO, |a, t| a + t.amount);
        let to_aave = supplied.get(tok).copied().unwrap_or_default().min(sold_raw);
        if !to_aave.is_zero() {
            carry_venues.insert("aave");
        }
        if !resupplied.is_zero() {
            carry_venues.insert("morpho");
        }
        let carried = to_aave + *resupplied;
        let (seized_u, sold_u, carried_u) = (units(*seized, d), units(sold_raw - to_aave, d), units(carried, d));
        e.collateral.push(sym(tok));
        e.seized += seized_u;
        e.sold += sold_u;
        e.carried += carried_u;
        e.kept += (seized_u - sold_u - carried_u).max(0.0);
        match seized_usd.get(&sym(tok)) {
            Some(usd) if seized_u > 0.0 => sold_usd += usd / seized_u * sold_u,
            _ => priced = false,
        }
    }

    // Loan tokens the actor got back from exit venues (or from unexplained counterparties).
    let lenders: BTreeSet<Address> = [MORPHO].into_iter().chain(e.flash.iter().filter(|f| *f == "balancer").map(|_| BALANCER_VAULT)).collect();
    let mut unknown: BTreeSet<Address> = BTreeSet::new();
    for t in transfers.iter().filter(|t| loans.contains(&t.token) && actors.contains(&t.to) && !actors.contains(&t.from) && !lenders.contains(&t.from)) {
        if !venue_addrs.contains(&t.from) {
            unknown.insert(t.from);
        }
        e.loan_in += units(t.amount, dec(&t.token));
    }
    for t in transfers.iter().filter(|t| actors.contains(&t.from) && !actors.contains(&t.to) && t.to != MORPHO && !venue_addrs.contains(&t.to)) {
        unknown.insert(t.to);
    }
    e.unknown_counterparties = unknown.into_iter().collect();

    let usd_loan = loans.iter().all(|t| sym(t).to_ascii_uppercase().contains("USD"));
    if priced && sold_usd > 0.0 {
        e.oracle_usd = Some(sold_usd);
        // Not when a Balancer flash loan muddles the vault's flows, or borrowed Aave funds count as proceeds.
        if usd_loan && !e.flash.iter().any(|f| f == "balancer") && !borrowed {
            e.slippage_bps = Some((1.0 - e.loan_in / sold_usd) * 1e4);
        }
    }

    let mut kinds: BTreeSet<&str> = venues.iter().map(|v| v.kind.as_str()).collect();
    if borrowed && e.carried > 0.0 {
        for v in &carry_venues {
            kinds.insert(if *v == "aave" { "carry-aave" } else { "carry-morpho" });
        }
    }
    e.kind = if !kinds.is_empty() {
        kinds.into_iter().collect::<Vec<_>>().join("+")
    } else if e.seized > 0.0 && e.kept > 0.5 * e.seized {
        "kept".into()
    } else {
        "unknown".into()
    };
    e.route = venues
        .iter()
        .map(|v| {
            let ins = v.token_in.join("/");
            let outs = v.token_out.join("/");
            if ins.is_empty() && outs.is_empty() { v.protocol.clone() } else { format!("{} {ins}→{outs}", v.protocol) }
        })
        .collect::<Vec<_>>()
        .join(" > ");
    e.venues = venues;
    e
}

impl Exit {
    pub fn venues_json(&self) -> String {
        json!(self.venues).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr_topic(a: Address) -> B256 {
        a.into_word()
    }
    fn amt(x: u64) -> Vec<u8> {
        U256::from(x).to_be_bytes::<32>().to_vec()
    }
    fn transfer(token: Address, from: Address, to: Address, x: u64) -> Log {
        Log { addr: token, topics: vec![topic("Transfer(address,address,uint256)"), addr_topic(from), addr_topic(to)], data: amt(x) }
    }

    #[test]
    fn two_hop_dex_exit_with_morpho_flash_loan() {
        // A typical exit: Morpho flash loan of USDC, a liquidation seizing an alt-coin (ALT),
        // ALT -> WETH on pool A, WETH -> USDC on pool B, repay Morpho.
        let (alt, usdc, weth) = (Address::repeat_byte(1), Address::repeat_byte(2), Address::repeat_byte(3));
        let (actor, eoa, pool_a, pool_b) = (Address::repeat_byte(0xAA), Address::repeat_byte(0xEE), Address::repeat_byte(0xA0), Address::repeat_byte(0xB0));
        let v3 = topic("Swap(address,address,int256,int256,uint160,uint128,int24)");
        let liq = Log { addr: MORPHO, topics: vec![crate::morpho::events::LIQUIDATE, B256::ZERO, addr_topic(actor), addr_topic(Address::repeat_byte(9))], data: vec![] };
        let flash = Log { addr: MORPHO, topics: vec![topic("FlashLoan(address,address,uint256)"), addr_topic(actor), addr_topic(usdc)], data: amt(1_000) };
        let logs = vec![
            transfer(usdc, MORPHO, actor, 1_000_000_000), // flash loan out
            flash,
            transfer(usdc, actor, MORPHO, 900_000_000), // repay the liquidation
            transfer(alt, MORPHO, actor, 800_000_000),  // seize 800 ALT (6 dec)
            liq,
            transfer(alt, actor, pool_a, 800_000_000),
            Log { addr: pool_a, topics: vec![v3, B256::ZERO, B256::ZERO], data: vec![] },
            transfer(weth, pool_a, pool_b, 400_000_000_000_000_000),
            Log { addr: pool_b, topics: vec![v3, B256::ZERO, B256::ZERO], data: vec![] },
            transfer(usdc, pool_b, actor, 990_000_000), // 990 USDC out
            transfer(usdc, actor, MORPHO, 1_000_000_000), // flash loan back
        ];
        let raw = Raw { from: eoa, to: Some(actor), gas_used: 1_925_555, gas_price: 10_000_000, priority: Some(1_000_000), logs };
        let tokens: HashMap<Address, TokenMeta> = [(alt, "ALT", 6), (usdc, "USDC", 6), (weth, "WETH", 18)].into_iter().map(|(a, s, d)| (a, TokenMeta { symbol: s.into(), decimals: d })).collect();
        let pools: HashMap<Address, PoolMeta> = [(pool_a, "slipstream3"), (pool_b, "uniV3")].into_iter().map(|(a, p)| (a, PoolMeta { protocol: p.into() })).collect();
        // The journal valued the 800 ALT at $1,000.
        let e = decode(&raw, Address::ZERO, &pools, &tokens, &["ALT/USDC".to_string()], &[("ALT".to_string(), 1_000.0)].into(), &[]);
        assert_eq!((e.via.as_str(), e.n_liq, e.actor), ("direct", 1, Some(actor)));
        assert_eq!(e.flash, vec!["morpho"]);
        assert_eq!(e.kind, "dex");
        assert_eq!(e.route, "slipstream3 ALT→WETH > uniV3 WETH→USDC");
        assert_eq!((e.seized, e.sold, e.kept), (800.0, 800.0, 0.0));
        assert_eq!(e.loan_in, 990.0, "the flash-loaned USDC from Morpho is not exit proceeds");
        assert!((e.slippage_bps.unwrap() - 100.0).abs() < 1e-9, "990 for $1,000 of collateral = 1%");
        assert!(e.unknown_counterparties.is_empty(), "{:?}", e.unknown_counterparties);
    }

    #[test]
    fn aave_carry_keeps_collateral_unsold() {
        // A carry exit: the seized collateral is supplied to Aave and USDC borrowed to repay Morpho.
        let (alt, usdc, a_alt, a_usdc) = (Address::repeat_byte(1), Address::repeat_byte(2), Address::repeat_byte(0x11), Address::repeat_byte(0x22));
        let solver = Address::repeat_byte(0x5A);
        let liq = Log { addr: MORPHO, topics: vec![crate::morpho::events::LIQUIDATE, B256::ZERO, addr_topic(solver), B256::ZERO], data: vec![] };
        let mut supply_data = amt(0);
        supply_data.extend(amt(800_000_000));
        let logs = vec![
            transfer(alt, MORPHO, solver, 800_000_000),
            liq,
            transfer(alt, solver, a_alt, 800_000_000),
            Log { addr: AAVE_POOL, topics: vec![topic("Supply(address,address,address,uint256,uint16)"), addr_topic(alt), addr_topic(solver)], data: supply_data },
            Log { addr: AAVE_POOL, topics: vec![topic("Borrow(address,address,address,uint256,uint8,uint256,uint16)"), addr_topic(usdc)], data: vec![] },
            transfer(usdc, a_usdc, solver, 900_000_000),
            transfer(usdc, solver, MORPHO, 900_000_000),
        ];
        let raw = Raw { from: Address::repeat_byte(0xEE), to: Some(Address::repeat_byte(0x77)), gas_used: 3_726_889, gas_price: 0, priority: None, logs };
        let tokens: HashMap<Address, TokenMeta> = [(alt, "ALT", 6), (usdc, "USDC", 6)].into_iter().map(|(a, s, d)| (a, TokenMeta { symbol: s.into(), decimals: d })).collect();
        let e = decode(&raw, Address::repeat_byte(0x77), &HashMap::new(), &tokens, &["ALT/USDC".to_string()], &[("ALT".to_string(), 1_000.0)].into(), &[]);
        assert_eq!(e.via, "atlas");
        assert_eq!(e.kind, "carry-aave");
        assert_eq!((e.carried, e.sold), (800.0, 0.0));
        assert_eq!(e.slippage_bps, None, "borrowed USDC is not exit proceeds");
    }

    #[test]
    fn morpho_carry_resupplies_the_seized_collateral() {
        // A Morpho carry exit: seize the collateral, supply it straight back to Morpho as the
        // liquidator's own collateral, borrow USDC from Morpho, repay. Net Morpho flow of it is 0.
        let (altb, usdc) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let solver = Address::repeat_byte(0x5A);
        let ev = |t: B256| Log { addr: MORPHO, topics: vec![t, B256::ZERO, addr_topic(solver), B256::ZERO], data: vec![] };
        let logs = vec![
            transfer(altb, MORPHO, solver, 5_000_000_000),
            ev(crate::morpho::events::LIQUIDATE),
            transfer(altb, solver, MORPHO, 5_000_000_000),
            ev(crate::morpho::events::SUPPLY_COLLATERAL),
            ev(crate::morpho::events::BORROW),
            transfer(usdc, MORPHO, solver, 1_900_000_000),
            transfer(usdc, solver, MORPHO, 1_900_000_000),
        ];
        let raw = Raw { from: Address::repeat_byte(0xEE), to: Some(Address::repeat_byte(0x77)), gas_used: 1_070_000, gas_price: 0, priority: None, logs };
        let tokens: HashMap<Address, TokenMeta> = [(altb, "ALTB", 6), (usdc, "USDC", 6)].into_iter().map(|(a, s, d)| (a, TokenMeta { symbol: s.into(), decimals: d })).collect();
        let e = decode(&raw, Address::repeat_byte(0x77), &HashMap::new(), &tokens, &["ALTB/USDC".to_string()], &[("ALTB".to_string(), 2_100.0)].into(), &[]);
        assert_eq!(e.kind, "carry-morpho");
        assert_eq!((e.seized, e.carried, e.sold, e.kept), (5_000.0, 5_000.0, 0.0, 0.0));
        assert_eq!(e.collateral, vec!["ALTB"]);
        assert_eq!(e.loan_in, 0.0, "USDC borrowed from Morpho is not exit proceeds");
    }
}
