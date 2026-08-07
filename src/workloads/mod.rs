//! Transaction workloads.
//!
//! Adding a workload: new file in this directory, a variant on both enums
//! below, and arms in `setup`/`next`. `next` must set everything the workload
//! knows (to, value, input, gas limit); the sender loop fills in the account
//! fields (from, nonce, fees, chain id).

mod transfer;

use alloy_primitives::Address;
use alloy_provider::DynProvider;
use celo_alloy_network::Celo;
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::Result;

pub use transfer::Transfer;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum WorkloadKind {
    /// 1-wei native self-transfers
    Transfer,
}

pub enum Workload {
    Transfer(Transfer),
}

impl Workload {
    pub fn new(kind: WorkloadKind) -> Self {
        match kind {
            WorkloadKind::Transfer => Self::Transfer(Transfer),
        }
    }

    /// One-time on-chain preparation (contract deploys), run with the root account.
    pub async fn setup(&mut self, _root: &DynProvider<Celo>) -> Result<()> {
        match self {
            Self::Transfer(_) => Ok(()),
        }
    }

    /// Produces the next transaction for `sender`.
    pub fn next(&self, sender: Address) -> CeloTransactionRequest {
        match self {
            Self::Transfer(workload) => workload.next(sender),
        }
    }
}
