// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";
import {IMorpho, IOracle, Id, MarketParams, Position} from "../src/interfaces/IMorpho.sol";
import {MorphoLiquidator} from "../src/MorphoLiquidator.sol";
import {GridQuoter} from "../src/sim/GridQuoter.sol";

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
}

/// Direct V3-pool routes on a Base fork: single pool, multi-hop through a PancakeSwap pool,
/// split across venues, and GridQuoter exactness (quote == executed output).
contract MorphoLiquidatorRouteForkTest is Test {
    IMorpho constant MORPHO = IMorpho(0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb);
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant CBXRP = 0xcb585250f852C6c6bf90434AB21A00f02833a4af;

    // WETH/USDC (WETH = token0)
    address constant UNI_WETH_USDC_500 = 0xd0b53D9277642d899DF5C87A3966A349A798F224;
    // cbXRP pools (WETH and USDC both sort below cbXRP, so cbXRP = token1 -> zeroForOne = false)
    address constant PANCAKE_XRP_WETH_500 = 0x170b6bFc071426aaB7C81AE12383219facBb6B6A;
    address constant SLIP_XRP_WETH_2000 = 0xA6da283cf7D06b2279626E787c329d6aCF7e2994;
    address constant UNI_XRP_USDC_10000 = 0x3733cbB1402C788900A0A152e61c9aA31888FB95;

    Id constant WETH_MARKET = Id.wrap(0x8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda);
    address constant WETH_BORROWER = 0x33B5a89294e778d14e8Ce4b0828889B6b81b6488;
    Id constant XRP_MARKET = Id.wrap(0xd4a903dc6d949519060c7707f9604fdc9772c046e05c2e3a8fce0bd7196e4109);
    address constant XRP_BORROWER = 0xCB493f715f7bF89d54E505e1EF3cA0d3122a576A;

    address owner = makeAddr("owner");
    address executor = makeAddr("executor");
    address receiver = makeAddr("receiver");
    MorphoLiquidator liq;
    GridQuoter quoter;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        // Pinned by default: these tests use specific real borrowers, whose positions change over
        // time (at the latest block one became healthy on 2026-10-18 and four tests failed).
        uint256 forkBlock = vm.envOr("FORK_BLOCK", uint256(52_728_600));
        if (forkBlock == 0) vm.createSelectFork(rpc);
        else vm.createSelectFork(rpc, forkBlock);
        liq = new MorphoLiquidator(MORPHO, owner, executor, receiver);
        vm.prank(owner);
        liq.approveMorpho(0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913, true); // USDC
        quoter = new GridQuoter();
    }

    function _hop(address pool, bool zeroForOne, bool chain, uint256 amountIn) internal pure returns (bytes memory) {
        uint8 flags = (zeroForOne ? 1 : 0) | (chain ? 2 : 0);
        return abi.encodePacked(pool, flags, uint128(amountIn));
    }

    function _crash(MarketParams memory mp, uint256 bps) internal {
        uint256 p = IOracle(mp.oracle).price();
        vm.mockCall(mp.oracle, abi.encodeWithSelector(IOracle.price.selector), abi.encode(p * (10_000 - bps) / 10_000));
    }

    function _quote(address[] memory pools, bool[] memory zfo, uint256 amount) internal returns (uint256) {
        GridQuoter.Path[] memory paths = new GridQuoter.Path[](1);
        paths[0] = GridQuoter.Path({pools: pools, zeroForOne: zfo});
        uint256[] memory amts = new uint256[](1);
        amts[0] = amount;
        (uint256[][] memory outs,) = quoter.quote(paths, amts);
        return outs[0][0];
    }

    /// Runs liquidateRoute and returns (profit, repaid, gas); `repaid` comes from Morpho's Liquidate
    /// event (exact), so profit + repaid is exactly what the route produced in loan token.
    function _usdcOut(MarketParams memory mp, address borrower, uint256 seize, bytes memory route)
        internal
        returns (uint256 profit, uint256 repaid, uint256 gasUsed)
    {
        vm.recordLogs();
        vm.prank(executor);
        uint256 g = gasleft();
        profit = liq.liquidateRoute(mp, borrower, seize, route, 0, block.number);
        gasUsed = g - gasleft();
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 sig = keccak256("Liquidate(bytes32,address,address,uint256,uint256,uint256,uint256,uint256)");
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter == address(MORPHO) && logs[i].topics[0] == sig) {
                (repaid,,,,) = abi.decode(logs[i].data, (uint256, uint256, uint256, uint256, uint256));
            }
        }
        require(repaid > 0, "no Morpho Liquidate event");
    }

    function test_route_singlePool_WETH_matchesQuote() public {
        MarketParams memory mp = MORPHO.idToMarketParams(WETH_MARKET);
        _crash(mp, 1_000);
        uint256 seize = 1 ether;
        address[] memory pools = new address[](1);
        pools[0] = UNI_WETH_USDC_500;
        bool[] memory zfo = new bool[](1);
        zfo[0] = true;
        uint256 quoted = _quote(pools, zfo, seize);
        uint256 collBefore = MORPHO.position(WETH_MARKET, WETH_BORROWER).collateral;

        (uint256 profit, uint256 repaid, uint256 used) = _usdcOut(mp, WETH_BORROWER, seize, _hop(UNI_WETH_USDC_500, true, false, seize));

        assertEq(collBefore - MORPHO.position(WETH_MARKET, WETH_BORROWER).collateral, seize);
        assertEq(IERC20(USDC).balanceOf(receiver), profit);
        assertEq(IERC20(WETH).balanceOf(address(liq)), 0);
        assertEq(IERC20(USDC).balanceOf(address(liq)), 0);
        assertGt(profit, 0);
        assertEq(profit + repaid, quoted, "quote != execution");
        emit log_named_decimal_uint("quoted USDC out", quoted, 6);
        emit log_named_decimal_uint("profit USDC", profit, 6);
        emit log_named_uint("gas liquidateRoute (1 hop)", used);
    }

    /// Quote must equal executed output exactly: run the same swap via the liquidator and compare
    /// (profit + repaid) with the quote, where repaid is read from Morpho's state change.
    function test_quoter_exact_vs_execution_twoHop() public {
        MarketParams memory mp = MORPHO.idToMarketParams(XRP_MARKET);
        _crash(mp, 500);
        uint256 seize = 1_000e6; // 1,000 cbXRP (6 decimals)
        address[] memory pools = new address[](2);
        pools[0] = PANCAKE_XRP_WETH_500;
        pools[1] = UNI_WETH_USDC_500;
        bool[] memory zfo = new bool[](2);
        zfo[0] = false;
        zfo[1] = true;
        uint256 quoted = _quote(pools, zfo, seize);
        assertGt(quoted, 0, "quote failed");

        bytes memory route = bytes.concat(_hop(PANCAKE_XRP_WETH_500, false, false, seize), _hop(UNI_WETH_USDC_500, true, true, 0));
        (uint256 profit, uint256 repaid, uint256 used) = _usdcOut(mp, XRP_BORROWER, seize, route);
        assertEq(profit + repaid, quoted, "quote != execution");
        assertEq(IERC20(WETH).balanceOf(address(liq)), 0, "WETH dust left");
        assertEq(IERC20(CBXRP).balanceOf(address(liq)), 0);
        emit log_named_decimal_uint("2-hop quoted USDC", quoted, 6);
        emit log_named_decimal_uint("2-hop profit USDC", profit, 6);
        emit log_named_uint("gas liquidateRoute (2 hops, pancake+uni)", used);
    }

    function test_route_split_threeVenues() public {
        MarketParams memory mp = MORPHO.idToMarketParams(XRP_MARKET);
        _crash(mp, 500);
        uint256 seize = 3_000e6;
        // 1/3 straight to USDC (Uni 1%), 1/3 via Pancake->WETH, 1/3 via Slipstream->WETH; WETH legs to Uni.
        bytes memory route = bytes.concat(
            _hop(UNI_XRP_USDC_10000, false, false, 1_000e6),
            _hop(PANCAKE_XRP_WETH_500, false, false, 1_000e6),
            _hop(UNI_WETH_USDC_500, true, true, 0),
            _hop(SLIP_XRP_WETH_2000, false, false, 1_000e6),
            _hop(UNI_WETH_USDC_500, true, true, 0)
        );
        (uint256 profit,, uint256 used) = _usdcOut(mp, XRP_BORROWER, seize, route);
        assertGt(profit, 0);
        assertEq(IERC20(WETH).balanceOf(address(liq)), 0);
        assertEq(IERC20(CBXRP).balanceOf(address(liq)), 0);
        emit log_named_decimal_uint("split profit USDC", profit, 6);
        emit log_named_uint("gas liquidateRoute (3 venues, 5 hops)", used);
    }

    function _frac(address pool, bool zeroForOne, uint256 bps) internal pure returns (bytes memory) {
        return abi.encodePacked(pool, uint8((zeroForOne ? 1 : 0) | 4), uint128(bps));
    }

    /// Pooled WETH leg: two cbXRP->WETH pools fill the contract with WETH, which is then split
    /// 60/40 across two WETH/USDC pools by balance share. No WETH may be left behind.
    function test_route_pooledConnectorSplit() public {
        MarketParams memory mp = MORPHO.idToMarketParams(XRP_MARKET);
        _crash(mp, 500);
        uint256 seize = 2_000e6;
        bytes memory route = bytes.concat(
            _hop(PANCAKE_XRP_WETH_500, false, false, 1_000e6),
            _hop(SLIP_XRP_WETH_2000, false, false, 1_000e6),
            _frac(UNI_WETH_USDC_500, true, 6_000),
            _frac(0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59, true, 10_000) // Slipstream WETH/USDC ts100: the rest
        );
        (uint256 profit,, uint256 used) = _usdcOut(mp, XRP_BORROWER, seize, route);
        assertGt(profit, 0);
        assertEq(IERC20(WETH).balanceOf(address(liq)), 0, "WETH left behind");
        assertEq(IERC20(CBXRP).balanceOf(address(liq)), 0);
        emit log_named_decimal_uint("pooled-connector profit USDC", profit, 6);
        emit log_named_uint("gas liquidateRoute (2 sources + 2 pooled sinks)", used);
    }

    /// A sink split that leaves part of the pooled connector unspent (last share 90%): the remainder
    /// is swept to the profit receiver, never left in the contract.
    function test_route_pooledConnectorRemainderIsSwept() public {
        MarketParams memory mp = MORPHO.idToMarketParams(XRP_MARKET);
        _crash(mp, 500);
        bytes memory route = bytes.concat(
            _hop(PANCAKE_XRP_WETH_500, false, false, 1_000e6),
            _hop(SLIP_XRP_WETH_2000, false, false, 1_000e6),
            _frac(UNI_WETH_USDC_500, true, 6_000),
            _frac(0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59, true, 9_000)
        );
        uint256 before = IERC20(WETH).balanceOf(receiver);
        _usdcOut(mp, XRP_BORROWER, 2_000e6, route);
        assertEq(IERC20(WETH).balanceOf(address(liq)), 0, "WETH left behind");
        uint256 swept = IERC20(WETH).balanceOf(receiver) - before;
        assertGt(swept, 0, "the 10% remainder went to the receiver");
        emit log_named_decimal_uint("connector remainder swept (WETH)", swept, 18);
    }

    /// The packed entry must do exactly what liquidateRoute does (same profit, same state).
    function test_packedEntry_equalsAbiEntry() public {
        MarketParams memory mp = MORPHO.idToMarketParams(XRP_MARKET);
        _crash(mp, 500);
        bytes memory route = bytes.concat(_hop(PANCAKE_XRP_WETH_500, false, false, 1_000e6), _hop(UNI_WETH_USDC_500, true, true, 0));
        uint256 snap = vm.snapshotState();
        (uint256 viaAbi,,) = _usdcOut(mp, XRP_BORROWER, 1_000e6, route);
        vm.revertToState(snap);
        bytes memory packed = abi.encodePacked(
            bytes1(0x00), mp.loanToken, mp.collateralToken, mp.oracle, mp.irm, uint64(mp.lltv),
            XRP_BORROWER, uint128(1_000e6), uint128(1), uint32(block.number), route
        );
        uint256 before = IERC20(USDC).balanceOf(receiver);
        vm.prank(executor);
        (bool ok, bytes memory ret) = address(liq).call(packed);
        assertTrue(ok, "packed call failed");
        assertEq(abi.decode(ret, (uint256)), viaAbi, "packed entry must return its profit");
        assertEq(IERC20(USDC).balanceOf(receiver) - before, viaAbi, "packed != abi");
        assertEq(IERC20(WETH).balanceOf(address(liq)), 0);
    }

    function test_route_rejectsMalformed() public {
        MarketParams memory mp = MORPHO.idToMarketParams(WETH_MARKET);
        vm.startPrank(executor);
        vm.expectRevert(MorphoLiquidator.BadRoute.selector);
        liq.liquidateRoute(mp, WETH_BORROWER, 1 ether, "", 0, block.number);
        vm.expectRevert(MorphoLiquidator.BadRoute.selector);
        liq.liquidateRoute(mp, WETH_BORROWER, 1 ether, hex"0102", 0, block.number);
        vm.stopPrank();
    }

    function test_swapCallbackFromStrangerRejected() public {
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.uniswapV3SwapCallback(1, -1, "");
        vm.prank(UNI_WETH_USDC_500); // even a real pool, when we are not swapping on it
        vm.expectRevert(MorphoLiquidator.Unauthorized.selector);
        liq.pancakeV3SwapCallback(1, -1, "");
    }
}
