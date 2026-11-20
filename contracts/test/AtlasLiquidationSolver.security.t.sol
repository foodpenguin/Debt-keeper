// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {AtlasLiquidationSolver, SolverCall} from "../src/AtlasLiquidationSolver.sol";
import {ProfitShares} from "../src/ProfitShares.sol";
import {MockERC20} from "./mocks/Mocks.sol";

/// WETH stand-in: an ERC-20 that pays out ETH on withdraw.
contract MockWETH is MockERC20 {
    function withdraw(uint256 amount) external {
        require(balanceOf[msg.sender] >= amount, "weth balance");
        balanceOf[msg.sender] -= amount;
        (bool ok,) = msg.sender.call{value: amount}("");
        require(ok, "eth");
    }

    receive() external payable {}
}

/// Stands in for a liquidation whose profit lands in the solver (a MorphoLiquidator with the solver
/// as profit receiver).
contract MockLiquidation {
    function pay(MockERC20 token, address to, uint256 amount) external {
        token.transfer(to, amount);
    }

    /// A call target that tries to pay the solver's in-flight balance out mid-operation.
    function payAndDistribute(MockERC20 token, AtlasLiquidationSolver s, uint256 amount) external {
        token.transfer(address(s), amount);
        s.distribute(address(token));
    }
}

/// V3-style pool stand-in for the bid swap: exact output of WETH for the other token at `inPerOut`
/// (1e18 = 1:1), WETH sent first, then the input asked for in the swap callback. `short` delivers
/// less than asked (a price limit hit); `greedy` asks for more than the price.
contract MockBidPool {
    address public immutable token0;
    address public immutable token1;
    address immutable weth;
    uint256 public inPerOut = 1e18;
    bool public short;
    bool public greedy;

    constructor(address weth_, address other, bool wethFirst) {
        weth = weth_;
        (token0, token1) = wethFirst ? (weth_, other) : (other, weth_);
    }

    function setModes(bool short_, bool greedy_) external {
        (short, greedy) = (short_, greedy_);
    }

    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160, bytes calldata data) external returns (int256 a0, int256 a1) {
        require(amountSpecified < 0, "exact output only");
        uint256 out = uint256(-amountSpecified);
        if (short) out -= 1;
        uint256 owed = out * inPerOut / 1e18 + (greedy ? 1 : 0);
        (address tin, address tout) = zeroForOne ? (token0, token1) : (token1, token0);
        require(tout == weth, "pool sells WETH");
        MockERC20(tout).transfer(recipient, out);
        uint256 before = MockERC20(tin).balanceOf(address(this));
        (a0, a1) = zeroForOne ? (int256(owed), -int256(out)) : (-int256(out), int256(owed));
        AtlasLiquidationSolver(payable(msg.sender)).uniswapV3SwapCallback(a0, a1, data);
        require(MockERC20(tin).balanceOf(address(this)) >= before + owed, "not paid");
    }
}

/// Atlas escrow stand-in: calls the solver like a metacall would and records the reconciliation.
contract MockAtlas {
    uint256 public gasLiability = 7;
    uint256 public borrowLiability;
    uint256 public reconciledGas;
    uint256 public reconciledValue;

    function solve(AtlasLiquidationSolver s, address from, address ee, address bidToken, uint256 bid, bytes calldata data) external payable returns (uint256) {
        return s.atlasSolverCall{value: msg.value}(from, ee, bidToken, bid, data, "");
    }

    function setBorrow(uint256 b) external {
        borrowLiability = b;
    }

    function setGasLiability(uint256 g) external {
        gasLiability = g;
    }

    function shortfall() external view returns (uint256, uint256) {
        return (gasLiability, borrowLiability);
    }

    function reconcile(uint256 maxApprovedGasSpend) external payable returns (uint256) {
        (reconciledGas, reconciledValue) = (maxApprovedGasSpend, msg.value);
        return 0;
    }
}

