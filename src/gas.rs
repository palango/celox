//! Intrinsic gas of the transaction shapes celox sends.
//!
//! Every workload presets its gas limit from one of these constructors so no
//! transaction ever needs `eth_estimateGas`; each constructor is the exact
//! protocol price of its shape, not an estimate with padding.

/// A plain native transfer with empty calldata.
pub const TRANSFER: u64 = 21_000;

/// A calldata-carrying transaction to an EOA.
///
/// Since EIP-7623 (Prague) a transaction costs the larger of the standard
/// price, `21_000 + 4 * tokens`, and the calldata floor, `21_000 + 10 * tokens`,
/// where `tokens = zero_bytes + 4 * non_zero_bytes`. Calling an EOA adds no
/// execution gas on top of the standard price, so the floor always wins and is
/// the exact cost of the transaction.
pub fn calldata_to_eoa(payload: &[u8]) -> u64 {
    let zero_bytes = payload.iter().filter(|byte| **byte == 0).count() as u64;
    let non_zero_bytes = payload.len() as u64 - zero_bytes;
    let tokens = zero_bytes + 4 * non_zero_bytes;
    TRANSFER + 10 * tokens
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_calldata_at_the_eip7623_floor() {
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
    fn prices_cip64_as_transfer_plus_surcharge() {
        assert_eq!(cip64_transfer(50_000), 71_000);
    }
}
