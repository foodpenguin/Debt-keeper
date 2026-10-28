// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {IMorpho, IOracle, Id, MarketParams} from "../src/interfaces/IMorpho.sol";
import {IV3Pool, MIN_SQRT_PRICE_LIMIT, MAX_SQRT_PRICE_LIMIT} from "../src/interfaces/IV3Pool.sol";
import {MorphoLiquidator} from "../src/MorphoLiquidator.sol";

interface IERC20A {
    function approve(address, uint256) external returns (bool);
    function transfer(address, uint256) external returns (bool);
}

interface IGasPriceOracle {
    function getL1FeeUpperBound(uint256 unsignedTxSize) external view returns (uint256);
}

/// Gas floor: Morpho liquidate + one direct pool swap + repayment approval, with NO checks at all
/// (no access control, no profit check, profit left in the contract). Our overhead = ours - floor.
contract FloorLiquidator {
    IMorpho immutable morpho;

    constructor(IMorpho m) {
        morpho = m;
    }

    function run(MarketParams calldata mp, address borrower, uint256 seize, address p, bool zfo) external {
        morpho.liquidate(mp, borrower, seize, 0, abi.encode(mp.loanToken, zfo, seize, p));
    }

    function onMorphoLiquidate(uint256 repaid, bytes calldata data) external {
        (address loan, bool zfo, uint256 seize, address pool) = abi.decode(data, (address, bool, uint256, address));
        IV3Pool(pool).swap(address(this), zfo, int256(seize), zfo ? MIN_SQRT_PRICE_LIMIT : MAX_SQRT_PRICE_LIMIT, "");
        IERC20A(loan).approve(address(morpho), repaid);
    }

    function uniswapV3SwapCallback(int256 a0, int256 a1, bytes calldata) external {
        address t = a0 > 0 ? IV3Pool(msg.sender).token0() : IV3Pool(msg.sender).token1();
        IERC20A(t).transfer(msg.sender, uint256(a0 > 0 ? a0 : a1));
    }
}