/// Run: forge test --match-contract AtlasLiquidationSolverSecurity  (no fork needed)
contract AtlasLiquidationSolverSecurityTest is Test {
    address owner = makeAddr("owner");
    address signer = makeAddr("signer");
    address receiver = makeAddr("receiver");
    address ee = makeAddr("executionEnvironment");
    MockERC20 usdc = new MockERC20();
    MockWETH weth = new MockWETH();
    MockLiquidation liquidation = new MockLiquidation();
    MockBidPool pool = new MockBidPool(address(weth), address(usdc), false);
    MockAtlas atlas = new MockAtlas();
    AtlasLiquidationSolver solver;

    function setUp() public {
        solver = new AtlasLiquidationSolver(address(atlas), address(weth), owner, signer, receiver);
        usdc.mint(address(liquidation), 1_000_000e6);
        weth.mint(address(pool), 1_000_000e6); // the bid pool sells WETH 1:1
        vm.deal(address(weth), 1_000_000e6); // ETH backing the mock WETH
        vm.startPrank(owner);
        solver.setAllowedTarget(address(liquidation), true);
        solver.setAllowedTarget(address(pool), true);
        vm.stopPrank();
    }

    /// The liquidation leaves `gain` USDC; the bid pool sells exactly the bid (plus the gas pay) of
    /// WETH for USDC 1:1.
    function _data(uint256 gain, uint256 minProfit) internal view returns (bytes memory) {
        return _dataGas(gain, minProfit, 0);
    }

    /// As `_data`, paying `gasPay` of the op's gas in ETH (the bid swap buys it too).
    function _dataGas(uint256 gain, uint256 minProfit, uint256 gasPay) internal view returns (bytes memory) {
        return _dataPool(gain, address(pool), minProfit, gasPay);
    }

    function _dataPool(uint256 gain, address bidPool, uint256 minProfit, uint256 gasPay) internal view returns (bytes memory) {
        SolverCall[] memory calls = new SolverCall[](1);
        calls[0] = SolverCall({target: address(liquidation), data: abi.encodeCall(MockLiquidation.pay, (usdc, address(solver), gain))});
        return abi.encode(calls, bidPool, address(usdc), minProfit, gasPay);
    }

    function _nothingAtRest() internal view {
        assertEq(usdc.balanceOf(address(solver)) + weth.balanceOf(address(solver)) + address(solver).balance, 0, "nothing at rest");
    }

    /// The op pays part of its gas in ETH bought with the proceeds and approves the rest of Atlas's
    /// liability against the bond.
    function test_paysItsGasInEthOutOfTheProceeds() public {
        atlas.setGasLiability(50e6);
        // 1000 gained; 100 bid + 30 gas bought with 130 USDC.
        uint256 net = atlas.solve(solver, signer, ee, address(0), 100e6, _dataGas(1_000e6, 870e6, 30e6));
        assertEq(net, 870e6, "the profit is net of the bid and the gas");
        assertEq(ee.balance, 100e6, "bid paid");
        assertEq(atlas.reconciledValue(), 30e6, "gas paid in ETH");
        assertEq(address(atlas).balance, 30e6);
        assertEq(atlas.reconciledGas(), 20e6, "the rest of the liability approved against the bond");
        assertEq(usdc.balanceOf(receiver), 870e6);
        _nothingAtRest();
    }

    /// A gasPayWei above Atlas's liability (a wrong parameter) pays only the liability; the extra
    /// WETH bought goes to the receiver, not to Atlas, and nothing stays here.
    function test_gasPayAboveTheLiabilityPaysOnlyTheLiability() public {
        atlas.setGasLiability(20e6);
        uint256 net = atlas.solve(solver, signer, ee, address(0), 100e6, _dataGas(1_000e6, 0, 50e6));
        assertEq(atlas.reconciledValue(), 20e6, "capped at the liability");
        assertEq(atlas.reconciledGas(), 0, "nothing left to approve against the bond");
        assertEq(net, 850e6);
        assertEq(weth.balanceOf(receiver), 30e6, "the WETH bought beyond the liability is swept");
        _nothingAtRest();
    }

    /// minProfit is the real net: after the bid AND the gas paid in ETH.
    function test_minProfitIsCheckedAfterTheGas() public {
        atlas.setGasLiability(50e6);
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.InsufficientProfit.selector, 850e6, 851e6));
        atlas.solve(solver, signer, ee, address(0), 100e6, _dataGas(1_000e6, 851e6, 50e6));
    }

    /// The swap must buy the bid plus the gas: a 120 USDC gain cannot pay for a 100 bid and 50 of gas
    /// (the pool asks for 150, more than the solver holds).
    function test_aBidPlusGasTheSwapCannotFundReverts() public {
        atlas.setGasLiability(50e6);
        vm.expectRevert();
        atlas.solve(solver, signer, ee, address(0), 100e6, _dataGas(120e6, 0, 50e6));
    }

    /// gasPayWei = 0: the previous behavior, the whole liability approved against the bond.
    function test_zeroGasPayLeavesTheWholeGasToTheBond() public {
        atlas.setGasLiability(50e6);
        atlas.solve(solver, signer, ee, address(0), 100e6, _dataGas(1_000e6, 900e6, 0));
        assertEq(atlas.reconciledValue(), 0, "no ETH for gas");
        assertEq(atlas.reconciledGas(), 50e6, "the whole liability against the bond");
        _nothingAtRest();
    }

    /// Borrowed ETH, the bid and the gas all come out of the same unwrap, each to its own place.
    function test_borrowBidAndGasAreSettledTogether() public {
        atlas.setGasLiability(50e6);
        atlas.setBorrow(3e6);
        vm.deal(address(this), 3e6);
        atlas.solve{value: 3e6}(solver, signer, ee, address(0), 100e6, _dataGas(1_000e6, 0, 40e6));
        assertEq(ee.balance, 100e6, "bid paid");
        assertEq(atlas.reconciledValue(), 43e6, "the loan back plus 40 of gas");
        assertEq(atlas.reconciledGas(), 10e6);
        _nothingAtRest();
    }

    function test_runsTheLiquidationPaysTheEthBidSettlesAndKeepsTheRest() public {
        uint256 net = atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 900e6));
        assertEq(net, 900e6, "1000 gained - 100 swapped for the bid");
        assertEq(ee.balance, 100e6, "bid paid in ETH to the execution environment");
        assertEq(usdc.balanceOf(receiver), 900e6);
        assertEq((atlas.reconciledGas()), 7, "gas liability approved");
        assertEq(usdc.balanceOf(address(solver)) + weth.balanceOf(address(solver)) + address(solver).balance, 0, "nothing at rest");
        assertEq(usdc.allowance(address(solver), address(pool)), 0, "the bid pool is paid in its callback, never approved");
    }

    /// Atlas lent ETH (sent as value): the bid must not spend it; it goes back through reconcile.
    function test_borrowedEthIsRepaidEvenWhenTheBidNeedsEth() public {
        atlas.setBorrow(3e6);
        vm.deal(address(this), 3e6);
        uint256 net = atlas.solve{value: 3e6}(solver, signer, ee, address(0), 100e6, _data(1_000e6, 0));
        assertEq(ee.balance, 100e6, "bid paid");
        assertEq(atlas.reconciledValue(), 3e6, "borrowed ETH repaid");
        assertEq(net, 900e6, "the bid's WETH was bought; the loan went back in its own ETH");
    }

    /// ETH Atlas sent beyond what it lent does not stay in the solver.
    function test_leftoverEthIsSwept() public {
        atlas.setBorrow(1e6);
        vm.deal(address(this), 5e6);
        // 5 ETH-wei sent, 1 lent: a 1 bid and the 1 repayment leave 3 behind.
        atlas.solve{value: 5e6}(solver, signer, ee, address(0), 1e6, _dataPool(1_000e6, address(0), 0, 0));
        assertEq(atlas.reconciledValue(), 1e6, "the loan went back");
        assertEq(address(solver).balance, 0, "nothing at rest");
        assertEq(receiver.balance, 3e6, "the rest went to the receiver");
    }

    function test_minProfitIsCheckedAfterTheBid() public {
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.InsufficientProfit.selector, 900e6, 901e6));
        atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 901e6));
    }

    /// The solver's liquidators send it their leftover collateral, which can be a later op's profit
    /// token: a balance already here is not this op's profit, but it is still swept.
    function test_tokensAlreadyHereDoNotCountAsProfit() public {
        usdc.mint(address(solver), 500e6);
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.InsufficientProfit.selector, 900e6, 901e6));
        atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 901e6));

        uint256 net = atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 900e6));
        assertEq(net, 900e6, "only this op's gain");
        assertEq(usdc.balanceOf(receiver), 1_400e6, "the old balance is swept with it");
        assertEq(usdc.balanceOf(address(solver)), 0, "nothing at rest");
    }

    function test_onlyAtlasForTheSignersOperation() public {
        bytes memory d = _data(1_000e6, 0);
        vm.expectRevert(AtlasLiquidationSolver.InvalidEntry.selector);
        solver.atlasSolverCall(signer, ee, address(0), 100e6, d, "");
        vm.expectRevert(AtlasLiquidationSolver.InvalidCaller.selector);
        atlas.solve(solver, makeAddr("someoneElse"), ee, address(0), 100e6, d);
    }

    function test_ethBidsOnlyAndPauseStopsIt() public {
        bytes memory d = _data(1_000e6, 0);
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.UnsupportedBidToken.selector, address(usdc)));
        atlas.solve(solver, signer, ee, address(usdc), 100e6, d);
        vm.prank(signer); // the hot wallet can stop it
        solver.pause();
        vm.expectRevert(AtlasLiquidationSolver.IsPaused.selector);
        atlas.solve(solver, signer, ee, address(0), 100e6, d);
        vm.expectRevert(AtlasLiquidationSolver.Unauthorized.selector);
        vm.prank(signer);
        solver.unpause();
    }

    function test_callAndBidSwapTargetsMustBeAllowlisted() public {
        bytes memory d = _data(1_000e6, 0);
        vm.prank(owner);
        solver.setAllowedTarget(address(liquidation), false);
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.TargetNotAllowed.selector, address(liquidation)));
        atlas.solve(solver, signer, ee, address(0), 100e6, d);
        vm.startPrank(owner);
        solver.setAllowedTarget(address(liquidation), true);
        solver.setAllowedTarget(address(pool), false);
        vm.stopPrank();
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.TargetNotAllowed.selector, address(pool)));
        atlas.solve(solver, signer, ee, address(0), 100e6, d);
    }

    function test_aBidTheSwapCannotFundReverts() public {
        // 50 USDC gained, 100 bid: the pool asks for more than the solver holds.
        vm.expectRevert(AtlasLiquidationSolver.Unauthorized.selector);
        atlas.solve(solver, signer, ee, address(0), 100e6, _data(50e6, 0));
    }

    /// The pool is paid only inside its own swap, by the active pool, once: anyone else calling the
    /// callback gets nothing.
    function test_onlyTheActiveBidPoolIsPaid() public {
        usdc.mint(address(solver), 10e6);
        vm.expectRevert(AtlasLiquidationSolver.Unauthorized.selector);
        solver.uniswapV3SwapCallback(0, 10e6, "");
        vm.expectRevert(AtlasLiquidationSolver.Unauthorized.selector);
        vm.prank(address(pool));
        solver.pancakeV3SwapCallback(0, 10e6, "");
    }

    /// A pool that delivers less WETH than the exact output asked for (its price limit) fails the op.
    function test_aShortBidSwapReverts() public {
        pool.setModes(true, false);
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.BidSwapShort.selector, 100e6 - 1, 100e6));
        atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 0));
    }

    /// A pool that asks more than the price still cannot take more than the profit token held, and
    /// minProfit bounds what it may take.
    function test_aGreedyBidPoolIsBoundedByMinProfit() public {
        pool.setModes(false, true);
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.InsufficientProfit.selector, 900e6 - 1, 900e6));
        atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 900e6));
    }

    /// WETH as token0 of the pool: the swap direction follows the pool's token order.
    function test_theBidSwapFollowsThePoolsTokenOrder() public {
        MockBidPool p2 = new MockBidPool(address(weth), address(usdc), true);
        weth.mint(address(p2), 1_000e6);
        vm.prank(owner);
        solver.setAllowedTarget(address(p2), true);
        uint256 net = atlas.solve(solver, signer, ee, address(0), 100e6, _dataPool(1_000e6, address(p2), 900e6, 0));
        assertEq(net, 900e6);
        assertEq(ee.balance, 100e6);
    }

    // ---- profit shares ----

    function _shares2() internal returns (address a, address b) {
        (a, b) = (makeAddr("shareA"), makeAddr("shareB"));
        address[] memory to = new address[](2);
        uint16[] memory bps = new uint16[](2);
        (to[0], to[1], bps[0], bps[1]) = (a, b, 2_000, 500);
        vm.prank(owner);
        solver.setShares(to, bps);
    }

    /// Each op's payout is split on the spot: 20% and 5% to the extra recipients, the rest to
    /// PROFIT_RECEIVER; minProfit is checked on the whole profit; one ProfitPaid with the total.
    function test_theProfitIsSplitByTheShares() public {
        (address a, address b) = _shares2();
        vm.expectEmit(true, false, false, true, address(solver));
        emit ProfitShares.ProfitPaid(address(usdc), 900e6);
        uint256 net = atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 900e6));
        assertEq(net, 900e6, "the profit reported (and checked) is before the split");
        assertEq(usdc.balanceOf(a), 180e6);
        assertEq(usdc.balanceOf(b), 45e6);
        assertEq(usdc.balanceOf(receiver), 675e6);
        _nothingAtRest();
    }

    /// Rounding goes to PROFIT_RECEIVER: nothing is left behind.
    function test_shareRoundingGoesToTheReceiver() public {
        (address a, address b) = _shares2();
        atlas.solve(solver, signer, ee, address(0), 0, _dataPool(1_003, address(0), 0, 0));
        assertEq(usdc.balanceOf(a), uint256(200));
        assertEq(usdc.balanceOf(b), uint256(50));
        assertEq(usdc.balanceOf(receiver), uint256(753));
        _nothingAtRest();
    }

    function test_onlyTheOwnerSetsValidShares() public {
        address[] memory to = new address[](1);
        uint16[] memory bps = new uint16[](1);
        (to[0], bps[0]) = (makeAddr("x"), 1_000);
        vm.expectRevert(AtlasLiquidationSolver.Unauthorized.selector);
        solver.setShares(to, bps);
        vm.startPrank(owner);
        bps[0] = 10_001;
        vm.expectRevert(ProfitShares.BadShares.selector);
        solver.setShares(to, bps); // above 100%
        (to[0], bps[0]) = (address(0), 1_000);
        vm.expectRevert(ProfitShares.BadShares.selector);
        solver.setShares(to, bps); // zero address
        (to[0], bps[0]) = (makeAddr("x"), 0);
        vm.expectRevert(ProfitShares.BadShares.selector);
        solver.setShares(to, bps); // zero share
        address[] memory dup = new address[](2);
        uint16[] memory dbps = new uint16[](2);
        (dup[0], dup[1], dbps[0], dbps[1]) = (makeAddr("x"), makeAddr("x"), 100, 100);
        vm.expectRevert(ProfitShares.BadShares.selector);
        solver.setShares(dup, dbps); // duplicate
        address[] memory five = new address[](5);
        uint16[] memory fbps = new uint16[](5);
        for (uint256 i; i < 5; ++i) (five[i], fbps[i]) = (address(uint160(0x1000 + i)), 100);
        vm.expectRevert(ProfitShares.BadShares.selector);
        solver.setShares(five, fbps); // more than MAX_SHARES
        solver.setShares(new address[](0), new uint16[](0)); // clearing is fine
        vm.stopPrank();
        (address[] memory got,) = solver.shares();
        assertEq(got.length, 0);
    }

    /// A share that cannot be paid (the token blocks the recipient, or it rejects ETH) goes to
    /// PROFIT_RECEIVER: the op still settles, nothing stays here.
    function test_anUnpayableShareGoesToTheReceiver() public {
        (address a, address b) = _shares2();
        usdc.setBlocked(a, true);
        vm.expectEmit(true, true, false, true, address(solver));
        emit ProfitShares.ShareSkipped(address(usdc), a, 180e6);
        uint256 net = atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 900e6));
        assertEq(net, 900e6);
        assertEq(usdc.balanceOf(a), 0);
        assertEq(usdc.balanceOf(b), 45e6);
        assertEq(usdc.balanceOf(receiver), 855e6, "the skipped 180 on top of its own 675");
        _nothingAtRest();
        // ETH: a recipient that cannot take it (no receive) is skipped the same way.
        address[] memory to = new address[](1);
        uint16[] memory bps = new uint16[](1);
        (to[0], bps[0]) = (address(liquidation), 5_000);
        vm.prank(owner);
        solver.setShares(to, bps);
        vm.deal(address(solver), 1e6);
        solver.distributeEth();
        assertEq(address(liquidation).balance, 0);
        assertEq(receiver.balance, 1e6);
    }

    /// Anyone may pay out a stray balance, only to the configured recipients.
    function test_anyoneCanDistributeAStrayBalanceToTheRecipients() public {
        (address a,) = _shares2();
        usdc.mint(address(solver), 100e6);
        vm.prank(makeAddr("anyone"));
        solver.distribute(address(usdc));
        assertEq(usdc.balanceOf(a), 20e6);
        assertEq(usdc.balanceOf(receiver), 75e6);
        vm.deal(address(solver), 1e6);
        solver.distributeEth();
        assertEq(a.balance, 0.2e6);
        assertEq(receiver.balance, 0.75e6);
    }

    /// A call target cannot reenter and pay out the balance in flight.
    function test_distributeIsRefusedDuringAnOperation() public {
        SolverCall[] memory calls = new SolverCall[](1);
        calls[0] = SolverCall({target: address(liquidation), data: abi.encodeCall(MockLiquidation.payAndDistribute, (usdc, solver, 1_000e6))});
        bytes memory d = abi.encode(calls, address(pool), address(usdc), uint256(0), uint256(0));
        vm.expectRevert(); // CallFailed(0, Busy())
        atlas.solve(solver, signer, ee, address(0), 100e6, d);
    }

    function test_adminIsOwnerOnly() public {
        vm.expectRevert(AtlasLiquidationSolver.Unauthorized.selector);
        solver.setAllowedTarget(address(0xbad), true);
        vm.expectRevert(AtlasLiquidationSolver.Unauthorized.selector);
        solver.setExecutor(address(0xbad));
        vm.expectRevert(AtlasLiquidationSolver.Unauthorized.selector);
        solver.rescueEth(payable(address(0xbad)), 1);
    }
}
