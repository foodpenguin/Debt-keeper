//! Morpho Blue math, mirroring the Solidity libraries bit-for-bit
//! (MathLib, SharesMathLib, liquidation formulas in Morpho.sol).

use alloy::primitives::U256;

pub use crate::num::{mul_div_down, mul_div_up, u256_f64, units};

pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
pub const ORACLE_PRICE_SCALE: U256 = U256::from_limbs([0xb34b9f1000000000, 0xc097ce7bc90715, 0, 0]); // 1e36
pub const VIRTUAL_SHARES: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);
pub const VIRTUAL_ASSETS: U256 = U256::from_limbs([1, 0, 0, 0]);
pub const MAX_LIQUIDATION_INCENTIVE_FACTOR: U256 = U256::from_limbs([1_150_000_000_000_000_000, 0, 0, 0]);
pub const LIQUIDATION_CURSOR: U256 = U256::from_limbs([300_000_000_000_000_000, 0, 0, 0]);

pub fn w_mul_down(x: U256, y: U256) -> U256 {
    mul_div_down(x, y, WAD)
}
pub fn w_div_down(x: U256, y: U256) -> U256 {
    mul_div_down(x, WAD, y)
}
pub fn w_div_up(x: U256, y: U256) -> U256 {
    mul_div_up(x, WAD, y)
}

/// e^(x*n) - 1 with a 3-term Taylor expansion, as in MathLib.wTaylorCompounded.
pub fn w_taylor_compounded(x: U256, n: U256) -> U256 {
    let first = x * n;
    let second = mul_div_down(first, first, U256::from(2) * WAD);
    let third = mul_div_down(second, first, U256::from(3) * WAD);
    first + second + third
}

#[allow(dead_code)] // mirrors SharesMathLib; used by tests and upcoming repay-by-shares path
pub fn to_shares_down(assets: U256, total_assets: U256, total_shares: U256) -> U256 {
    mul_div_down(assets, total_shares + VIRTUAL_SHARES, total_assets + VIRTUAL_ASSETS)
}
pub fn to_shares_up(assets: U256, total_assets: U256, total_shares: U256) -> U256 {
    mul_div_up(assets, total_shares + VIRTUAL_SHARES, total_assets + VIRTUAL_ASSETS)
}
pub fn to_assets_down(shares: U256, total_assets: U256, total_shares: U256) -> U256 {
    mul_div_down(shares, total_assets + VIRTUAL_ASSETS, total_shares + VIRTUAL_SHARES)
}
pub fn to_assets_up(shares: U256, total_assets: U256, total_shares: U256) -> U256 {
    mul_div_up(shares, total_assets + VIRTUAL_ASSETS, total_shares + VIRTUAL_SHARES)
}

/// LIF = min(1.15, 1 / (1 - cursor * (1 - lltv)))
pub fn liquidation_incentive_factor(lltv: U256) -> U256 {
    let lif = w_div_down(WAD, WAD - w_mul_down(LIQUIDATION_CURSOR, WAD - lltv));
    lif.min(MAX_LIQUIDATION_INCENTIVE_FACTOR)
}

/// Borrow side of a market, after interest accrual.
#[derive(Clone, Copy, Debug, Default)]
pub struct BorrowTotals {
    pub total_borrow_assets: U256,
    pub total_borrow_shares: U256,
}

/// Accrue interest the same way Morpho._accrueInterest does (borrow side only).
pub fn accrue_borrow(total_borrow_assets: U256, total_borrow_shares: U256, borrow_rate: U256, elapsed: u64) -> BorrowTotals {
    let interest = w_mul_down(total_borrow_assets, w_taylor_compounded(borrow_rate, U256::from(elapsed)));
    BorrowTotals { total_borrow_assets: total_borrow_assets + interest, total_borrow_shares }
}

#[derive(Clone, Copy, Debug)]
pub struct Health {
    /// Debt in loan-token units (rounded up, like Morpho._isHealthy).
    pub borrowed: U256,
    /// Max debt allowed by collateral * price * lltv.
    pub max_borrow: U256,
}

impl Health {
    pub fn is_liquidatable(&self) -> bool {
        self.borrowed > self.max_borrow
    }
    /// max_borrow / borrowed (f64, for display and ranking only).
    pub fn factor(&self) -> f64 {
        if self.borrowed.is_zero() {
            return f64::INFINITY;
        }
        u256_f64(self.max_borrow) / u256_f64(self.borrowed)
    }
}

