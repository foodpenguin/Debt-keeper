// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {IMorpho, IOracle, Id, MarketParams, Position} from "../src/interfaces/IMorpho.sol";
import {MorphoLiquidator, SwapStep} from "../src/MorphoLiquidator.sol";

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
}

/// @dev Uniswap V3 SwapRouter02 (no deadline field in the struct).
interface ISwapRouter02 {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256);
}

/// Fork test on Base mainnet: forces a real WETH/USDC borrower under water by mocking the
/// Morpho oracle 10% lower, then liquidates with zero capital and exits through Uniswap V3.
/// Run: forge test --match-contract Fork -vv   (BASE_RPC_URL optional; FORK_BLOCK optional)
contract MorphoLiquidatorForkTest is Test {
    IMorpho constant MORPHO = IMorpho(0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb);
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant UNI_V3_ROUTER = 0x2626664c2603336E57B271c5C0b26F421741e481;
    // WETH/USDC, LLTV 86%
    Id constant MARKET = Id.wrap(0x8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda);
    // A large borrower (HF ~1.06 when this test was written)
    address constant BORROWER = 0x33B5a89294e778d14e8Ce4b0828889B6b81b6488;

    address owner = makeAddr("owner");
    address executor = makeAddr("executor");
    address receiver = makeAddr("receiver");

    MorphoLiquidator liq;
    MarketParams mp;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        // Pinned, so results do not drift with the chain (FORK_BLOCK overrides it; FORK_BLOCK=0 = latest).
        uint256 forkBlock = vm.envOr("FORK_BLOCK", uint256(53206447));
        if (forkBlock == 0) vm.createSelectFork(rpc);
        else vm.createSelectFork(rpc, forkBlock);

        mp = MORPHO.idToMarketParams(MARKET);
        liq = new MorphoLiquidator(MORPHO, owner, executor, receiver);
        vm.prank(owner);
        liq.approveMorpho(0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913, true); // USDC
        vm.prank(owner);
        liq.setAllowedTarget(UNI_V3_ROUTER, true);
    }

    function _crashOracle(uint256 bps) internal {
        uint256 p = IOracle(mp.oracle).price();
        vm.mockCall(mp.oracle, abi.encodeWithSelector(IOracle.price.selector), abi.encode(p * (10_000 - bps) / 10_000));
    }

    function _swapWethToUsdc(uint256 amountIn) internal view returns (SwapStep[] memory swaps) {
        swaps = new SwapStep[](1);
        swaps[0] = SwapStep({
            target: UNI_V3_ROUTER,
            tokenIn: WETH,
            amountIn: amountIn,
            data: abi.encodeWithSelector(
                ISwapRouter02.exactInputSingle.selector,
                ISwapRouter02.ExactInputSingleParams({
                    tokenIn: WETH,
                    tokenOut: USDC,
                    fee: 500,
                    recipient: address(liq),
                    amountIn: amountIn,
                    amountOutMinimum: 0,
                    sqrtPriceLimitX96: 0
                })
            )
        });
    }

    function test_liquidate_profitable() public {
        Position memory before = MORPHO.position(MARKET, BORROWER);
        assertGt(before.borrowShares, 0, "borrower has no debt at this block");
        _crashOracle(1_000);

        uint256 seize = 1 ether;
        vm.prank(executor);
        uint256 profit = liq.liquidate(mp, BORROWER, seize, 0, _swapWethToUsdc(seize), 1e6, block.number);

        Position memory afterPos = MORPHO.position(MARKET, BORROWER);
        assertEq(before.collateral - afterPos.collateral, seize, "seized amount");
        assertLt(afterPos.borrowShares, before.borrowShares, "debt not reduced");
        assertEq(IERC20(USDC).balanceOf(receiver), profit, "profit not forwarded");
        assertEq(IERC20(USDC).balanceOf(address(liq)), 0, "usdc left in contract");
        assertEq(IERC20(WETH).balanceOf(address(liq)), 0, "weth left in contract");
        emit log_named_decimal_uint("profit USDC", profit, 6);
    }

    function test_revert_notExecutor() public {
        _crashOracle(1_000);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.liquidate(mp, BORROWER, 1 ether, 0, _swapWethToUsdc(1 ether), 0, block.number);
    }

    function test_revert_expired() public {
        _crashOracle(1_000);
        vm.prank(executor);
        vm.expectRevert(MorphoLiquidator.Expired.selector);
        liq.liquidate(mp, BORROWER, 1 ether, 0, _swapWethToUsdc(1 ether), 0, block.number - 1);
    }

    function test_revert_minProfitTooHigh() public {
        _crashOracle(1_000);
        vm.prank(executor);
        vm.expectRevert();
        liq.liquidate(mp, BORROWER, 1 ether, 0, _swapWethToUsdc(1 ether), 1_000_000e6, block.number);
    }

    function test_revert_healthyPosition() public {
        // No oracle crash: position is healthy, Morpho must refuse.
        vm.prank(executor);
        vm.expectRevert();
        liq.liquidate(mp, BORROWER, 1 ether, 0, _swapWethToUsdc(1 ether), 0, block.number);
    }

    function test_revert_callbackFromOutsider() public {
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.onMorphoLiquidate(1, abi.encode(USDC, new SwapStep[](0)));
        // Even Morpho's address cannot call it when no liquidation is in flight.
        vm.prank(address(MORPHO));
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.onMorphoLiquidate(1, abi.encode(USDC, new SwapStep[](0)));
    }

    function test_admin_onlyOwner() public {
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.setExecutor(address(1));
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.rescue(USDC, address(this), 1);
        vm.prank(owner);
        liq.setExecutor(address(1));
        assertEq(liq.executor(), address(1));
    }
}
