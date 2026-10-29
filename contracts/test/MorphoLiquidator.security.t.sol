// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {IMorpho, MarketParams} from "../src/interfaces/IMorpho.sol";
import {MorphoLiquidator, SwapStep} from "../src/MorphoLiquidator.sol";
import {
    MockERC20,
    MockMorpho,
    MockRouter,
    ThiefRouter,
    GreedyRouter,
    ReentrantRouter,
    CallbackForger,
    MockV3Pool,
    TokenSwitchingPool
} from "./mocks/Mocks.sol";

/// Attack and misuse scenarios against the liquidator, using mocks (no fork, fast).
contract MorphoLiquidatorSecurityTest is Test {
    address owner = makeAddr("owner");
    address executor = makeAddr("executor");
    address receiver = makeAddr("receiver");
    address attacker = makeAddr("attacker");

    MockERC20 loan;
    MockERC20 coll;
    MockMorpho morpho;
    MockRouter router;
    MorphoLiquidator liq;
    MarketParams mp;

    uint256 constant SEIZE = 1_000e18;

    function setUp() public {
        loan = new MockERC20();
        coll = new MockERC20();
        morpho = new MockMorpho(loan, coll);
        router = new MockRouter();
        coll.mint(address(morpho), 1_000_000e18);
        loan.mint(address(router), 1_000_000e18);
        liq = new MorphoLiquidator(IMorpho(address(morpho)), owner, executor, receiver);
        mp = MarketParams({loanToken: address(loan), collateralToken: address(coll), oracle: address(0), irm: address(0), lltv: 0.86e18});
        vm.startPrank(owner);
        liq.setAllowedTarget(address(router), true);
        liq.approveMorpho(address(loan), true);
        vm.stopPrank();
    }

    function _step(address target, uint256 amount) internal view returns (SwapStep[] memory s) {
        s = new SwapStep[](1);
        s[0] = SwapStep({target: target, tokenIn: address(coll), amountIn: amount, data: abi.encodeCall(MockRouter.swap, (coll, loan, amount))});
    }

    function _allow(address target) internal {
        vm.prank(owner);
        liq.setAllowedTarget(target, true);
    }

    function _run(SwapStep[] memory s, uint256 minProfit) internal returns (uint256) {
        vm.prank(executor);
        return liq.liquidate(mp, address(0xB0B), SEIZE, 0, s, minProfit, block.number);
    }

    // ------------------------------------------------------------- happy path

    function test_happyPath_noFundsOrAllowancesLeft() public {
        uint256 profit = _run(_step(address(router), SEIZE), 1);
        assertEq(profit, SEIZE / 10, "10% bonus at 1:1");
        assertEq(loan.balanceOf(receiver), profit);
        assertEq(loan.balanceOf(address(liq)), 0, "loan left in contract");
        assertEq(coll.balanceOf(address(liq)), 0, "collateral left in contract");
        assertEq(coll.allowance(address(liq), address(router)), 0, "router allowance not reset");
        assertGt(loan.allowance(address(liq), address(morpho)), type(uint128).max, "standing Morpho allowance");
        assertEq(loan.allowance(address(liq), attacker), 0);
    }

    // ------------------------------------------------------------- routing abuse

    function test_targetNotAllowlisted() public {
        MockRouter other = new MockRouter();
        loan.mint(address(other), 1_000_000e18);
        vm.expectRevert(abi.encodeWithSelector(MorphoLiquidator.TargetNotAllowed.selector, address(other)));
        _run(_step(address(other), SEIZE), 0);
    }

    /// A poisoned route that makes the contract approve the attacker on a token is impossible:
    /// tokens are never allowlisted targets.
    function test_tokenApproveTrickBlocked() public {
        SwapStep[] memory s = new SwapStep[](1);
        s[0] = SwapStep({target: address(coll), tokenIn: address(coll), amountIn: 0, data: abi.encodeCall(MockERC20.approve, (attacker, type(uint256).max))});
        vm.expectRevert(abi.encodeWithSelector(MorphoLiquidator.TargetNotAllowed.selector, address(coll)));
        _run(s, 0);
        assertEq(coll.allowance(address(liq), attacker), 0);
    }

    function test_thiefRouterGainsNothing() public {
        ThiefRouter thief = new ThiefRouter();
        _allow(address(thief));
        SwapStep[] memory s = _step(address(thief), SEIZE);
        s[0].data = abi.encodeCall(ThiefRouter.swap, (coll, loan, SEIZE));
        vm.expectRevert(); // Morpho cannot pull the repayment -> whole tx reverts
        _run(s, 0);
        assertEq(coll.balanceOf(address(thief)), 0, "theft must be rolled back");
    }

    function test_greedyRouterCannotExceedAllowance() public {
        GreedyRouter greedy = new GreedyRouter();
        _allow(address(greedy));
        loan.mint(address(greedy), 1_000_000e18);
        coll.mint(address(liq), SEIZE); // extra balance the router would like to grab
        SwapStep[] memory s = _step(address(greedy), SEIZE);
        s[0].data = abi.encodeCall(GreedyRouter.swap, (coll, loan, SEIZE));
        vm.expectRevert();
        _run(s, 0);
    }

    // ------------------------------------------------------------- callback / reentrancy

    function test_reentrancyFromRouterBlocked() public {
        ReentrantRouter r = new ReentrantRouter();
        r.arm(liq, mp);
        _allow(address(r));
        SwapStep[] memory s = _step(address(r), SEIZE);
        s[0].data = abi.encodeCall(ReentrantRouter.swap, (coll, loan, SEIZE));
        // Inner liquidate() reverts with Unauthorized (router is not the executor) -> SwapFailed.
        vm.expectRevert(abi.encodeWithSelector(MorphoLiquidator.SwapFailed.selector, 0, abi.encodeWithSelector(MorphoLiquidator.Unauthorized.selector)));
        _run(s, 0);
    }

    function test_forgedCallbackDuringFlightBlocked() public {
        CallbackForger f = new CallbackForger();
        _allow(address(f));
        SwapStep[] memory s = _step(address(f), SEIZE);
        s[0].data = abi.encodeCall(CallbackForger.swap, (coll, loan, SEIZE));
        vm.expectRevert(abi.encodeWithSelector(MorphoLiquidator.SwapFailed.selector, 0, abi.encodeWithSelector(MorphoLiquidator.Unauthorized.selector)));
        _run(s, 0);
    }

    function test_callbackOutsideFlightBlocked() public {
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.onMorphoLiquidate(1, abi.encode(address(loan), new SwapStep[](0)));
        vm.prank(address(morpho));
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.onMorphoLiquidate(1, abi.encode(address(loan), new SwapStep[](0)));
    }

    // ------------------------------------------------------------- malicious collateral

    function test_feeOnTransferCollateralRejected() public {
        coll.setFee(100); // 1% skimmed on every transfer
        vm.expectRevert(abi.encodeWithSelector(MorphoLiquidator.CollateralShortfall.selector, SEIZE * 99 / 100, SEIZE));
        _run(_step(address(router), SEIZE), 0);
    }

    // ------------------------------------------------------------- modes / profit

    function test_unsupportedModes() public {
        vm.startPrank(executor);
        vm.expectRevert(MorphoLiquidator.UnsupportedMode.selector);
        liq.liquidate(mp, address(0xB0B), 0, 1, new SwapStep[](0), 0, block.number);
        vm.expectRevert(MorphoLiquidator.UnsupportedMode.selector);
        liq.liquidate(mp, address(0xB0B), SEIZE, 1, new SwapStep[](0), 0, block.number);
        vm.stopPrank();
    }

    function test_minProfitEnforced() public {
        vm.expectRevert(abi.encodeWithSelector(MorphoLiquidator.InsufficientProfit.selector, SEIZE / 10, SEIZE / 10 + 1));
        _run(_step(address(router), SEIZE), SEIZE / 10 + 1);
    }

    /// Whatever the bonus, a successful call always returns at least minProfit to the receiver.
    function testFuzz_profitAtLeastMin(uint16 repayBps, uint96 minProfit) public {
        repayBps = uint16(bound(repayBps, 5_000, 12_000));
        vm.store(address(morpho), bytes32(uint256(0)), bytes32(uint256(repayBps)));
        vm.prank(executor);
        try liq.liquidate(mp, address(0xB0B), SEIZE, 0, _step(address(router), SEIZE), minProfit, block.number) returns (uint256 p) {
            assertGe(p, minProfit);
            assertEq(loan.balanceOf(receiver), p);
        } catch {
            // Revert is the only allowed alternative; nothing may have moved.
            assertEq(loan.balanceOf(receiver), 0);
        }
        assertEq(loan.balanceOf(address(liq)), 0);
        assertEq(coll.balanceOf(address(liq)), 0);
    }

    // ------------------------------------------------------------- direct pool routes

    function _poolRoute(MockV3Pool pool, uint8 mode) internal returns (bytes memory) {
        pool.setMode(mode);
        return abi.encodePacked(address(pool), uint8(1), uint128(SEIZE));
    }

    function test_route_honestPool() public {
        MockV3Pool pool = new MockV3Pool(coll, loan);
        loan.mint(address(pool), 1_000_000e18);
        bytes memory route = _poolRoute(pool, 0);
        vm.prank(executor);
        uint256 p = liq.liquidateRoute(mp, address(0xB0B), SEIZE, route, 1, block.number);
        assertEq(p, SEIZE / 10);
        assertEq(coll.balanceOf(address(liq)), 0);
        assertEq(loan.balanceOf(address(liq)), 0);
    }

    function test_route_poolAskingMoreThanInputGetsNothing() public {
        MockV3Pool pool = new MockV3Pool(coll, loan);
        loan.mint(address(pool), 1_000_000e18);
        coll.mint(address(liq), SEIZE); // extra collateral sitting there would be the prize
        bytes memory route = _poolRoute(pool, 1);
        vm.prank(executor);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.liquidateRoute(mp, address(0xB0B), SEIZE, route, 0, block.number);
    }

    /// Route mode has no collateral sweep, so a route must spend exactly the seized amount.
    function test_route_mustSpendExactlySeized() public {
        MockV3Pool pool = new MockV3Pool(coll, loan);
        loan.mint(address(pool), 1_000_000e18);
        pool.setMode(0);
        bytes memory half = abi.encodePacked(address(pool), uint8(1), uint128(SEIZE / 2));
        vm.prank(executor);
        vm.expectRevert(abi.encodeWithSelector(MorphoLiquidator.RouteSpendMismatch.selector, SEIZE / 2, SEIZE));
        liq.liquidateRoute(mp, address(0xB0B), SEIZE, half, 0, block.number);
    }

    /// A pool reporting a positive output must be rejected before its value feeds a chained hop
    /// (it would wrap into a huge amount and flip the next swap to exact-output).
    function test_route_positiveOutputRejected() public {
        MockV3Pool pool = new MockV3Pool(coll, loan);
        loan.mint(address(pool), 1_000_000e18);
        bytes memory route = _poolRoute(pool, 4);
        vm.prank(executor);
        vm.expectRevert(MorphoLiquidator.BadRoute.selector);
        liq.liquidateRoute(mp, address(0xB0B), SEIZE, route, 0, block.number);
    }

    function test_route_doubleCallbackRejected() public {
        MockV3Pool pool = new MockV3Pool(coll, loan);
        loan.mint(address(pool), 1_000_000e18);
        coll.mint(address(liq), SEIZE);
        bytes memory route = _poolRoute(pool, 2);
        vm.prank(executor);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.liquidateRoute(mp, address(0xB0B), SEIZE, route, 0, block.number);
    }

    function test_route_poolPayingNothingReverts() public {
        MockV3Pool pool = new MockV3Pool(coll, loan);
        bytes memory route = _poolRoute(pool, 3);
        vm.prank(executor);
        vm.expectRevert(); // Morpho cannot be repaid -> whole tx reverts, pool keeps nothing
        liq.liquidateRoute(mp, address(0xB0B), SEIZE, route, 0, block.number);
        assertEq(coll.balanceOf(address(pool)), 0);
    }

    /// A pool that runs out of liquidity takes less than its hop input: the liquidation still goes
    /// through, and the collateral it did not take is swept instead of staying at rest.
    function test_route_partialFillLeftoverIsSwept() public {
        MockV3Pool pool = new MockV3Pool(coll, loan);
        loan.mint(address(pool), 1_000_000e18);
        bytes memory route = _poolRoute(pool, 5);
        vm.prank(executor);
        uint256 p = liq.liquidateRoute(mp, address(0xB0B), SEIZE, route, 1, block.number);
        assertEq(p, SEIZE * 95 / 100 - SEIZE * 9 / 10, "profit on the 95% that was sold");
        assertEq(coll.balanceOf(address(liq)), 0, "nothing at rest");
        assertEq(coll.balanceOf(receiver), SEIZE * 5 / 100, "unsold collateral swept");
    }

    /// The token a pool is paid in is read before its swap: switching `token0` inside the swap does
    /// not make the contract pay it the loan token it has just received.
    function test_route_poolCannotSwitchItsPaymentToken() public {
        TokenSwitchingPool pool = new TokenSwitchingPool(coll, loan);
        loan.mint(address(pool), 1_000_000e18);
        bytes memory route = abi.encodePacked(address(pool), uint8(1), uint128(SEIZE));
        vm.prank(executor);
        uint256 p = liq.liquidateRoute(mp, address(0xB0B), SEIZE, route, 1, block.number);
        assertEq(p, SEIZE / 10);
        assertEq(coll.balanceOf(address(pool)), SEIZE, "paid in collateral");
        assertEq(loan.balanceOf(address(liq)), 0);
    }

    /// A source leg sells the seized collateral and nothing else.
    function test_route_sourceLegMustSellCollateral() public {
        MockV3Pool pool = new MockV3Pool(loan, coll); // token0 = loan token
        coll.mint(address(pool), 1_000_000e18);
        bytes memory route = abi.encodePacked(address(pool), uint8(1), uint128(SEIZE));
        vm.prank(executor);
        vm.expectRevert(MorphoLiquidator.BadRoute.selector);
        liq.liquidateRoute(mp, address(0xB0B), SEIZE, route, 0, block.number);
    }

    // ------------------------------------------------------------- packed entry

    function _packedCall(address borrower, bytes memory route) internal view returns (bytes memory) {
        return abi.encodePacked(
            bytes1(0x00), mp.loanToken, mp.collateralToken, mp.oracle, mp.irm, uint64(mp.lltv),
            borrower, uint128(SEIZE), uint128(1), uint32(block.number), route
        );
    }

    function test_packed_happyPathAndAuth() public {
        MockV3Pool pool = new MockV3Pool(coll, loan);
        loan.mint(address(pool), 1_000_000e18);
        bytes memory route = _poolRoute(pool, 0);
        bytes memory cd = _packedCall(address(0xB0B), route);
        (bool ok, bytes memory ret) = address(liq).call(cd); // not the executor
        assertFalse(ok);
        assertEq(bytes4(ret), MorphoLiquidator.Unauthorized.selector);
        vm.prank(executor);
        (ok,) = address(liq).call(cd);
        assertTrue(ok);
        assertEq(loan.balanceOf(receiver), SEIZE / 10);
    }

    function test_packed_malformedRejected() public {
        bytes memory route = abi.encodePacked(address(router), uint8(1), uint128(SEIZE));
        bytes memory good = _packedCall(address(0xB0B), route);
        vm.startPrank(executor);
        (bool ok, bytes memory ret) = address(liq).call(bytes.concat(good, hex"00")); // trailing byte
        assertFalse(ok);
        assertEq(bytes4(ret), MorphoLiquidator.BadRoute.selector);
        bytes memory head = new bytes(145);
        (ok, ret) = address(liq).call(head); // header only, no hop
        assertFalse(ok);
        assertEq(bytes4(ret), MorphoLiquidator.BadRoute.selector);
        good[0] = 0x01; // wrong tag
        (ok, ret) = address(liq).call(good);
        assertFalse(ok);
        vm.stopPrank();
    }

    /// The packed calldata's first 4 bytes (tag + loan token prefix) must never hit a real
    /// function selector, for every loan token we may use.
    function test_packed_noSelectorCollision() public pure {
        bytes4[19] memory sels = [
            MorphoLiquidator.liquidate.selector, MorphoLiquidator.liquidateRoute.selector,
            MorphoLiquidator.onMorphoLiquidate.selector, MorphoLiquidator.uniswapV3SwapCallback.selector,
            MorphoLiquidator.pancakeV3SwapCallback.selector, MorphoLiquidator.setExecutor.selector,
            MorphoLiquidator.setAllowedTarget.selector, MorphoLiquidator.approveMorpho.selector,
            MorphoLiquidator.pause.selector, MorphoLiquidator.unpause.selector,
            MorphoLiquidator.transferOwnership.selector, MorphoLiquidator.acceptOwnership.selector,
            MorphoLiquidator.rescue.selector, bytes4(keccak256("owner()")), bytes4(keccak256("executor()")),
            bytes4(keccak256("pendingOwner()")),
            bytes4(keccak256("paused()")), bytes4(keccak256("allowedTarget(address)")), bytes4(keccak256("MORPHO()"))
        ];
        address[4] memory loans = [
            0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913, // USDC
            0x60a3E35Cc302bFA44Cb288Bc5a4F316Fdb1adb42, // EURC
            0x4200000000000000000000000000000000000006, // WETH
            0xfde4C96c8593536E31F229EA8f37b2ADa2699bb2 // USDT
        ];
        for (uint256 i; i < loans.length; ++i) {
            bytes4 prefix = bytes4(abi.encodePacked(bytes1(0x00), loans[i]));
            for (uint256 j; j < sels.length; ++j) {
                assertTrue(prefix != sels[j], "packed prefix collides with a selector");
            }
        }
    }

    // ------------------------------------------------------------- admin

    function test_pauseKillSwitch() public {
        vm.prank(executor);
        liq.pause();
        vm.expectRevert(MorphoLiquidator.IsPaused.selector);
        _run(_step(address(router), SEIZE), 0);
        vm.prank(executor);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.unpause();
        vm.prank(attacker);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.pause();
        vm.prank(owner);
        liq.unpause();
        _run(_step(address(router), SEIZE), 0);
    }

    function test_twoStepOwnership() public {
        address newOwner = makeAddr("newOwner");
        vm.prank(owner);
        liq.transferOwnership(newOwner);
        assertEq(liq.owner(), owner, "not moved until accepted");
        vm.prank(attacker);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.acceptOwnership();
        vm.prank(newOwner);
        liq.acceptOwnership();
        assertEq(liq.owner(), newOwner);
        assertEq(liq.pendingOwner(), address(0));
    }

    function test_onlyOwnerAdmin() public {
        vm.startPrank(executor);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.setAllowedTarget(attacker, true);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.setExecutor(attacker);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.rescue(address(loan), attacker, 1);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.transferOwnership(attacker);
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.approveMorpho(address(loan), true);
        vm.stopPrank();
    }
}
