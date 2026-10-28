// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {FlashLiquidator, LiquidationCall} from "../src/FlashLiquidator.sol";
import {IMorphoFlashLoan, IPoolManager} from "../src/interfaces/IFlash.sol";
import {SwapStep} from "../src/MorphoLiquidator.sol";
import {MockERC20, MockRouter} from "./mocks/Mocks.sol";

/// A lending protocol without a liquidate callback: pulls the debt, sends the collateral.
contract MockLender {
    MockERC20 public immutable debt;
    MockERC20 public immutable collateral;
    /// Collateral sent per debt unit repaid, in bps (11_000 = 10% bonus at a 1:1 price).
    uint256 public bonusBps = 11_000;

    constructor(MockERC20 debt_, MockERC20 collateral_) {
        debt = debt_;
        collateral = collateral_;
    }

    function setBonus(uint256 bps) external {
        bonusBps = bps;
    }

    function liquidate(uint256 repay) external {
        debt.transferFrom(msg.sender, address(this), repay);
        collateral.transfer(msg.sender, repay * bonusBps / 10_000);
    }
}

/// Plays Morpho AND the PoolManager (the liquidator is deployed with this address for both), so
/// the sender checks pass and only the transient funding tag guards the callbacks.
contract MockFunder {
    enum Mode {
        Honest, // lends, calls its own callback once, takes the loan back
        Silent, // never calls back
        Cross, // calls the OTHER source's callback
        Twice // calls its own callback twice
    }

    Mode public mode;
    FlashLiquidator public liq;
    uint256 internal synced;

    function set(Mode m, FlashLiquidator l) external {
        (mode, liq) = (m, l);
    }

    // ---- Morpho flash loan
    function flashLoan(address token, uint256 assets, bytes calldata data) external {
        if (mode == Mode.Silent) return;
        if (mode == Mode.Cross) {
            liq.unlockCallback(data);
            return;
        }
        MockERC20(token).transfer(msg.sender, assets);
        liq.onMorphoFlashLoan(assets, data);
        if (mode == Mode.Twice) liq.onMorphoFlashLoan(assets, data);
        MockERC20(token).transferFrom(msg.sender, address(this), assets);
    }

    // ---- V4 flash accounting (deltas reduced to "what was taken must come back")
    function unlock(bytes calldata data) external returns (bytes memory) {
        if (mode == Mode.Silent) return "";
        if (mode == Mode.Cross) {
            liq.onMorphoFlashLoan(0, data);
            return "";
        }
        liq.unlockCallback(data);
        if (mode == Mode.Twice) liq.unlockCallback(data);
        return "";
    }

    function take(address currency, address to, uint256 amount) external {
        MockERC20(currency).transfer(to, amount);
    }

    function sync(address currency) external {
        synced = MockERC20(currency).balanceOf(address(this));
    }

    function settle() external payable returns (uint256) {
        return 0;
    }
}

