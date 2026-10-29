//! Gas limits for the transaction shapes celox sends.
//!
//! Every workload presets its gas limit from one of these constructors so no
//! transaction ever needs `eth_estimateGas`. The pure-transfer and calldata
//! shapes are exact protocol prices; shapes that run contract code carry an
//! upper bound on execution, since unused gas is refunded but an underestimate
//! is an out-of-gas revert.

/// A plain native transfer with empty calldata.
pub const TRANSFER: u64 = 21_000;

/// Calldata tokens under EIP-7623: a zero byte counts once, a non-zero byte
/// four times.
fn tokens(payload: &[u8]) -> u64 {
    let zero_bytes = payload.iter().filter(|byte| **byte == 0).count() as u64;
    let non_zero_bytes = payload.len() as u64 - zero_bytes;
    zero_bytes + 4 * non_zero_bytes
}

/// Intrinsic gas of a transaction carrying `payload`, plus `execution` gas for
/// the code it runs.
///
/// Since EIP-7623 (Prague) a transaction costs the larger of the standard
/// price, `21_000 + 4 * tokens + execution`, and the calldata floor,
/// `21_000 + 10 * tokens`. The floor wins only when execution is cheap enough
/// that the transaction is mostly paying to publish data.
pub fn contract_call(payload: &[u8], execution: u64) -> u64 {
    let tokens = tokens(payload);
    let standard = TRANSFER + 4 * tokens + execution;
    let floor = TRANSFER + 10 * tokens;
    standard.max(floor)
}

/// A calldata-carrying transaction to an EOA. Calling an EOA runs no code, so
/// the calldata floor always wins and is the exact cost of the transaction.
pub fn calldata_to_eoa(payload: &[u8]) -> u64 {
    contract_call(payload, 0)
}

/// A CIP-64 native transfer paying fees in a fee currency.
///
/// CIP-64 adds a per-currency intrinsic surcharge covering the fee token's
/// debit/credit calls; it is set in the FeeCurrencyDirectory and must be read
/// from there, never hardcoded (mainnet ranges from 50k for Mento stables to
/// 128k for the USDC adapter).
pub fn cip64_transfer(currency_intrinsic: u64) -> u64 {
    TRANSFER + currency_intrinsic
}

/// Upper bound on executing `transfer(address,uint256)` on an ERC-20.
///
/// Worst case is a recipient whose balance slot is still zero: 20_000 to set it
/// plus 2_100 for touching it cold, the sender's own slot, a Transfer event and
/// dispatch. A fresh-recipient transfer measured about 25_000 of execution both
/// on cUSD (mainnet state) and on the celo-reth dev node's fee currency. Fee
/// currencies are often proxies, which add a cold delegatecall and an
/// implementation-slot load on top, so this keeps roughly four times the
/// measured cost. Unused gas is refunded; an underestimate is an out-of-gas
/// revert, which celox counts as a failed transaction.
const ERC20_TRANSFER_EXECUTION: u64 = 100_000;

/// An ERC-20 transaction carrying the `transfer` payload.
pub fn erc20_transfer(payload: &[u8]) -> u64 {
    contract_call(payload, ERC20_TRANSFER_EXECUTION)
}

/// A CIP-64 ERC-20 transfer: the fee-currency surcharge on top of an ERC-20
/// `transfer`.
pub fn cip64_erc20_transfer(currency_intrinsic: u64, payload: &[u8]) -> u64 {
    erc20_transfer(payload) + currency_intrinsic
}

/// Upper bound for one storage slot written by `StateWriter.write`.
///
/// A slot that is still zero costs the most: 20_000 to set it plus 2_100 for
/// touching it cold, and the loop around it adds a few hundred. Slots written
/// by an earlier transaction cost about 5_000 instead, so this over-provisions
/// once a key range has been filled — safe, because the difference is refunded.
const STATE_WRITE: u64 = 23_000;

/// A `StateWriter.write` call that bumps `writes` slots.
pub fn state_writes(payload: &[u8], writes: u64) -> u64 {
    // Dispatch and the mapping-slot hashing outside the loop.
    const CALL_OVERHEAD: u64 = 5_000;
    contract_call(payload, CALL_OVERHEAD + writes * STATE_WRITE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_calldata_to_an_eoa_at_the_eip7623_floor() {
        // Empty calldata is a plain transfer.
        assert_eq!(calldata_to_eoa(&[]), 21_000);
        // 100 zero bytes: 100 tokens at 10 gas.
        assert_eq!(calldata_to_eoa(&[0x00; 100]), 22_000);
        // 100 non-zero bytes: 400 tokens at 10 gas.
        assert_eq!(calldata_to_eoa(&[0xff; 100]), 25_000);
        // Two zero and two non-zero bytes: 2 + 8 = 10 tokens.
        assert_eq!(calldata_to_eoa(&[0x00, 0x01, 0x00, 0x02]), 21_100);
    }

    #[test]
    fn execution_gas_lifts_a_call_above_the_floor() {
        let payload = [0xff; 100]; // 400 tokens: floor 25_000, standard 22_600 + execution
        assert_eq!(contract_call(&payload, 0), 25_000);
        // Execution below the floor's headroom still prices at the floor.
        assert_eq!(contract_call(&payload, 1_000), 25_000);
        // Above it, the standard price wins.
        assert_eq!(contract_call(&payload, 10_000), 32_600);
    }

    #[test]
    fn prices_cip64_as_transfer_plus_surcharge() {
        assert_eq!(cip64_transfer(50_000), 71_000);
    }

    #[test]
    fn prices_an_erc20_transfer_from_its_payload() {
        // Selector, address word, amount word: execution dominates the floor,
        // so the standard price applies.
        let payload = [0xff; 68];
        assert_eq!(
            erc20_transfer(&payload),
            21_000 + 4 * 272 + ERC20_TRANSFER_EXECUTION
        );
        // Zero bytes in the address word make the same call cheaper.
        let mut sparse = payload;
        sparse[4..36].fill(0);
        assert_eq!(
            erc20_transfer(&payload) - erc20_transfer(&sparse),
            4 * 3 * 32
        );
        // CIP-64 adds the currency's surcharge and nothing else.
        assert_eq!(
            cip64_erc20_transfer(50_000, &payload),
            erc20_transfer(&payload) + 50_000
        );
    }

    #[test]
    fn state_writes_scale_with_the_slot_count() {
        let payload = [0x00; 68];
        let one = state_writes(&payload, 1);
        let two = state_writes(&payload, 2);
        assert_eq!(two - one, STATE_WRITE);
        assert!(one > TRANSFER + STATE_WRITE);
    }
}
