use alloy_primitives::{Address, U256};
use celo_alloy_network::TransactionBuilder;
use celo_alloy_rpc_types::CeloTransactionRequest;
use rand::Rng;

use crate::gas;

/// Recipient of every calldata transaction. The address is arbitrary — any
/// keyless EOA does, and one nobody spends from keeps the payload inert. It is
/// deliberately not the zero address, which node tooling and explorers treat as
/// a burn address and which some clients reject as a `to` field.
const BURNER: Address = Address::new([0xca; 20]);

/// Calldata workload: random payload of a fixed size to a burner EOA, so each
/// transaction pays for its calldata without executing any code.
pub struct Calldata {
    data_size: usize,
}

impl Calldata {
    pub fn new(data_size: usize) -> Self {
        Self { data_size }
    }

    pub fn next(&self, _sender: Address) -> CeloTransactionRequest {
        let mut payload = vec![0u8; self.data_size];
        rand::rng().fill_bytes(&mut payload);

        CeloTransactionRequest::default()
            .to(BURNER)
            .value(U256::ZERO)
            .gas_limit(gas::calldata_to_eoa(&payload))
            .with_input(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_random_calldata_to_the_burner() {
        let sender = Address::repeat_byte(0x42);
        let request = Calldata::new(2048).next(sender);

        assert!(!request.is_cip64());
        assert_eq!(request.inner.as_ref().to, Some(BURNER.into()));
        assert_eq!(request.inner.as_ref().value, Some(U256::ZERO));

        // The payload is random per call, so price the request's own bytes.
        let payload = payload_of(&request);
        assert_eq!(payload.len(), 2048);
        assert_eq!(
            request.inner.as_ref().gas,
            Some(gas::calldata_to_eoa(payload))
        );
    }

    #[test]
    fn draws_a_fresh_payload_per_call() {
        let workload = Calldata::new(32);
        let sender = Address::repeat_byte(0x42);

        let first = workload.next(sender);
        let second = workload.next(sender);
        assert_ne!(payload_of(&first), payload_of(&second));
    }

    fn payload_of(request: &CeloTransactionRequest) -> &[u8] {
        request
            .inner
            .as_ref()
            .input
            .input
            .as_ref()
            .expect("input is set")
    }
}
