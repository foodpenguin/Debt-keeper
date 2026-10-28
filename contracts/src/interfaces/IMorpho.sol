// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.28;

type Id is bytes32;

struct MarketParams {
    address loanToken;
    address collateralToken;
    address oracle;
    address irm;
    uint256 lltv;
}

struct Position {
    uint256 supplyShares;
    uint128 borrowShares;
    uint128 collateral;
}

struct Market {
    uint128 totalSupplyAssets;
    uint128 totalSupplyShares;
    uint128 totalBorrowAssets;
    uint128 totalBorrowShares;
    uint128 lastUpdate;
    uint128 fee;
}

/// @dev Subset of the Morpho Blue interface used by the liquidator.
interface IMorpho {
    function liquidate(
        MarketParams memory marketParams,
        address borrower,
        uint256 seizedAssets,
        uint256 repaidShares,
        bytes memory data
    ) external returns (uint256 seized, uint256 repaid);

    function idToMarketParams(Id id) external view returns (MarketParams memory);
    function position(Id id, address user) external view returns (Position memory);
    function market(Id id) external view returns (Market memory);
    function accrueInterest(MarketParams memory marketParams) external;
}

interface IMorphoLiquidateCallback {
    function onMorphoLiquidate(uint256 repaidAssets, bytes calldata data) external;
}

interface IOracle {
    /// @dev Price of 1 collateral asset quoted in loan asset, scaled by 1e36 (adjusted for decimals).
    function price() external view returns (uint256);
}
