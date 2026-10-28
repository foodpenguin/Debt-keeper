// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {AtlasLiquidationSolver, SolverCall} from "../src/AtlasLiquidationSolver.sol";
import {SwapStep} from "../src/MorphoLiquidator.sol";
import {MockERC20, MockRouter} from "./mocks/Mocks.sol";

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
    MockRouter router = new MockRouter();
    MockAtlas atlas = new MockAtlas();
    AtlasLiquidationSolver solver;

    function setUp() public {
        solver = new AtlasLiquidationSolver(address(atlas), address(weth), owner, signer, receiver);
        usdc.mint(address(liquidation), 1_000_000e6);
        weth.mint(address(router), 1_000_000e6); // the router pays WETH 1:1
        vm.deal(address(weth), 1_000_000e6); // ETH backing the mock WETH
        vm.startPrank(owner);
        solver.setAllowedTarget(address(liquidation), true);
        solver.setAllowedTarget(address(router), true);
        vm.stopPrank();
    }

    /// The liquidation leaves `gain` USDC; `swapIn` of it is swapped 1:1 into WETH for the bid.
    function _data(uint256 gain, uint256 swapIn, uint256 minProfit) internal view returns (bytes memory) {
        SolverCall[] memory calls = new SolverCall[](1);
        calls[0] = SolverCall({target: address(liquidation), data: abi.encodeCall(MockLiquidation.pay, (usdc, address(solver), gain))});
        SwapStep memory bidSwap = SwapStep({target: address(router), tokenIn: address(usdc), amountIn: swapIn, data: abi.encodeCall(MockRouter.swap, (usdc, MockERC20(address(weth)), swapIn))});
        return abi.encode(calls, bidSwap, address(usdc), minProfit);
    }

    function test_runsTheLiquidationPaysTheEthBidSettlesAndKeepsTheRest() public {
        uint256 net = atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 100e6, 900e6));
        assertEq(net, 900e6, "1000 gained - 100 swapped for the bid");
        assertEq(ee.balance, 100e6, "bid paid in ETH to the execution environment");
        assertEq(usdc.balanceOf(receiver), 900e6);
        assertEq((atlas.reconciledGas()), 7, "gas liability approved");
        assertEq(usdc.balanceOf(address(solver)) + weth.balanceOf(address(solver)) + address(solver).balance, 0, "nothing at rest");
        assertEq(usdc.allowance(address(solver), address(router)), 0, "bid-swap allowance reset");
    }

    /// Atlas lent ETH (sent as value): the bid must not spend it; it goes back through reconcile.
    function test_borrowedEthIsRepaidEvenWhenTheBidNeedsEth() public {
        atlas.setBorrow(3e6);
        vm.deal(address(this), 3e6);
        uint256 net = atlas.solve{value: 3e6}(solver, signer, ee, address(0), 100e6, _data(1_000e6, 103e6, 0));
        assertEq(ee.balance, 100e6, "bid paid");
        assertEq(atlas.reconciledValue(), 3e6, "borrowed ETH repaid");
        assertEq(net, 897e6, "the extra 3 USDC swapped covered the bid's ETH while the loan went back");
    }

    /// ETH Atlas sent beyond what it lent does not stay in the solver.
    function test_leftoverEthIsSwept() public {
        atlas.setBorrow(1e6);
        vm.deal(address(this), 5e6);
        // 5 ETH-wei sent, 1 lent: a 1 bid and the 1 repayment leave 3 behind.
        atlas.solve{value: 5e6}(solver, signer, ee, address(0), 1e6, _data(1_000e6, 0, 0));
        assertEq(atlas.reconciledValue(), 1e6, "the loan went back");
        assertEq(address(solver).balance, 0, "nothing at rest");
        assertEq(receiver.balance, 3e6, "the rest went to the receiver");
    }

    function test_minProfitIsCheckedAfterTheBid() public {
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.InsufficientProfit.selector, 900e6, 901e6));
        atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 100e6, 901e6));
    }

    function test_onlyAtlasForTheSignersOperation() public {
        bytes memory d = _data(1_000e6, 100e6, 0);
        vm.expectRevert(AtlasLiquidationSolver.InvalidEntry.selector);
        solver.atlasSolverCall(signer, ee, address(0), 100e6, d, "");
        vm.expectRevert(AtlasLiquidationSolver.InvalidCaller.selector);
        atlas.solve(solver, makeAddr("someoneElse"), ee, address(0), 100e6, d);
    }

    function test_ethBidsOnlyAndPauseStopsIt() public {
        bytes memory d = _data(1_000e6, 100e6, 0);
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
        bytes memory d = _data(1_000e6, 100e6, 0);
        vm.prank(owner);
        solver.setAllowedTarget(address(liquidation), false);
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.TargetNotAllowed.selector, address(liquidation)));
        atlas.solve(solver, signer, ee, address(0), 100e6, d);
        vm.startPrank(owner);
        solver.setAllowedTarget(address(liquidation), true);
        solver.setAllowedTarget(address(router), false);
        vm.stopPrank();
        vm.expectRevert(abi.encodeWithSelector(AtlasLiquidationSolver.TargetNotAllowed.selector, address(router)));
        atlas.solve(solver, signer, ee, address(0), 100e6, d);
    }

    function test_aBidTheSwapCannotFundReverts() public {
        // 50 USDC swapped, 100 bid: not enough WETH to unwrap.
        vm.expectRevert();
        atlas.solve(solver, signer, ee, address(0), 100e6, _data(1_000e6, 50e6, 0));
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
