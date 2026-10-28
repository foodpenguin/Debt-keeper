// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {IMorpho, IOracle, Id, MarketParams, Position} from "../src/interfaces/IMorpho.sol";
import {FlashLiquidator, LiquidationCall} from "../src/FlashLiquidator.sol";
import {IMorphoFlashLoan, IPoolManager} from "../src/interfaces/IFlash.sol";
import {SwapStep} from "../src/MorphoLiquidator.sol";

interface IERC20F {
    function balanceOf(address) external view returns (uint256);
}

interface ISwapRouter02F {
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

/// Fork test on Base mainnet for the callback-free path: Morpho stands in for a protocol without a
/// liquidate callback (`liquidate` with empty data just pulls the repayment), funded by a real
/// Morpho flash loan or by real V4 flash accounting. A WETH/USDC borrower is pushed under water by
/// mocking the oracle 10% lower.
/// Run: forge test --match-contract FlashLiquidatorFork -vv   (BASE_RPC_URL optional; -j 1 on free plans)
contract FlashLiquidatorForkTest is Test {
    IMorpho constant MORPHO = IMorpho(0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb);
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant UNI_V3_ROUTER = 0x2626664c2603336E57B271c5C0b26F421741e481;
    Id constant MARKET = Id.wrap(0x8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda);
    address constant BORROWER = 0x33B5a89294e778d14e8Ce4b0828889B6b81b6488;

    address owner = makeAddr("owner");
    address exec = makeAddr("exec");
    address receiver = makeAddr("receiver");
    FlashLiquidator liq;
    MarketParams mp;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        // Pinned, so results do not drift with the chain (FORK_BLOCK overrides it; FORK_BLOCK=0 = latest).
        uint256 forkBlock = vm.envOr("FORK_BLOCK", uint256(53206447));
        if (forkBlock == 0) vm.createSelectFork(rpc);
        else vm.createSelectFork(rpc, forkBlock);
        mp = MORPHO.idToMarketParams(MARKET);
        liq = new FlashLiquidator(IMorphoFlashLoan(address(MORPHO)), IPoolManager(POOL_MANAGER), owner, exec, receiver);
        vm.startPrank(owner);
        liq.setAllowedProtocol(address(MORPHO), true);
        liq.setAllowedTarget(UNI_V3_ROUTER, true);
        vm.stopPrank();
        uint256 p = IOracle(mp.oracle).price();
        vm.mockCall(mp.oracle, abi.encodeWithSelector(IOracle.price.selector), abi.encode(p * 9_000 / 10_000));
    }

    function _call(uint256 seize) internal view returns (LiquidationCall memory) {
        return LiquidationCall({target: address(MORPHO), data: abi.encodeCall(IMorpho.liquidate, (mp, BORROWER, seize, 0, ""))});
    }

    function _sell(uint256 amountIn) internal view returns (SwapStep[] memory s) {
        s = new SwapStep[](1);
        s[0] = SwapStep({
            target: UNI_V3_ROUTER,
            tokenIn: WETH,
            amountIn: amountIn,
            data: abi.encodeCall(
                ISwapRouter02F.exactInputSingle,
                (ISwapRouter02F.ExactInputSingleParams({
                        tokenIn: WETH,
                        tokenOut: USDC,
                        fee: 500,
                        recipient: address(liq),
                        amountIn: amountIn,
                        amountOutMinimum: 0,
                        sqrtPriceLimitX96: 0
                    }))
            )
        });
    }

    /// Flash-borrows more than the repayment (headroom): Morpho pulls exactly its repaidAssets, the
    /// rest is returned to the funder with the loan, and only the real gain counts as profit.
    function _liquidateWith(uint8 funding) internal returns (uint256 profit) {
        Position memory before = MORPHO.position(MARKET, BORROWER);
        assertGt(before.borrowShares, 0, "borrower has no debt at this block");
        uint256 seize = 1 ether;
        uint256 funderBefore = IERC20F(USDC).balanceOf(funding == 1 ? address(MORPHO) : POOL_MANAGER);
        vm.prank(exec);
        profit = liq.liquidate(funding, USDC, 10_000e6, WETH, _call(seize), _sell(seize), 1e6, block.number);
        Position memory afterPos = MORPHO.position(MARKET, BORROWER);
        assertEq(before.collateral - afterPos.collateral, seize, "seized amount");
        assertLt(afterPos.borrowShares, before.borrowShares, "debt reduced");
        assertEq(IERC20F(USDC).balanceOf(receiver), profit, "profit forwarded");
        assertEq(IERC20F(USDC).balanceOf(address(liq)) + IERC20F(WETH).balanceOf(address(liq)), 0, "nothing at rest");
        if (funding == 2) assertEq(IERC20F(USDC).balanceOf(POOL_MANAGER), funderBefore, "PoolManager repaid exactly");
        else assertGe(IERC20F(USDC).balanceOf(address(MORPHO)), funderBefore, "Morpho repaid (and received the liquidation repayment)");
    }

    function test_morphoFlashLoanFundsACallbackFreeLiquidation() public {
        uint256 profit = _liquidateWith(1);
        assertGt(profit, 1e6);
        emit log_named_decimal_uint("profit USDC (Morpho flash loan)", profit, 6);
    }

    function test_v4FlashAccountingFundsACallbackFreeLiquidation() public {
        uint256 s = vm.snapshotState();
        uint256 viaMorpho = _liquidateWith(1);
        vm.revertToState(s);
        uint256 viaV4 = _liquidateWith(2);
        assertEq(viaV4, viaMorpho, "same liquidation, same profit (both 0 fee)");
        emit log_named_decimal_uint("profit USDC (V4 flash accounting)", viaV4, 6);
    }

    function test_unprofitableExitReverts() public {
        vm.prank(exec);
        vm.expectRevert(); // InsufficientProfit
        liq.liquidate(2, USDC, 10_000e6, WETH, _call(1 ether), _sell(1 ether), 1_000_000e6, block.number);
    }

    function test_protocolTargetMustBeAllowlisted() public {
        vm.prank(owner);
        liq.setAllowedProtocol(address(MORPHO), false);
        vm.prank(exec);
        vm.expectRevert(); // ProtocolNotAllowed, wrapped by the funder's callback revert
        liq.liquidate(2, USDC, 10_000e6, WETH, _call(1 ether), _sell(1 ether), 0, block.number);
    }
}
