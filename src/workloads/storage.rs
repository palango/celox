use alloy_primitives::{Address, U256};
use alloy_provider::DynProvider;
use alloy_sol_types::{SolCall, sol};
use celo_alloy_network::{Celo, TransactionBuilder};
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{Result, ensure};
use rand::RngExt;
use tracing::info;

use crate::{
    deploy::{self, Origin},
    gas,
};

sol! {
    #[sol(rpc)]
    StateWriter,
    "contracts/StateWriter.json"
}

/// Most `--contracts` the storage workload deploys or reuses.
pub const MAX_CONTRACTS: u64 = 1_000;

/// Storage workload: every transaction calls one of `--contracts` writer
/// contracts, bumping a configurable number of its storage slots. With one
/// contract all senders contend for a single storage trie (central state);
/// with many, each block touches many tries and account-trie paths, the spread
/// mainnet traffic has.
///
/// `key_space` decides what kind of pressure that is: a wide range spreads the
/// writes over fresh slots and grows the trie, a narrow one concentrates them
/// on hot slots that every sender rewrites.
pub struct Storage {
    contracts: Vec<Address>,
    writes: u64,
    key_space: u64,
}

impl Storage {
    /// Deploys, or reuses under `salt`, the `contracts` writers the senders
    /// share.
    pub async fn deploy(
        root: &DynProvider<Celo>,
        salt: u64,
        contracts: u64,
        writes: u64,
        key_space: u64,
    ) -> Result<Self> {
        let salts: Vec<_> = (0..contracts)
            .map(|index| deploy::salt(salt, index))
            .collect();
        let deployed = deploy::ensure_all(root, &salts, &StateWriter::BYTECODE).await?;
        for (contract, origin) in &deployed {
            match origin {
                Origin::Reused => info!(%contract, writes, key_space, "reusing state writer"),
                _ => info!(%contract, %origin, writes, key_space, "state writer deployed"),
            }
        }
        Ok(Self {
            contracts: deployed.into_iter().map(|(contract, _)| contract).collect(),
            writes,
            key_space,
        })
    }

    pub fn next(&self, _sender: Address) -> CeloTransactionRequest {
        // A random contract and start spread concurrent senders across the
        // tries and the key space instead of queueing them on one slot range;
        // `check` guarantees `writes <= key_space`, so every write stays inside.
        let mut rng = rand::rng();
        let contract = self.contracts[rng.random_range(0..self.contracts.len())];
        let start = rng.random_range(0..=self.key_space - self.writes);
        let payload = StateWriter::writeCall {
            start: U256::from(start),
            count: U256::from(self.writes),
        }
        .abi_encode();

        CeloTransactionRequest::default()
            .to(contract)
            .value(U256::ZERO)
            .gas_limit(gas::state_writes(&payload, self.writes))
            .with_input(payload)
    }
}

/// Validates the storage workload's flags, naming the ones that conflict.
pub fn check(writes: u64, key_space: u64, contracts: u64) -> Result<()> {
    ensure!(
        writes <= key_space,
        "--writes ({writes}) must not exceed --key-space ({key_space}): each transaction \
         writes a consecutive run of slots inside the key space"
    );
    ensure!(
        (1..=MAX_CONTRACTS).contains(&contracts),
        "--contracts must be between 1 and {MAX_CONTRACTS}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workload(writes: u64, key_space: u64) -> Storage {
        Storage {
            contracts: vec![Address::repeat_byte(0x5c)],
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
        assert_eq!(
            request.inner.as_ref().to,
            Some(workload.contracts[0].into())
        );
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

    #[test]
    fn picks_every_contract() {
        let contracts: Vec<Address> = (1..=3).map(Address::repeat_byte).collect();
        let workload = Storage {
            contracts: contracts.clone(),
            writes: 1,
            key_space: 100,
        };
        let seen: std::collections::HashSet<_> = (0..1_000)
            .map(|_| workload.next(Address::ZERO).inner.as_ref().to)
            .collect();
        for contract in contracts {
            assert!(
                seen.contains(&Some(contract.into())),
                "{contract} never picked"
            );
        }
    }

    #[test]
    fn start_stays_inside_the_range() {
        let full = workload(10, 10);
        for _ in 0..200 {
            assert_eq!(call_of(&full.next(Address::ZERO)).start, U256::ZERO);
        }
        let partial = workload(3, 10);
        for _ in 0..1_000 {
            assert!(call_of(&partial.next(Address::ZERO)).start <= U256::from(7));
        }
    }

    #[test]
    fn rejects_more_writes_than_the_key_space() {
        let message = check(6, 5, 1).unwrap_err().to_string();
        assert!(
            message.contains("--writes") && message.contains("--key-space"),
            "{message}"
        );
        assert!(check(5, 5, 1).is_ok());
    }

    #[test]
    fn bounds_the_contract_count() {
        assert!(
            check(1, 1, 0)
                .unwrap_err()
                .to_string()
                .contains("--contracts")
        );
        assert!(
            check(1, 1, 1_001)
                .unwrap_err()
                .to_string()
                .contains("--contracts")
        );
        assert!(check(1, 1, 1_000).is_ok());
    }
}
