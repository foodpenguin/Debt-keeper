// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";
import {AtlasLiquidationSolver, SolverCall} from "../src/AtlasLiquidationSolver.sol";
import {SwapStep} from "../src/MorphoLiquidator.sol";

// ABI-compatible copies of the Atlas operation types (field order and types match the deployed
// Atlas's metacall; written here, not imported).
struct AUserOp {
    address from;
    address to;
    uint256 value;
    uint256 gas;
    uint256 maxFeePerGas;
    uint256 nonce;
    uint256 deadline;
    address dapp;
    address control;
    uint32 callConfig;
    uint32 dappGasLimit;
    uint32 solverGasLimit;
    uint24 bundlerSurchargeRate;
    address sessionKey;
    bytes data;
    bytes signature;
}

struct ASolverOp {
    address from;
    address to;
    uint256 value;
    uint256 gas;
    uint256 maxFeePerGas;
    uint256 deadline;
    address solver;
    address control;
    bytes32 userOpHash;
    address bidToken;
    uint256 bidAmount;
    bytes data;
    bytes signature;
}

struct ADAppOp {
    address from;
    address to;
    uint256 nonce;
    uint256 deadline;
    address control;
    address bundler;
    bytes32 userOpHash;
    bytes32 callChainHash;
    bytes signature;
}

struct ADAppConfig {
    address to;
    uint32 callConfig;
    address bidToken;
    uint32 solverGasLimit;
    uint32 dappGasLimit;
    uint128 bundlerSurchargeRate;
}

interface IAtlasMeta {
    function metacall(AUserOp calldata userOp, ASolverOp[] calldata solverOps, ADAppOp calldata dAppOp, address gasRefundBeneficiary) external payable returns (bool);
    function depositAndBond(uint256 amountToBond) external payable;
    function balanceOfBonded(address account) external view returns (uint256);
    function VERIFICATION() external view returns (address);
    function L2_GAS_CALCULATOR() external view returns (address);
}

interface IAtlasVerificationA {
    function validateCalls(ADAppConfig calldata, AUserOp calldata, ASolverOp[] calldata, ADAppOp calldata, uint256, uint256, address, bool)
        external
        returns (uint256 allSolversGasLimit, uint256 allSolversCalldataGas, uint256 bidFindOverhead, uint8 validCallsResult);
    function verifySolverOp(ASolverOp calldata, bytes32, uint256, address, bool) external view returns (uint256);
}

interface IL2GasCalculatorA {
    function getCalldataGas(uint256 calldataLength) external view returns (uint256);
}

interface IERC20G {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
}

interface ISwapRouter02G {
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

/// Stands in for the solver's liquidations: pays the solver USDC (the proceeds a liquidation leaves).
contract ProfitSource {
    function pay(address token, address to, uint256 amount) external {
        IERC20G(token).transfer(to, amount);
    }
}

/// The solver's gas inside a REAL Atlas metacall on Base: how the bond is checked before the op runs,
/// and how the gas the op pays in ETH out of its proceeds (`gasPayWei`) settles against the bond.
///
/// Replays the SVR metacall of block 54148059 (tx 0x78a150ca…, bundled at 15.005 gwei: userOp 0.5M,
/// dappGasLimit 2M, solver ops 2.2M + 6.0M, tx gas limit 11.106M) one block before, with the
/// winner's op replaced by ours (same 2.2M gas) and the runner-up's real op behind it. The real
/// userOp (the oracle update), dAppOp and Atlas escrow run; only AtlasVerification is mocked: the
/// signatures and the call-chain hash cannot be produced for a new solver op, so `validateCalls`
/// returns what it would compute (the ops' gas sums, by Atlas's own formula) and `verifySolverOp`
/// passes. The bond check (`_validateSolverOpGasAndValue`), `reconcile` and `_settle` are Atlas's.
/// Run: forge test --match-contract AtlasBondGasFork -vv   (BASE_RPC_URL: an archive endpoint)
contract AtlasBondGasForkTest is Test {
    IAtlasMeta constant ATLAS = IAtlasMeta(0x583dcFef0D240DC80753F0F0B26513feE27D9B77);
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant UNI_V3_ROUTER = 0x2626664c2603336E57B271c5C0b26F421741e481;
    address constant BUNDLER = 0xEeaCF719DBbbE9a2a06C076ab6Bd5BEb08e8C41d;
    uint256 constant FORK_BLOCK = 54148058;
    uint256 constant GAS_PRICE = 15.005 gwei;
    uint256 constant OP_GAS = 2_200_000;
    /// What the bundler adds to the gas limits (userOp + dappGasLimit + the solver ops): 11,106,240
    /// - 0.5M - 2M - 2.2M - 6.0M.
    uint256 constant BUNDLER_OVERHEAD = 406_240;
    /// The bond threshold measured by replaying the original metacall for its 2.2M op (research).
    uint256 constant MEASURED_THRESHOLD = 0.0846 ether;
    uint256 constant INSUFFICIENT_ESCROW = 1 << 13;
    uint256 constant BID = 0.001 ether;

    address owner = makeAddr("owner");
    address signer = makeAddr("signer");
    address receiver = makeAddr("receiver");
    AtlasLiquidationSolver solver;
    ProfitSource source;
    bytes input;
    /// false: the metacall starts with exactly the gas the original one had (its gas limit minus
    /// its own intrinsic cost), so the liability is the original winner's; true: the gas limit is
    /// sized like the bundler sizes it and only OUR calldata's intrinsic cost is taken (what our
    /// op would really face; a lighter calldata leaves more gas, so a slightly larger liability).
    bool bundlerSized;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        vm.createSelectFork(rpc, FORK_BLOCK);
        input = vm.parseBytes(vm.readFile("test/fixtures/svr_metacall_54148059.hex"));
        solver = new AtlasLiquidationSolver(address(ATLAS), WETH, owner, signer, receiver);
        source = new ProfitSource();
        deal(USDC, address(source), 1_000_000e6);
        vm.startPrank(owner);
        solver.setAllowedTarget(address(source), true);
        solver.setAllowedTarget(UNI_V3_ROUTER, true);
        vm.stopPrank();
    }

