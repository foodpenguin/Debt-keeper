//! The solver EOA's atlETH bond on Atlas, now that solver ops pay their own gas out of their
//! proceeds (`gasPayWei`, atlas_venue.rs): Atlas credits the bond with what an op paid beyond its
//! real gas charge and takes any shortfall from it, so the bond drifts around where it was put
//! instead of shrinking with every win. Concrete to Atlas (kept out of the generic trait layer).
//!
//! - Reads: the bond state (bonded, unbonding, unbonded balance, when an unbonding completes), and
//!   the DappControl's gas terms that enter every solver's maximum liability.
//! - The band `[atlas.solver] bond_min_eth` / `bond_max_eth`: below the min no op is submitted; above
//!   the max the excess is due to leave the bond. `next_step` decides what to do about it: alert, or
//!   (with `bond_auto_withdraw`) unbond the excess, wait until Atlas lets it go (`ESCROW_DURATION`
//!   blocks after the account's last bond change: a charge to the bond during the wait restarts
//!   it, so the chain's `unbondingCompleteBlock` is what counts), withdraw it and send the ETH on to
//!   PROFIT_RECEIVER. One step at a time, each a tx from the hot wallet through the executor.
//! - Our ops' results: Atlas's `SolverTxResult` logs for the signer, so the ledger can tell won ops
//!   (and the gas they paid) from failed ones.

use alloy::{
    eips::BlockId,
    primitives::{Address, B256, Bytes, U256},
    providers::Provider,
    rpc::types::Filter,
    sol,
    sol_types::{SolCall, SolEvent},
};
use anyhow::{Context, Result};

use crate::morpho::onchain::multicall;

sol! {
    interface IAtlasBond {
        function balanceOf(address account) external view returns (uint256);
        function balanceOfBonded(address account) external view returns (uint256);
        function balanceOfUnbonding(address account) external view returns (uint256);
        function unbondingCompleteBlock(address account) external view returns (uint256);
        function unbond(uint256 amount) external;
        function withdraw(uint256 amount) external;
    }
    interface IDappGas {
        function getDAppGasLimit() external view returns (uint32);
        function getBundlerSurchargeRate() external view returns (uint24);
    }
    event SolverTxResult(address indexed solverTo, address indexed solverFrom, address indexed dAppControl, address bidToken, uint256 bidAmount, bool executed, bool success, uint256 result);
}

/// Executor key group of the bond's own txs (unbond, withdraw, the ETH sent on): one at a time, and
/// told apart from liquidations when they settle. Far from any market index.
pub const BOND_TX_GROUP: u64 = 0xb00d_0000;
/// Key ids of the three steps (last byte).
pub const TAG_UNBOND: u8 = 1;
pub const TAG_WITHDRAW: u8 = 2;
pub const TAG_SEND: u8 = 3;
/// Gas limits of the bond's txs (an atlETH unbond / withdraw is ~40-60k; sending ETH to a cold
/// wallet that may be a contract wallet leaves room above 21k).
pub const BOND_TX_GAS: u64 = 150_000;
pub const SEND_GAS: u64 = 60_000;

/// The DappControl's terms in every solver's maximum liability besides its own gas.
#[derive(Debug, Clone, Copy)]
pub struct DappGas {
    pub dapp_gas_limit: u64,
    /// The bundler surcharge (share, e.g. 0.0), added to Atlas's own.
    pub bundler_surcharge: f64,
}

/// Reads the DappControl's dappGasLimit and bundler surcharge rate (scale 1e4).
pub async fn read_dapp_gas<P: Provider>(p: &P, control: Address) -> Result<DappGas> {
    let calls: Vec<(Address, Bytes)> = vec![(control, IDappGas::getDAppGasLimitCall {}.abi_encode().into()), (control, IDappGas::getBundlerSurchargeRateCall {}.abi_encode().into())];
    let r = multicall(p, &calls, BlockId::latest()).await.context("DappControl gas terms")?;
    let limit = r[0].as_ref().and_then(|b| IDappGas::getDAppGasLimitCall::abi_decode_returns(b).ok()).context("DappControl.getDAppGasLimit()")?;
    let rate = r[1].as_ref().and_then(|b| IDappGas::getBundlerSurchargeRateCall::abi_decode_returns(b).ok()).context("DappControl.getBundlerSurchargeRate()")?;
    Ok(DappGas { dapp_gas_limit: limit as u64, bundler_surcharge: rate.to::<u64>() as f64 / 10_000.0 })
}