pub fn health(borrow_shares: U256, collateral: U256, price: U256, lltv: U256, t: BorrowTotals) -> Health {
    let borrowed = to_assets_up(borrow_shares, t.total_borrow_assets, t.total_borrow_shares);
    let max_borrow = w_mul_down(mul_div_down(collateral, price, ORACLE_PRICE_SCALE), lltv);
    Health { borrowed, max_borrow }
}

/// What a liquidation seizing `seized` collateral costs, as computed by Morpho.liquidate.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // `seized` is consumed by the executor (next stage)
pub struct LiquidationQuote {
    pub seized: U256,
    pub repaid_shares: U256,
    pub repaid_assets: U256,
}

pub fn quote_by_seized(seized: U256, price: U256, lltv: U256, t: BorrowTotals) -> LiquidationQuote {
    let lif = liquidation_incentive_factor(lltv);
    let seized_quoted = mul_div_up(seized, price, ORACLE_PRICE_SCALE);
    let repaid_shares = to_shares_up(w_div_up(seized_quoted, lif), t.total_borrow_assets, t.total_borrow_shares);
    let repaid_assets = to_assets_up(repaid_shares, t.total_borrow_assets, t.total_borrow_shares);
    LiquidationQuote { seized, repaid_shares, repaid_assets }
}

/// Share of a debt-limited seize left on the table (1e-5): repaying the whole debt is exact only
/// for the totals we accrued. If Morpho accrues less interest than our model at execution (an
/// earlier timestamp, a lower IRM rate), the same seize needs more shares than the borrower holds
/// and `borrowShares -= repaidShares` panics (0x11). 1e-5 covers ~25 min of accrual at 20% APR.
const DEBT_LIMITED_SEIZE_MARGIN: u64 = 100_000;

/// Largest collateral amount that can be seized: limited by the collateral held and by
/// the borrower's debt (repaidShares must not exceed borrowShares, or Morpho reverts).
pub fn max_seizable(borrow_shares: U256, collateral: U256, price: U256, lltv: U256, t: BorrowTotals) -> U256 {
    if price.is_zero() {
        return U256::ZERO;
    }
    let lif = liquidation_incentive_factor(lltv);
    let debt = to_assets_down(borrow_shares, t.total_borrow_assets, t.total_borrow_shares);
    let by_debt = mul_div_down(w_mul_down(debt, lif), ORACLE_PRICE_SCALE, price);
    let mut seize = if by_debt < collateral { by_debt - (by_debt / U256::from(DEBT_LIMITED_SEIZE_MARGIN)).max(U256::from(1)).min(by_debt) } else { collateral };
    // Rounding in quote_by_seized can push repaid_shares 1 above borrow_shares; step down until valid.
    for _ in 0..8 {
        if quote_by_seized(seize, price, lltv, t).repaid_shares <= borrow_shares || seize.is_zero() {
            break;
        }
        seize -= seize / U256::from(1_000_000u64) + U256::from(1);
    }
    seize
}


#[cfg(test)]
mod tests {
    use super::*;

    fn wad(x: f64) -> U256 {
        U256::from((x * 1e18) as u128)
    }

    #[test]
    fn constants() {
        assert_eq!(ORACLE_PRICE_SCALE, U256::from(10).pow(U256::from(36)));
        assert_eq!(WAD, U256::from(10).pow(U256::from(18)));
    }

    #[test]
    fn lif_matches_formula() {
        // 1 / (1 - 0.3 * 0.14) = 1.04384...
        let lif = liquidation_incentive_factor(wad(0.86));
        assert!((u256_f64(lif) / 1e18 - 1.0 / (1.0 - 0.3 * 0.14)).abs() < 1e-12);
        // 62.5% LLTV -> 1/(1-0.3*0.375) = 1.12676
        let lif = liquidation_incentive_factor(wad(0.625));
        assert!((u256_f64(lif) / 1e18 - 1.126760563).abs() < 1e-8);
        // Very low LLTV is capped at 1.15
        assert_eq!(liquidation_incentive_factor(wad(0.1)), MAX_LIQUIDATION_INCENTIVE_FACTOR);
    }

    #[test]
    fn rounding_directions() {
        assert_eq!(mul_div_down(U256::from(7), U256::from(1), U256::from(2)), U256::from(3));
        assert_eq!(mul_div_up(U256::from(7), U256::from(1), U256::from(2)), U256::from(4));
        assert_eq!(mul_div_up(U256::from(8), U256::from(1), U256::from(2)), U256::from(4));
    }

