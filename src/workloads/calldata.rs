use alloy_primitives::{Address, U256};
use celo_alloy_network::TransactionBuilder;
use celo_alloy_rpc_types::CeloTransactionRequest;
use rand::Rng;

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
            .gas_limit(intrinsic_gas(&payload))
            .with_input(payload)
    }
}

/// Intrinsic gas of a calldata-carrying transaction to an EOA.
///
/// Since EIP-7623 (Prague) a transaction costs the larger of the standard
/// price, `21_000 + 4 * tokens`, and the calldata floor, `21_000 + 10 * tokens`,
/// where `tokens = zero_bytes + 4 * non_zero_bytes`. Calling an EOA adds no
/// execution gas on top of the standard price, so the floor always wins and is
/// the exact cost of the transaction.
fn intrinsic_gas(payload: &[u8]) -> u64 {
    let zero_bytes = payload.iter().filter(|byte| **byte == 0).count() as u64;
    let non_zero_bytes = payload.len() as u64 - zero_bytes;
    let tokens = zero_bytes + 4 * non_zero_bytes;
    21_000 + 10 * tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_calldata_at_the_eip7623_floor() {
        // Empty calldata is a plain transfer.
        assert_eq!(intrinsic_gas(&[]), 21_000);
        // 100 zero bytes: 100 tokens at 10 gas.
        assert_eq!(intrinsic_gas(&[0x00; 100]), 22_000);
        // 100 non-zero bytes: 400 tokens at 10 gas.
        assert_eq!(intrinsic_gas(&[0xff; 100]), 25_000);
        // Two zero and two non-zero bytes: 2 + 8 = 10 tokens.
        assert_eq!(intrinsic_gas(&[0x00, 0x01, 0x00, 0x02]), 21_100);
    }

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
        assert_eq!(request.inner.as_ref().gas, Some(intrinsic_gas(payload)));
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
