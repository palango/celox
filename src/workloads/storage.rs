use std::sync::atomic::{AtomicUsize, Ordering};

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_provider::{DynProvider, Provider};
use alloy_sol_types::{SolCall, SolValue, sol};
use celo_alloy_network::{Celo, TransactionBuilder};
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{Result, bail, ensure};
use futures::{StreamExt, TryStreamExt, stream};
use rand::RngExt;
use tracing::info;

use super::Config;
use crate::{
    deploy::{self, CREATE2_DEPLOYER, Origin, Root},
    gas,
    lane::RPC_TIMEOUT,
};

sol! {
    #[sol(rpc)]
    StateWriter,
    "contracts/StateWriter.json"
}

/// Most `--contracts` the storage workload deploys or reuses.
pub const MAX_CONTRACTS: u64 = 1_000;

/// Storage reads in flight while a prefill scans for its pending chunks.
const SCAN_CONCURRENCY: usize = 32;

/// Storage workload: every transaction calls one of `--contracts` writer
/// contracts, bumping a configurable number of its storage slots. With one
/// contract all senders contend for a single storage trie (central state);
/// with many, each block touches many tries and account-trie paths, the spread
/// mainnet traffic has.
///
/// `key_space` decides what kind of pressure that is: a wide range spreads the
/// writes over fresh slots and grows the trie, a narrow one concentrates them
/// on hot slots that every sender rewrites.
///
/// With `--prefill` the workload instead writes every slot of the key range
/// once, chunk by chunk, so later runs with the same `--salt` update existing
/// slots in a large trie rather than create fresh ones.
pub struct Storage {
    contracts: Vec<Address>,
    writes: u64,
    key_space: u64,
    prefill: Option<Prefill>,
}

/// The chunks a prefill still has to write, handed out once each in order.
struct Prefill {
    /// (contract index, start) of each chunk whose probe slot is still zero.
    pending: Vec<(usize, u64)>,
    cursor: AtomicUsize,
}

impl Prefill {
    fn new(pending: Vec<(usize, u64)>) -> Self {
        Self {
            pending,
            cursor: AtomicUsize::new(0),
        }
    }

    /// The next chunk; past the end, the last one again (re-incrementing a
    /// written chunk is harmless). Senders stop shortly after exhaustion.
    fn next(&self) -> (usize, u64) {
        let index = self.cursor.fetch_add(1, Ordering::Relaxed);
        self.pending
            .get(index)
            .or(self.pending.last())
            .copied()
            .unwrap_or_default()
    }

    fn remaining(&self) -> usize {
        self.pending
            .len()
            .saturating_sub(self.cursor.load(Ordering::Relaxed))
    }
}

impl Storage {
    /// Deploys, or reuses under `--salt`, the writer contracts the senders
    /// share. A prefill then finds the chunks still to write; a measured run
    /// reports whether its key range was prefilled.
    pub async fn deploy(root: &Root<'_>, config: &Config) -> Result<Self> {
        let Config {
            salt,
            contracts,
            writes,
            key_space,
            prefill,
            ..
        } = *config;
        if prefill && !deploy::has_code(root.provider, CREATE2_DEPLOYER).await? {
            bail!(
                "--prefill needs the CREATE2 deployer at {CREATE2_DEPLOYER}, which this chain \
                 lacks: without it no later run could find the prefilled contracts"
            );
        }
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
        let mut storage = Self {
            contracts: deployed.into_iter().map(|(contract, _)| contract).collect(),
            writes,
            key_space,
            prefill: None,
        };
        if prefill {
            let pending = storage.pending_chunks(root.provider).await?;
            let total = chunk_starts(key_space, writes).len() * storage.contracts.len();
            info!("prefill: {} of {total} chunks pending", pending.len());
            storage.prefill = Some(Prefill::new(pending));
        } else {
            storage.report_sentinel(root.provider).await?;
        }
        Ok(storage)
    }

    /// The (contract index, start) chunks whose probe slot still reads zero.
    /// Chunks are ordered start-major, so consecutive transactions land on
    /// different contracts.
    async fn pending_chunks(&self, provider: &DynProvider<Celo>) -> Result<Vec<(usize, u64)>> {
        let chunks: Vec<(usize, u64)> = chunk_starts(self.key_space, self.writes)
            .into_iter()
            .flat_map(|start| (0..self.contracts.len()).map(move |index| (index, start)))
            .collect();
        let written: Vec<bool> = stream::iter(chunks.iter().map(|&(index, start)| {
            self.is_written(
                provider,
                self.contracts[index],
                probe_slot(self.key_space, self.writes, start),
            )
        }))
        .buffered(SCAN_CONCURRENCY)
        .try_collect()
        .await?;
        Ok(chunks
            .into_iter()
            .zip(written)
            .filter_map(|(chunk, written)| (!written).then_some(chunk))
            .collect())
    }