    // ------------------------------------------------------------------ helpers

    function _decoded() internal view returns (AUserOp memory u, ASolverOp[] memory ops, ADAppOp memory d) {
        bytes memory args = new bytes(input.length - 4);
        for (uint256 i; i < args.length; ++i) args[i] = input[i + 4];
        (u, ops, d,) = abi.decode(args, (AUserOp, ASolverOp[], ADAppOp, address));
    }

    /// Our solverOpData: 2,000 USDC of "liquidation" proceeds, the bid + gasPay bought as WETH on
    /// Uniswap V3.
    function _opData(uint256 gasPay) internal view returns (bytes memory) {
        SolverCall[] memory calls = new SolverCall[](1);
        calls[0] = SolverCall({target: address(source), data: abi.encodeCall(ProfitSource.pay, (USDC, address(solver), 2_000e6))});
        ISwapRouter02G.ExactOutputSingleParams memory p = ISwapRouter02G.ExactOutputSingleParams({
            tokenIn: USDC, tokenOut: WETH, fee: 500, recipient: address(solver), amountOut: BID + gasPay, amountInMaximum: type(uint128).max, sqrtPriceLimitX96: 0
        });
        SwapStep memory bidSwap = SwapStep({target: UNI_V3_ROUTER, tokenIn: USDC, amountIn: type(uint256).max, data: abi.encodeCall(ISwapRouter02G.exactOutputSingle, (p))});
        return abi.encode(calls, bidSwap, USDC, uint256(0), gasPay);
    }

    function _ourOp(AUserOp memory u, ADAppOp memory d, uint256 gasPay) internal view returns (ASolverOp memory op) {
        op.from = signer;
        op.to = address(ATLAS);
        op.gas = OP_GAS;
        op.maxFeePerGas = u.maxFeePerGas;
        op.deadline = u.deadline;
        op.solver = address(solver);
        op.control = u.control;
        op.userOpHash = d.userOpHash;
        op.bidAmount = BID;
        op.data = _opData(gasPay);
    }

    /// Intrinsic calldata cost of a tx carrying `data` (4 per zero byte, 16 per other).
    function _calldataCost(bytes memory data) internal pure returns (uint256 g) {
        for (uint256 i; i < data.length; ++i) g += data[i] == 0 ? 4 : 16;
    }

    struct Run {
        bool won; // our op won (its proceeds reached the receiver)
        bool ourOpRan; // a SolverTxResult for our op was emitted (the metacall did not revert)
        uint256 result; // our op's SolverOutcome bits
        uint256 bondAfter;
    }