/// Run: forge test --match-contract FlashLiquidatorSecurity  (no fork needed)
contract FlashLiquidatorSecurityTest is Test {
    uint8 constant F_MORPHO = 1;
    uint8 constant F_V4 = 2;
    address owner = makeAddr("owner");
    address exec = makeAddr("exec");
    address receiver = makeAddr("receiver");
    MockERC20 debt = new MockERC20();
    MockERC20 coll = new MockERC20();
    MockLender lender = new MockLender(debt, coll);
    MockRouter router = new MockRouter();
    MockFunder funder = new MockFunder();
    FlashLiquidator liq;

    function setUp() public {
        liq = new FlashLiquidator(
            IMorphoFlashLoan(address(funder)), IPoolManager(address(funder)), owner, exec, receiver
        );
        debt.mint(address(funder), 1_000_000e6); // the funder's liquidity
        coll.mint(address(lender), 1_000_000e6);
        debt.mint(address(router), 1_000_000e6); // the router pays out debt token 1:1
        vm.startPrank(owner);
        liq.setAllowedProtocol(address(lender), true);
        liq.setAllowedTarget(address(router), true);
        vm.stopPrank();
    }

    function _call(uint256 repay) internal view returns (LiquidationCall memory) {
        return LiquidationCall({target: address(lender), data: abi.encodeCall(MockLender.liquidate, (repay))});
    }

    /// Sell all seized collateral 1:1 for the debt token.
    function _sellAll() internal view returns (SwapStep[] memory s) {
        s = new SwapStep[](1);
        s[0] = SwapStep({
            target: address(router),
            tokenIn: address(coll),
            amountIn: type(uint256).max,
            data: abi.encodeCall(MockRouter.swap, (coll, debt, 1_100e6))
        });
    }

    function _run(uint8 funding, uint256 minProfit) internal returns (uint256) {
        vm.prank(exec);
        return liq.liquidate(funding, address(debt), 1_000e6, address(coll), _call(1_000e6), _sellAll(), minProfit, block.number);
    }

    function test_bothFundingSourcesLiquidateRepayAndPayTheReceiver() public {
        funder.set(MockFunder.Mode.Honest, liq);
        uint8[2] memory sources = [liq.FUND_MORPHO(), liq.FUND_V4()];
        for (uint256 i; i < 2; ++i) {
            uint256 funderBefore = debt.balanceOf(address(funder));
            uint256 profit = _run(sources[i], 99e6);
            assertEq(profit, 100e6, "10% bonus sold 1:1");
            assertEq(debt.balanceOf(address(funder)), funderBefore, "funder repaid exactly");
            assertEq(debt.balanceOf(address(liq)) + coll.balanceOf(address(liq)), 0, "nothing at rest");
            assertEq(debt.balanceOf(receiver), 100e6 * (i + 1));
        }
    }

    function test_minProfitEnforced() public {
        funder.set(MockFunder.Mode.Honest, liq);
        vm.expectRevert(abi.encodeWithSelector(FlashLiquidator.InsufficientProfit.selector, 100e6, 101e6));
        _run(F_MORPHO, 101e6);
    }

    function test_onlyExecutorAndNotExpiredOrPaused() public {
        funder.set(MockFunder.Mode.Honest, liq);
        vm.expectRevert(FlashLiquidator.Unauthorized.selector);
        liq.liquidate(1, address(debt), 1_000e6, address(coll), _call(1_000e6), _sellAll(), 0, block.number);
        vm.prank(exec);
        vm.expectRevert(FlashLiquidator.Expired.selector);
        liq.liquidate(1, address(debt), 1_000e6, address(coll), _call(1_000e6), _sellAll(), 0, block.number - 1);
        vm.prank(exec);
        liq.pause();
        vm.expectRevert(FlashLiquidator.IsPaused.selector);
        _run(1, 0);
    }

    function test_unknownFundingAndDisabledV4Revert() public {
        vm.expectRevert(FlashLiquidator.FundingDisabled.selector);
        _run(3, 0);
        FlashLiquidator noV4 = new FlashLiquidator(IMorphoFlashLoan(address(funder)), IPoolManager(address(0)), owner, exec, receiver);
        vm.prank(exec);
        vm.expectRevert(FlashLiquidator.FundingDisabled.selector);
        noV4.liquidate(2, address(debt), 1, address(coll), _call(1), _sellAll(), 0, block.number);
    }

    function test_callbacksRejectStrangersAndCallsOutsideAnOwnLiquidation() public {
        bytes memory data = abi.encode(_call(1), _sellAll());
        vm.expectRevert(FlashLiquidator.Unauthorized.selector);
        liq.onMorphoFlashLoan(1, data);
        vm.expectRevert(FlashLiquidator.Unauthorized.selector);
        liq.unlockCallback(data);
        // The real funder address, but no liquidation of ours in flight.
        vm.startPrank(address(funder));
        vm.expectRevert(FlashLiquidator.Unauthorized.selector);
        liq.onMorphoFlashLoan(1, data);
        vm.expectRevert(FlashLiquidator.Unauthorized.selector);
        liq.unlockCallback(data);
        vm.stopPrank();
    }

    function test_crossSourceAndSecondCallbacksAreRejected() public {
        funder.set(MockFunder.Mode.Cross, liq);
        vm.expectRevert(FlashLiquidator.Unauthorized.selector);
        _run(F_MORPHO, 0);
        vm.expectRevert(FlashLiquidator.Unauthorized.selector);
        _run(F_V4, 0);
        funder.set(MockFunder.Mode.Twice, liq);
        vm.expectRevert(FlashLiquidator.Unauthorized.selector);
        _run(F_MORPHO, 0);
        vm.expectRevert(FlashLiquidator.Unauthorized.selector);
        _run(F_V4, 0);
    }

    function test_funderThatNeverCallsBackReverts() public {
        funder.set(MockFunder.Mode.Silent, liq);
        vm.expectRevert(FlashLiquidator.NotFunded.selector);
        _run(F_MORPHO, 0);
        vm.expectRevert(FlashLiquidator.NotFunded.selector);
        _run(F_V4, 0);
    }

    function test_liquidationAndSwapTargetsMustBeAllowlisted() public {
        funder.set(MockFunder.Mode.Honest, liq);
        vm.prank(owner);
        liq.setAllowedProtocol(address(lender), false);
        vm.expectRevert(abi.encodeWithSelector(FlashLiquidator.ProtocolNotAllowed.selector, address(lender)));
        _run(F_MORPHO, 0);
        vm.startPrank(owner);
        liq.setAllowedProtocol(address(lender), true);
        liq.setAllowedTarget(address(router), false);
        vm.stopPrank();
        vm.expectRevert(abi.encodeWithSelector(FlashLiquidator.TargetNotAllowed.selector, address(router)));
        _run(F_MORPHO, 0);
    }

    /// A swap router is never a liquidation target: it would get the allowance on borrowed funds.
    function test_anAllowlistedRouterIsNotAnAllowedProtocol() public {
        funder.set(MockFunder.Mode.Honest, liq);
        LiquidationCall memory viaRouter = LiquidationCall({target: address(router), data: abi.encodeCall(MockRouter.swap, (debt, coll, 1))});
        vm.prank(exec);
        vm.expectRevert(abi.encodeWithSelector(FlashLiquidator.ProtocolNotAllowed.selector, address(router)));
        liq.liquidate(F_MORPHO, address(debt), 1_000e6, address(coll), viaRouter, _sellAll(), 0, block.number);
    }

    function test_aLiquidationThatSeizesNothingReverts() public {
        funder.set(MockFunder.Mode.Honest, liq);
        lender.setBonus(0);
        vm.expectRevert(FlashLiquidator.NothingSeized.selector);
        _run(F_MORPHO, 0);
    }

    function test_allowancesAreResetAfterTheCall() public {
        funder.set(MockFunder.Mode.Honest, liq);
        _run(F_MORPHO, 0);
        assertEq(debt.allowance(address(liq), address(lender)), 0);
        assertEq(coll.allowance(address(liq), address(router)), 0);
    }
}
