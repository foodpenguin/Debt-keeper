// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

/// @notice Simulation only (injected by state override, never deployed): stands in for the Atlas
///         escrow when a solver operation is simulated outside a real metacall. Reports the gas
///         liability the simulation sets in storage slot 0 (`gasLiability`, the bot's estimate of what
///         Atlas would report; unset = 0) and no borrow, and accepts any reconciliation, keeping the
///         ETH paid. The simulated profit is then the solver's net after the bid AND the gas it pays
///         in ETH.
contract AtlasStub {
    /// Slot 0: set by the simulation's state override.
    uint256 public gasLiability;

    function shortfall() external view returns (uint256, uint256 borrowLiability) {
        return (gasLiability, 0);
    }

    function reconcile(uint256) external payable returns (uint256 owed) {
        return 0;
    }
}
