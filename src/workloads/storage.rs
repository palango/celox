use alloy_primitives::{Address, U256};
use alloy_provider::DynProvider;
use alloy_sol_types::{SolCall, sol};
use celo_alloy_network::{Celo, TransactionBuilder};
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::Result;
use rand::RngExt;
use tracing::info;

use crate::gas;

sol! {
    #[sol(rpc)]
    StateWriter,
    "contracts/StateWriter.json"
}

/// Storage workload: every transaction calls one shared contract, bumping a
/// configurable number of its storage slots. All senders write into the same
/// contract's storage trie, so the load contends for central state.
///
/// `key_space` decides what kind of pressure that is: a wide range spreads the
/// writes over fresh slots and grows the trie, a narrow one concentrates them
/// on hot slots that every sender rewrites.
pub struct Storage {
    contract: Address,
    writes: u64,
    key_space: u64,
}

impl Storage {
    /// Deploys the contract every sender will write to.
    pub async fn deploy(root: &DynProvider<Celo>, writes: u64, key_space: u64) -> Result<Self> {
        let contract = *StateWriter::deploy(root).await?.address();
        info!(%contract, writes, key_space, "state writer deployed");
        Ok(Self {
            contract,
            writes,
            key_space,
        })
    }

    pub fn next(&self, _sender: Address) -> CeloTransactionRequest {
        // A random start spreads concurrent senders across the key space
        // instead of queueing them all on one slot range.
        let start = rand::rng().random_range(0..self.key_space);
        let payload = StateWriter::writeCall {
            start: U256::from(start),
            count: U256::from(self.writes),
        }
        .abi_encode();

        CeloTransactionRequest::default()
            .to(self.contract)
            .value(U256::ZERO)
            .gas_limit(gas::state_writes(&payload, self.writes))
            .with_input(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workload(writes: u64, key_space: u64) -> Storage {
        Storage {
            contract: Address::repeat_byte(0x5c),
            writes,
            key_space,
        }
    }

    fn call_of(request: &CeloTransactionRequest) -> StateWriter::writeCall {
        let payload = request
            .inner
            .as_ref()
            .input
            .input
            .clone()
            .expect("input is set");
        StateWriter::writeCall::abi_decode(&payload).expect("decodes as write()")
    }

    #[test]
    fn calls_the_deployed_contract_with_the_configured_write_count() {
        let workload = workload(7, 1024);
        let request = workload.next(Address::repeat_byte(0x42));

        assert!(!request.is_cip64());
        assert_eq!(request.inner.as_ref().to, Some(workload.contract.into()));
        assert_eq!(request.inner.as_ref().value, Some(U256::ZERO));

        let call = call_of(&request);
        assert_eq!(call.count, U256::from(7));
        assert!(call.start < U256::from(1024));

        let payload = request.inner.as_ref().input.input.clone().unwrap();
        assert_eq!(
            request.inner.as_ref().gas,
            Some(gas::state_writes(&payload, 7))
        );
    }

    #[test]
    fn spreads_writes_across_the_key_space() {
        let workload = workload(1, u64::MAX);
        let first = call_of(&workload.next(Address::ZERO)).start;
        let second = call_of(&workload.next(Address::ZERO)).start;
        assert_ne!(first, second);
    }
}