    /// One metacall with our op first and the runner-up's real op behind it, our signer bonding
    /// `bond` beforehand. Gas sized like the SVR bundler: userOp + dappGasLimit + the ops + overhead.
    /// AtlasVerification, mocked: the sums it computes (execution gas capped at solverGasLimit,
    /// calldata gas by the L2 gas calculator, as Atlas charges them), and every solver op valid.
    function _mockVerification(ASolverOp[] memory ops, uint256 solverGasLimit) internal {
        IL2GasCalculatorA l2 = IL2GasCalculatorA(ATLAS.L2_GAS_CALCULATOR());
        uint256 execGas;
        uint256 calldataGas;
        for (uint256 i; i < ops.length; ++i) {
            execGas += ops[i].gas < solverGasLimit ? ops[i].gas : solverGasLimit;
            calldataGas += l2.getCalldataGas(ops[i].data.length + 608);
        }
        address verification = ATLAS.VERIFICATION();
        vm.mockCall(verification, abi.encodeWithSelector(IAtlasVerificationA.validateCalls.selector), abi.encode(execGas, calldataGas, uint256(0), uint8(0)));
        vm.mockCall(verification, abi.encodeWithSelector(IAtlasVerificationA.verifySolverOp.selector), abi.encode(uint256(0)));
    }

    /// The metacall calldata (our op first, the runner-up's real op behind it) and the gas the
    /// bundler would give it at entry: tx gas limit (userOp + dappGasLimit + ops + overhead) minus
    /// the intrinsic cost.
    function _metacall(uint256 gasPay) internal returns (bytes memory call, uint256 gasAtEntry) {
        (AUserOp memory u, ASolverOp[] memory real, ADAppOp memory d) = _decoded();
        ASolverOp[] memory ops = new ASolverOp[](2);
        ops[0] = _ourOp(u, d, gasPay);
        ops[1] = real[1];
        _mockVerification(ops, u.solverGasLimit);
        call = abi.encodeCall(IAtlasMeta.metacall, (u, ops, d, address(0)));
        uint256 txGas = u.gas + u.dappGasLimit + ops[0].gas + ops[1].gas + BUNDLER_OVERHEAD;
        gasAtEntry = txGas - 21_000 - _calldataCost(bundlerSized ? call : input);
    }

