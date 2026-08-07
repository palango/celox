use alloy_primitives::{Address, U256};
use celo_alloy_rpc_types::CeloTransactionRequest;

use crate::{celo::FeeCurrency, gas};

/// CIP-64 workload: a 1-wei self-transfer paying gas in a fee currency, so the
/// whole alternative-fee path — directory lookup, debit, credit — is exercised
/// on every transaction.
pub struct Cip64 {
    fee_currency: Address,
    gas_limit: u64,
}

impl Cip64 {
    pub fn new(fee_currency: FeeCurrency) -> Self {
        Self {
            fee_currency: fee_currency.token,
            gas_limit: gas::cip64_transfer(fee_currency.intrinsic_gas),
        }
    }

    pub fn next(&self, sender: Address) -> CeloTransactionRequest {
        CeloTransactionRequest::default()
            .to(sender)
            .value(U256::from(1))
            .gas_limit(self.gas_limit)
            .fee_currency(self.fee_currency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_cip64_self_transfer_with_surcharged_gas() {
        let sender = Address::repeat_byte(0x42);
        let token = Address::repeat_byte(0xfe);
        let workload = Cip64::new(FeeCurrency {
            token,
            intrinsic_gas: 50_000,
            decimals: 18,
        });
        let request = workload.next(sender);

        assert!(request.is_cip64());
        assert_eq!(request.fee_currency, Some(token));
        assert_eq!(request.inner.as_ref().to, Some(sender.into()));
        assert_eq!(request.inner.as_ref().value, Some(U256::from(1)));
        assert_eq!(request.inner.as_ref().gas, Some(71_000));
    }
}
