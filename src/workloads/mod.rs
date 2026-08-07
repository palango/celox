//! Transaction workloads.
//!
//! Adding a workload: new file in this directory, a variant on both enums
//! below, and arms in `setup`/`next`; a workload with a CLI knob also gets a
//! field on `Config`. `next` must set everything the workload knows (to, value,
//! input, gas limit); the sender loop fills in the account fields (from, nonce,
//! fees, chain id).

mod calldata;
mod transfer;

use alloy_primitives::Address;
use alloy_provider::DynProvider;
use celo_alloy_network::Celo;
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::Result;

pub use calldata::Calldata;
pub use transfer::Transfer;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum WorkloadKind {
    /// 1-wei native self-transfers
    Transfer,
    /// Random calldata to a burner account
    Calldata,
}

/// Workload settings from the CLI. Each workload reads only the fields it needs.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub data_size: usize,
}

pub enum Workload {
    Transfer(Transfer),
    Calldata(Calldata),
}

impl Workload {
    pub fn new(kind: WorkloadKind, config: Config) -> Self {
        match kind {
            WorkloadKind::Transfer => Self::Transfer(Transfer),
            WorkloadKind::Calldata => Self::Calldata(Calldata::new(config.data_size)),
        }
    }

    /// One-time on-chain preparation (contract deploys), run with the root account.
    pub async fn setup(&mut self, _root: &DynProvider<Celo>) -> Result<()> {
        match self {
            Self::Transfer(_) | Self::Calldata(_) => Ok(()),
        }
    }

    /// Produces the next transaction for `sender`.
    pub fn next(&self, sender: Address) -> CeloTransactionRequest {
        match self {
            Self::Transfer(workload) => workload.next(sender),
            Self::Calldata(workload) => workload.next(sender),
        }
    }
}