/// Pinned-block gas benchmark (run with --match-contract GasBench -vv). Numbers are comparable
/// across contract versions because block, borrower, oracle shock and route are fixed.
contract GasBenchForkTest is Test {
    IMorpho constant MORPHO = IMorpho(0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb);
    uint256 constant BENCH_BLOCK = 52_719_900;
    address constant UNI_WETH_USDC_500 = 0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant PANCAKE_XRP_WETH_500 = 0x170b6bFc071426aaB7C81AE12383219facBb6B6A;
    address constant SLIP_XRP_WETH_2000 = 0xA6da283cf7D06b2279626E787c329d6aCF7e2994;
    address constant SLIP_WETH_USDC_100 = 0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59;
    Id constant WETH_MARKET = Id.wrap(0x8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda);
    address constant WETH_BORROWER = 0x33B5a89294e778d14e8Ce4b0828889B6b81b6488;
    Id constant XRP_MARKET = Id.wrap(0xd4a903dc6d949519060c7707f9604fdc9772c046e05c2e3a8fce0bd7196e4109);
    address constant XRP_BORROWER = 0xCB493f715f7bF89d54E505e1EF3cA0d3122a576A;

    address owner = makeAddr("owner");
    address executor = makeAddr("executor");
    address receiver = makeAddr("receiver");
    MorphoLiquidator liq;
    FloorLiquidator floor;
    MarketParams wethMp;
    MarketParams xrpMp;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        vm.createSelectFork(rpc, BENCH_BLOCK);
        liq = new MorphoLiquidator(MORPHO, owner, executor, receiver);
        vm.prank(owner);
        liq.approveMorpho(0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913, true); // USDC
        floor = new FloorLiquidator(MORPHO);
        wethMp = MORPHO.idToMarketParams(WETH_MARKET);
        xrpMp = MORPHO.idToMarketParams(XRP_MARKET);
        _setup(liq);
        // Steady state: the profit receiver already holds USDC (no fresh-slot SSTORE per profit).
        deal(wethMp.loanToken, receiver, 1e6);
    }

    /// One-time owner setup the optimized contract may rely on (e.g. standing Morpho approvals).
    function _setup(MorphoLiquidator l) internal virtual {
        // (no-op for the baseline contract)
        l;
    }

    function _crash(MarketParams memory mp, uint256 bps) internal {
        uint256 p = IOracle(mp.oracle).price();
        vm.mockCall(mp.oracle, abi.encodeWithSelector(IOracle.price.selector), abi.encode(p * (10_000 - bps) / 10_000));
    }

    function _hop(address pool, uint8 flags, uint256 amount) internal pure returns (bytes memory) {
        return abi.encodePacked(pool, flags, uint128(amount));
    }

    function _ours(MarketParams memory mp, address borrower, uint256 seize, bytes memory route) internal returns (uint256 used) {
        bytes memory cd = abi.encodeCall(MorphoLiquidator.liquidateRoute, (mp, borrower, seize, route, 0, block.number));
        vm.prank(executor);
        uint256 g = gasleft();
        (bool ok, bytes memory ret) = address(liq).call(cd);
        used = g - gasleft();
        require(ok, string(ret));
        emit log_named_uint("calldata bytes", cd.length);
        uint256 nz;
        for (uint256 i; i < cd.length; ++i) if (cd[i] != 0) ++nz;
        emit log_named_uint("intrinsic calldata gas (abi)", nz * 16 + (cd.length - nz) * 4);
        uint256 l1 = IGasPriceOracle(0x420000000000000000000000000000000000000F).getL1FeeUpperBound(cd.length + 100);
        emit log_named_uint("L1 fee upper bound (wei)", l1);
    }

    function _packed(MarketParams memory mp, address borrower, uint256 seize, bytes memory route) internal view returns (bytes memory) {
        return abi.encodePacked(
            bytes1(0x00), mp.loanToken, mp.collateralToken, mp.oracle, mp.irm, uint64(mp.lltv),
            borrower, uint128(seize), uint128(0), uint32(block.number), route
        );
    }

    function _oursPacked(MarketParams memory mp, address borrower, uint256 seize, bytes memory route) internal returns (uint256 used) {
        bytes memory cd = _packed(mp, borrower, seize, route);
        vm.prank(executor);
        uint256 g = gasleft();
        (bool ok, bytes memory ret) = address(liq).call(cd);
        used = g - gasleft();
        require(ok, string(ret));
        emit log_named_uint("calldata bytes (packed)", cd.length);
        uint256 nz;
        for (uint256 i; i < cd.length; ++i) if (cd[i] != 0) ++nz;
        emit log_named_uint("intrinsic calldata gas (packed)", nz * 16 + (cd.length - nz) * 4);
        uint256 l1 = IGasPriceOracle(0x420000000000000000000000000000000000000F).getL1FeeUpperBound(cd.length + 100);
        emit log_named_uint("L1 fee upper bound (wei, packed)", l1);
    }

    function test_bench_weth_1hop_packed() public {
        _crash(wethMp, 1_000);
        emit log_named_uint("OURS  weth 1-hop gas (packed)", _oursPacked(wethMp, WETH_BORROWER, 1 ether, _hop(UNI_WETH_USDC_500, 1, 1 ether)));
    }

    function test_bench_xrp_pooled_split_packed() public {
        _crash(xrpMp, 500);
        bytes memory route = bytes.concat(
            _hop(PANCAKE_XRP_WETH_500, 0, 1_000e6),
            _hop(SLIP_XRP_WETH_2000, 0, 1_000e6),
            _hop(UNI_WETH_USDC_500, 5, 6_000),
            _hop(SLIP_WETH_USDC_100, 5, 10_000)
        );
        emit log_named_uint("OURS  xrp pooled split gas (packed)", _oursPacked(xrpMp, XRP_BORROWER, 2_000e6, route));
    }

    function test_bench_weth_1hop() public {
        _crash(wethMp, 1_000);
        uint256 used = _ours(wethMp, WETH_BORROWER, 1 ether, _hop(UNI_WETH_USDC_500, 1, 1 ether));
        emit log_named_uint("OURS  weth 1-hop gas", used);
    }

    function test_bench_weth_1hop_floor() public {
        _crash(wethMp, 1_000);
        uint256 g = gasleft();
        floor.run(wethMp, WETH_BORROWER, 1 ether, UNI_WETH_USDC_500, true);
        emit log_named_uint("FLOOR weth 1-hop gas", g - gasleft());
    }

    function test_bench_xrp_2hop() public {
        _crash(xrpMp, 500);
        bytes memory route = bytes.concat(_hop(PANCAKE_XRP_WETH_500, 0, 1_000e6), _hop(UNI_WETH_USDC_500, 3, 0));
        emit log_named_uint("OURS  xrp 2-hop gas", _ours(xrpMp, XRP_BORROWER, 1_000e6, route));
    }

    function test_bench_xrp_pooled_split() public {
        _crash(xrpMp, 500);
        bytes memory route = bytes.concat(
            _hop(PANCAKE_XRP_WETH_500, 0, 1_000e6),
            _hop(SLIP_XRP_WETH_2000, 0, 1_000e6),
            _hop(UNI_WETH_USDC_500, 5, 6_000),
            _hop(SLIP_WETH_USDC_100, 5, 10_000)
        );
        emit log_named_uint("OURS  xrp 2 src + 2 pooled sinks gas", _ours(xrpMp, XRP_BORROWER, 2_000e6, route));
    }
}
