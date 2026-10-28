//! Incremental position tracking from Morpho events: any event that can change a
//! position's debt or collateral marks the involved addresses for a re-read.
//! Liquidate events are also decoded in full for loss analysis.


use alloy::{
    primitives::{Address, B256, U256, b256},
    providers::Provider,
    rpc::types::Filter,
};
use anyhow::Result;
use serde_json::Value;

use crate::protocol::Touched;

// keccak256 of the Morpho Blue event signatures
pub const BORROW: B256 = b256!("570954540bed6b1304a87dfe815a5eda4a648f7097a16240dcd85c9b5fd42a43");
const REPAY: B256 = b256!("52acb05cebbd3cd39715469f22afbf5a17496295ef3bc9bb5944056c63ccaa09");
pub const SUPPLY_COLLATERAL: B256 = b256!("a3b9472a1399e17e123f3c2e6586c23e504184d504de59cdaa2b375e880c6184");
const WITHDRAW_COLLATERAL: B256 = b256!("e80ebd7cc9223d7382aab2e0d1d6155c65651f83d53c8b9b06901d167e321142");
pub const LIQUIDATE: B256 = b256!("a4946ede45d0c6f06a0f5ce92c9ad3b4751452d2fe0e25010783bcab57a67e41");

pub const POSITION_EVENTS: [B256; 5] = [BORROW, REPAY, SUPPLY_COLLATERAL, WITHDRAW_COLLATERAL, LIQUIDATE];

/// A real liquidation that happened on-chain (someone else's, or ours later).
#[derive(Debug, Clone)]
pub struct LiquidationEvent {
    /// `caller` topic: usually the liquidator's contract.
    pub liquidator: Address,
    pub borrower: Address,
    pub repaid_assets: U256,
    pub seized_assets: U256,
    pub bad_debt_assets: U256,
    pub block: u64,
    pub tx: B256,
    pub tx_index: u64,
}

/// Events in [from, to], fetched in chunks of at most `chunk` blocks (endpoints cap the range).
/// Positions: market id -> addresses (both indexed address topics, to be safe).
pub async fn touched_positions<P: Provider>(p: &P, morpho: Address, markets: &[B256], from: u64, to: u64, chunk: u64) -> Result<Touched<LiquidationEvent>> {
    let mut logs = Vec::new();
    let mut start = from;
    while start <= to {
        let end = (start + chunk.max(1) - 1).min(to);
        let filter = Filter::new()
            .address(morpho)
            .event_signature(POSITION_EVENTS.to_vec())
            .topic1(markets.to_vec())
            .from_block(start)
            .to_block(end);
        logs.extend(p.get_logs(&filter).await?);
        start = end + 1;
    }
    let mut out = Touched::default();
    for log in logs {
        let topics = log.topics();
        if topics.len() < 4 {
            continue;
        }
        let set = out.positions.entry(topics[1]).or_default();
        for t in &topics[2..4] {
            set.insert(Address::from_word(*t));
        }
        if topics[0] == LIQUIDATE {
            // data: repaidAssets, repaidShares, seizedAssets, badDebtAssets, badDebtShares
            let data = log.data().data.as_ref();
            let word = |i: usize| data.get(i * 32..(i + 1) * 32).map(U256::from_be_slice).unwrap_or_default();
            out.liquidations.push((topics[1], LiquidationEvent {
                liquidator: Address::from_word(topics[2]),
                borrower: Address::from_word(topics[3]),
                repaid_assets: word(0),
                seized_assets: word(2),
                bad_debt_assets: word(3),
                block: log.block_number.unwrap_or_default(),
                tx: log.transaction_hash.unwrap_or_default(),
                tx_index: log.transaction_index.unwrap_or_default(),
            }));
        }
    }
    Ok(out)
}

/// Who sent a transaction and what it paid: (from EOA, priority fee per gas, total fee incl. L1, in wei).
pub async fn tx_cost<P: Provider>(p: &P, tx: B256) -> Result<(Address, u128, U256)> {
    // Raw JSON so the OP-stack `l1Fee` receipt field is available.
    let r: Value = p.raw_request("eth_getTransactionReceipt".into(), (tx,)).await?;
    let t: Value = p.raw_request("eth_getTransactionByHash".into(), (tx,)).await?;
    let hex = |v: &Value| v.as_str().and_then(|s| U256::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or_default();
    let from: Address = r["from"].as_str().unwrap_or_default().parse().unwrap_or_default();
    let fee = hex(&r["gasUsed"]) * hex(&r["effectiveGasPrice"]) + hex(&r["l1Fee"]);
    let prio = hex(&t["maxPriorityFeePerGas"]).to::<u128>();
    Ok((from, prio, fee))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::keccak256;

    #[test]
    fn event_topics_match_signatures() {
        let id = "bytes32";
        assert_eq!(BORROW, keccak256(format!("Borrow({id},address,address,address,uint256,uint256)")));
        assert_eq!(REPAY, keccak256(format!("Repay({id},address,address,uint256,uint256)")));
        assert_eq!(SUPPLY_COLLATERAL, keccak256(format!("SupplyCollateral({id},address,address,uint256)")));
        assert_eq!(WITHDRAW_COLLATERAL, keccak256(format!("WithdrawCollateral({id},address,address,address,uint256)")));
        assert_eq!(LIQUIDATE, keccak256(format!("Liquidate({id},address,address,uint256,uint256,uint256,uint256,uint256)")));
    }
}
