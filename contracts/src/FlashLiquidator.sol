// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {IMorphoFlashLoan, IPoolManager} from "./interfaces/IFlash.sol";
import {SwapStep} from "./MorphoLiquidator.sol";

interface IERC20Bal {
    function balanceOf(address) external view returns (uint256);
}

/// @notice The lending protocol's own liquidation call (e.g. Aave `liquidationCall`, Morpho
///         `liquidate` without callback data): `target` pulls `debtAmount` of the debt token from
///         this contract and sends it the collateral.
struct LiquidationCall {
    address target;
    bytes data;
}

/// @title FlashLiquidator
/// @notice Liquidates on lending protocols that have no liquidate callback: the repayment is
///         flash-borrowed first, the protocol's liquidation call repays the debt and sends the
///         collateral, the collateral is sold for the debt token, the flash debt is repaid and what
///         is left is the profit. No capital of its own.
/// @dev Two funding sources, chosen per call (both 0 fee):
///      - FUND_MORPHO: Morpho Blue `flashLoan` -> `onMorphoFlashLoan`;
///      - FUND_V4: Uniswap V4 PoolManager `unlock` -> `unlockCallback` -> `take` ... `sync` /
///        transfer / `settle` (swap steps must not go through V4 then: the manager is locked by us).
///      Security model (same as MorphoLiquidator, plus the funding guards of AeroMigrationArb):
///      - only `executor` (hot wallet) can start; only `owner` (cold wallet) changes settings,
///        unpauses or moves funds; ownership moves in two steps;
///      - each funding callback only runs inside our own liquidation funded by that very source,
///        once (transient funding tag + the funder as sender); a funder that never calls back makes
///        the call revert (`NotFunded`);
///      - the liquidation call may only go to an owner-allowlisted lending protocol
///        (`allowedProtocol`) and every swap step to an owner-allowlisted router (`allowedTarget`):
///        two lists, so a router can never be handed the debt-token allowance meant for the
///        protocol; allowances are exact and reset to zero;
///      - the liquidation must deliver collateral (`NothingSeized`);
///      - the funder is repaid exactly what was borrowed; the whole tx reverts unless the debt-token
///        balance left is at least `minProfit`; profit and leftover collateral go to PROFIT_RECEIVER
///        (the contract holds nothing at rest).
contract FlashLiquidator {
    uint8 public constant FUND_MORPHO = 1;
    uint8 public constant FUND_V4 = 2;

    IMorphoFlashLoan public immutable MORPHO;
    /// @dev address(0): V4 funding disabled.
    IPoolManager public immutable POOL_MANAGER;
    address public immutable PROFIT_RECEIVER;

    address public owner;
    /// @dev executor and paused share one storage slot.
    address public executor;
    bool public paused;
    address public pendingOwner;
    /// @notice Swap routers the exit steps may call.
    mapping(address => bool) public allowedTarget;
    /// @notice Lending protocols the liquidation call may go to (they get the debt-token allowance).
    mapping(address => bool) public allowedProtocol;

    /// @dev Which funder may call back right now (FUND_*); 0 = none / already called back.
    uint256 private transient fundingInFlight;
    address private transient debtInFlight;
    uint256 private transient amountInFlight;
    address private transient collateralInFlight;

    error Unauthorized();
    error Expired();
    error IsPaused();
    error FundingDisabled();
    error NotFunded();
    error NothingSeized();
    error TargetNotAllowed(address target);
    error ProtocolNotAllowed(address target);
    error LiquidationFailed(bytes reason);
    error SwapFailed(uint256 index, bytes reason);
    error InsufficientProfit(uint256 profit, uint256 minProfit);
    error TokenCallFailed(address token);

    event ExecutorSet(address executor);
    event OwnershipTransferStarted(address indexed pendingOwner);
    event OwnerSet(address owner);
    event PausedSet(bool paused);
    event TargetAllowed(address indexed target, bool allowed);
    event ProtocolAllowed(address indexed protocol, bool allowed);

    modifier onlyOwner() {
        if (msg.sender != owner) revert Unauthorized();
        _;
    }

    constructor(IMorphoFlashLoan morpho, IPoolManager poolManager, address owner_, address executor_, address profitReceiver_) {
        MORPHO = morpho;
        POOL_MANAGER = poolManager;
        PROFIT_RECEIVER = profitReceiver_;
        owner = owner_;
        executor = executor_;
        emit OwnerSet(owner_);
        emit ExecutorSet(executor_);
    }

    /// @param funding FUND_MORPHO or FUND_V4.
    /// @param debtToken What the liquidation repays (and what the profit is counted in).
    /// @param debtAmount Flash-borrowed amount: what the liquidation call pulls, plus any headroom
    ///        (unspent headroom simply ends up in the profit).
    /// @param collateralToken What the liquidation call sends and `swaps` sell.
    /// @param liquidation The protocol's liquidation call; its target must be an allowed protocol.
    /// @param swaps Collateral -> debt token, through allowlisted targets.
    /// @param minProfit Minimum debt token left after repaying the flash debt; reverts below it.
    /// @param deadlineBlock Reverts if included after this block (stale opportunity).
    function liquidate(
        uint8 funding,
        address debtToken,
        uint256 debtAmount,
        address collateralToken,
        LiquidationCall calldata liquidation,
        SwapStep[] calldata swaps,
        uint256 minProfit,
        uint256 deadlineBlock
    ) external returns (uint256 profit) {
        if (msg.sender != executor) revert Unauthorized();
        if (block.number > deadlineBlock) revert Expired();
        if (paused) revert IsPaused();
        bytes memory data = abi.encode(liquidation, swaps);
        fundingInFlight = funding;
        debtInFlight = debtToken;
        amountInFlight = debtAmount;
        collateralInFlight = collateralToken;
        if (funding == FUND_MORPHO) {
            MORPHO.flashLoan(debtToken, debtAmount, data);
        } else if (funding == FUND_V4) {
            if (address(POOL_MANAGER) == address(0)) revert FundingDisabled();
            POOL_MANAGER.unlock(data);
        } else {
            revert FundingDisabled();
        }
        // The funder returned without calling back: nothing was borrowed, nothing liquidated.
        if (fundingInFlight != 0) revert NotFunded();

        profit = IERC20Bal(debtToken).balanceOf(address(this));
        if (profit < minProfit) revert InsufficientProfit(profit, minProfit);
        if (profit > 0) _safeTransfer(debtToken, PROFIT_RECEIVER, profit);
        // Collateral the swaps did not sell (rounding dust, partial exit).
        uint256 leftover = IERC20Bal(collateralToken).balanceOf(address(this));
        if (leftover > 0) _safeTransfer(collateralToken, PROFIT_RECEIVER, leftover);
    }

    /// @dev Morpho sent `assets` of the debt token; it pulls them back right after this returns.
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        if (msg.sender != address(MORPHO) || fundingInFlight != FUND_MORPHO) revert Unauthorized();
        fundingInFlight = 0;
        _liquidateAndSell(data);
        _forceApprove(debtInFlight, address(MORPHO), assets);
    }

    /// @dev The PoolManager is unlocked for us: take the debt token, liquidate, sell, pay back exactly
    ///      what we took (sync -> transfer -> settle) so our delta nets to zero before unlock returns.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        if (msg.sender != address(POOL_MANAGER) || fundingInFlight != FUND_V4) revert Unauthorized();
        fundingInFlight = 0;
        (address debt, uint256 amount) = (debtInFlight, amountInFlight);
        POOL_MANAGER.take(debt, address(this), amount);
        _liquidateAndSell(data);
        POOL_MANAGER.sync(debt);
        _safeTransfer(debt, address(POOL_MANAGER), amount);
        POOL_MANAGER.settle();
        return "";
    }

    /// @dev The liquidation itself, whatever funded it: repay through the protocol, receive the
    ///      collateral, sell it. Repaying the funder is the caller's job.
    function _liquidateAndSell(bytes calldata data) private {
        (LiquidationCall memory liq, SwapStep[] memory swaps) = abi.decode(data, (LiquidationCall, SwapStep[]));
        if (!allowedProtocol[liq.target]) revert ProtocolNotAllowed(liq.target);
        address debt = debtInFlight;
        address collateral = collateralInFlight;
        uint256 before = IERC20Bal(collateral).balanceOf(address(this));
        _forceApprove(debt, liq.target, amountInFlight);
        (bool ok, bytes memory ret) = liq.target.call(liq.data);
        if (!ok) revert LiquidationFailed(ret);
        _forceApprove(debt, liq.target, 0);
        if (IERC20Bal(collateral).balanceOf(address(this)) <= before) revert NothingSeized();
        _runSteps(swaps);
    }

    function _runSteps(SwapStep[] memory swaps) private {
        for (uint256 i; i < swaps.length; ++i) {
            SwapStep memory s = swaps[i];
            if (!allowedTarget[s.target]) revert TargetNotAllowed(s.target);
            uint256 amount = s.amountIn == type(uint256).max ? IERC20Bal(s.tokenIn).balanceOf(address(this)) : s.amountIn;
            _forceApprove(s.tokenIn, s.target, amount);
            (bool ok, bytes memory ret) = s.target.call(s.data);
            if (!ok) revert SwapFailed(i, ret);
            _forceApprove(s.tokenIn, s.target, 0);
        }
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

    function setAllowedProtocol(address protocol, bool allowed) external onlyOwner {
        allowedProtocol[protocol] = allowed;
        emit ProtocolAllowed(protocol, allowed);
    }

    /// @notice The executor (hot wallet) or the owner may pause, e.g. an operator stop with the hot
    ///         key at hand (the bot itself never calls it; its kill switch is a file); only owner may
    ///         resume.
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
