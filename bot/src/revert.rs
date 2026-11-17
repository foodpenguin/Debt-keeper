//! Human-readable revert reasons for the liquidator / solver contracts. A reverted simulation
//! carries the raw revert blob (often a custom error, or a wrapper error whose `reason` is the
//! inner contract's own revert); decoding it turns journal lines like `CallFailed(0x5c0dee5d)` into
//! `CallFailed(#0, NotFunded)`. Concrete to these contracts' errors — kept out of the generic trait
//! layer (`protocol`/`venue`/`dex`), per the framework rule.

use alloy::{
    primitives::Bytes,
    sol,
    sol_types::SolError,
};

sol! {
    // Wrappers whose `reason` is the inner contract's revert (recursed into).
    error CallFailed(uint256 index, bytes reason);       // AtlasLiquidationSolver
    error SwapFailed(uint256 index, bytes reason);        // MorphoLiquidator / FlashLiquidator
    error BidSwapFailed(bytes reason);                    // AtlasLiquidationSolver
    error LiquidationFailed(bytes reason);                // FlashLiquidator
    // Errors with values.
    error TargetNotAllowed(address target);
    error ProtocolNotAllowed(address target);
    error CollateralShortfall(uint256 balance, uint256 expected);
    error RouteSpendMismatch(uint256 spent, uint256 seized);
    error InsufficientProfit(uint256 profit, uint256 minProfit);
    error UnsupportedBidToken(address token);
    error TokenCallFailed(address token);
    // Zero-argument errors.
    error NotFunded();
    error NothingSeized();
    error Unauthorized();
    error Expired();
    error IsPaused();
    error BadRoute();
    error BadBatch();
    error UnsupportedMode();
    error InvalidEntry();
    error InvalidCaller();
    error EthTransferFailed();
    error FundingDisabled();
    // Solidity standard.
    error Error(string message);
    error Panic(uint256 code);
}

/// A revert blob as readable text, recursing into the wrapper errors' nested `reason`. Unknown
/// selectors fall back to the raw hex (never dropped), empty data to an out-of-gas-style note.
pub fn decode_revert(data: &[u8]) -> String {
    if data.len() < 4 {
        return if data.is_empty() { "empty revert (out of gas / invalid opcode?)".into() } else { format!("0x{}", alloy::hex::encode(data)) };
    }
    let sel: [u8; 4] = data[..4].try_into().unwrap();
    // Wrappers: recurse into the inner revert they carry.
    if sel == CallFailed::SELECTOR && let Ok(e) = CallFailed::abi_decode(data) {
        return format!("CallFailed(#{}, {})", e.index, decode_revert(&e.reason));
    }
    if sel == SwapFailed::SELECTOR && let Ok(e) = SwapFailed::abi_decode(data) {
        return format!("SwapFailed(#{}, {})", e.index, decode_revert(&e.reason));
    }
    if sel == BidSwapFailed::SELECTOR && let Ok(e) = BidSwapFailed::abi_decode(data) {
        return format!("BidSwapFailed({})", decode_revert(&e.reason));
    }
    if sel == LiquidationFailed::SELECTOR && let Ok(e) = LiquidationFailed::abi_decode(data) {
        return format!("LiquidationFailed({})", decode_revert(&e.reason));
    }
    // Standard Error(string) / Panic(uint256).
    if sel == Error::SELECTOR && let Ok(e) = Error::abi_decode(data) {
        return format!("Error({:?})", e.message);
    }
    if sel == Panic::SELECTOR && let Ok(e) = Panic::abi_decode(data) {
        return format!("Panic({:#x})", e.code);
    }
    // Errors with values.
    if sel == TargetNotAllowed::SELECTOR && let Ok(e) = TargetNotAllowed::abi_decode(data) {
        return format!("TargetNotAllowed({})", e.target);
    }
    if sel == ProtocolNotAllowed::SELECTOR && let Ok(e) = ProtocolNotAllowed::abi_decode(data) {
        return format!("ProtocolNotAllowed({})", e.target);
    }
    if sel == CollateralShortfall::SELECTOR && let Ok(e) = CollateralShortfall::abi_decode(data) {
        return format!("CollateralShortfall(balance={}, expected={})", e.balance, e.expected);
    }
    if sel == RouteSpendMismatch::SELECTOR && let Ok(e) = RouteSpendMismatch::abi_decode(data) {
        return format!("RouteSpendMismatch(spent={}, seized={})", e.spent, e.seized);
    }
    if sel == InsufficientProfit::SELECTOR && let Ok(e) = InsufficientProfit::abi_decode(data) {
        return format!("InsufficientProfit(profit={}, min={})", e.profit, e.minProfit);
    }
    if sel == UnsupportedBidToken::SELECTOR && let Ok(e) = UnsupportedBidToken::abi_decode(data) {
        return format!("UnsupportedBidToken({})", e.token);
    }
    if sel == TokenCallFailed::SELECTOR && let Ok(e) = TokenCallFailed::abi_decode(data) {
        return format!("TokenCallFailed({})", e.token);
    }
    // Zero-argument errors: name by selector.
    for (s, name) in [
        (NotFunded::SELECTOR, "NotFunded"),
        (NothingSeized::SELECTOR, "NothingSeized"),
        (Unauthorized::SELECTOR, "Unauthorized"),
        (Expired::SELECTOR, "Expired"),
        (IsPaused::SELECTOR, "IsPaused"),
        (BadRoute::SELECTOR, "BadRoute"),
        (BadBatch::SELECTOR, "BadBatch"),
        (UnsupportedMode::SELECTOR, "UnsupportedMode"),
        (InvalidEntry::SELECTOR, "InvalidEntry"),
        (InvalidCaller::SELECTOR, "InvalidCaller"),
        (EthTransferFailed::SELECTOR, "EthTransferFailed"),
        (FundingDisabled::SELECTOR, "FundingDisabled"),
    ] {
        if sel == s {
            return name.into();
        }
    }
    format!("0x{}", alloy::hex::encode(data))
}

