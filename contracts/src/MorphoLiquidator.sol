// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {IMorpho, IMorphoLiquidateCallback, MarketParams} from "./interfaces/IMorpho.sol";
import {IV3Pool, MIN_SQRT_PRICE_LIMIT, MAX_SQRT_PRICE_LIMIT} from "./interfaces/IV3Pool.sol";

interface IERC20Min {
    function balanceOf(address) external view returns (uint256);
}

/// @notice One exit swap executed inside the Morpho liquidation callback.
/// @dev `amountIn` is the exact allowance granted to `target` for `tokenIn`
///      (type(uint256).max = this contract's whole balance of `tokenIn`).
struct SwapStep {
    address target;
    address tokenIn;
    uint256 amountIn;
    bytes data;
}

/// @title MorphoLiquidator
/// @notice Liquidates Morpho Blue positions without capital: Morpho sends the collateral first,
///         the callback swaps it to the loan token, Morpho then pulls the repayment.
/// @dev Security model:
///      - only `executor` (hot wallet) can start a liquidation; only `owner` (cold wallet) can
///        change settings, unpause or move funds; ownership moves in two steps;
///      - the callback only runs inside our own liquidation (transient flag + Morpho sender);
///      - aggregator steps may only call owner-allowlisted targets, so a poisoned off-chain route
///        cannot make the contract call a token's `approve`/`transfer` or anything else. Route-mode
///        pools are NOT allowlisted (too many to curate): the contract only ever calls their
///        `swap`/`token0`/`token1` and pays them inside their own callback, at most the hop input,
///        and a route must spend exactly the seized collateral; with nothing held at rest, a fake
///        pool can only make the tx revert;
///      - the collateral actually received must match what Morpho says it seized (rejects
///        fee-on-transfer / rebasing / malicious collateral);
///      - swap-target allowances are exact and reset to zero; Morpho holds a standing allowance on
///        the loan tokens (set once by the owner via `approveMorpho`): Morpho only pulls from the
///        caller of the action it executes, and no funds stay in the contract after a tx;
///      - the whole tx reverts unless the loan-token balance grew by at least `minProfit`.
///      Two exit modes: aggregator steps (`liquidate`, allowlisted targets + exact approvals) and a
///      direct V3-pool route (`liquidateRoute`, no approvals: pools are paid inside their own swap
///      callback, which only the pool currently being swapped may trigger, for at most its input).
contract MorphoLiquidator is IMorphoLiquidateCallback {
    IMorpho public immutable MORPHO;
    /// @notice Where every profit goes (cold wallet). Immutable to keep a cold SLOAD off the hot
    ///         path; changing it means deploying a new liquidator (cheap on Base).
    address public immutable PROFIT_RECEIVER;

    address public owner;
    /// @dev executor and paused share one storage slot: the hot path reads both with one SLOAD.
    address public executor;
    /// @notice Kill switch: executor or owner can pause, only owner can unpause.
    bool public paused;
    address public pendingOwner;
    mapping(address => bool) public allowedTarget;

    /// @dev Set only while our own liquidate() call is in flight, so the callback cannot be triggered by anyone else.
    bool private transient inFlight;
    /// @dev Collateral the callback must see (= seizedAssets; the contract holds nothing at rest).
    uint256 private transient expectedCollateral;
    address private transient collateralInFlight;
    /// @dev Pool whose swap() is running right now, and the most it may ask to be paid.
    address private transient activePool;
    uint256 private transient activeMaxIn;
    /// @dev Route mode: the pooled connector token split by share across sink hops (bit2), whose
    ///      rounding remainder is swept after the liquidation.
    address private transient connectorInFlight;

    /// @dev Packed production entry (see fallback): tag(1) loan(20) collateral(20) oracle(20) irm(20)
    ///      lltv(8) borrower(20) seized(16) minProfit(16) deadlineBlock(4) | route...
    uint256 private constant PACKED_HEAD = 145;
    bytes1 private constant PACKED_TAG = 0x00;

    /// @dev Route hop: pool(20) | flags(1) | amount(16). flags:
    ///      bit0 = zeroForOne;
    ///      bit1 = amountIn is the previous hop output (multi-hop chain);
    ///      bit2 = amount is a share in bps of the contract's current balance of the hop input
    ///             token (pooled intermediate token split across several pools; 10000 = all).
    uint256 private constant HOP_LEN = 37;
    bytes1 private constant MODE_ROUTE = 0x01;

    error Unauthorized();
    error Expired();
    error IsPaused();
    error UnsupportedMode();
    error TargetNotAllowed(address target);
    error CollateralShortfall(uint256 balance, uint256 expected);
    error InsufficientProfit(uint256 profit, uint256 minProfit);
    error SwapFailed(uint256 index, bytes reason);
    error TokenCallFailed(address token);
    error BadRoute();
    error RouteSpendMismatch(uint256 spent, uint256 seized);

    event ExecutorSet(address executor);
    event OwnershipTransferStarted(address indexed pendingOwner);
    event OwnerSet(address owner);
    event PausedSet(bool paused);
    event TargetAllowed(address indexed target, bool allowed);

    modifier onlyOwner() {
        if (msg.sender != owner) revert Unauthorized();
        _;
    }

    constructor(IMorpho morpho, address owner_, address executor_, address profitReceiver_) {
        MORPHO = morpho;
        PROFIT_RECEIVER = profitReceiver_;
        owner = owner_;
        executor = executor_;
        emit OwnerSet(owner_);
        emit ExecutorSet(executor_);
    }

    /// @param seizedAssets Exact collateral to seize. Must be non-zero: the deterministic amount
    ///        is what lets the callback verify the collateral it actually received.
    /// @param repaidShares Must be zero (repay-by-shares mode is not supported).
    /// @param minProfit Minimum gain in loan token after repayment; reverts below it.
    /// @param deadlineBlock Reverts if included after this block (stale opportunity).
    function liquidate(
        MarketParams calldata marketParams,
        address borrower,
        uint256 seizedAssets,
        uint256 repaidShares,
        SwapStep[] calldata swaps,
        uint256 minProfit,
        uint256 deadlineBlock
    ) external returns (uint256 profit) {
        if (repaidShares != 0) revert UnsupportedMode();
        return _liquidate(marketParams, borrower, seizedAssets, abi.encode(marketParams.loanToken, swaps), minProfit, deadlineBlock);
    }

    /// @notice Same as `liquidate`, but the exit is a packed route of direct V3-pool swaps.
    /// @param route Concatenated hops, see HOP_LEN.
    function liquidateRoute(
        MarketParams calldata marketParams,
        address borrower,
        uint256 seizedAssets,
        bytes calldata route,
        uint256 minProfit,
        uint256 deadlineBlock
    ) external returns (uint256 profit) {
        if (route.length == 0 || route.length % HOP_LEN != 0) revert BadRoute();
        return _liquidate(
            marketParams, borrower, seizedAssets, abi.encodePacked(MODE_ROUTE, marketParams.loanToken, route), minProfit, deadlineBlock
        );
    }

    /// @notice Packed production entry: same as `liquidateRoute` with ~1/3 of the calldata
    ///         (no ABI padding; lltv as uint64, amounts as uint128, deadline as uint32). Layout:
    ///         0x00 | loan | collateral | oracle | irm | lltv(8) | borrower | seized(16) |
    ///         minProfit(16) | deadlineBlock(4) | route (37-byte hops)
    fallback() external {
        bytes calldata d = msg.data;
        if (d.length < PACKED_HEAD + HOP_LEN || d[0] != PACKED_TAG || (d.length - PACKED_HEAD) % HOP_LEN != 0) {
            revert BadRoute();
        }
        MarketParams memory mp = MarketParams({
            loanToken: address(bytes20(d[1:21])),
            collateralToken: address(bytes20(d[21:41])),
            oracle: address(bytes20(d[41:61])),
            irm: address(bytes20(d[61:81])),
            lltv: uint64(bytes8(d[81:89]))
        });
        uint256 profit = _liquidate(
            mp,
            address(bytes20(d[89:109])),
            uint128(bytes16(d[109:125])),
            abi.encodePacked(MODE_ROUTE, mp.loanToken, d[PACKED_HEAD:]),
            uint128(bytes16(d[125:141])),
            uint32(bytes4(d[141:145]))
        );
        // Return the profit like the ABI entries do (fallback has no declared return value).
        assembly ("memory-safe") {
            mstore(0, profit)
            return(0, 32)
        }
    }

    function _liquidate(
        MarketParams memory marketParams,
        address borrower,
        uint256 seizedAssets,
        bytes memory data,
        uint256 minProfit,
        uint256 deadlineBlock
    ) private returns (uint256 profit) {
        // Cheapest checks first so a stale or unwanted call fails for minimal gas.
        if (msg.sender != executor) revert Unauthorized();
        if (block.number > deadlineBlock) revert Expired();
        if (paused) revert IsPaused();
        if (seizedAssets == 0) revert UnsupportedMode();

        inFlight = true;
        collateralInFlight = marketParams.collateralToken;
        expectedCollateral = seizedAssets;
        // Transient state lives for the whole tx: a second liquidation in the same tx (a solver
        // bundling several) must not inherit the previous one's connector.
        connectorInFlight = address(0);
        MORPHO.liquidate(marketParams, borrower, seizedAssets, 0, data);
        inFlight = false;

        // Route mode spends exactly the seized collateral, so only aggregator mode can leave
        // collateral behind and needs the sweep.
        bool sweep = data[0] != MODE_ROUTE;
        profit = _settle(marketParams.loanToken, marketParams.collateralToken, minProfit, sweep);
        // A pooled connector (e.g. WETH between collateral and loan pools) can keep a rounding
        // remainder after the share-split sink hops: swept too, so nothing stays at rest.
        address connector = connectorInFlight;
        if (connector != address(0) && connector != marketParams.loanToken) {
            uint256 rest = IERC20Min(connector).balanceOf(address(this));
            if (rest > 0) _safeTransfer(connector, PROFIT_RECEIVER, rest);
        }
    }

    /// @dev Called by Morpho after the collateral has been sent to this contract.
    function onMorphoLiquidate(uint256 repaidAssets, bytes calldata data) external {
        if (msg.sender != address(MORPHO) || !inFlight) revert Unauthorized();
        // Everything Morpho said it seized must actually be here before any swap runs.
        uint256 have = IERC20Min(collateralInFlight).balanceOf(address(this));
        if (have < expectedCollateral) revert CollateralShortfall(have, expectedCollateral);

        address loanToken;
        if (data[0] == MODE_ROUTE) {
            loanToken = address(bytes20(data[1:21]));
            // Route mode has no sweep: it must spend exactly what was seized, or collateral would
            // sit in the contract and weaken the empty-at-rest assumptions of later checks.
            uint256 spent = _runRoute(data[21:]);
            if (spent != expectedCollateral) revert RouteSpendMismatch(spent, expectedCollateral);
        } else {
            SwapStep[] memory swaps;
            (loanToken, swaps) = abi.decode(data, (address, SwapStep[]));
            _runSteps(swaps);
        }

        // Morpho now pulls repaidAssets using the standing allowance (see approveMorpho).
        loanToken;
        repaidAssets;
    }

    function _runSteps(SwapStep[] memory swaps) private {
        for (uint256 i; i < swaps.length; ++i) {
            SwapStep memory s = swaps[i];
            if (!allowedTarget[s.target]) revert TargetNotAllowed(s.target);
            uint256 amount = s.amountIn == type(uint256).max ? IERC20Min(s.tokenIn).balanceOf(address(this)) : s.amountIn;
            _forceApprove(s.tokenIn, s.target, amount);
            (bool ok, bytes memory ret) = s.target.call(s.data);
            if (!ok) revert SwapFailed(i, ret);
            _forceApprove(s.tokenIn, s.target, 0);
        }
    }

    /// @dev Executes exact-input swaps directly on V3-style pools. The output of each hop lands
    ///      here; a hop flagged "chain" spends exactly the previous hop output.
    /// @return spent Sum of the explicit-amount hops (the collateral source legs).
    function _runRoute(bytes calldata route) private returns (uint256 spent) {
        uint256 prevOut;
        for (uint256 o; o < route.length; o += HOP_LEN) {
            address pool = address(bytes20(route[o:o + 20]));
            uint8 flags = uint8(route[o + 20]);
            bool zeroForOne = flags & 1 != 0;
            uint256 amountIn = uint256(uint128(bytes16(route[o + 21:o + HOP_LEN])));
            if (flags & 2 != 0) {
                amountIn = prevOut;
            } else if (flags & 4 != 0) {
                address tokenIn = zeroForOne ? IV3Pool(pool).token0() : IV3Pool(pool).token1();
                amountIn = IERC20Min(tokenIn).balanceOf(address(this)) * amountIn / 10_000;
                connectorInFlight = tokenIn;
            } else {
                spent += amountIn;
            }
            activePool = pool;
            activeMaxIn = amountIn;
            (int256 a0, int256 a1) = IV3Pool(pool).swap(
                address(this), zeroForOne, int256(amountIn), zeroForOne ? MIN_SQRT_PRICE_LIMIT : MAX_SQRT_PRICE_LIMIT, ""
            );
            int256 out = zeroForOne ? a1 : a0;
            // An honest exact-input swap reports the output as negative. A positive value would
            // wrap into a huge amount and turn the next chained hop into an exact-output swap.
            if (out >= 0) revert BadRoute();
            prevOut = uint256(-out);
        }
        activePool = address(0);
    }

    function uniswapV3SwapCallback(int256 amount0, int256 amount1, bytes calldata) external {
        _payPool(amount0, amount1);
    }

    function pancakeV3SwapCallback(int256 amount0, int256 amount1, bytes calldata) external {
        _payPool(amount0, amount1);
    }

    /// @dev Pays the pool its input. Only the pool we are swapping on right now, once, and never
    ///      more than the hop exact input: a fake pool or a reentrant call gets nothing.
    function _payPool(int256 amount0, int256 amount1) private {
        address pool = activePool;
        if (msg.sender != pool || pool == address(0)) revert Unauthorized();
        activePool = address(0);
        (address token, uint256 owed) =
            amount0 > 0 ? (IV3Pool(pool).token0(), uint256(amount0)) : (IV3Pool(pool).token1(), uint256(amount1));
        if (owed > activeMaxIn) revert Unauthorized();
        _safeTransfer(token, pool, owed);
    }

    // ---------------------------------------------------------------- admin

    function setExecutor(address executor_) external onlyOwner {
        executor = executor_;
        emit ExecutorSet(executor_);
    }

    /// @notice Standing allowance so Morpho can pull repayments without a per-tx approve
    ///         (saves an SSTORE + refund cycle on every liquidation). Call once per loan token.
    function approveMorpho(address token, bool allowed) external onlyOwner {
        _forceApprove(token, address(MORPHO), allowed ? type(uint256).max : 0);
    }

    function setAllowedTarget(address target, bool allowed) external onlyOwner {
        allowedTarget[target] = allowed;
        emit TargetAllowed(target, allowed);
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

    /// @notice Step 1 of an ownership transfer; the new owner must call acceptOwnership().
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

    /// @notice Recover tokens stuck in the contract.
    function rescue(address token, address to, uint256 amount) external onlyOwner {
        _safeTransfer(token, to, amount);
    }

    // ---------------------------------------------------------------- internal

    /// @dev The loan-token balance left after Morpho pulled its repayment is the profit (the
    ///      contract starts every tx empty). Checks it against minProfit, then sends profit and any
    ///      leftover collateral to profitReceiver.
    function _settle(address loanToken, address collateralToken, uint256 minProfit, bool sweep) private returns (uint256 profit) {
        profit = IERC20Min(loanToken).balanceOf(address(this));
        if (profit < minProfit) revert InsufficientProfit(profit, minProfit);

        if (profit > 0) _safeTransfer(loanToken, PROFIT_RECEIVER, profit);
        if (sweep) {
            // Sweep any collateral the aggregator swaps did not use (rounding dust, partial exit).
            uint256 leftover = IERC20Min(collateralToken).balanceOf(address(this));
            if (leftover > 0) _safeTransfer(collateralToken, PROFIT_RECEIVER, leftover);
        }
    }

    /// @dev Handles tokens that return no bool (USDT-style) and tokens that require a zero allowance first.
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
