// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {IMorpho, IOracle, Id, MarketParams} from "../src/interfaces/IMorpho.sol";
import {MorphoLiquidator, SwapStep} from "../src/MorphoLiquidator.sol";
import {AtlasLiquidationSolver, SolverCall} from "../src/AtlasLiquidationSolver.sol";
import {AtlasStub} from "../src/sim/AtlasStub.sol";

interface IERC20B {
    function approve(address, uint256) external returns (bool);
    function transfer(address, uint256) external returns (bool);
    function transferFrom(address, address, uint256) external returns (bool);
    function balanceOf(address) external view returns (uint256);
}

interface ISwapRouter02B {
    struct ExactOutputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountOut;
        uint256 amountInMaximum;
        uint160 sqrtPriceLimitX96;
    }

    function exactOutputSingle(ExactOutputSingleParams calldata params) external payable returns (uint256);
}

/// Liquidations with no swap and no checks, the loan token pre-funded: the gas floor of a
/// flash-loan batch (liquidate N, then one exit) without its single swap.
contract BareBatch {
    IMorpho immutable morpho;

    constructor(IMorpho m) {
        morpho = m;
    }

    function run(MarketParams calldata mp, address[] calldata b, uint256[] calldata s) external {
        IERC20B(mp.loanToken).approve(address(morpho), type(uint256).max);
        for (uint256 i; i < b.length; ++i) morpho.liquidate(mp, b[i], s[i], 0, "");
    }
}

/// A test-only exit that pays the loan token at the oracle price for any size, with no DEX. Lets the
/// batch be measured at the winner's real sizes, isolating the flash-loan + liquidation gas from the
/// exit swap (whose cost is the DEX's, not the solver's).
contract MockExitRouter {
    address immutable loan;
    uint256 immutable price; // collateral -> loan, 1e36 scale (Morpho oracle convention)

    constructor(address loan_, uint256 price_) {
        loan = loan_;
        price = price_;
    }

    function sell(address tokenIn, uint256 amountIn) external {
        IERC20B(tokenIn).transferFrom(msg.sender, address(this), amountIn);
        IERC20B(loan).transfer(msg.sender, amountIn * price / 1e36);
    }
}