    #[test]
    fn health_boundary() {
        // 1 WETH collateral at 2500 USDC (price scaled: 2500e6 * 1e36 / 1e18 = 2500e24), LLTV 86%.
        let price = U256::from(2500u64) * U256::from(10).pow(U256::from(24));
        let t = BorrowTotals { total_borrow_assets: U256::from(1_000_000_000u64), total_borrow_shares: U256::from(1_000_000_000u64) * VIRTUAL_SHARES };
        let collateral = WAD;
        // max borrow = 2150 USDC
        let shares_ok = to_shares_down(U256::from(2_150_000_000u64), t.total_borrow_assets, t.total_borrow_shares);
        let h = health(shares_ok, collateral, price, wad(0.86), t);
        assert_eq!(h.max_borrow, U256::from(2_150_000_000u64));
        assert!(!h.is_liquidatable());
        let shares_bad = to_shares_up(U256::from(2_150_000_100u64), t.total_borrow_assets, t.total_borrow_shares);
        assert!(health(shares_bad, collateral, price, wad(0.86), t).is_liquidatable());
    }

    #[test]
    fn seize_quote_and_cap() {
        let price = U256::from(2500u64) * U256::from(10).pow(U256::from(24));
        let t = BorrowTotals { total_borrow_assets: U256::from(10_000_000_000u64), total_borrow_shares: U256::from(10_000_000_000u64) * VIRTUAL_SHARES };
        let lltv = wad(0.86);
        // Seizing 0.1 WETH (250 USDC) repays 250 / 1.0438 ~= 239.5 USDC
        let q = quote_by_seized(WAD / U256::from(10), price, lltv, t);
        let repaid = u256_f64(q.repaid_assets) / 1e6;
        assert!((repaid - 250.0 / 1.043841336).abs() < 0.01, "repaid {repaid}");
        // Borrower owes 1000 USDC with 10 WETH collateral: seizable is bounded by debt, not collateral.
        let shares = to_shares_down(U256::from(1_000_000_000u64), t.total_borrow_assets, t.total_borrow_shares);
        let s = max_seizable(shares, U256::from(10) * WAD, price, lltv, t);
        assert!(quote_by_seized(s, price, lltv, t).repaid_shares <= shares);
        assert!(units(s, 18) < 0.42 && units(s, 18) > 0.41, "seize {}", units(s, 18));
    }

    #[test]
    fn debt_limited_seize_survives_less_accrual() {
        // 2026-10-18 regression: seize sized on totals accrued a few seconds ahead of the block
        // Morpho executes in. Morpho then accrues less, needs more shares for the same seize, and
        // `borrowShares -= repaidShares` panicked (0x11) for every full-debt liquidation.
        let price = U256::from(2500u64) * U256::from(10).pow(U256::from(24));
        let lltv = wad(0.86);
        let base = BorrowTotals { total_borrow_assets: U256::from(10_000_000_000u64), total_borrow_shares: U256::from(10_000_000_000u64) * VIRTUAL_SHARES };
        let shares = to_shares_down(U256::from(1_000_000_000u64), base.total_borrow_assets, base.total_borrow_shares);
        // Model: 20% APR accrued 60 s further than the chain will.
        let rate = U256::from((0.20f64 / 31_536_000.0 * 1e18) as u128);
        let ahead = accrue_borrow(base.total_borrow_assets, base.total_borrow_shares, rate, 60);
        let s = max_seizable(shares, U256::from(10) * WAD, price, lltv, ahead);
        assert!(quote_by_seized(s, price, lltv, base).repaid_shares <= shares, "Morpho would underflow borrowShares");
        // Collateral-limited (bad debt): everything is seized, no margin.
        let tiny = U256::from(10_000_000_000_000_000u64); // 0.01 WETH = 25 USDC for 1000 USDC of debt
        assert_eq!(max_seizable(shares, tiny, price, lltv, base), tiny);
    }

    #[test]
    fn taylor_small_rate() {
        // 5% APR per second for one year ~ e^0.05 - 1 = 0.05127
        let rate = U256::from((0.05f64 / 31_536_000.0 * 1e18) as u128);
        let g = u256_f64(w_taylor_compounded(rate, U256::from(31_536_000u64))) / 1e18;
        assert!((g - 0.051271).abs() < 1e-4);
    }
}
