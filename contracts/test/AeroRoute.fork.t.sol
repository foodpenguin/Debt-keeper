// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {GridQuoter} from "../src/sim/GridQuoter.sol";

interface IClFactory {
    function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address);
}

/// The direct router on the Aero CL pools (Aerodrome + Velodrome merger, 2026-10-21): the factory
/// answers getPool(a, b, tickSpacing) like Slipstream, its pools quote through GridQuoter, and the
/// output follows the tx gas price (the priority-fee tax), which is why plans are simulated again
/// at the bid (runner::checked_priority) before they are sent.
contract AeroRouteForkTest is Test {
    address constant AERO_CL_FACTORY = 0x9CB220581b91b0C83223012FE69688b92aD07a76;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    // Uniswap V3 WETH/USDC 0.05%, the price reference.
    address constant UNI_WETH_USDC_500 = 0xd0b53D9277642d899DF5C87A3966A349A798F224;
    GridQuoter quoter;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        vm.createSelectFork(rpc, vm.envOr("FORK_BLOCK_AERO", uint256(53_367_000)));
        quoter = new GridQuoter();
    }

    function _quote(address pool, uint256 amount) internal returns (uint256) {
        GridQuoter.Path[] memory paths = new GridQuoter.Path[](1);
        address[] memory pools = new address[](1);
        pools[0] = pool;
        bool[] memory zfo = new bool[](1);
        zfo[0] = true; // WETH = token0
        paths[0] = GridQuoter.Path({pools: pools, zeroForOne: zfo});
        uint256[] memory amts = new uint256[](1);
        amts[0] = amount;
        (uint256[][] memory outs,) = quoter.quote(paths, amts);
        return outs[0][0];
    }

    function test_aeroClPoolQuotesNearTheMarket() public {
        address pool = IClFactory(AERO_CL_FACTORY).getPool(WETH, USDC, 50);
        assertTrue(pool != address(0), "Aero CL WETH/USDC tick spacing 50");
        uint256 aero = _quote(pool, 0.1 ether);
        uint256 uni = _quote(UNI_WETH_USDC_500, 0.1 ether);
        assertGt(aero, 0, "Aero CL pool quotes through GridQuoter");
        // Same market: within 2% of the Uniswap pool.
        assertApproxEqRel(aero, uni, 0.02e18);
        emit log_named_decimal_uint("Aero CL out (0.1 WETH)", aero, 6);
        emit log_named_decimal_uint("Uni V3 out (0.1 WETH)", uni, 6);
    }

    function test_aeroOutputAtAHighPriorityIsNoBetter() public {
        address pool = IClFactory(AERO_CL_FACTORY).getPool(WETH, USDC, 50);
        vm.fee(0.01 gwei);
        vm.txGasPrice(0.01 gwei);
        uint256 low = _quote(pool, 0.1 ether);
        vm.txGasPrice(5 gwei);
        uint256 high = _quote(pool, 0.1 ether);
        assertLe(high, low, "a higher tx gas price never improves the Aero output");
        emit log_named_decimal_uint("out at 0.01 gwei", low, 6);
        emit log_named_decimal_uint("out at 5 gwei", high, 6);
    }
}
