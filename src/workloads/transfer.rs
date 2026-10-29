use alloy_primitives::{Address, U256};
use celo_alloy_rpc_types::CeloTransactionRequest;
use rand::Rng;

use crate::gas;

/// Where the `transfer` and `cip64` workloads send their 1 wei. For `transfer`
/// the transaction is identical either way — 21,000 gas, no calldata — so the
/// mode chooses what state pressure the run applies, not what it costs; `cip64`
/// documents its own modes.
#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Recipient {
    /// The sender itself: burns gas without touching any other account.
    #[value(name = "self")]
    Sender,
    /// A fresh random address per transaction: `transfer` mints a brand-new
    /// account, `cip64` writes a new balance slot in the fee-currency token.
    Fresh,
}

/// Baseline workload: a 1-wei native transfer. With `Recipient::Sender` the
/// senders only ever burn gas; with `Recipient::Fresh` each transaction lands
/// its wei on an address nobody has seen, so the chain must insert a new
/// account-trie leaf per transaction. The trie keys leaves by
/// `keccak(address)`, so uniformly random addresses give the keccak-uniform
/// scatter that real new-account load has.
pub struct Transfer {
    recipient: Recipient,
}

impl Transfer {
    pub fn new(recipient: Recipient) -> Self {
        Self { recipient }
    }

    pub fn next(&self, sender: Address) -> CeloTransactionRequest {
        let to = match self.recipient {
            Recipient::Sender => sender,
            Recipient::Fresh => fresh_address(),
        };
        CeloTransactionRequest::default()
            .to(to)
            .value(U256::from(1))
            .gas_limit(gas::TRANSFER)
    }
}

/// A uniformly random address. Whatever is sent there is unrecoverable by
/// construction, which is the point: an account below any plausible dust
/// threshold, whose only purpose is to exist in the trie.
pub(crate) fn fresh_address() -> Address {
    let mut bytes = [0u8; 20];
    rand::rng().fill_bytes(&mut bytes);
    Address::from(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_plain_native_self_transfer() {
        let sender = Address::repeat_byte(0x42);
        let request = Transfer::new(Recipient::Sender).next(sender);

        assert!(!request.is_cip64());
        assert_eq!(request.inner.as_ref().to, Some(sender.into()));
        assert_eq!(request.inner.as_ref().value, Some(U256::from(1)));
        assert_eq!(request.inner.as_ref().gas, Some(21_000));
    }

    #[test]
    fn fresh_mode_pays_the_same_but_never_sends_to_the_sender() {
        let sender = Address::repeat_byte(0x42);
        let request = Transfer::new(Recipient::Fresh).next(sender);

        assert!(!request.is_cip64());
        assert_ne!(request.inner.as_ref().to, Some(sender.into()));
        assert_eq!(request.inner.as_ref().value, Some(U256::from(1)));
        assert_eq!(request.inner.as_ref().gas, Some(21_000));
    }

    #[test]
    fn fresh_mode_draws_a_new_recipient_per_transaction() {
        let workload = Transfer::new(Recipient::Fresh);
        let sender = Address::repeat_byte(0x42);

        let first = workload.next(sender);
        let second = workload.next(sender);
        assert_ne!(first.inner.as_ref().to, second.inner.as_ref().to);
    }
}
