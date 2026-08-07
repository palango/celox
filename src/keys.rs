use alloy_primitives::keccak256;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Context, Result};

/// Derives the sender key for `index` from the root key.
///
/// Children are `keccak256(root_priv || "celox" || index)`. Deriving from the
/// root *private key* rather than its address keeps the children exactly as
/// secret as the root. The scheme is deterministic, so the same root always
/// controls the same sender accounts and leftover balances survive between runs.
pub fn derive_sender(root: &PrivateKeySigner, index: u32) -> Result<PrivateKeySigner> {
    let mut preimage = Vec::with_capacity(32 + 5 + 4);
    preimage.extend_from_slice(root.to_bytes().as_slice());
    preimage.extend_from_slice(b"celox");
    preimage.extend_from_slice(&index.to_be_bytes());
    PrivateKeySigner::from_bytes(&keccak256(&preimage))
        .context("derived bytes are not a valid secp256k1 key (astronomically unlikely)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PrivateKeySigner {
        // anvil's well-known dev key 0
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
            .parse()
            .unwrap()
    }

    #[test]
    fn derivation_is_deterministic_and_distinct() {
        let a0 = derive_sender(&root(), 0).unwrap();
        let a0_again = derive_sender(&root(), 0).unwrap();
        let a1 = derive_sender(&root(), 1).unwrap();

        assert_eq!(a0.address(), a0_again.address());
        assert_ne!(a0.address(), a1.address());
        assert_ne!(a0.address(), root().address());
    }

    #[test]
    fn derivation_is_stable_across_versions() {
        // Pinned golden addresses: a change here means existing sender accounts
        // (and their balances) become unreachable for every user of the tool.
        let addr0 = derive_sender(&root(), 0).unwrap().address().to_string();
        let addr1 = derive_sender(&root(), 1).unwrap().address().to_string();
        assert_eq!(addr0, "0x18989333Ed8cb4d322cDb0d3b5EeEA2b2B83bA4E");
        assert_eq!(addr1, "0x933200397351f0c3b3c0A2d4c5A25333e968A6FB");
    }
}
