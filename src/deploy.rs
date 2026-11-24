//! Deterministic deploys of workload contracts.
//!
//! Workload contracts go through the standard CREATE2 deployer
//! (`0x4e59…956c`, on the scion forks' mainnet state and predeployed by anvil),
//! so an address is a function of salt and init code alone. Runs that pass the
//! same `--salt` therefore share contracts — a storage prefill and the measured
//! runs after it — while the default random salt keeps today's fresh contracts
//! per run. A bytecode change moves every address, so a new contract version
//! never inherits an old one's state.

use std::time::Duration;

use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use alloy_provider::{DynProvider, Provider};
use alloy_sol_types::SolValue;
use celo_alloy_network::{Celo, ReceiptResponse, TransactionBuilder};
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{Result, bail};
use futures::future::join_all;
use tracing::{debug, warn};

use crate::lane::RPC_TIMEOUT;

/// The standard deterministic deployer (Arachnid's): calldata is
/// `salt ‖ init_code`, and it CREATE2s the contract.
pub const CREATE2_DEPLOYER: Address = address!("4e59b44847b379578588920ca78fbf26c0b4956c");

/// Bound on waiting for a deploy to land. Generous: it runs once, before
/// funding, and a slow block is not a reason to abort a run.
const DEPLOY_RECEIPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Where a workload contract came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Already at its address: an earlier run, or a concurrent one, deployed it.
    Reused,
    /// Deployed by this run.
    Deployed,
    /// Deployed with a plain CREATE because the chain has no CREATE2 deployer,
    /// so the address will not repeat.
    PlainCreate,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Reused => "reused",
            Self::Deployed => "deployed",
            Self::PlainCreate => "plain-create",
        })
    }
}

/// The salt of the `index`-th contract under `base`:
/// `keccak256(abi.encode(uint256(base), uint256(index)))`.
pub fn salt(base: u64, index: u64) -> B256 {
    keccak256((U256::from(base), U256::from(index)).abi_encode())
}

/// CREATE2 address of `init_code` deployed by `deployer` with `salt` (EIP-1014).
pub fn address_with(deployer: Address, salt: B256, init_code: &[u8]) -> Address {
    deployer.create2_from_code(salt, init_code)
}

/// CREATE2 address of `init_code` under the standard deployer.
pub fn address_of(salt: B256, init_code: &[u8]) -> Address {
    address_with(CREATE2_DEPLOYER, salt, init_code)
}

/// Whether `address` holds code.
pub async fn has_code(provider: &DynProvider<Celo>, address: Address) -> Result<bool> {
    let code =
        tokio::time::timeout(RPC_TIMEOUT, provider.get_code_at(address).into_future()).await??;
    Ok(!code.is_empty())
}

/// What a deploy attempt amounts to, given whether its transaction landed and
/// whether code is at the address afterwards. A failed deploy with code present
/// is a lost race against a concurrent run, which deployed the same contract.
fn after_deploy(landed: bool, code_present: bool) -> Result<Origin> {
    match (landed, code_present) {
        (true, true) => Ok(Origin::Deployed),
        (false, true) => Ok(Origin::Reused),
        (true, false) => bail!(
            "the CREATE2 deployer accepted the deploy but left no code at the expected address"
        ),
        (false, false) => {
            bail!("deploying a workload contract failed and nothing is at its address")
        }
    }
}

