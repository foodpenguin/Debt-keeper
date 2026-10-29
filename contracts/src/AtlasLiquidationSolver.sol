// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {SwapStep} from "./MorphoLiquidator.sol";

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
///         runs liquidations (`calls`), converts part of the profit to WETH (`bidSwap`) to pay the ETH
///         bid to the execution environment, settles its gas with Atlas, and keeps the rest.
/// @dev Written against the Atlas solver interface (`atlasSolverCall`), not derived from Atlas code.
///      Security model:
///      - only ATLAS may call `atlasSolverCall`, and only for a solver operation signed by `executor`
///        (the solver op's `from`, our hot wallet); only `owner` changes settings or moves funds;
///      - every call target and the bid-swap router must be owner-allowlisted; the bid-swap
///        allowance is exact and reset to zero;
///      - bids are in ETH only (SVR); the bid goes to the execution environment Atlas names;
///      - the whole call reverts unless the profit token this call gained, after paying the bid, is
///        at least `minProfit` (a balance change: tokens a liquidator left here earlier, e.g. its
///        leftover collateral, never count); the whole profit-token balance and any leftover WETH
///        then go to PROFIT_RECEIVER.
///      `atlasSolverCall` returns the net profit (callers that expect no return value ignore it).
contract AtlasLiquidationSolver {
    address public immutable ATLAS;
    address public immutable WETH;
    address public immutable PROFIT_RECEIVER;

    address public owner;
    /// @dev The solver operation signer (Atlas `solverOpFrom`). Shares a slot with `paused`.
    address public executor;
    bool public paused;
    address public pendingOwner;
    mapping(address => bool) public allowedTarget;

    error InvalidEntry();
    error InvalidCaller();
    error IsPaused();
    error UnsupportedBidToken(address token);
    error TargetNotAllowed(address target);
    error CallFailed(uint256 index, bytes reason);
    error BidSwapFailed(bytes reason);
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

    constructor(address atlas, address weth, address owner_, address executor_, address profitReceiver_) {
        ATLAS = atlas;
        WETH = weth;
        PROFIT_RECEIVER = profitReceiver_;
        owner = owner_;
        executor = executor_;
        emit OwnerSet(owner_);
        emit ExecutorSet(executor_);
    }

    /// @param solverOpFrom The solver operation's signer; must be `executor`.
    /// @param executionEnvironment Receives the bid.
    /// @param bidToken Must be address(0) (ETH).
    /// @param solverOpData abi.encode(SolverCall[] calls, SwapStep bidSwap, address profitToken,
    ///        uint256 minProfit). `bidSwap.target` = address(0): no swap (the calls leave WETH).
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
        (SolverCall[] memory calls, SwapStep memory bidSwap, address profitToken, uint256 minProfit) =
            abi.decode(solverOpData, (SolverCall[], SwapStep, address, uint256));
        // The liquidators this solver drives send it their leftover collateral, so a later op's
        // profit token may already sit here: only what this call adds is profit.
        uint256 before = IERC20S(profitToken).balanceOf(address(this));

        for (uint256 i; i < calls.length; ++i) {
            if (!allowedTarget[calls[i].target]) revert TargetNotAllowed(calls[i].target);
            (bool ok, bytes memory ret) = calls[i].target.call(calls[i].data);
            if (!ok) revert CallFailed(i, ret);
        }
        if (bidSwap.target != address(0)) _swap(bidSwap);

        // What Atlas lent (up to the value it sent) is repaid in ETH below: keep it aside, so the
        // bid never spends it. Unwrap WETH for whatever the ETH balance cannot cover.
        (, uint256 borrowLiability) = IAtlasEscrow(ATLAS).shortfall();
        uint256 repay = borrowLiability < msg.value ? borrowLiability : msg.value;
        uint256 eth = address(this).balance;
        if (eth < bidAmount + repay) IWETHS(WETH).withdraw(bidAmount + repay - eth);
        if (bidAmount > 0) {
            (bool sent,) = executionEnvironment.call{value: bidAmount}("");
            if (!sent) revert EthTransferFailed();
        }

        // Settle with Atlas: repay the borrowed ETH and approve the gas liability (read now, after
        // all the work) against the solver's bonded balance.
        (uint256 gasLiability,) = IAtlasEscrow(ATLAS).shortfall();
        IAtlasEscrow(ATLAS).reconcile{value: repay}(gasLiability);

        uint256 bal = IERC20S(profitToken).balanceOf(address(this));
        profit = bal > before ? bal - before : 0;
        if (profit < minProfit) revert InsufficientProfit(profit, minProfit);
        if (bal > 0) _safeTransfer(profitToken, PROFIT_RECEIVER, bal);
        if (profitToken != WETH) {
            uint256 weth = IERC20S(WETH).balanceOf(address(this));
            if (weth > 0) _safeTransfer(WETH, PROFIT_RECEIVER, weth);
        }
        // ETH left after the bid and the settlement (value Atlas sent beyond what it lent, unwrap
        // rounding): nothing stays at rest.
        uint256 rest = address(this).balance;
        if (rest > 0) {
            (bool sent,) = PROFIT_RECEIVER.call{value: rest}("");
            if (!sent) revert EthTransferFailed();
        }
    }

    /// @dev WETH unwrapping sends ETH here.
    receive() external payable {}

    function _swap(SwapStep memory s) private {
        if (!allowedTarget[s.target]) revert TargetNotAllowed(s.target);
        uint256 amount = s.amountIn == type(uint256).max ? IERC20S(s.tokenIn).balanceOf(address(this)) : s.amountIn;
        _forceApprove(s.tokenIn, s.target, amount);
        (bool ok, bytes memory ret) = s.target.call(s.data);
        if (!ok) revert BidSwapFailed(ret);
        _forceApprove(s.tokenIn, s.target, 0);
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

    function _forceApprove(address token, address spender, uint256 amount) private {
        if (!_tokenCall(token, abi.encodeWithSelector(0x095ea7b3, spender, amount))) {
            if (!_tokenCall(token, abi.encodeWithSelector(0x095ea7b3, spender, 0))) revert TokenCallFailed(token);
            if (!_tokenCall(token, abi.encodeWithSelector(0x095ea7b3, spender, amount))) revert TokenCallFailed(token);
        }
    }

    function _safeTransfer(address token, address to, uint256 amount) private {
        if (!_tokenCall(token, abi.encodeWithSelector(0xa9059cbb, to, amount))) revert TokenCallFailed(token);
    }

    function _tokenCall(address token, bytes memory data) private returns (bool) {
        (bool ok, bytes memory ret) = token.call(data);
        return ok && (ret.length == 0 || (ret.length == 32 && abi.decode(ret, (bool)))) && token.code.length > 0;
    }
}
