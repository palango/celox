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
mod storage;
mod transfer;

use alloy_primitives::Address;
use alloy_provider::DynProvider;
use celo_alloy_network::Celo;
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{OptionExt, Result};

pub use calldata::Calldata;
pub use cip64::Cip64;
pub use storage::Storage;
pub use transfer::Transfer;

use crate::celo::FeeCurrency;

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum WorkloadKind {
    /// 1-wei native self-transfers
    Transfer,
    /// Random calldata to a burner account
    Calldata,
    /// 1-wei self-transfers paying gas in a fee currency (CIP-64)
    Cip64,
    /// Storage writes against one shared contract
    Storage,
}

/// Workload settings from the CLI. Each workload reads only the fields it needs.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub data_size: usize,
    pub fee_currency: Option<FeeCurrency>,
    pub writes: u64,
    pub key_space: u64,
}

pub enum Workload {
    Transfer(Transfer),
    Calldata(Calldata),
    Cip64(Cip64),
    Storage(Storage),
}

impl Workload {
    /// Builds the workload, running any on-chain preparation it needs (a
    /// contract deploy) with the root account.
    pub async fn new(kind: WorkloadKind, config: Config, root: &DynProvider<Celo>) -> Result<Self> {
        Ok(match kind {
            WorkloadKind::Transfer => Self::Transfer(Transfer),
            WorkloadKind::Calldata => Self::Calldata(Calldata::new(config.data_size)),
            WorkloadKind::Cip64 => Self::Cip64(Cip64::new(
                config
                    .fee_currency
                    .ok_or_eyre("the cip64 workload needs a resolved fee currency")?,
            )),
            WorkloadKind::Storage => {
                Self::Storage(Storage::deploy(root, config.writes, config.key_space).await?)
            }
        })
    }

    /// Produces the next transaction for `sender`.
    pub fn next(&self, sender: Address) -> CeloTransactionRequest {
        match self {
            Self::Transfer(workload) => workload.next(sender),
            Self::Calldata(workload) => workload.next(sender),
            Self::Cip64(workload) => workload.next(sender),
            Self::Storage(workload) => workload.next(sender),
        }
    }
}
