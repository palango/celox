//! Transaction workloads.
//!
//! Adding a workload: new file in this directory, a variant on both enums
//! below, and an arm in `new`/`next`; a workload with a CLI knob also gets a
//! field on `Config`. `next` must set everything the workload knows (to, value,
//! input, gas limit); the sender loop fills in the account fields (from, nonce,
//! fees).
//!
//! Chain facts that another subsystem also needs (the cip64 fee currency: the
//! funder's token leg depends on it) are resolved once by main and passed in
//! `Config`. Facts only the workload itself needs (the address of a contract it
//! deploys) are resolved by the workload while `new` builds it, so that by the
//! time a workload exists it is ready to produce transactions.

mod calldata;
mod cip64;
pub mod cold_read;
pub mod storage;
pub(crate) mod transfer;

use alloy_primitives::Address;
use alloy_provider::DynProvider;
use celo_alloy_network::Celo;
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{OptionExt, Result};

pub use calldata::Calldata;
pub use cip64::Cip64;
pub use cold_read::ColdRead;
pub use storage::Storage;
pub use transfer::{Recipient, Transfer};

use crate::celo::FeeCurrency;

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum WorkloadKind {
    /// 1-wei native transfers; --recipient picks self-transfers or fresh accounts
    Transfer,
    /// Random calldata to a burner account
    Calldata,
    /// Gas paid in a fee currency (CIP-64); --recipient picks a 1-wei native
    /// self-transfer or a 1-wei transfer of the fee currency to a fresh address
    Cip64,
    /// Storage writes across --contracts writer contracts; --prefill fills
    /// their key range once for later runs to update
    Storage,
    /// Reads of random, never-repeated keys: native balances, or
    /// --read-token balances, --reads per transaction
    ColdRead,
}

/// Workload settings from the CLI. Each workload reads only the fields it needs.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub recipient: Recipient,
    pub data_size: usize,
    pub fee_currency: Option<FeeCurrency>,
    pub writes: u64,
    pub key_space: u64,
    /// Namespaces workload contracts' CREATE2 addresses (see `deploy`).
    pub salt: u64,
    /// Writer contracts the `storage` workload spreads its writes over.
    pub contracts: u64,
    /// Write the `storage` key range once instead of measuring (see `Storage`).
    pub prefill: bool,
    /// State reads per `cold-read` transaction.
    pub reads: u64,
    /// ERC-20 whose balances `cold-read` probes, instead of native balances.
    pub read_token: Option<Address>,
}

pub enum Workload {
    Transfer(Transfer),
    Calldata(Calldata),
    Cip64(Cip64),
    Storage(Storage),
    ColdRead(ColdRead),
}

impl Workload {
    /// Builds the workload, running any on-chain preparation it needs (a
    /// contract deploy) with the root account.
    pub async fn new(kind: WorkloadKind, config: Config, root: &DynProvider<Celo>) -> Result<Self> {
        Ok(match kind {
            WorkloadKind::Transfer => Self::Transfer(Transfer::new(config.recipient)),
            WorkloadKind::Calldata => Self::Calldata(Calldata::new(config.data_size)),
            WorkloadKind::Cip64 => Self::Cip64(Cip64::new(
                config
                    .fee_currency
                    .ok_or_eyre("the cip64 workload needs a resolved fee currency")?,
                config.recipient,
            )),
            WorkloadKind::Storage => Self::Storage(Storage::deploy(root, &config).await?),
            WorkloadKind::ColdRead => Self::ColdRead(
                ColdRead::new(root, config.salt, config.reads, config.read_token).await?,
            ),
        })
    }

    /// A representative transaction, for checking gas before funding. Unlike
    /// `next` it never consumes a prefill chunk.
    pub fn sample(&self, sender: Address) -> CeloTransactionRequest {
        match self {
            Self::Storage(workload) => workload.sample(),
            _ => self.next(sender),
        }
    }

    /// Whether the workload has nothing left to send: only a storage prefill
    /// that has handed out its last chunk.
    pub fn is_exhausted(&self) -> bool {
        matches!(self, Self::Storage(workload) if workload.is_exhausted())
    }

    /// Chunks a storage prefill has not handed out, or `None` for any other run.
    pub fn prefill_remaining(&self) -> Option<usize> {
        match self {
            Self::Storage(workload) => workload.prefill_remaining(),
            _ => None,
        }
    }

    /// Produces the next transaction for `sender`.
    pub fn next(&self, sender: Address) -> CeloTransactionRequest {
        match self {
            Self::Transfer(workload) => workload.next(sender),
            Self::Calldata(workload) => workload.next(sender),
            Self::Cip64(workload) => workload.next(sender),
            Self::Storage(workload) => workload.next(sender),
            Self::ColdRead(workload) => workload.next(sender),
        }
    }
}