    /// Whether `slots[slot]` of `contract` is non-zero.
    async fn is_written(
        &self,
        provider: &DynProvider<Celo>,
        contract: Address,
        slot: u64,
    ) -> Result<bool> {
        let key = U256::from_be_bytes(slot_key(slot).0);
        let value = tokio::time::timeout(
            RPC_TIMEOUT,
            provider.get_storage_at(contract, key).into_future(),
        )
        .await??;
        Ok(!value.is_zero())
    }

    /// Logs whether a measured run's writes will update existing slots or
    /// create fresh ones, judged by the range's very last slot.
    async fn report_sentinel(&self, provider: &DynProvider<Celo>) -> Result<()> {
        let last = *self.contracts.last().expect("at least one contract");
        if self.is_written(provider, last, self.key_space - 1).await? {
            info!("key range prefilled: writes update existing slots (~5k gas each)");
        } else {
            info!("not prefilled: writes create fresh slots (~22k gas each)");
        }
        Ok(())
    }

    /// Whether a prefill has handed out every pending chunk. A measured run
    /// never is.
    pub fn is_exhausted(&self) -> bool {
        self.prefill
            .as_ref()
            .is_some_and(|prefill| prefill.remaining() == 0)
    }

    /// Chunks a prefill has not handed out yet, or `None` for a measured run.
    pub fn prefill_remaining(&self) -> Option<usize> {
        self.prefill.as_ref().map(Prefill::remaining)
    }

    pub fn next(&self, _sender: Address) -> CeloTransactionRequest {
        let (contract, start) = match &self.prefill {
            Some(prefill) => {
                let (index, start) = prefill.next();
                (self.contracts[index], start)
            }
            None => self.random_target(),
        };
        self.request(contract, start)
    }

    /// A representative transaction that does not advance a prefill.
    pub fn sample(&self) -> CeloTransactionRequest {
        let (contract, start) = self.random_target();
        self.request(contract, start)
    }

    fn random_target(&self) -> (Address, u64) {
        // A random contract and start spread concurrent senders across the
        // tries and the key space instead of queueing them on one slot range;
        // `check` guarantees `writes <= key_space`, so every write stays inside.
        let mut rng = rand::rng();
        let contract = self.contracts[rng.random_range(0..self.contracts.len())];
        (contract, rng.random_range(0..=self.key_space - self.writes))
    }