    function _run(uint256 bond, uint256 gasPay) internal returns (Run memory r) {
        // Before anything Atlas computes: the L2 gas calculator divides the L1 fee by tx.gasprice.
        vm.txGasPrice(GAS_PRICE);
        vm.deal(signer, bond);
        vm.prank(signer);
        ATLAS.depositAndBond{value: bond}(bond);
        (bytes memory call, uint256 gasAtEntry) = _metacall(gasPay);

        uint256 before = IERC20G(USDC).balanceOf(receiver);
        vm.recordLogs();
        vm.prank(BUNDLER, BUNDLER);
        (bool ok,) = address(ATLAS).call{gas: gasAtEntry}(call);
        ok; // a reverted metacall (no winner) leaves no logs: ourOpRan stays false
        r.won = IERC20G(USDC).balanceOf(receiver) > before;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 topic = keccak256("SolverTxResult(address,address,address,address,uint256,bool,bool,uint256)");
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter == address(ATLAS) && logs[i].topics[0] == topic && address(uint160(uint256(logs[i].topics[1]))) == address(solver)) {
                (,,,, r.result) = abi.decode(logs[i].data, (address, uint256, bool, bool, uint256));
                r.ourOpRan = true;
            }
        }
        r.bondAfter = ATLAS.balanceOfBonded(signer);
    }

    function _diff(uint256 a, uint256 b) internal pure returns (uint256) {
        return a > b ? a - b : b - a;
    }

    // ------------------------------------------------------------------ tests

    /// The least bond with which our op runs (binary search to 1e-6 ETH).
    function _threshold() internal returns (uint256 hi) {
        uint256 lo = 0.05 ether;
        hi = 0.12 ether;
        uint256 s = vm.snapshotState();
        while (hi - lo > 1e12) {
            uint256 mid = (lo + hi) / 2;
            Run memory r = _run(mid, 0);
            vm.revertToState(s);
            if (r.won) hi = mid;
            else lo = mid;
        }
    }

    function _logThreshold(string memory what, uint256 t) internal {
        uint256 liabilityGas = t * 10_000 / 11_000 / GAS_PRICE;
        emit log_named_decimal_uint(string.concat(what, ": bond threshold (ETH)"), t, 18);
        emit log_named_uint(string.concat(what, ": liability gas (threshold / 1.1 / gas price)"), liabilityGas);
        emit log_named_uint(string.concat(what, ": non-solver gas (liability - op gas)"), liabilityGas - OP_GAS);
    }

    /// Atlas refuses to run the op (InsufficientEscrow, before it executes) when the bond does not
    /// cover the op's maximum liability, and the runner-up wins; with the original metacall's gas,
    /// the threshold matches the one measured for its 2.2M op (within 1%).
    function test_theBondMustCoverTheOpsMaximumLiability() public {
        uint256 hi = _threshold();
        _logThreshold("original metacall gas", hi);
        emit log_named_decimal_uint("measured on the original metacall (ETH)", MEASURED_THRESHOLD, 18);
        assertLe(_diff(hi, MEASURED_THRESHOLD), MEASURED_THRESHOLD / 100, "threshold within 1% of the measurement");
        uint256 s = vm.snapshotState();

        // Just below: Atlas skips our op with InsufficientEscrow and the runner-up wins.
        Run memory below = _run(hi - hi / 200, 0);
        assertFalse(below.won, "not run");
        assertTrue(below.ourOpRan, "the metacall went on");
        assertTrue(below.result & INSUFFICIENT_ESCROW != 0, "InsufficientEscrow");
        vm.revertToState(s);
        Run memory above = _run(hi + hi / 200, 0);
        assertTrue(above.won, "run and won");
        assertEq(above.result, 0);
    }

    /// What our own op faces when the bundler sizes the gas limit the same way but our calldata is
    /// lighter: slightly more gas at entry, so a slightly larger liability. The bot's non-solver gas
    /// constant must cover this one, not the original's.
    function test_theLiabilityGrowsWithTheGasLeftAtEntry() public {
        uint256 original = _threshold();
        bundlerSized = true;
        uint256 ours = _threshold();
        _logThreshold("bundler-sized, our calldata", ours);
        assertGt(ours, original, "a lighter calldata leaves more gas at entry");
        assertLe(ours - original, original * 3 / 100, "by a few percent");
    }

    /// gasPayWei = 0 is the previous behavior: the whole real gas charge comes out of the bond.
    function test_withoutGasPayTheWholeChargeComesOutOfTheBond() public {
        uint256 bond = 0.2 ether;
        Run memory r = _run(bond, 0);
        assertTrue(r.won);
        uint256 charge = bond - r.bondAfter;
        emit log_named_decimal_uint("gas charge taken from the bond (ETH)", charge, 18);
        assertGt(charge, 0);
        assertLt(charge, MEASURED_THRESHOLD, "the real charge is below the maximum liability");
    }

    /// Paying about the real gas charge in ETH leaves the bond almost where it was; paying more
    /// credits the excess to the bond, paying less takes the shortfall from it.
    function test_gasPaidFromTheProceedsSettlesAgainstTheBond() public {
        uint256 bond = 0.2 ether;
        uint256 s = vm.snapshotState();
        uint256 charge = bond - _run(bond, 0).bondAfter; // the real charge, all from the bond
        vm.revertToState(s);

        Run memory exact = _run(bond, charge);
        assertTrue(exact.won);
        uint256 moved = _diff(exact.bondAfter, bond);
        emit log_named_decimal_uint("real gas charge (ETH)", charge, 18);
        emit log_named_decimal_uint("bond change when paying it in ETH (ETH)", moved, 18);
        assertLt(moved, charge / 20, "bond moves by < 5% of the gas charge");
        vm.revertToState(s);

        uint256 delta = charge / 2;
        Run memory over = _run(bond, charge + delta);
        assertTrue(over.won);
        assertGt(over.bondAfter, bond, "overpaying credits the bond");
        emit log_named_decimal_uint("bond after overpaying by half the charge (ETH)", over.bondAfter, 18);
        assertLe(_diff(over.bondAfter - bond, delta), charge / 20, "credited by the excess");
        vm.revertToState(s);

        Run memory under = _run(bond, charge - delta);
        assertTrue(under.won);
        assertLt(under.bondAfter, bond, "underpaying takes from the bond");
        emit log_named_decimal_uint("bond after underpaying by half the charge (ETH)", under.bondAfter, 18);
        assertLe(_diff(bond - under.bondAfter, delta), charge / 20, "charged the shortfall");
    }
}
