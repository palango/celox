use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use celo_alloy_rpc_types::CeloTransactionRequest;

use crate::{
    celo::{FeeCurrency, IErc20},
    gas,
    workloads::{Recipient, transfer::fresh_address},
};

/// CIP-64 workload: every transaction pays gas in a fee currency, so the whole
/// alternative-fee path — directory lookup, debit, credit — runs on each one.
///
/// `Recipient` chooses what the transaction itself does, and the two modes
/// stress different things:
///
/// - `Sender` sends 1 wei of the native token back to the sender. Nothing but
///   the fee path is exercised, which makes it the control.
/// - `Fresh` sends 1 wei of the **fee-currency token** to a brand-new random
///   address. A CIP-64 transaction cannot give native value a fresh
///   destination and stay a pure fee-path test, so the fresh mode moves the
///   ERC-20 instead: it writes a balance slot that was zero, growing the
///   token's own storage trie once per transaction, on top of the fee debit
///   and credit. That is the heaviest of the two by some margin.
///
/// The token moved is the fee currency itself rather than an arbitrary ERC-20,
/// because the funder already tops every sender up with it (`--fund-token`).
/// A different token would need its own funding leg to be worth anything.
pub struct Cip64 {
    fee_currency: Address,
    recipient: Recipient,
    /// The FeeCurrencyDirectory's surcharge for `fee_currency`.
    currency_intrinsic: u64,
}

impl Cip64 {
    pub fn new(fee_currency: FeeCurrency, recipient: Recipient) -> Self {
        Self {
            fee_currency: fee_currency.token,
            recipient,
            currency_intrinsic: fee_currency.intrinsic_gas,
        }
    }

    pub fn next(&self, sender: Address) -> CeloTransactionRequest {
        match self.recipient {
            Recipient::Sender => CeloTransactionRequest::default()
                .to(sender)
                .value(U256::from(1))
                .gas_limit(gas::cip64_transfer(self.currency_intrinsic))
                .fee_currency(self.fee_currency),
            Recipient::Fresh => {
                let payload = IErc20::transferCall {
                    to: fresh_address(),
                    value: U256::from(1),
                }
                .abi_encode();
                // Priced from the payload rather than a constant: the intrinsic
                // cost depends on how many zero bytes the encoded address
                // happens to have, and a random address is not fixed.
                let gas_limit = gas::cip64_erc20_transfer(self.currency_intrinsic, &payload);
                CeloTransactionRequest::default()
                    .to(self.fee_currency)
                    .input(Bytes::from(payload).into())
                    .gas_limit(gas_limit)
                    .fee_currency(self.fee_currency)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn currency() -> FeeCurrency {
        FeeCurrency {
            token: Address::repeat_byte(0xfe),
            intrinsic_gas: 50_000,
            decimals: 18,
        }
    }

    #[test]
    fn produces_cip64_self_transfer_with_surcharged_gas() {
        let sender = Address::repeat_byte(0x42);
        let request = Cip64::new(currency(), Recipient::Sender).next(sender);

        assert!(request.is_cip64());
        assert_eq!(request.fee_currency, Some(currency().token));
        assert_eq!(request.inner.as_ref().to, Some(sender.into()));
        assert_eq!(request.inner.as_ref().value, Some(U256::from(1)));
        assert_eq!(request.inner.as_ref().gas, Some(71_000));
    }

    #[test]
    fn fresh_mode_sends_one_wei_of_the_fee_currency_to_a_new_address() {
        let sender = Address::repeat_byte(0x42);
        let request = Cip64::new(currency(), Recipient::Fresh).next(sender);

        // Still CIP-64: gas is paid in the fee currency either way.
        assert!(request.is_cip64());
        assert_eq!(request.fee_currency, Some(currency().token));

        // The call goes to the token, not to an EOA.
        assert_eq!(request.inner.as_ref().to, Some(currency().token.into()));
        assert_eq!(request.inner.as_ref().value, None);

        // ...and it is a transfer of exactly 1 wei to somebody new.
        let input = request.inner.as_ref().input.input().expect("calldata");
        let call = IErc20::transferCall::abi_decode(input).expect("decodes as transfer");
        assert_eq!(call.value, U256::from(1));
        assert_ne!(call.to, sender);
        assert_ne!(call.to, Address::ZERO);

        // Enough gas for a cold balance-slot write plus the fee surcharge.
        let gas = request.inner.as_ref().gas.expect("gas limit");
        assert!(
            gas > 150_000,
            "{gas} should cover execution and the surcharge"
        );
    }

    #[test]
    fn fresh_mode_picks_a_different_destination_every_time() {
        let sender = Address::repeat_byte(0x42);
        let workload = Cip64::new(currency(), Recipient::Fresh);
        let destination = |request: CeloTransactionRequest| {
            let input = request
                .inner
                .as_ref()
                .input
                .input()
                .expect("calldata")
                .clone();
            IErc20::transferCall::abi_decode(&input)
                .expect("transfer")
                .to
        };
        assert_ne!(
            destination(workload.next(sender)),
            destination(workload.next(sender))
        );
    }
}