/// The signer's atlETH at one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BondState {
    pub bonded: U256,
    /// Unbonding (not usable for new ops' liability, still charged if needed).
    pub unbonding: U256,
    /// Unbonded, withdrawable at once.
    pub free: U256,
    /// The block after which an unbonding may be withdrawn (last bond change + ESCROW_DURATION).
    pub complete_block: u64,
    pub block: u64,
}

/// Reads the signer's bond state (one multicall).
pub async fn read_bond_state<P: Provider>(p: &P, atlas: Address, account: Address, block: u64) -> Result<BondState> {
    let calls: Vec<(Address, Bytes)> = vec![
        (atlas, IAtlasBond::balanceOfBondedCall { account }.abi_encode().into()),
        (atlas, IAtlasBond::balanceOfUnbondingCall { account }.abi_encode().into()),
        (atlas, IAtlasBond::balanceOfCall { account }.abi_encode().into()),
        (atlas, IAtlasBond::unbondingCompleteBlockCall { account }.abi_encode().into()),
    ];
    let r = multicall(p, &calls, BlockId::number(block)).await.context("Atlas bond state")?;
    let word = |i: usize| r[i].as_ref().filter(|b| b.len() >= 32).map(|b| U256::from_be_slice(&b[..32])).with_context(|| format!("Atlas bond state: read {i} failed"));
    Ok(BondState { bonded: word(0)?, unbonding: word(1)?, free: word(2)?, complete_block: word(3)?.try_into().unwrap_or(u64::MAX), block })
}

/// What the bond needs next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BondStep {
    /// Within the band, nothing under way.
    Idle,
    /// Below `bond_min_eth`: no op is submitted until it is topped up (an operator step).
    BelowMin,
    /// Above `bond_max_eth` with auto-withdraw off: alert only.
    AboveMax { excess: U256 },
    /// Start: unbond this excess.
    Unbond(U256),
    /// An unbonding is under way: withdrawable after this block (Atlas's release block plus a margin).
    Wait { until_block: u64 },
    /// Withdraw this unbonding atlETH to the hot wallet.
    Withdraw(U256),
    /// Send this ETH, withdrawn earlier, on to PROFIT_RECEIVER.
    Send(U256),
    /// Auto-withdraw paused: the signer holds unbonded atlETH it did not unbond (an operator's,
    /// e.g. a `deposit` not yet bonded). `withdraw` takes that balance first, so the bot would sweep
    /// it; it waits until the operator bonds or withdraws it.
    OperatorFunds { free: U256 },
}

/// Blocks waited past Atlas's release block (`unbondingCompleteBlock`) before withdrawing: a charge
/// to the bond between our reading and the withdraw's inclusion restarts Atlas's wait and makes the
/// withdraw revert; a small margin makes that unlikely (it costs only a few seconds).
pub const RELEASE_MARGIN_BLOCKS: u64 = 5;

/// The next step for the bond, given its state, the band and what is pending from an earlier step
/// (ETH withdrawn but not yet sent on). One step at a time: nothing new starts while an unbonding or
/// a withdrawn amount is pending. Below the min nothing moves out (an operator decides), and atlETH
/// the bot did not unbond is never swept.
pub fn next_step(s: &BondState, min_wei: U256, max_wei: U256, auto_withdraw: bool, pending_send: Option<U256>) -> BondStep {
    if let Some(v) = pending_send.filter(|v| !v.is_zero()) {
        return BondStep::Send(v);
    }
    if s.bonded < min_wei {
        return BondStep::BelowMin;
    }
    if auto_withdraw && !s.unbonding.is_zero() {
        if !s.free.is_zero() {
            return BondStep::OperatorFunds { free: s.free };
        }
        let until_block = s.complete_block.saturating_add(RELEASE_MARGIN_BLOCKS);
        if s.block <= until_block {
            return BondStep::Wait { until_block };
        }
        return BondStep::Withdraw(s.unbonding);
    }
    if s.bonded > max_wei {
        let excess = s.bonded - max_wei;
        return match (auto_withdraw, s.free.is_zero()) {
            (true, true) => BondStep::Unbond(excess),
            (true, false) => BondStep::OperatorFunds { free: s.free },
            (false, _) => BondStep::AboveMax { excess },
        };
    }
    BondStep::Idle
}