    fn request(&self, contract: Address, start: u64) -> CeloTransactionRequest {
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

/// Starts of the `writes`-slot chunks covering `0..key_space`, the last one
/// clamped to `key_space − writes` so no write passes the range.
fn chunk_starts(key_space: u64, writes: u64) -> Vec<u64> {
    (0..key_space)
        .step_by(writes as usize)
        .map(|start| start.min(key_space - writes))
        .collect()
}

/// The slot whose value says whether the chunk at `start` was written: one no
/// other chunk writes. Every chunk but the last starts below the clamped last
/// chunk, so its first slot is its own; the last chunk owns the range's final
/// slot. (A chunk's last slot would not do: the clamped last chunk overlaps the
/// one before it whenever `writes` does not divide `key_space`.)
fn probe_slot(key_space: u64, writes: u64, start: u64) -> u64 {
    if start == key_space - writes {
        key_space - 1
    } else {
        start
    }
}

/// Storage key of `slots[slot]` in `StateWriter`, whose mapping sits at slot 0.
fn slot_key(slot: u64) -> B256 {
    keccak256((U256::from(slot), U256::ZERO).abi_encode())
}

/// Validates the storage workload's flags, naming the ones that conflict.
pub fn check(
    writes: u64,
    key_space: u64,
    contracts: u64,
    prefill: bool,
    salt: Option<u64>,
) -> Result<()> {
    ensure!(
        writes <= key_space,
        "--writes ({writes}) must not exceed --key-space ({key_space}): each transaction \
         writes a consecutive run of slots inside the key space"
    );
    ensure!(
        (1..=MAX_CONTRACTS).contains(&contracts),
        "--contracts must be between 1 and {MAX_CONTRACTS}"
    );
    ensure!(
        !prefill || salt.is_some(),
        "--prefill needs --salt: the measured runs that follow must pass the same salt to \
         find the prefilled contracts"
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
            prefill: None,
        }
    }

    fn prefilling(pending: Vec<(usize, u64)>) -> Storage {
        Storage {
            contracts: vec![Address::repeat_byte(1), Address::repeat_byte(2)],
            writes: 5,
            key_space: 50,
            prefill: Some(Prefill::new(pending)),
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
            prefill: None,
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
        let message = check(6, 5, 1, false, None).unwrap_err().to_string();
        assert!(
            message.contains("--writes") && message.contains("--key-space"),
            "{message}"
        );
        assert!(check(5, 5, 1, false, None).is_ok());
    }

    #[test]
    fn bounds_the_contract_count() {
        assert!(
            check(1, 1, 0, false, None)
                .unwrap_err()
                .to_string()
                .contains("--contracts")
        );
        assert!(
            check(1, 1, 1_001, false, None)
                .unwrap_err()
                .to_string()
                .contains("--contracts")
        );
        assert!(check(1, 1, 1_000, false, None).is_ok());
    }

    #[test]
    fn chunks_cover_the_range_exactly() {
        // The last chunk is clamped so slot 11 is written and nothing passes 12.
        assert_eq!(chunk_starts(12, 5), vec![0, 5, 7]);
        assert_eq!(chunk_starts(10, 5), vec![0, 5]);
        assert_eq!(chunk_starts(5, 5), vec![0]);
    }

    #[test]
    fn exhausts_after_the_last_pending_chunk() {
        let workload = prefilling(vec![(1, 10), (0, 45)]);
        let first = workload.next(Address::ZERO);
        assert!(!workload.is_exhausted());
        let second = workload.next(Address::ZERO);
        assert!(workload.is_exhausted());

        assert_eq!(
            first.inner.as_ref().to,
            Some(Address::repeat_byte(2).into())
        );
        assert_eq!(call_of(&first).start, U256::from(10));
        assert_eq!(
            second.inner.as_ref().to,
            Some(Address::repeat_byte(1).into())
        );
        assert_eq!(call_of(&second).start, U256::from(45));
        // Requests after exhaustion re-issue the last chunk (harmless increments).
        assert_eq!(call_of(&workload.next(Address::ZERO)).start, U256::from(45));
    }

    #[test]
    fn sample_does_not_consume_a_chunk() {
        let workload = prefilling(vec![(0, 0)]);
        assert_eq!(call_of(&workload.sample()).count, U256::from(5));
        assert!(!workload.is_exhausted());
        workload.next(Address::ZERO);
        assert!(workload.is_exhausted());
        assert!(!prefilling(vec![(0, 0)]).is_exhausted());
    }

    #[test]
    fn a_measured_run_is_never_exhausted() {
        let workload = workload(1, 10);
        workload.next(Address::ZERO);
        assert!(!workload.is_exhausted());
    }

    #[test]
    fn prefill_requires_a_salt() {
        let message = check(5, 50, 3, true, None).unwrap_err().to_string();
        assert!(
            message.contains("--prefill") && message.contains("--salt"),
            "{message}"
        );
        assert!(check(5, 50, 3, true, Some(7)).is_ok());
    }

    #[test]
    fn slot_keys_match_solidity_mapping_layout() {
        // `cast index uint256 0 0`: slots[0] of a mapping at storage slot 0.
        assert_eq!(
            slot_key(0),
            alloy_primitives::b256!(
                "ad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5"
            )
        );
    }

    /// A chunk counts as written when its probe slot is non-zero, so that slot
    /// must belong to no other chunk: otherwise a lost chunk whose probe an
    /// overlapping (clamped last) chunk wrote would never be resent.
    #[test]
    fn probe_slots_belong_to_one_chunk() {
        for (key_space, writes) in [(12, 5), (52, 5), (50, 5), (5, 5), (100_000, 500), (7, 3)] {
            let starts = chunk_starts(key_space, writes);
            for &start in &starts {
                let probe = probe_slot(key_space, writes, start);
                let owners = starts
                    .iter()
                    .filter(|&&other| (other..other + writes).contains(&probe))
                    .count();
                assert_eq!(
                    owners, 1,
                    "probe {probe} of chunk {start} in ({key_space}, {writes})"
                );
                assert!((start..start + writes).contains(&probe));
            }
        }
    }
}
