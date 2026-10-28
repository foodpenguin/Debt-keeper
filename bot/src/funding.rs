//! Where a liquidation's repayment comes from.
//!
//! - **The protocol's own callback** (Morpho Blue `liquidate` -> `onMorphoLiquidate`): the
//!   collateral arrives first and is sold inside the callback, the protocol then pulls the
//!   repayment. No borrowing at all; `sim::Simulator` (MorphoLiquidator) is this case.
//! - **A flash source**, for protocols without such a callback (Aave-style `liquidationCall` pulls
//!   the repayment up front): the repayment is borrowed first, then repaid from the sale of the
//!   collateral (`flash_venue::FlashLiquidatorVenue`, contract `FlashLiquidator.sol`). Two models,
//!   deliberately two traits, because the contract talks to them differently:
//!   - `FlashLoan`: borrow a fixed amount, the lender calls back, repay amount + fee before the
//!     callback returns (Morpho Blue: 0 fee);
//!   - `FlashAccounting`: a singleton holding the reserves tracks the caller's deltas while it is
//!     unlocked; take now, settle the net before unlock returns (Uniswap V4 PoolManager: no fee).

use alloy::primitives::{Address, U256, address};

/// Callback-style flash loan with a fixed fee.
pub trait FlashLoan: Send + Sync {
    fn name(&self) -> &'static str;
    /// The contract that lends and calls back.
    fn lender(&self) -> Address;
    /// Fee for borrowing `amount`, in the same token.
    fn fee(&self, amount: U256) -> U256;
}

/// Take-and-settle flash accounting on a singleton.
pub trait FlashAccounting: Send + Sync {
    fn name(&self) -> &'static str;
    /// The singleton that holds the reserves and tracks deltas until settlement.
    fn manager(&self) -> Address;
    /// Fee for taking `amount` and settling it back, in the same token (V4: none).
    fn fee(&self, _amount: U256) -> U256 {
        U256::ZERO
    }
}

/// Morpho Blue: `flashLoan(token, assets, data)` -> `onMorphoFlashLoan`, no fee.
#[derive(Debug, Clone, Copy)]
pub struct MorphoBlue {
    pub address: Address,
}

impl FlashLoan for MorphoBlue {
    fn name(&self) -> &'static str {
        "morpho"
    }
    fn lender(&self) -> Address {
        self.address
    }
    fn fee(&self, _amount: U256) -> U256 {
        U256::ZERO
    }
}

/// Uniswap V4: `unlock(data)` -> `unlockCallback`, then `take` / `sync` + transfer + `settle`.
/// While the liquidator holds the lock no swap step may go through V4 itself.
#[derive(Debug, Clone, Copy)]
pub struct UniswapV4 {
    pub pool_manager: Address,
}

impl FlashAccounting for UniswapV4 {
    fn name(&self) -> &'static str {
        "v4"
    }
    fn manager(&self) -> Address {
        self.pool_manager
    }
}

/// Uniswap V4 PoolManager on Base.
pub const BASE_POOL_MANAGER: Address = address!("498581fF718922c3f8e6A244956aF099B2652b2b");

/// A flash source a `FlashLiquidator` call can be funded by: what the venue needs from a
/// `FlashLoan` / `FlashAccounting` implementation, plus the contract's selector for it.
///
/// Adding a source takes both sides: an implementation of this trait, and a matching branch in
/// `FlashLiquidator.sol` (its entry `funding` value, the call that starts the flash, and the
/// callback it receives, guarded like the existing two). The contract repays exactly what it
/// borrowed, so a source that charges a fee also needs the contract to repay amount + fee (the bot
/// refuses a source with a fee until then).
pub trait FlashSource: Send + Sync {
    fn name(&self) -> &'static str;
    /// The contract that holds the capital.
    fn holder(&self) -> Address;
    fn fee(&self, amount: U256) -> U256;
    /// `FlashLiquidator.liquidate`'s `funding` argument (`FUND_MORPHO` = 1, `FUND_V4` = 2).
    fn mode(&self) -> u8;
}

impl FlashSource for MorphoBlue {
    fn name(&self) -> &'static str {
        FlashLoan::name(self)
    }
    fn holder(&self) -> Address {
        self.lender()
    }
    fn fee(&self, amount: U256) -> U256 {
        FlashLoan::fee(self, amount)
    }
    fn mode(&self) -> u8 {
        1
    }
}

impl FlashSource for UniswapV4 {
    fn name(&self) -> &'static str {
        FlashAccounting::name(self)
    }
    fn holder(&self) -> Address {
        self.manager()
    }
    fn fee(&self, amount: U256) -> U256 {
        FlashAccounting::fee(self, amount)
    }
    fn mode(&self) -> u8 {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_name_their_contract_mode_and_fee() {
        let m: Box<dyn FlashSource> = Box::new(MorphoBlue { address: Address::repeat_byte(1) });
        let v: Box<dyn FlashSource> = Box::new(UniswapV4 { pool_manager: BASE_POOL_MANAGER });
        assert_eq!((m.name(), m.holder(), m.mode(), m.fee(U256::from(9u8))), ("morpho", Address::repeat_byte(1), 1, U256::ZERO));
        assert_eq!((v.name(), v.holder(), v.mode(), v.fee(U256::from(9u8))), ("v4", BASE_POOL_MANAGER, 2, U256::ZERO));
    }
}
