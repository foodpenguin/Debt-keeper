//! ABI of this repo's own contracts (`contracts/src`), declared once: the types they share
//! (`SwapStep`) must stay identical across the venues that encode for them.

use alloy::sol;

sol! {
    struct LMarketParams { address loanToken; address collateralToken; address oracle; address irm; uint256 lltv; }
    /// One exit swap (MorphoLiquidator / FlashLiquidator `SwapStep`; AtlasLiquidationSolver's bid swap).
    struct SwapStep { address target; address tokenIn; uint256 amountIn; bytes data; }
    interface IMorphoLiquidator {
        function liquidate(LMarketParams marketParams, address borrower, uint256 seizedAssets, uint256 repaidShares, SwapStep[] swaps, uint256 minProfit, uint256 deadlineBlock) external returns (uint256 profit);
        function liquidateRoute(LMarketParams marketParams, address borrower, uint256 seizedAssets, bytes route, uint256 minProfit, uint256 deadlineBlock) external returns (uint256 profit);
    }

    /// FlashLiquidator's `LiquidationCall`.
    struct LiquidationCall { address target; bytes data; }
    interface IFlashLiquidator {
        function liquidate(uint8 funding, address debtToken, uint256 debtAmount, address collateralToken, LiquidationCall liquidation, SwapStep[] swaps, uint256 minProfit, uint256 deadlineBlock) external returns (uint256 profit);
    }

    /// AtlasLiquidationSolver's `SolverCall`.
    struct SolverCall { address target; bytes data; }
    interface IAtlasSolver {
        function atlasSolverCall(address solverOpFrom, address executionEnvironment, address bidToken, uint256 bidAmount, bytes solverOpData, bytes extraReturnData) external payable returns (uint256 profit);
    }
}