/// What to do about a bond tx sent earlier that the executor is no longer tracking (a restart, or
/// one it reported dropped), from its receipt (`Some(success)` = mined).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondTxCheck {
    /// Mined: apply its effect (if it succeeded) and forget it.
    Settled { success: bool },
    /// No receipt yet, still within its deadline plus a grace: look again at the next check.
    Pending,
    /// No receipt well past its deadline: it never landed (replaced or expired); forget it.
    Gone,
}

/// Blocks past a bond tx's deadline after which a missing receipt means it never landed.
pub const BOND_TX_GRACE_BLOCKS: u64 = 100;

pub fn check_bond_tx(receipt: Option<bool>, deadline_block: u64, block: u64) -> BondTxCheck {
    match receipt {
        Some(success) => BondTxCheck::Settled { success },
        None if block > deadline_block + BOND_TX_GRACE_BLOCKS => BondTxCheck::Gone,
        None => BondTxCheck::Pending,
    }
}

pub fn unbond_calldata(amount: U256) -> Bytes {
    IAtlasBond::unbondCall { amount }.abi_encode().into()
}

pub fn withdraw_calldata(amount: U256) -> Bytes {
    IAtlasBond::withdrawCall { amount }.abi_encode().into()
}

/// A bond tx's executor key.
pub fn tx_key(tag: u8) -> onchain_core::executor::TxKey {
    onchain_core::executor::TxKey::new(BOND_TX_GROUP, B256::with_last_byte(tag))
}

/// One of our solver ops as Atlas ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpResult {
    pub block: u64,
    pub tx: Option<B256>,
    pub bid_wei: U256,
    pub executed: bool,
    /// Won: ran and paid its bid (its gas then settled against what it paid and the bond).
    pub success: bool,
    /// Atlas's SolverOutcome bits (0 = success).
    pub result: U256,
}

/// Atlas's `SolverTxResult` logs for ops signed by `signer` in blocks `from..=to`.
pub async fn scan_results<P: Provider>(p: &P, atlas: Address, signer: Address, from: u64, to: u64) -> Result<Vec<OpResult>> {
    let filter = Filter::new().address(atlas).event_signature(SolverTxResult::SIGNATURE_HASH).topic2(signer.into_word()).from_block(from).to_block(to);
    let logs = p.get_logs(&filter).await.context("Atlas SolverTxResult logs")?;
    Ok(logs.iter().filter_map(|l| decode_result(&l.inner, l.block_number?, l.transaction_hash)).collect())
}

