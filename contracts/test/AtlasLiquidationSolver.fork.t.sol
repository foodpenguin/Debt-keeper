// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {IMorpho, IOracle, Id, MarketParams} from "../src/interfaces/IMorpho.sol";
import {MorphoLiquidator, SwapStep} from "../src/MorphoLiquidator.sol";
import {AtlasLiquidationSolver, SolverCall} from "../src/AtlasLiquidationSolver.sol";
import {AtlasStub} from "../src/sim/AtlasStub.sol";

interface IAtlasLive {
    function shortfall() external view returns (uint256 gasLiability, uint256 borrowLiability);
    function reconcile(uint256 maxApprovedGasSpend) external payable returns (uint256 owed);
    function getAtlasSurchargeRate() external view returns (uint256);
}

interface IERC20A {
    function balanceOf(address) external view returns (uint256);
}

interface ISwapRouter02A {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    struct ExactOutputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountOut;
        uint256 amountInMaximum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256);
    function exactOutputSingle(ExactOutputSingleParams calldata params) external payable returns (uint256);
}

/// Fork test on Base mainnet: one SVR-style solver operation outside a real metacall. AtlasStub plays
/// the Atlas escrow (no gas liability), a real WETH/USDC borrower is pushed under water by mocking
/// the oracle 10% lower, the solver liquidates through a MorphoLiquidator it owns, buys the ETH bid
/// with part of the USDC and pays it to the execution environment.
/// Run: forge test --match-contract AtlasLiquidationSolverFork -vv   (BASE_RPC_URL optional)
contract AtlasLiquidationSolverForkTest is Test {
    IMorpho constant MORPHO = IMorpho(0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb);
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant UNI_V3_ROUTER = 0x2626664c2603336E57B271c5C0b26F421741e481;
    Id constant MARKET = Id.wrap(0x8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda);
    address constant BORROWER = 0x33B5a89294e778d14e8Ce4b0828889B6b81b6488;

    address owner = makeAddr("owner");
    address signer = makeAddr("signer");
    address receiver = makeAddr("receiver");
    address ee = makeAddr("executionEnvironment");
    AtlasStub atlas;
    AtlasLiquidationSolver solver;
    MorphoLiquidator liq;
    MarketParams mp;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        // Pinned, so results do not drift with the chain (FORK_BLOCK overrides it; FORK_BLOCK=0 = latest).
        uint256 forkBlock = vm.envOr("FORK_BLOCK", uint256(53206447));
        if (forkBlock == 0) vm.createSelectFork(rpc);
        else vm.createSelectFork(rpc, forkBlock);
        mp = MORPHO.idToMarketParams(MARKET);
        atlas = new AtlasStub();
        solver = new AtlasLiquidationSolver(address(atlas), WETH, owner, signer, receiver);
        // The solver's own liquidator: executor and profit receiver are the solver.
        liq = new MorphoLiquidator(MORPHO, owner, address(solver), address(solver));
        vm.startPrank(owner);
        liq.approveMorpho(USDC, true);
        liq.setAllowedTarget(UNI_V3_ROUTER, true);
        solver.setAllowedTarget(address(liq), true);
        solver.setAllowedTarget(UNI_V3_ROUTER, true);
        vm.stopPrank();
        uint256 p = IOracle(mp.oracle).price();
        vm.mockCall(mp.oracle, abi.encodeWithSelector(IOracle.price.selector), abi.encode(p * 9_000 / 10_000));
    }

    function _liquidation(uint256 seize) internal view returns (SolverCall memory) {
        SwapStep[] memory exit = new SwapStep[](1);
        exit[0] = SwapStep({
            target: UNI_V3_ROUTER,
            tokenIn: WETH,
            amountIn: seize,
            data: abi.encodeCall(
                ISwapRouter02A.exactInputSingle,
                (ISwapRouter02A.ExactInputSingleParams({tokenIn: WETH, tokenOut: USDC, fee: 500, recipient: address(liq), amountIn: seize, amountOutMinimum: 0, sqrtPriceLimitX96: 0}))
            )
        });
        return SolverCall({target: address(liq), data: abi.encodeCall(MorphoLiquidator.liquidate, (mp, BORROWER, seize, 0, exit, 0, block.number))});
    }

    /// Buys exactly `bid` WETH with at most all the USDC the solver holds.
    function _bidSwap(uint256 bid) internal view returns (SwapStep memory) {
        return SwapStep({
            target: UNI_V3_ROUTER,
            tokenIn: USDC,
            amountIn: type(uint256).max,
            data: abi.encodeCall(
                ISwapRouter02A.exactOutputSingle,
                (ISwapRouter02A.ExactOutputSingleParams({tokenIn: USDC, tokenOut: WETH, fee: 500, recipient: address(solver), amountOut: bid, amountInMaximum: type(uint128).max, sqrtPriceLimitX96: 0}))
            )
        });
    }

    function _op(uint256 bid, uint256 minProfit) internal view returns (bytes memory) {
        SolverCall[] memory calls = new SolverCall[](1);
        calls[0] = _liquidation(1 ether);
        return abi.encode(calls, _bidSwap(bid), USDC, minProfit);
    }

    function test_solverLiquidatesPaysTheEthBidAndKeepsTheRest() public {
        uint256 s = vm.snapshotState();
        vm.prank(address(atlas));
        uint256 noBid = solver.atlasSolverCall(signer, ee, address(0), 0, abi.encode(_calls(), _noSwap(), USDC, 0), "");
        vm.revertToState(s);

        uint256 bid = 0.01 ether;
        vm.prank(address(atlas));
        uint256 net = solver.atlasSolverCall(signer, ee, address(0), bid, _op(bid, 1e6), "");
        assertEq(ee.balance, bid, "bid paid in ETH");
        assertEq(IERC20A(USDC).balanceOf(receiver), net, "net profit forwarded");
        assertLt(net, noBid, "the bid was paid out of the profit");
        assertEq(IERC20A(USDC).balanceOf(address(solver)) + IERC20A(WETH).balanceOf(address(solver)) + address(solver).balance, 0, "nothing at rest");
        assertEq(IERC20A(USDC).balanceOf(address(liq)) + IERC20A(WETH).balanceOf(address(liq)), 0, "liquidator empty");
        emit log_named_decimal_uint("net USDC without bid", noBid, 6);
        emit log_named_decimal_uint("net USDC after a 0.01 ETH bid", net, 6);
    }

    function test_aBidAboveTheProfitReverts() public {
        vm.prank(address(atlas));
        vm.expectRevert(); // the bid swap cannot buy it, or InsufficientProfit
        solver.atlasSolverCall(signer, ee, address(0), 1_000 ether, _op(1_000 ether, 0), "");
    }

    /// The settlement calls the solver makes exist on the Atlas deployed on Base, with the semantics
    /// the solver assumes: nothing owed outside a metacall, and reconcile refused there (WrongPhase).
    /// (A full metacall needs the user's and the DApp's signatures, so it cannot be replayed here.)
    function test_theDeployedAtlasEscrowHasTheSettlementCallsTheSolverMakes() public {
        IAtlasLive real = IAtlasLive(0x583dcFef0D240DC80753F0F0B26513feE27D9B77);
        (uint256 gasLiability, uint256 borrowLiability) = real.shortfall();
        assertEq(gasLiability + borrowLiability, 0, "outside a metacall nothing is owed");
        vm.expectRevert(bytes4(0xe2586bcc)); // WrongPhase()
        real.reconcile(0);
        uint256 rate = real.getAtlasSurchargeRate();
        assertGt(rate, 0);
        assertLt(rate, 10_000, "scale 1e4: below 100%");
        emit log_named_decimal_uint("Atlas gas surcharge", rate, 4);
    }

    function _calls() internal view returns (SolverCall[] memory calls) {
        calls = new SolverCall[](1);
        calls[0] = _liquidation(1 ether);
    }

    function _noSwap() internal pure returns (SwapStep memory s) {}
}
