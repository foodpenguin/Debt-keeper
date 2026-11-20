// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {IV3Pool, MIN_SQRT_PRICE_LIMIT, MAX_SQRT_PRICE_LIMIT} from "./interfaces/IV3Pool.sol";
import {ProfitShares} from "./ProfitShares.sol";

interface IERC20S {
    function balanceOf(address) external view returns (uint256);
}

interface IWETHS {
    function withdraw(uint256 amount) external;
}

/// @dev The Atlas escrow surface a solver settles with (Fastlane Atlas, used by Chainlink SVR).
interface IAtlasEscrow {
    function shortfall() external view returns (uint256 gasLiability, uint256 borrowLiability);
    function reconcile(uint256 maxApprovedGasSpend) external payable returns (uint256 owed);
}

/// @notice One call of the solver's plan (e.g. a liquidation through a MorphoLiquidator deployed with
///         this solver as executor and profit receiver).
struct SolverCall {
    address target;
    bytes data;
}

/// @title AtlasLiquidationSolver
/// @notice Solver contract for Chainlink SVR auctions run by Atlas: in the auction winner's slot it
///         runs liquidations (`calls`), buys the ETH bid and its own gas to Atlas with part of the
///         profit (one exact-output swap on an allowlisted pool, `bidPool`), and pays the rest out
///         by the owner-set shares (`ProfitShares`).
/// @dev Written against the Atlas solver interface (`atlasSolverCall`), not derived from Atlas code.
///      Gas: the op pays `gasPayWei` of its gas in ETH out of the proceeds (Atlas credits any excess
///      over the real charge back to the bonded atlETH and takes any shortfall from it), and approves
///      the rest of Atlas's maximum liability against the bond. With `gasPayWei = 0` the whole gas is
///      taken from the bond.
///      Security model:
///      - only ATLAS may call `atlasSolverCall`, and only for a solver operation signed by `executor`
///        (the solver op's `from`, our hot wallet); only `owner` changes settings or moves funds;
///      - every call target and the bid pool must be owner-allowlisted; the bid pool is paid inside
///        its own swap callback (only the pool currently swapping, once, in the profit token, at most
///        the profit token held), so no allowance is ever granted;
///      - bids are in ETH only (SVR); the bid goes to the execution environment Atlas names;
///      - the ETH paid for gas never exceeds the gas liability Atlas reports for this op;
///      - the whole call reverts unless the profit token this call gained, after paying the bid and
///        the gas, is at least `minProfit` (a balance change: tokens a liquidator left here earlier,
///        e.g. its leftover collateral, never count); the whole profit-token balance and any leftover
///        WETH and ETH are then paid out by the shares (extra recipients set by the owner, the rest
///        to PROFIT_RECEIVER; `minProfit` is checked on the whole profit before the split).
///      `atlasSolverCall` returns the net profit (callers that expect no return value ignore it).
contract AtlasLiquidationSolver is ProfitShares {
    address public immutable ATLAS;
    address public immutable WETH;

    address public owner;
    /// @dev The solver operation signer (Atlas `solverOpFrom`). Shares a slot with `paused`.
    address public executor;
    bool public paused;
    address public pendingOwner;
    mapping(address => bool) public allowedTarget;

    /// @dev Set for the whole of atlasSolverCall: `distribute` is refused while it runs.
    bool private transient operating;
    /// @dev The bid pool whose swap() is running right now, the token it is paid in, and the most it
    ///      may ask to be paid.
    address private transient activePool;
    address private transient activeToken;
    uint256 private transient activeMaxIn;

    error InvalidEntry();
    error InvalidCaller();
    error IsPaused();
    error UnsupportedBidToken(address token);
    error TargetNotAllowed(address target);
    error CallFailed(uint256 index, bytes reason);
    error BidSwapShort(uint256 got, uint256 wanted);
    error InsufficientProfit(uint256 profit, uint256 minProfit);
    error TokenCallFailed(address token);
    error EthTransferFailed();
    error Unauthorized();

    event ExecutorSet(address executor);
    event OwnershipTransferStarted(address indexed pendingOwner);
    event OwnerSet(address owner);
    event PausedSet(bool paused);
    event TargetAllowed(address indexed target, bool allowed);

    modifier onlyOwner() {
        if (msg.sender != owner) revert Unauthorized();
        _;
    }

    constructor(address atlas, address weth, address owner_, address executor_, address profitReceiver_) ProfitShares(profitReceiver_) {
        ATLAS = atlas;
        WETH = weth;
        owner = owner_;
        executor = executor_;
        emit OwnerSet(owner_);
        emit ExecutorSet(executor_);
    }

    /// @param solverOpFrom The solver operation's signer; must be `executor`.
    /// @param executionEnvironment Receives the bid.
    /// @param bidToken Must be address(0) (ETH).
    /// @param solverOpData abi.encode(SolverCall[] calls, address bidPool, address profitToken,
    ///        uint256 minProfit, uint256 gasPayWei). `bidPool` (an allowlisted V3-style pool of the
    ///        profit token and WETH) sells profit token for exactly the bid plus `gasPayWei` of WETH;
    ///        address(0): no swap (the calls leave WETH). `gasPayWei`: ETH paid toward this op's gas
    ///        (capped at Atlas's gas liability); 0 = all of it from the bond.
    function atlasSolverCall(
        address solverOpFrom,
        address executionEnvironment,
        address bidToken,
        uint256 bidAmount,
        bytes calldata solverOpData,
        bytes calldata
    ) external payable returns (uint256 profit) {
        if (msg.sender != ATLAS) revert InvalidEntry();
        if (solverOpFrom != executor) revert InvalidCaller();
        if (paused) revert IsPaused();
        if (bidToken != address(0)) revert UnsupportedBidToken(bidToken);
        (SolverCall[] memory calls, address bidPool, address profitToken, uint256 minProfit, uint256 gasPayWei) =
            abi.decode(solverOpData, (SolverCall[], address, address, uint256, uint256));
        operating = true;
        // The liquidators this solver drives send it their leftover collateral, so a later op's
        // profit token may already sit here: only what this call adds is profit.
        uint256 before = IERC20S(profitToken).balanceOf(address(this));

        for (uint256 i; i < calls.length; ++i) {
            if (!allowedTarget[calls[i].target]) revert TargetNotAllowed(calls[i].target);
            (bool ok, bytes memory ret) = calls[i].target.call(calls[i].data);
            if (!ok) revert CallFailed(i, ret);
        }
        if (bidPool != address(0)) _buyWeth(bidPool, profitToken, bidAmount + gasPayWei);

        // Both liabilities are fixed for the whole solver operation: the gas one is Atlas's maximum
        // for this op (from gas limits, not gas used), the borrow one what it lent. What Atlas lent
        // (up to the value it sent) is repaid in ETH, so the bid never spends it; the gas paid in ETH
        // never exceeds the liability (a wrong gasPayWei cannot overpay). Unwrap WETH for whatever
        // the ETH balance cannot cover.
        (uint256 gasLiability, uint256 borrowLiability) = IAtlasEscrow(ATLAS).shortfall();
        uint256 repay = borrowLiability < msg.value ? borrowLiability : msg.value;
        uint256 gasPay = gasPayWei < gasLiability ? gasPayWei : gasLiability;
        uint256 eth = address(this).balance;
        if (eth < bidAmount + repay + gasPay) IWETHS(WETH).withdraw(bidAmount + repay + gasPay - eth);
        if (bidAmount > 0) {
            (bool sent,) = executionEnvironment.call{value: bidAmount}("");
            if (!sent) revert EthTransferFailed();
        }

        // Settle with Atlas: repay the borrowed ETH, pay `gasPay` of the gas in ETH, and approve the
        // rest of the gas liability against the solver's bonded balance. At settlement Atlas credits
        // the bond with what the ETH paid beyond the real gas charge, or takes the shortfall from it.
        IAtlasEscrow(ATLAS).reconcile{value: repay + gasPay}(gasLiability - gasPay);

        uint256 bal = IERC20S(profitToken).balanceOf(address(this));
        profit = bal > before ? bal - before : 0;
        if (profit < minProfit) revert InsufficientProfit(profit, minProfit);
        if (bal > 0) _payout(profitToken, bal);
        if (profitToken != WETH) {
            uint256 weth = IERC20S(WETH).balanceOf(address(this));
            if (weth > 0) _payout(WETH, weth);
        }
        // ETH left after the bid and the settlement (value Atlas sent beyond what it lent, unwrap
        // rounding): nothing stays at rest.
        uint256 rest = address(this).balance;
        if (rest > 0) _payoutEth(rest);
        operating = false;
    }

    /// @dev WETH unwrapping sends ETH here.
    receive() external payable {}

    /// @dev Buys exactly `amountOut` of WETH with `tokenIn` on the allowlisted `pool` (exact output),
    ///      paying it inside its swap callback. The pool may ask for at most the `tokenIn` held.
    function _buyWeth(address pool, address tokenIn, uint256 amountOut) private {
        if (!allowedTarget[pool]) revert TargetNotAllowed(pool);
        // Selling token0 for token1 (WETH) is zeroForOne; a pool of other tokens cannot be paid in
        // `tokenIn` (its own balance check fails) or does not deliver WETH (checked below).
        bool zeroForOne = IV3Pool(pool).token0() == tokenIn;
        activePool = pool;
        activeToken = tokenIn;
        activeMaxIn = IERC20S(tokenIn).balanceOf(address(this));
        (int256 a0, int256 a1) =
            IV3Pool(pool).swap(address(this), zeroForOne, -int256(amountOut), zeroForOne ? MIN_SQRT_PRICE_LIMIT : MAX_SQRT_PRICE_LIMIT, "");
        activePool = address(0);
        // Exact output stops short only at the price limit: then the pool delivered less.
        int256 out = zeroForOne ? a1 : a0;
        uint256 got = out < 0 ? uint256(-out) : 0;
        if (got < amountOut) revert BidSwapShort(got, amountOut);
    }

    function uniswapV3SwapCallback(int256 amount0, int256 amount1, bytes calldata) external {
        _payPool(amount0, amount1);
    }

    function pancakeV3SwapCallback(int256 amount0, int256 amount1, bytes calldata) external {
        _payPool(amount0, amount1);
    }

    /// @dev Pays the bid pool its input: only the pool swapping right now, once, in the profit token,
    ///      never more than the profit token held when the swap started.
    function _payPool(int256 amount0, int256 amount1) private {
        address pool = activePool;
        if (msg.sender != pool || pool == address(0)) revert Unauthorized();
        activePool = address(0);
        uint256 owed = uint256(amount0 > 0 ? amount0 : amount1);
        if (owed > activeMaxIn) revert Unauthorized();
        _safeTransfer(activeToken, pool, owed);
    }

    function _operating() internal view override returns (bool) {
        return operating;
    }

    // ---------------------------------------------------------------- admin

    function setExecutor(address executor_) external onlyOwner {
        executor = executor_;
        emit ExecutorSet(executor_);
    }

    function setAllowedTarget(address target, bool allowed) external onlyOwner {
        allowedTarget[target] = allowed;
        emit TargetAllowed(target, allowed);
    }

    /// @notice The extra profit recipients and their shares in bps (PROFIT_RECEIVER gets the rest);
    ///         empty arrays = everything to PROFIT_RECEIVER. See ProfitShares.
    function setShares(address[] calldata to, uint16[] calldata bps) external onlyOwner {
        _setShares(to, bps);
    }

    /// @notice Owner or the executor (the hot wallet that signs the operations) may pause; only
    ///         owner may resume.
    function pause() external {
        if (msg.sender != owner && msg.sender != executor) revert Unauthorized();
        paused = true;
        emit PausedSet(true);
    }

    function unpause() external onlyOwner {
        paused = false;
        emit PausedSet(false);
    }

    function transferOwnership(address newOwner) external onlyOwner {
        pendingOwner = newOwner;
        emit OwnershipTransferStarted(newOwner);
    }

    function acceptOwnership() external {
        if (msg.sender != pendingOwner) revert Unauthorized();
        owner = msg.sender;
        pendingOwner = address(0);
        emit OwnerSet(msg.sender);
    }

    function rescue(address token, address to, uint256 amount) external onlyOwner {
        _safeTransfer(token, to, amount);
    }

    function rescueEth(address to, uint256 amount) external onlyOwner {
        (bool sent,) = to.call{value: amount}("");
        if (!sent) revert EthTransferFailed();
    }

    // ---------------------------------------------------------------- internal

    function _safeTransfer(address token, address to, uint256 amount) private {
        if (!_tokenCall(token, abi.encodeWithSelector(0xa9059cbb, to, amount))) revert TokenCallFailed(token);
    }

    function _tokenCall(address token, bytes memory data) private returns (bool) {
        (bool ok, bytes memory ret) = token.call(data);
        return ok && (ret.length == 0 || (ret.length == 32 && abi.decode(ret, (bool)))) && token.code.length > 0;
    }
}
