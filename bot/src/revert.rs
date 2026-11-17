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

/// Longest error text kept in the journal after the decoded reason (a pathological blob cannot bloat
/// it). Decoding reads the revert data before this cut, so it never truncates what gets decoded.
const EXPLAIN_TEXT_CAP: usize = 4096;

/// One whitespace-separated token as revert hex: surrounding quotes/punctuation (`"0x…"`, `0x…,`,
/// `(0x…)`) are trimmed. `allow_empty` accepts a bare "0x" (an empty revert).
fn hex_token(t: &str, allow_empty: bool) -> Option<Bytes> {
    let t = t.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    (t.starts_with("0x") && (allow_empty || t.len() > 2)).then(|| t.parse().ok()).flatten()
}

/// The revert data a sim error carries. A `SimReverted` anywhere in the chain formats its message as
/// "<reason> 0x<data>": its last token is the data (possibly an empty "0x"). Any other error (e.g. a
/// node error quoting the data) falls back to the last parseable 0x token in the text.
fn revert_data(e: &anyhow::Error) -> Option<Bytes> {
    if let Some(s) = e.chain().find_map(|c| c.downcast_ref::<crate::venue::SimReverted>())
        && let Some(d) = s.message.rsplit(char::is_whitespace).next().and_then(|t| hex_token(t, true))
    {
        return Some(d);
    }
    format!("{e:#}").rsplit(char::is_whitespace).find_map(|t| hex_token(t, false))
}

/// A sim error as a journal string: the decoded revert reason when the error carries revert data,
/// followed by the error text (capped at EXPLAIN_TEXT_CAP chars). The revert data is decoded in full,
/// whatever its size; only the trailing text is cut.
pub fn explain(e: &anyhow::Error) -> String {
    let text: String = format!("{e:#}").chars().take(EXPLAIN_TEXT_CAP).collect();
    match revert_data(e) {
        Some(data) => format!("{} [{text}]", decode_revert(&data)),
        None => text,
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

    #[test]
    fn a_large_revert_is_decoded_in_full_not_cut_by_the_text_cap() {
        // ~3 KB of revert data = ~6 K hex chars, past the 4096-char text cap: decoding must read the
        // data itself, not the capped text (which cuts the hex mid-way).
        let reason = "x".repeat(3000);
        let data = Error { message: reason.clone() }.abi_encode();
        let sim = crate::venue::SimReverted { message: format!("execution reverted 0x{}", alloy::hex::encode(&data)), block_number: None, probe: None };
        let e = anyhow::Error::new(sim).context("batch op sim");
        let out = explain(&e);
        assert!(out.starts_with(&format!("Error({reason:?})")), "decoded in full: {}", &out[..80.min(out.len())]);
        assert!(out.len() < reason.len() + 4096 + 64, "the trailing text stays capped");
    }

    #[test]
    fn revert_data_is_found_through_quotes_punctuation_and_empty_reverts() {
        // A node error that quotes the data (not a SimReverted): the token is `"0x…"`.
        let nf = alloy::hex::encode(NotFunded {}.abi_encode());
        let e = anyhow::anyhow!("server returned an error response: error code 3: execution reverted, data: \"0x{nf}\"");
        assert!(explain(&e).starts_with("NotFunded"), "{}", explain(&e));
        // Trailing punctuation on the data token (a chained message ending in "0x….").
        let e = anyhow::anyhow!("execution reverted: 0x{nf}.");
        assert!(explain(&e).starts_with("NotFunded"), "{}", explain(&e));
        // An empty revert from a SimReverted reads as such, not as the raw text.
        let sim = crate::venue::SimReverted { message: "execution reverted 0x".into(), block_number: None, probe: None };
        assert!(explain(&anyhow::Error::new(sim)).starts_with("empty revert"));
    }
}
