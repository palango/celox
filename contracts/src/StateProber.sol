// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IBalanceOf {
    function balanceOf(address account) external view returns (uint256);
}

/// Cold-read load generator.
///
/// Each call derives `count` pseudo-random addresses from `keccak256(seed, i)`
/// and reads state for each, so with a fresh seed per transaction every lookup
/// misses the node's caches and goes to its database. The reads are summed into
/// the return value so none of them can be optimised away.
contract StateProber {
    /// Reads the native balance of each derived address: a cold account
    /// access, a lookup in the full account table.
    function probeAccounts(uint256 seed, uint256 count) external view returns (uint256 acc) {
        for (uint256 i = 0; i < count; ++i) {
            acc += address(uint160(uint256(keccak256(abi.encode(seed, i))))).balance;
        }
    }

    /// Reads `token.balanceOf` of each derived address: a cold storage slot in
    /// a large contract's balance mapping.
    function probeToken(address token, uint256 seed, uint256 count) external view returns (uint256 acc) {
        for (uint256 i = 0; i < count; ++i) {
            acc += IBalanceOf(token).balanceOf(address(uint160(uint256(keccak256(abi.encode(seed, i))))));
        }
    }
}