fn decode_result(log: &alloy::primitives::Log, block: u64, tx: Option<B256>) -> Option<OpResult> {
    let e = SolverTxResult::decode_log(log).ok()?;
    Some(OpResult { block, tx, bid_wei: e.bidAmount, executed: e.executed, success: e.success, result: e.result })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth(milli: u64) -> U256 {
        U256::from(milli) * U256::from(10u64).pow(U256::from(15u64))
    }

    fn state(bonded: u64, unbonding: u64, free: u64, complete_block: u64, block: u64) -> BondState {
        BondState { bonded: eth(bonded), unbonding: eth(unbonding), free: eth(free), complete_block, block }
    }

    #[test]
    fn the_bond_band_and_one_withdrawal_at_a_time() {
        let (min, max) = (eth(60), eth(100));
        // In the band: nothing. Below: no ops. Above, auto off: an alert only.
        assert_eq!(next_step(&state(80, 0, 0, 0, 10), min, max, true, None), BondStep::Idle);
        assert_eq!(next_step(&state(50, 0, 0, 0, 10), min, max, true, None), BondStep::BelowMin);
        assert_eq!(next_step(&state(130, 0, 0, 0, 10), min, max, false, None), BondStep::AboveMax { excess: eth(30) });
        // Above, auto on: unbond just the excess.
        assert_eq!(next_step(&state(130, 0, 0, 0, 10), min, max, true, None), BondStep::Unbond(eth(30)));
        // Unbonding under way: wait past Atlas's release block plus the margin, then withdraw it.
        let until = 74 + RELEASE_MARGIN_BLOCKS;
        assert_eq!(next_step(&state(100, 30, 0, 74, 75), min, max, true, None), BondStep::Wait { until_block: until }, "past the release block but within the margin");
        assert_eq!(next_step(&state(100, 30, 0, 74, until + 1), min, max, true, None), BondStep::Withdraw(eth(30)));
        // Nothing new starts while unbonding, even if the bond grew above the max again meanwhile.
        assert_eq!(next_step(&state(140, 30, 0, 74, 70), min, max, true, None), BondStep::Wait { until_block: until });
        // Withdrawn ETH waiting to be sent on comes first.
        assert_eq!(next_step(&state(100, 0, 0, 74, 80), min, max, true, Some(eth(30))), BondStep::Send(eth(30)));
        // Unbonded atlETH the bot did not unbond (an operator's deposit) is never swept: withdraw
        // would take it first, so auto-withdraw pauses (review B#8); alone it is left alone.
        assert_eq!(next_step(&state(100, 30, 5, 74, 90), min, max, true, None), BondStep::OperatorFunds { free: eth(5) });
        assert_eq!(next_step(&state(130, 0, 5, 0, 90), min, max, true, None), BondStep::OperatorFunds { free: eth(5) });
        assert_eq!(next_step(&state(100, 0, 5, 0, 90), min, max, true, None), BondStep::Idle);
        // Below the min: alerted, and nothing moves out even with an unbonding ready (review B#8).
        assert_eq!(next_step(&state(50, 30, 0, 74, 90), min, max, true, None), BondStep::BelowMin);
        // Auto off: an operator's unbonding is left alone.
        assert_eq!(next_step(&state(80, 30, 0, 74, 90), min, max, false, None), BondStep::Idle);
    }

    #[test]
    fn a_bond_tx_out_of_the_executors_sight_is_reconciled_from_its_receipt() {
        // Mined (seen after a restart, or after being reported dropped): its effect applies.
        assert_eq!(check_bond_tx(Some(true), 100, 120), BondTxCheck::Settled { success: true });
        assert_eq!(check_bond_tx(Some(false), 100, 120), BondTxCheck::Settled { success: false });
        // No receipt: wait through the deadline plus the grace, then forget it.
        assert_eq!(check_bond_tx(None, 100, 100 + BOND_TX_GRACE_BLOCKS), BondTxCheck::Pending);
        assert_eq!(check_bond_tx(None, 100, 101 + BOND_TX_GRACE_BLOCKS), BondTxCheck::Gone);
    }

    #[test]
    fn bond_tx_calldata_and_keys() {
        assert_eq!(&unbond_calldata(eth(1))[..4], IAtlasBond::unbondCall::SELECTOR.as_slice());
        assert_eq!(IAtlasBond::withdrawCall::abi_decode(&withdraw_calldata(eth(2))).unwrap().amount, eth(2));
        assert_ne!(tx_key(TAG_UNBOND), tx_key(TAG_WITHDRAW));
        assert_eq!(tx_key(TAG_SEND).group, BOND_TX_GROUP);
    }

    #[test]
    fn decodes_our_ops_results() {
        let signer = Address::repeat_byte(0x5e);
        let e = SolverTxResult { solverTo: Address::repeat_byte(0x01), solverFrom: signer, dAppControl: Address::repeat_byte(0x02), bidToken: Address::ZERO, bidAmount: eth(1), executed: false, success: false, result: U256::from(1u64 << 13) };
        let log = alloy::primitives::Log { address: Address::repeat_byte(0xa7), data: e.encode_log_data() };
        let r = decode_result(&log, 42, None).unwrap();
        assert_eq!((r.block, r.bid_wei, r.executed, r.success, r.result), (42, eth(1), false, false, U256::from(0x2000u64)), "InsufficientEscrow: not run");
    }
}
