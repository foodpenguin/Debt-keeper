//! On-chain reads: Morpho market/position state, oracle prices, IRM rates, via Multicall3.

use alloy::{
    eips::BlockId,
    primitives::{Address, B256, Bytes, U256},
    providers::Provider,
    sol,
    sol_types::SolCall,
};
use anyhow::{Context, Result, bail};

use super::math::{self, BorrowTotals};

pub use crate::chain::multicall;


sol! {
    #[derive(Debug, PartialEq, Eq)]
    struct MarketParams { address loanToken; address collateralToken; address oracle; address irm; uint256 lltv; }
    #[derive(Debug, Default)]
    struct MarketState { uint128 totalSupplyAssets; uint128 totalSupplyShares; uint128 totalBorrowAssets; uint128 totalBorrowShares; uint128 lastUpdate; uint128 fee; }
    #[derive(Debug, Default)]
    struct PositionState { uint256 supplyShares; uint128 borrowShares; uint128 collateral; }

    interface IMorpho {
        function idToMarketParams(bytes32 id) external view returns (MarketParams memory);
        function market(bytes32 id) external view returns (MarketState memory);
        function position(bytes32 id, address user) external view returns (PositionState memory);
        function liquidate(MarketParams marketParams, address borrower, uint256 seizedAssets, uint256 repaidShares, bytes data) external returns (uint256, uint256);
    }
    interface IOracle { function price() external view returns (uint256); }
    interface IIrm { function borrowRateView(MarketParams memory marketParams, MarketState memory market) external view returns (uint256); }
    interface IERC20Meta {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
    }
}

/// Static facts about a market, loaded once.
#[derive(Clone, Debug)]
pub struct MarketMeta {
    pub id: B256,
    pub name: String,
    pub params: MarketParams,
    pub loan_decimals: u8,
    pub collateral_decimals: u8,
    pub loan_symbol: String,
    pub collateral_symbol: String,
}

/// Per-block market state, with interest accrued up to the block timestamp.
#[derive(Clone, Debug)]
#[allow(dead_code)] // `raw` is kept for the simulator / debugging
pub struct MarketSnapshot {
    pub price: U256,
    pub totals: BorrowTotals,
    pub raw: MarketState,
}

pub async fn load_market_meta<P: Provider>(p: &P, morpho: Address, id: B256, name: &str) -> Result<MarketMeta> {
    let block = BlockId::latest();
    let r = multicall(p, &[(morpho, IMorpho::idToMarketParamsCall { id }.abi_encode().into())], block).await?;
    let params = IMorpho::idToMarketParamsCall::abi_decode_returns(r[0].as_ref().context("idToMarketParams failed")?)?;
    if params.loanToken == Address::ZERO {
        bail!("market {id} not found on Morpho");
    }
    let calls = vec![
        (params.loanToken, IERC20Meta::decimalsCall {}.abi_encode().into()),
        (params.collateralToken, IERC20Meta::decimalsCall {}.abi_encode().into()),
        (params.loanToken, IERC20Meta::symbolCall {}.abi_encode().into()),
        (params.collateralToken, IERC20Meta::symbolCall {}.abi_encode().into()),
    ];
    let r = multicall(p, &calls, block).await?;
    let dec = |i: usize| r[i].as_ref().and_then(|b| IERC20Meta::decimalsCall::abi_decode_returns(b).ok()).unwrap_or(18);
    let sym = |i: usize| r[i].as_ref().and_then(|b| IERC20Meta::symbolCall::abi_decode_returns(b).ok()).unwrap_or_else(|| "?".into());
    Ok(MarketMeta {
        id,
        name: name.to_string(),
        loan_decimals: dec(0),
        collateral_decimals: dec(1),
        loan_symbol: sym(2),
        collateral_symbol: sym(3),
        params,
    })
}

