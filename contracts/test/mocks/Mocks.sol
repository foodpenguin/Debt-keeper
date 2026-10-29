// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {MarketParams, IMorphoLiquidateCallback} from "../../src/interfaces/IMorpho.sol";
import {MorphoLiquidator, SwapStep} from "../../src/MorphoLiquidator.sol";

contract MockERC20 {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    /// Fee taken on every transfer, in bps (fee-on-transfer token when > 0).
    uint256 public feeBps;

    function setFee(uint256 bps) external {
        feeBps = bps;
    }

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _move(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 a = allowance[from][msg.sender];
        require(a >= amount, "allowance");
        allowance[from][msg.sender] = a - amount;
        _move(from, to, amount);
        return true;
    }

    function _move(address from, address to, uint256 amount) private {
        require(balanceOf[from] >= amount, "balance");
        balanceOf[from] -= amount;
        uint256 fee = amount * feeBps / 10_000;
        balanceOf[to] += amount - fee;
    }
}

/// Morpho stand-in with the same callback ordering: send collateral, call back, pull repayment.
contract MockMorpho {
    MockERC20 public immutable loan;
    MockERC20 public immutable collateral;
    /// repaid = seized * repayBps / 10_000 (e.g. 9_000 = 10% liquidation bonus at a 1:1 price)
    uint256 public repayBps = 9_000;

    constructor(MockERC20 loan_, MockERC20 collateral_) {
        loan = loan_;
        collateral = collateral_;
    }

    function liquidate(MarketParams memory, address, uint256 seized, uint256, bytes memory data)
        external
        returns (uint256, uint256)
    {
        uint256 repaid = seized * repayBps / 10_000;
        collateral.transfer(msg.sender, seized);
        IMorphoLiquidateCallback(msg.sender).onMorphoLiquidate(repaid, data);
        loan.transferFrom(msg.sender, address(this), repaid);
        return (seized, repaid);
    }
}

/// Honest 1:1 router.
contract MockRouter {
    function swap(MockERC20 tokenIn, MockERC20 tokenOut, uint256 amountIn) external {
        tokenIn.transferFrom(msg.sender, address(this), amountIn);
        tokenOut.transfer(msg.sender, amountIn);
    }
}

/// Takes the collateral and pays nothing back.
contract ThiefRouter {
    function swap(MockERC20 tokenIn, MockERC20, uint256 amountIn) external {
        tokenIn.transferFrom(msg.sender, address(this), amountIn);
    }
}

/// Tries to pull more than it was approved for.
contract GreedyRouter {
    function swap(MockERC20 tokenIn, MockERC20 tokenOut, uint256 amountIn) external {
        tokenIn.transferFrom(msg.sender, address(this), amountIn * 2);
        tokenOut.transfer(msg.sender, amountIn);
    }
}

/// Re-enters the liquidator during the swap.
contract ReentrantRouter {
    MorphoLiquidator public liq;
    MarketParams public mp;

    function arm(MorphoLiquidator liq_, MarketParams memory mp_) external {
        liq = liq_;
        mp = mp_;
    }

    function swap(MockERC20, MockERC20, uint256) external {
        liq.liquidate(mp, address(1), 1, 0, new SwapStep[](0), 0, block.number);
    }
}

/// Calls the callback directly while a liquidation is in flight.
contract CallbackForger {
    function swap(MockERC20, MockERC20, uint256) external {
        MorphoLiquidator(msg.sender).onMorphoLiquidate(0, abi.encode(address(0), new SwapStep[](0)));
    }
}

interface IV3CallbackLike {
    function uniswapV3SwapCallback(int256, int256, bytes calldata) external;
}

/// V3-pool lookalike: token0 = collateral (sold), token1 = loan (bought), 1:1 price.
/// `mode` 0 = honest, 1 = asks for 2x the input, 2 = calls back twice, 3 = pays nothing out,
/// 4 = reports the output with the wrong (positive) sign, 5 = runs out of liquidity and fills only
/// 95% of the input.
contract MockV3Pool {
    MockERC20 public immutable token0;
    MockERC20 public immutable token1;
    uint8 public mode;

    constructor(MockERC20 t0, MockERC20 t1) {
        token0 = t0;
        token1 = t1;
    }

    function setMode(uint8 m) external {
        mode = m;
    }

    function swap(address recipient, bool, int256 amountSpecified, uint160, bytes calldata)
        external
        returns (int256, int256)
    {
        uint256 amountIn = uint256(amountSpecified);
        if (mode == 5) amountIn = amountIn * 95 / 100;
        if (mode != 3) token1.transfer(recipient, amountIn);
        int256 ask = mode == 1 ? int256(amountIn * 2) : int256(amountIn);
        IV3CallbackLike(msg.sender).uniswapV3SwapCallback(ask, -int256(amountIn), "");
        if (mode == 2) IV3CallbackLike(msg.sender).uniswapV3SwapCallback(ask, -int256(amountIn), "");
        return (ask, mode == 4 ? int256(amountIn) : -int256(amountIn));
    }
}

/// Fake pool that reports collateral as token0 when the hop starts, then switches token0 to the
/// loan token inside its swap, hoping to be paid in what the contract has just received.
contract TokenSwitchingPool {
    MockERC20 public immutable collateral;
    MockERC20 public immutable loan;
    bool private switched;

    constructor(MockERC20 c, MockERC20 l) {
        collateral = c;
        loan = l;
    }

    function token0() external view returns (address) {
        return switched ? address(loan) : address(collateral);
    }

    function token1() external view returns (address) {
        return address(loan);
    }

    function swap(address recipient, bool, int256 amountSpecified, uint160, bytes calldata)
        external
        returns (int256, int256)
    {
        uint256 amountIn = uint256(amountSpecified);
        loan.transfer(recipient, amountIn);
        switched = true;
        IV3CallbackLike(msg.sender).uniswapV3SwapCallback(int256(amountIn), -int256(amountIn), "");
        switched = false;
        return (int256(amountIn), -int256(amountIn));
    }
}
