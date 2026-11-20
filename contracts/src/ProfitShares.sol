// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

/// @title ProfitShares
/// @notice Splits every payout between up to MAX_SHARES extra recipients, each at its own share in
///         basis points, and the primary PROFIT_RECEIVER, which gets the rest (rounding included).
/// @dev Paid out on the spot (no balance accrues): the contracts that inherit this keep nothing at
///      rest. Only the owner sets the recipients and shares; anyone may `distribute` a balance that
///      sits in the contract (e.g. tokens sent to it by mistake), which can only ever reach the
///      configured recipients, and never while the inheriting contract's own operation is running
///      (`_operating`), so a callee cannot reenter and pay out funds in flight.
///      A share that cannot be paid (a recipient that rejects ETH or is blocked by the token, or burns
///      more than SHARE_ETH_GAS) goes to PROFIT_RECEIVER instead (`ShareSkipped`): one bad recipient
///      must not make every liquidation revert. Only a failed payment to PROFIT_RECEIVER reverts.
abstract contract ProfitShares {
    /// @notice The primary recipient: everything the extra shares do not take. Immutable to keep an
    ///         SLOAD off the hot path; changing it means deploying a new contract.
    address public immutable PROFIT_RECEIVER;

    uint256 public constant MAX_SHARES = 4;
    uint256 private constant BPS = 10_000;
    /// @notice Gas an extra recipient's ETH transfer may use (a plain account or a smart wallet's
    ///         receive needs far less): a recipient cannot burn the operation's gas.
    uint256 public constant SHARE_ETH_GAS = 50_000;

    /// @dev One storage slot per recipient: address(20) | bps(2).
    struct Share {
        address to;
        uint16 bps;
    }

    Share[] private _shares;

    error BadShares();
    error ShareTransferFailed(address token, address to);
    error Busy();

    event SharesSet(address[] to, uint16[] bps);
    /// @notice A share of `amount` of `token` (address(0) = ETH) could not be paid to `to`: it went to
    ///         PROFIT_RECEIVER.
    event ShareSkipped(address indexed token, address indexed to, uint256 amount);
    /// @notice One payout of `amount` of `token` (address(0) = ETH), whoever it was split between.
    event ProfitPaid(address indexed token, uint256 amount);

    constructor(address profitReceiver_) {
        PROFIT_RECEIVER = profitReceiver_;
    }

    /// @notice The extra recipients and their shares (bps); PROFIT_RECEIVER gets 10000 - their sum.
    function shares() external view returns (address[] memory to, uint16[] memory bps) {
        uint256 n = _shares.length;
        to = new address[](n);
        bps = new uint16[](n);
        for (uint256 i; i < n; ++i) {
            (to[i], bps[i]) = (_shares[i].to, _shares[i].bps);
        }
    }

    /// @notice Pays this contract's whole balance of `token` out by the current shares. Anyone may
    ///         call it; the funds can only go to the configured recipients.
    function distribute(address token) external {
        if (_operating()) revert Busy();
        uint256 bal = _balanceOf(token, address(this));
        if (bal > 0) _payout(token, bal);
    }

    /// @notice Same as `distribute` for this contract's ETH balance.
    function distributeEth() external {
        if (_operating()) revert Busy();
        uint256 bal = address(this).balance;
        if (bal > 0) _payoutEth(bal);
    }

    /// @dev Replaces the extra recipients (empty = everything to PROFIT_RECEIVER). Each must be a
    ///      distinct non-zero address with a non-zero share; the shares may not exceed 10000 in total.
    function _setShares(address[] calldata to, uint16[] calldata bps) internal {
        if (to.length != bps.length || to.length > MAX_SHARES) revert BadShares();
        delete _shares;
        uint256 total;
        for (uint256 i; i < to.length; ++i) {
            if (to[i] == address(0) || bps[i] == 0) revert BadShares();
            for (uint256 j; j < i; ++j) {
                if (to[j] == to[i]) revert BadShares();
            }
            total += bps[i];
            _shares.push(Share({to: to[i], bps: bps[i]}));
        }
        if (total > BPS) revert BadShares();
        emit SharesSet(to, bps);
    }

    /// @dev Pays `amount` of `token` (held by this contract) out by the shares.
    function _payout(address token, uint256 amount) internal {
        uint256 rest = amount;
        uint256 n = _shares.length;
        for (uint256 i; i < n; ++i) {
            Share memory s = _shares[i];
            uint256 part = amount * s.bps / BPS;
            if (part == 0) continue;
            if (_transfer(token, s.to, part)) {
                rest -= part;
            } else {
                emit ShareSkipped(token, s.to, part);
            }
        }
        if (rest > 0 && !_transfer(token, PROFIT_RECEIVER, rest)) revert ShareTransferFailed(token, PROFIT_RECEIVER);
        emit ProfitPaid(token, amount);
    }

    /// @dev Pays `amount` of ETH (held by this contract) out by the shares.
    function _payoutEth(uint256 amount) internal {
        uint256 rest = amount;
        uint256 n = _shares.length;
        for (uint256 i; i < n; ++i) {
            Share memory s = _shares[i];
            uint256 part = amount * s.bps / BPS;
            if (part == 0) continue;
            (bool sent,) = s.to.call{value: part, gas: SHARE_ETH_GAS}("");
            if (sent) {
                rest -= part;
            } else {
                emit ShareSkipped(address(0), s.to, part);
            }
        }
        if (rest > 0) {
            (bool sent,) = PROFIT_RECEIVER.call{value: rest}("");
            if (!sent) revert ShareTransferFailed(address(0), PROFIT_RECEIVER);
        }
        emit ProfitPaid(address(0), amount);
    }

    /// @dev Whether the inheriting contract's own operation is running (distribute is refused then).
    function _operating() internal view virtual returns (bool);

    function _balanceOf(address token, address who) private view returns (uint256 bal) {
        (bool ok, bytes memory ret) = token.staticcall(abi.encodeWithSelector(0x70a08231, who));
        if (ok && ret.length >= 32) bal = abi.decode(ret, (uint256));
    }

    /// @dev ERC20 transfer tolerant of tokens that return no bool; false (never a revert) when it fails.
    function _transfer(address token, address to, uint256 amount) private returns (bool) {
        (bool ok, bytes memory ret) = token.call(abi.encodeWithSelector(0xa9059cbb, to, amount));
        return ok && (ret.length == 0 || (ret.length == 32 && abi.decode(ret, (bool)))) && token.code.length > 0;
    }
}
