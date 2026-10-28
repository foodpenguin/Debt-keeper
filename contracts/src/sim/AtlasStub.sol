// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

/// @notice Simulation only (injected by state override, never deployed): stands in for the Atlas
///         escrow when a solver operation is simulated outside a real metacall. Reports no
///         liability and accepts any reconciliation, so the simulation measures the solver's own
///         economics (liquidations, bid swap, bid) with Atlas's gas accounting left out.
contract AtlasStub {
    function shortfall() external pure returns (uint256 gasLiability, uint256 borrowLiability) {
        return (0, 0);
    }

    function reconcile(uint256) external payable returns (uint256 owed) {
        return 0;
    }
}
