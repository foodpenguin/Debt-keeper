// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

/// @dev Flash loan (fixed amount, repaid inside the callback): Morpho Blue, 0 fee. Morpho calls back
///      `onMorphoFlashLoan(assets, data)` on the caller and pulls `assets` back after it returns.
interface IMorphoFlashLoan {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

/// @dev Flash accounting (take now, settle the net delta before unlock returns): Uniswap V4
///      PoolManager. It calls back `unlockCallback(data)` on the caller. `Currency` is an address in
///      the ABI.
interface IPoolManager {
    function unlock(bytes calldata data) external returns (bytes memory);
    function take(address currency, address to, uint256 amount) external;
    function sync(address currency) external;
    function settle() external payable returns (uint256 paid);
}