/// The trailing revert hex of a sim error (`SimReverted` formats its message as "... 0x<data>").
fn revert_hex(err: &str) -> Option<Bytes> {
    err.rsplit(char::is_whitespace).find(|t| t.starts_with("0x") && t.len() > 2).and_then(|t| t.parse().ok())
}

/// A sim error as a journal string: the decoded revert reason when the error carries revert data,
/// followed by the full error (capped so a pathological blob cannot bloat the journal). Never
/// truncates the revert hex itself to a few hundred chars the way the old 200-char cut did.
pub fn explain(e: &anyhow::Error) -> String {
    let full: String = format!("{e:#}").chars().take(4096).collect();
    match revert_hex(&full) {
        Some(data) => format!("{} [{full}]", decode_revert(&data)),
        None => full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, U256};

    #[test]
    fn decodes_nested_and_valued_errors() {
        // CallFailed(#0, NotFunded): a solver call whose inner MorphoLiquidator reverted NotFunded.
        let inner = NotFunded {}.abi_encode();
        let outer = CallFailed { index: U256::ZERO, reason: inner.into() }.abi_encode();
        assert_eq!(decode_revert(&outer), "CallFailed(#0, NotFunded)");

        // BidSwapFailed wrapping a plain Error(string).
        let es = Error { message: "STF".into() }.abi_encode();
        let bid = BidSwapFailed { reason: es.into() }.abi_encode();
        assert_eq!(decode_revert(&bid), r#"BidSwapFailed(Error("STF"))"#);

        // A valued error.
        let cs = CollateralShortfall { balance: U256::from(90u64), expected: U256::from(100u64) }.abi_encode();
        assert_eq!(decode_revert(&cs), "CollateralShortfall(balance=90, expected=100)");

        // A zero-arg error by selector.
        assert_eq!(decode_revert(&RouteSpendMismatch { spent: U256::from(1u64), seized: U256::from(2u64) }.abi_encode()), "RouteSpendMismatch(spent=1, seized=2)");
        assert_eq!(decode_revert(&TargetNotAllowed { target: Address::repeat_byte(0xaa) }.abi_encode()), format!("TargetNotAllowed({})", Address::repeat_byte(0xaa)));

        // Unknown selector -> raw hex; empty -> note.
        assert_eq!(decode_revert(&[0xde, 0xad, 0xbe, 0xef]), "0xdeadbeef");
        assert!(decode_revert(&[]).contains("empty revert"));
    }

    #[test]
    fn explain_pulls_the_revert_hex_out_of_a_sim_error() {
        let data = BidSwapFailed { reason: Bytes::new() }.abi_encode();
        let msg = format!("sim reverted: execution reverted 0x{}", alloy::hex::encode(&data));
        let e = anyhow::anyhow!("{msg}");
        let out = explain(&e);
        assert!(out.starts_with("BidSwapFailed("), "{out}");
        assert!(out.contains("sim reverted"), "keeps the full error too: {out}");
    }
}