/// Makes sure a contract with `init_code` exists at each salt's address,
/// deploying the missing ones. On a chain without the CREATE2 deployer it falls
/// back to plain CREATEs, whose addresses later runs cannot find again.
///
/// Missing contracts are all submitted first and their receipts awaited
/// together, so a hundred contracts cost about one block rather than a hundred.
pub async fn ensure_all(
    root: &DynProvider<Celo>,
    salts: &[B256],
    init_code: &Bytes,
) -> Result<Vec<(Address, Origin)>> {
    if !has_code(root, CREATE2_DEPLOYER).await? {
        warn!(
            deployer = %CREATE2_DEPLOYER,
            "no CREATE2 deployer on this chain: deploying with plain CREATE, so later runs \
             cannot reuse these contracts"
        );
        let mut deployed = Vec::with_capacity(salts.len());
        for _ in salts {
            deployed.push((plain_create(root, init_code).await?, Origin::PlainCreate));
        }
        return Ok(deployed);
    }
    let addresses: Vec<Address> = salts
        .iter()
        .map(|salt| address_of(*salt, init_code))
        .collect();
    let mut present = Vec::with_capacity(addresses.len());
    for address in &addresses {
        present.push(has_code(root, *address).await?);
    }

    // Submit every missing deploy, then wait for all of them.
    let mut pending = Vec::new();
    for (index, salt) in salts.iter().enumerate() {
        if present[index] {
            continue;
        }
        let mut input = salt.to_vec();
        input.extend_from_slice(init_code);
        let request = CeloTransactionRequest::default()
            .to(CREATE2_DEPLOYER)
            .value(U256::ZERO)
            .with_input(Bytes::from(input));
        match tokio::time::timeout(RPC_TIMEOUT, root.send_transaction(request)).await {
            Ok(Ok(sent)) => pending.push((index, Some(sent))),
            Ok(Err(error)) => {
                debug!(%error, address = %addresses[index], "deploy submission failed");
                pending.push((index, None));
            }
            Err(_elapsed) => pending.push((index, None)),
        }
    }
    let landed = join_all(pending.into_iter().map(|(index, sent)| async move {
        let landed = match sent {
            Some(sent) => matches!(
                tokio::time::timeout(DEPLOY_RECEIPT_TIMEOUT, sent.get_receipt()).await,
                Ok(Ok(receipt)) if receipt.status()
            ),
            None => false,
        };
        (index, landed)
    }))
    .await;

    let mut origins: Vec<Origin> = vec![Origin::Reused; addresses.len()];
    for (index, landed) in landed {
        origins[index] = after_deploy(landed, has_code(root, addresses[index]).await?)?;
    }
    Ok(addresses.into_iter().zip(origins).collect())
}

/// Deploys `init_code` with a plain CREATE and returns the new address.
async fn plain_create(root: &DynProvider<Celo>, init_code: &Bytes) -> Result<Address> {
    let request = CeloTransactionRequest::default().with_deploy_code(init_code.clone());
    let sent = tokio::time::timeout(RPC_TIMEOUT, root.send_transaction(request)).await??;
    let receipt = tokio::time::timeout(DEPLOY_RECEIPT_TIMEOUT, sent.get_receipt()).await??;
    match receipt.contract_address() {
        Some(address) if receipt.status() => Ok(address),
        _ => bail!("deploying a workload contract with plain CREATE failed"),
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256, hex};

    use super::*;

    /// EIP-1014's first two worked examples.
    #[test]
    fn address_matches_eip1014_vectors() {
        assert_eq!(
            address_with(Address::ZERO, B256::ZERO, &hex!("00")),
            address!("4D1A2e2bB4F88F0250f26Ffff098B0b30B26BF38")
        );
        assert_eq!(
            address_with(
                address!("deadbeef00000000000000000000000000000000"),
                B256::ZERO,
                &hex!("00")
            ),
            address!("B928f69Bb1D91Cd65274e3c79d8986362984fDA3")
        );
    }

    #[test]
    fn address_of_uses_the_canonical_deployer() {
        let salt = b256!("0000000000000000000000000000000000000000000000000000000000000001");
        assert_eq!(
            address_of(salt, &hex!("6000")),
            address_with(CREATE2_DEPLOYER, salt, &hex!("6000"))
        );
    }

    #[test]
    fn salt_is_stable_and_sensitive_to_base_and_index() {
        assert_eq!(salt(7, 0), salt(7, 0));
        assert_ne!(salt(7, 0), salt(7, 1));
        assert_ne!(salt(7, 0), salt(8, 0));
    }

    /// Two Jobs racing the same deploy: the loser's transaction fails, but the
    /// winner's code is at the address, so the loser reuses it.
    #[test]
    fn a_failed_deploy_with_code_present_is_reuse() {
        assert_eq!(after_deploy(false, true).unwrap(), Origin::Reused);
    }

    #[test]
    fn a_failed_deploy_with_no_code_is_an_error() {
        assert!(after_deploy(false, false).is_err());
    }

    #[test]
    fn a_deploy_that_lands_code_is_deployed() {
        assert_eq!(after_deploy(true, true).unwrap(), Origin::Deployed);
    }

    #[test]
    fn a_successful_receipt_without_code_is_an_error() {
        assert!(after_deploy(true, false).is_err());
    }
}
