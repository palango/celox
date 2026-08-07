use alloy_primitives::{Address, U256};
use celo_alloy_rpc_types::CeloTransactionRequest;

/// Baseline workload: a 1-wei self-transfer, so senders only ever burn gas.
pub struct Transfer;

impl Transfer {
    pub fn next(&self, sender: Address) -> CeloTransactionRequest {
        CeloTransactionRequest::default()
            .to(sender)
            .value(U256::from(1))
            .gas_limit(21_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_plain_native_transfer() {
        let sender = Address::repeat_byte(0x42);
        let request = Transfer.next(sender);

        assert!(!request.is_cip64());
        assert_eq!(request.inner.as_ref().to, Some(sender.into()));
        assert_eq!(request.inner.as_ref().value, Some(U256::from(1)));
        assert_eq!(request.inner.as_ref().gas, Some(21_000));
    }
}
