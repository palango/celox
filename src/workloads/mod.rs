//! Transaction workloads.
//!
//! Adding a workload: new file in this directory, a variant on both enums
//! below, and arms in `setup`/`next`; a workload with a CLI knob also gets a
//! field on `Config`. `next` must set everything the workload knows (to, value,
//! input, gas limit); the sender loop fills in the account fields (from, nonce,
//! fees, chain id).

mod calldata;
mod cip64;
mod transfer;

use alloy_primitives::Address;
use alloy_provider::DynProvider;
use celo_alloy_network::Celo;
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{OptionExt, Result};

pub use calldata::Calldata;
pub use cip64::Cip64;
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
}

/// Workload settings from the CLI. Each workload reads only the fields it needs.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub data_size: usize,
    pub fee_currency: Option<FeeCurrency>,
}

pub enum Workload {
    Transfer(Transfer),
    Calldata(Calldata),
    Cip64(Cip64),
}

impl Workload {
    pub fn new(kind: WorkloadKind, config: Config) -> Result<Self> {
        Ok(match kind {
            WorkloadKind::Transfer => Self::Transfer(Transfer),
            WorkloadKind::Calldata => Self::Calldata(Calldata::new(config.data_size)),
            WorkloadKind::Cip64 => Self::Cip64(Cip64::new(
                config
                    .fee_currency
                    .ok_or_eyre("the cip64 workload needs a resolved fee currency")?,
            )),
        })
    }

    /// One-time on-chain preparation (contract deploys), run with the root account.
    pub async fn setup(&mut self, _root: &DynProvider<Celo>) -> Result<()> {
        match self {
            Self::Transfer(_) | Self::Calldata(_) | Self::Cip64(_) => Ok(()),
        }
    }

    /// Produces the next transaction for `sender`.
    pub fn next(&self, sender: Address) -> CeloTransactionRequest {
        match self {
            Self::Transfer(workload) => workload.next(sender),
            Self::Calldata(workload) => workload.next(sender),
            Self::Cip64(workload) => workload.next(sender),
        }
    }
}
