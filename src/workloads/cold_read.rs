use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, sol};
use celo_alloy_network::TransactionBuilder;
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{Result, bail, ensure};
use tracing::info;

use crate::{
    deploy::{self, Origin, Root},
    gas,
    lane::RPC_TIMEOUT,
};

sol! {
    #[sol(rpc)]
    StateProber,
    "contracts/StateProber.json"
}

sol! {
    #[sol(rpc)]
    interface IBalanceOf {
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Most `--reads` one transaction makes. It keeps the analytic gas limit far
/// from overflow; counts that cannot fit a block are rejected by main's
/// block-gas-limit check before funding.
pub const MAX_READS: u64 = 20_000;

/// Cold-read workload: every transaction asks `StateProber` to read state at
/// `reads` addresses derived from a fresh random seed, so no lookup repeats and
/// each one misses the node's caches. Without `--read-token` it reads native
/// balances (cold accounts); with it, `token.balanceOf` (cold slots of one big
/// contract's storage, such as cUSD's balance mapping).
pub struct ColdRead {
    prober: Address,
    reads: u64,
    token: Option<Address>,
}

impl ColdRead {
    /// Checks `token` answers `balanceOf`, then deploys, or reuses under
    /// `salt`, the prober contract.
    pub async fn new(
        root: &Root<'_>,
        salt: u64,
        reads: u64,
        token: Option<Address>,
    ) -> Result<Self> {
        if let Some(token) = token {
            let answers = deploy::has_code(root.provider, token).await?
                && matches!(
                    tokio::time::timeout(
                        RPC_TIMEOUT,
                        IBalanceOf::new(token, root.provider)
                            .balanceOf(Address::ZERO)
                            .call(),
                    )
                    .await,
                    Ok(Ok(_))
                );
            if !answers {
                bail!("--read-token {token} is not an ERC-20 this chain can call");
            }
        }
        let deployed =
            deploy::ensure_all(root, &[deploy::salt(salt, 0)], &StateProber::BYTECODE).await?;
        let (prober, origin) = deployed[0];
        match origin {
            Origin::Reused => info!(%prober, reads, ?token, "reusing state prober"),
            _ => info!(%prober, %origin, reads, ?token, "state prober deployed"),
        }
        Ok(Self {
            prober,
            reads,
            token,
        })
    }

    pub fn next(&self, _sender: Address) -> CeloTransactionRequest {
        let seed = U256::from(rand::random::<u64>());
        let count = U256::from(self.reads);
        let (payload, gas_limit) = match self.token {
            None => {
                let payload = StateProber::probeAccountsCall { seed, count }.abi_encode();
                let gas_limit = gas::cold_account_reads(&payload, self.reads);
                (payload, gas_limit)
            }
            Some(token) => {
                let payload = StateProber::probeTokenCall { token, seed, count }.abi_encode();
                let gas_limit = gas::cold_token_reads(&payload, self.reads);
                (payload, gas_limit)
            }
        };
        CeloTransactionRequest::default()
            .to(self.prober)
            .value(U256::ZERO)
            .gas_limit(gas_limit)
            .with_input(payload)
    }
}

/// Validates `--reads`.
pub fn check(reads: u64) -> Result<()> {
    ensure!(
        (1..=MAX_READS).contains(&reads),
        "--reads must be between 1 and {MAX_READS}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall;

    use super::*;

    fn workload(token: Option<Address>) -> ColdRead {
        ColdRead {
            prober: Address::repeat_byte(0x9b),
            reads: 200,
            token,
        }
    }

    fn payload_of(request: &CeloTransactionRequest) -> Vec<u8> {
        request
            .inner
            .as_ref()
            .input
            .input
            .clone()
            .expect("input is set")
            .to_vec()
    }

    #[test]
    fn fresh_seed_every_call() {
        let workload = workload(None);
        let seed = |request: &CeloTransactionRequest| {
            StateProber::probeAccountsCall::abi_decode(&payload_of(request))
                .unwrap()
                .seed
        };
        let first = workload.next(Address::ZERO);
        assert_eq!(first.inner.as_ref().to, Some(workload.prober.into()));
        assert_ne!(seed(&first), seed(&workload.next(Address::ZERO)));
    }

    #[test]
    fn routes_to_token_probe_when_configured() {
        let token = Address::repeat_byte(0x76);
        let request = workload(Some(token)).next(Address::ZERO);
        let call = StateProber::probeTokenCall::abi_decode(&payload_of(&request)).unwrap();
        assert_eq!(call.token, token);
        assert_eq!(call.count, U256::from(200));
        assert_eq!(
            request.inner.as_ref().gas,
            Some(gas::cold_token_reads(&payload_of(&request), 200))
        );
    }

    #[test]
    fn rejects_a_read_count_out_of_range() {
        assert!(check(0).unwrap_err().to_string().contains("--reads"));
        assert!(
            check(MAX_READS + 1)
                .unwrap_err()
                .to_string()
                .contains("--reads")
        );
        assert!(check(MAX_READS).is_ok());
    }
}