/// Loads market state + oracle price + IRM rate for many markets in one round trip,
/// then accrues interest to `timestamp`.
///
/// The IRM rate needs a market state as input. When `prev` (last block's snapshots) is given,
/// the rate is computed from that state inside the same multicall, so a block costs one round
/// trip instead of two; over a ~2 s accrual window the stale-rate error is negligible.
pub async fn load_snapshots<P: Provider>(
    p: &P,
    morpho: Address,
    metas: &[MarketMeta],
    block: BlockId,
    timestamp: u64,
    prev: Option<&[Option<MarketSnapshot>]>,
) -> Result<Vec<Option<MarketSnapshot>>> {
    let irm_call = |m: &MarketMeta, state: &MarketState| -> (Address, Bytes) {
        (m.params.irm, IIrm::borrowRateViewCall { marketParams: m.params.clone(), market: state.clone() }.abi_encode().into())
    };
    let mut calls = Vec::with_capacity(metas.len() * 3);
    // Index of the in-round IRM call for each market, when prev state was available.
    let mut irm_index: Vec<Option<usize>> = vec![None; metas.len()];
    let mut layout = Vec::with_capacity(metas.len());
    for (i, m) in metas.iter().enumerate() {
        let base = calls.len();
        calls.push((morpho, IMorpho::marketCall { id: m.id }.abi_encode().into()));
        calls.push((m.params.oracle, IOracle::priceCall {}.abi_encode().into()));
        if m.params.irm != Address::ZERO
            && let Some(Some(prev_snap)) = prev.and_then(|p| p.get(i))
        {
            irm_index[i] = Some(calls.len());
            calls.push(irm_call(m, &prev_snap.raw));
        }
        layout.push(base);
    }
    let r = multicall(p, &calls, block).await?;
    let mut states = Vec::with_capacity(metas.len());
    for &base in &layout {
        let state = r[base].as_ref().and_then(|b| IMorpho::marketCall::abi_decode_returns(b).ok());
        let price = r[base + 1].as_ref().and_then(|b| IOracle::priceCall::abi_decode_returns(b).ok());
        states.push((state, price));
    }
    let mut rates: Vec<Option<U256>> = irm_index
        .iter()
        .map(|j| j.and_then(|j| r[j].as_ref()).and_then(|b| IIrm::borrowRateViewCall::abi_decode_returns(b).ok()))
        .collect();

    // Second round only for markets with no previous state (first block) or a failed rate call.
    let mut missing = Vec::new();
    let mut second = Vec::new();
    for (i, m) in metas.iter().enumerate() {
        if rates[i].is_none() && m.params.irm != Address::ZERO
            && let (Some(state), _) = &states[i]
        {
            missing.push(i);
            second.push(irm_call(m, state));
        }
    }
    if !second.is_empty() {
        let r2 = multicall(p, &second, block).await?;
        for (k, i) in missing.into_iter().enumerate() {
            rates[i] = r2[k].as_ref().and_then(|b| IIrm::borrowRateViewCall::abi_decode_returns(b).ok());
        }
    }

    let mut out = Vec::with_capacity(metas.len());
    for (i, (state, price)) in states.into_iter().enumerate() {
        let (Some(state), Some(price)) = (state, price) else {
            out.push(None);
            continue;
        };
        let rate = rates[i].unwrap_or(U256::ZERO);
        let last = state.lastUpdate as u64;
        let elapsed = timestamp.saturating_sub(last);
        let totals = math::accrue_borrow(U256::from(state.totalBorrowAssets), U256::from(state.totalBorrowShares), rate, elapsed);
        out.push(Some(MarketSnapshot { price, totals, raw: state }));
    }
    Ok(out)
}

/// Batched position reads. Returns (borrowShares, collateral) per (market, user); None if the call failed.
pub async fn load_positions<P: Provider>(
    p: &P,
    morpho: Address,
    keys: &[(B256, Address)],
    block: BlockId,
) -> Result<Vec<Option<(U256, U256)>>> {
    let calls: Vec<(Address, Bytes)> =
        keys.iter().map(|(id, user)| (morpho, IMorpho::positionCall { id: *id, user: *user }.abi_encode().into())).collect();
    let r = multicall(p, &calls, block).await?;
    Ok(r.into_iter()
        .map(|b| {
            b.and_then(|b| IMorpho::positionCall::abi_decode_returns(&b).ok())
                .map(|pos| (U256::from(pos.borrowShares), U256::from(pos.collateral)))
        })
        .collect())
}