/// Gas of one Atlas solver op that liquidates N cbXRP/USDC positions, replaying the SVR auction of
/// 2026-11-07 02:53 UTC (metacall 0x78a150ca…, block 54148059: the winner liquidated these 30
/// borrowers). Forked one block before, oracle mocked to the post-update price. Each position is
/// seized at 1/SCALE of what the winner seized, so the DEX exits stay executable; gas per
/// liquidation does not depend on the size, the swap's does (more ticks crossed when larger).
/// Run: forge test --match-contract AtlasBundleGas -vv
contract AtlasBundleGasForkTest is Test {
    IMorpho constant MORPHO = IMorpho(0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb);
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant UNI_V3_ROUTER = 0x2626664c2603336E57B271c5C0b26F421741e481;
    address constant UNI_WETH_USDC_500 = 0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant PANCAKE_XRP_WETH_500 = 0x170b6bFc071426aaB7C81AE12383219facBb6B6A;
    Id constant XRP_MARKET = Id.wrap(0xd4a903dc6d949519060c7707f9604fdc9772c046e05c2e3a8fce0bd7196e4109);
    uint256 constant FORK_BLOCK = 54148058;
    uint256 constant PRICE_AFTER = 1164024870000000000000000000000000000;
    uint256 constant SCALE = 1000;

    address owner = makeAddr("owner");
    address signer = makeAddr("signer");
    address receiver = makeAddr("receiver");
    address ee = makeAddr("executionEnvironment");
    AtlasStub atlas;
    AtlasLiquidationSolver solver;
    MorphoLiquidator liq;
    BareBatch bare;
    MockExitRouter mockExit;
    MarketParams mp;
    address[30] b;
    uint256[30] s;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        vm.createSelectFork(rpc, FORK_BLOCK);
        mp = MORPHO.idToMarketParams(XRP_MARKET);
        atlas = new AtlasStub();
        solver = new AtlasLiquidationSolver(address(atlas), WETH, owner, signer, receiver);
        liq = new MorphoLiquidator(MORPHO, owner, address(solver), address(solver));
        bare = new BareBatch(MORPHO);
        mockExit = new MockExitRouter(USDC, PRICE_AFTER);
        vm.startPrank(owner);
        liq.approveMorpho(USDC, true);
        liq.setAllowedTarget(address(mockExit), true); // batch mock-exit (steps mode)
        solver.setAllowedTarget(address(liq), true);
        solver.setAllowedTarget(UNI_V3_ROUTER, true);
        vm.stopPrank();
        vm.mockCall(mp.oracle, abi.encodeWithSelector(IOracle.price.selector), abi.encode(PRICE_AFTER));
        deal(USDC, receiver, 1e6); // steady state: the receiver already holds USDC
        deal(USDC, address(bare), 10_000_000e6);
        deal(USDC, address(mockExit), 100_000_000e6); // the mock exit pays USDC at the oracle price
        deal(USDC, address(MORPHO), 100_000_000e6); // headroom so the flash loan always has liquidity
        b[0] = 0x75d871189219172514178AD4ab4E35444c8c2d0c; s[0] = 194871528250;
        b[1] = 0x2e6a6B91C5B0432f0dA00C7b0DD3e65baef0E709; s[1] = 24975700233;
        b[2] = 0x504B9B3058064B12b8C2B207B898e2e618A9d153; s[2] = 22546096380;
        b[3] = 0x649F8d8ED3ABFBd1E51D8A742E1e7b912323dD72; s[3] = 7370824601;
        b[4] = 0xEe71D8fc58C5ed00F43be2f6d595D6016B60327C; s[4] = 6934270404;
        b[5] = 0x8D1fde5e7617Bc6A2CE0567496a184197071EB68; s[5] = 5015270594;
        b[6] = 0xB112d024d4b3D410397f3A7c3649307DC28fbADF; s[6] = 3192199077;
        b[7] = 0x1c5FA08a52F4F6e1A5Ffe41B95820fB980B569Fd; s[7] = 2888735071;
        b[8] = 0x9940cD2C669803094E814887C58C407934b9c5f6; s[8] = 2738680438;
        b[9] = 0xABe404782dEE0e8CA60C85bdd53df7C135473E3F; s[9] = 2111191975;
        b[10] = 0x1E330F2982336eb543171688969a3C2850e78912; s[10] = 1740590328;
        b[11] = 0x73fcE575E59dfDdBB7B3a44f7Bcd7e0B4f2b0F72; s[11] = 1607555647;
        b[12] = 0x73BC561906ed51895B06a7C0C95D7a0FA1897736; s[12] = 1444300256;
        b[13] = 0x22b09fd34B9E74004Ec790C666fBb83652411841; s[13] = 1434749979;
        b[14] = 0x5d0140cF83501D3dAB7b37F80bBc40F5065B1Aa8; s[14] = 1432151525;
        b[15] = 0x0783911677fb78dD9988dBC2a432efaAE66675Df; s[15] = 1197679318;
        b[16] = 0x36Bf5B879e252aC40D7b08CF107038326ee768f6; s[16] = 977388787;
        b[17] = 0x69A49e7DC504892EE84be2157fC9eAE1bBc6F042; s[17] = 903665066;
        b[18] = 0x78c27954B16F415A3b6e8Cb39077df2C00355e4E; s[18] = 827804435;
        b[19] = 0x5f447be2d969710F66495a6C4554fd528dD9ee7d; s[19] = 707909576;
        b[20] = 0x1d6Aa5588f6a4F2835e5a379Be6fc6fA36F7ce37; s[20] = 654554420;
        b[21] = 0x0891462a426DB46b74B3E8cda5f19cd0e6ee6B04; s[21] = 450615394;
        b[22] = 0xa2dF3961A2DCaB2d852f8327a36410A1F777686F; s[22] = 435653519;
        b[23] = 0xffeF7D27b449F9427367F1ea5Ab0ADA780504c0D; s[23] = 428793427;
        b[24] = 0x0908E7E5a5Bd0fB5e4b19d7cB3B1082D0755e567; s[24] = 426762271;
        b[25] = 0xAc69098eC95DDAf8b0a044833928B7720AA965F1; s[25] = 389598180;
        b[26] = 0xcFCD5FAC8243e8B1E84b7835f3De895bE69FAe10; s[26] = 387869406;
        b[27] = 0xa057AD9304ba0F40aa05F33dA4e7204C0943A9b1; s[27] = 356024953;
        b[28] = 0x441b0c57F706D5cC1266a4A568fEE78027E48db3; s[28] = 354454130;
        b[29] = 0x42714b6546A41BC47ff586ca999D9ba37293764C; s[29] = 317456873;

    }

    function _hop(address pool, uint8 flags, uint256 amount) internal pure returns (bytes memory) {
        return abi.encodePacked(pool, flags, uint128(amount));
    }

    /// The liquidator's packed entry with a 2-hop direct exit (cbXRP -> WETH -> USDC).
    function _packed(uint256 i) internal view returns (bytes memory) {
        uint256 seize = s[i] / SCALE;
        bytes memory route = bytes.concat(_hop(PANCAKE_XRP_WETH_500, 0, seize), _hop(UNI_WETH_USDC_500, 3, 0));
        return abi.encodePacked(
            bytes1(0x00), mp.loanToken, mp.collateralToken, mp.oracle, mp.irm, uint64(mp.lltv),
            b[i], uint128(seize), uint128(0), uint32(block.number), route
        );
    }

    function _op(uint256 n, uint256 bid) internal view returns (bytes memory) {
        SolverCall[] memory calls = new SolverCall[](n);
        for (uint256 i; i < n; ++i) calls[i] = SolverCall({target: address(liq), data: _packed(i)});
        SwapStep memory bidSwap = SwapStep({
            target: UNI_V3_ROUTER,
            tokenIn: USDC,
            amountIn: type(uint256).max,
            data: abi.encodeCall(
                ISwapRouter02B.exactOutputSingle,
                (ISwapRouter02B.ExactOutputSingleParams({tokenIn: USDC, tokenOut: WETH, fee: 500, recipient: address(solver), amountOut: bid, amountInMaximum: type(uint128).max, sqrtPriceLimitX96: 0}))
            )
        });
        return abi.encode(calls, bidSwap, USDC, uint256(0));
    }

    function _measure(uint256 n) internal {
        bytes memory data = _op(n, 0.001 ether);
        vm.prank(address(atlas));
        uint256 g = gasleft();
        solver.atlasSolverCall(signer, ee, address(0), 0.001 ether, data, "");
        uint256 used = g - gasleft();
        uint256 nz;
        for (uint256 i; i < data.length; ++i) if (data[i] != 0) ++nz;
        emit log_named_uint("positions", n);
        emit log_named_uint("solver op gas (execution)", used);
        emit log_named_uint("solverOpData bytes", data.length);
        emit log_named_uint("solverOpData calldata gas", nz * 16 + (data.length - nz) * 4);
    }

    function test_ours_01() public { _measure(1); }
    function test_ours_02() public { _measure(2); }
    function test_ours_05() public { _measure(5); }
    function test_ours_10() public { _measure(10); }
    function test_ours_20() public { _measure(20); }
    function test_ours_30() public { _measure(30); }

    function _bare(uint256 n) internal {
        address[] memory bs = new address[](n);
        uint256[] memory ss = new uint256[](n);
        for (uint256 i; i < n; ++i) (bs[i], ss[i]) = (b[i], s[i] / SCALE);
        uint256 g = gasleft();
        bare.run(mp, bs, ss);
        emit log_named_uint("positions", n);
        emit log_named_uint("bare liquidations gas", g - gasleft());
    }

    function test_bare_01() public { _bare(1); }
    function test_bare_10() public { _bare(10); }
    function test_bare_30() public { _bare(30); }

    // ---- batch: one solver op, one flash loan, N liquidations, a single exit ----

    function _bidSwap(uint256 bid) internal view returns (SwapStep memory) {
        return SwapStep({
            target: UNI_V3_ROUTER,
            tokenIn: USDC,
            amountIn: type(uint256).max,
            data: abi.encodeCall(
                ISwapRouter02B.exactOutputSingle,
                (ISwapRouter02B.ExactOutputSingleParams({tokenIn: USDC, tokenOut: WETH, fee: 500, recipient: address(solver), amountOut: bid, amountInMaximum: type(uint128).max, sqrtPriceLimitX96: 0}))
            )
        });
    }

    function _batchItems(uint256 n, bool scale) internal view returns (bytes memory items, uint256 total) {
        for (uint256 i; i < n; ++i) {
            uint256 seize = scale ? s[i] / SCALE : s[i];
            items = bytes.concat(items, abi.encodePacked(b[i], uint128(seize)));
            total += seize;
        }
    }

    /// The liquidator's packed batch entry (tag 0x01) over `n` items and `exit`. flashAssets is the
    /// seized value (above the sum of the repayments) plus a small margin.
    function _batchPacked(bytes memory items, uint256 n, uint256 total, bytes memory exit) internal view returns (bytes memory) {
        uint256 flashAssets = total * PRICE_AFTER / 1e36 + 1e6;
        return abi.encodePacked(
            bytes1(0x01), mp.loanToken, mp.collateralToken, mp.oracle, mp.irm, uint64(mp.lltv),
            uint128(flashAssets), uint128(0), uint32(block.number), uint16(n), items, exit
        );
    }

    /// MODE_ROUTE exit: one 2-hop direct route selling all the seized cbXRP (cbXRP -> WETH -> USDC).
    function _routeExit(uint256 total) internal view returns (bytes memory) {
        return abi.encodePacked(bytes1(0x01), _hop(PANCAKE_XRP_WETH_500, 0, total), _hop(UNI_WETH_USDC_500, 3, 0));
    }

    /// MODE_STEPS exit: the mock oracle-priced router (no DEX), to measure the batch without the swap.
    function _mockExitSteps(uint256 total) internal view returns (bytes memory) {
        SwapStep[] memory st = new SwapStep[](1);
        st[0] = SwapStep({target: address(mockExit), tokenIn: mp.collateralToken, amountIn: total, data: abi.encodeCall(MockExitRouter.sell, (mp.collateralToken, total))});
        return abi.encodePacked(bytes1(0x02), abi.encode(st));
    }

    function _batchOp(bytes memory packed, uint256 bid) internal view returns (bytes memory) {
        SolverCall[] memory calls = new SolverCall[](1);
        calls[0] = SolverCall({target: address(liq), data: packed});
        return abi.encode(calls, _bidSwap(bid), USDC, uint256(0));
    }

    function _runBatch(bytes memory data) internal returns (uint256 used) {
        vm.prank(address(atlas));
        uint256 g = gasleft();
        solver.atlasSolverCall(signer, ee, address(0), 0.001 ether, data, "");
        used = g - gasleft();
    }

    /// Batch at the 1/SCALE sizes with the real 2-hop direct exit (the full on-chain cost).
    function _measureBatch(uint256 n) internal {
        (bytes memory items, uint256 total) = _batchItems(n, true);
        bytes memory data = _batchOp(_batchPacked(items, n, total, _routeExit(total)), 0.001 ether);
        uint256 nz;
        for (uint256 i; i < data.length; ++i) if (data[i] != 0) ++nz;
        emit log_named_uint("positions", n);
        emit log_named_uint("batch op gas, real 2-hop exit", _runBatch(data));
        emit log_named_uint("solverOpData calldata gas", nz * 16 + (data.length - nz) * 4);
    }

    /// Batch at the winner's real sizes with the oracle-priced mock exit: flash loan + N liquidations
    /// + a trivial exit, i.e. the batch gas minus the DEX swap. This is the 4.4 acceptance figure.
    function _measureBatchMock(uint256 n) internal {
        (bytes memory items, uint256 total) = _batchItems(n, false);
        bytes memory data = _batchOp(_batchPacked(items, n, total, _mockExitSteps(total)), 0.001 ether);
        emit log_named_uint("positions", n);
        emit log_named_uint("batch op gas, flash+liquidations (mock exit)", _runBatch(data));
    }

    function test_batch_01() public { _measureBatch(1); }
    function test_batch_05() public { _measureBatch(5); }
    function test_batch_10() public { _measureBatch(10); }
    function test_batch_20() public { _measureBatch(20); }
    function test_batch_30() public { _measureBatch(30); }

    function test_batch_mock_01() public { _measureBatchMock(1); }
    function test_batch_mock_05() public { _measureBatchMock(5); }
    function test_batch_mock_10() public { _measureBatchMock(10); }
    function test_batch_mock_20() public { _measureBatchMock(20); }
    function test_batch_mock_30() public { _measureBatchMock(30); }

    /// The winner's own sizes, liquidations only (what its 30-position op paid before its RFQ exit).
    function test_bare_30_full_size() public {
        address[] memory bs = new address[](30);
        uint256[] memory ss = new uint256[](30);
        for (uint256 i; i < 30; ++i) (bs[i], ss[i]) = (b[i], s[i]);
        uint256 g = gasleft();
        bare.run(mp, bs, ss);
        emit log_named_uint("bare liquidations gas, winner sizes", g - gasleft());
    }
}
