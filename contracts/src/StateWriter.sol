// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// Storage-write load generator.
///
/// Every sender writes into this one contract's storage, so the writes contend
/// for a single account's storage trie — the property a load test wants from a
/// "central state" workload, without a DEX's moving parts. The caller chooses
/// how many slots a transaction touches and where they sit, so the same
/// contract covers both state growth (a wide key range, mostly fresh slots)
/// and hot-slot contention (a narrow one).
contract StateWriter {
    mapping(uint256 => uint256) public slots;

    /// Bumps `count` consecutive slots starting at `start`.
    ///
    /// Slots are incremented rather than assigned so that a write is always a
    /// real state change, even when two transactions land on the same slot in
    /// the same block.
    function write(uint256 start, uint256 count) external {
        for (uint256 i = 0; i < count; ++i) {
            unchecked {
                slots[start + i] += 1;
            }
        }
    }
}
