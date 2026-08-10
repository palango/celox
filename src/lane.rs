//! One account's transaction pipeline: a local nonce counter plus a bounded
//! window of in-flight transactions. Both the senders (one lane each, driven by
//! a pacer) and the funder (one lane for the root, drained to completion) push
//! requests through a lane, so nonce assignment, submission, the pool-cap
//! window and receipt waiting live in one place.

use std::{
    future::IntoFuture,
    time::{Duration, Instant},
};

use alloy_primitives::Address;
use alloy_provider::{DynProvider, Provider};
use celo_alloy_network::{Celo, ReceiptResponse};
use celo_alloy_rpc_types::CeloTransactionRequest;
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use tracing::{debug, warn};

use crate::fees::FeeQuote;

/// Bound on every submission and nonce RPC. Without it a connection that
/// accepts a request but never answers blocks the caller's loop where a stop
/// signal cannot reach it. Public because main's drain deadline must exceed
/// `receipt_timeout + RPC_TIMEOUT`, the worst legitimate drain.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// The per-account pending limit nodes typically enforce (reth: 16). It is a
/// node policy rather than a protocol rule, so celox uses it as the funder's
/// window and warns — rather than refuses — when a sender's window exceeds it.
pub const MAX_WINDOW: usize = 16;

/// How often to re-ask for a receipt after the watcher itself has errored.
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Receipt wait result: latency plus `Some(status)`, or `None` when no receipt
/// arrived within the timeout.
type ReceiptWait = BoxFuture<'static, (Duration, Option<bool>)>;

/// Outcome of a submission attempt. A rejection means the node refused the
/// transaction or never answered — in both cases the local nonce may now be
/// wrong, so the caller should drain and resync.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Submitted {
    Sent,
    Rejected,
}

pub struct Lane {
    provider: DynProvider<Celo>,
    account: Address,
    nonce: u64,
    window: usize,
    receipt_timeout: Duration,
    in_flight: FuturesUnordered<ReceiptWait>,
    /// Rejections since the last accepted submission. A systemic rejection —
    /// an underpriced fee, an underfunded account — hits every transaction, so
    /// only the first of a run is worth a warning.
    rejection_streak: u32,
}

impl Lane {
    /// Opens a lane for `account`, fetching its pending nonce.
    pub async fn open(
        provider: DynProvider<Celo>,
        account: Address,
        window: usize,
        receipt_timeout: Duration,
    ) -> eyre::Result<Self> {
        let nonce = pending_nonce(&provider, account).await?;
        Ok(Self {
            provider,
            account,
            nonce,
            window,
            receipt_timeout,
            in_flight: FuturesUnordered::new(),
            rejection_streak: 0,
        })
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    pub fn has_capacity(&self) -> bool {
        self.in_flight.len() < self.window
    }

    /// Refetches the on-chain nonce after a rejection or drop left the local
    /// counter suspect. Only meaningful once the window has drained.
    pub async fn resync(&mut self) -> eyre::Result<()> {
        self.nonce = pending_nonce(&self.provider, self.account).await?;
        debug!(account = %self.account, nonce = self.nonce, "nonce resynced");
        Ok(())
    }

    /// Fills the account fields on `request`, submits it, and on success queues
    /// its receipt wait and advances the nonce.
    pub async fn submit(&mut self, request: CeloTransactionRequest, fees: FeeQuote) -> Submitted {
        // Chain id is left to the provider's ChainIdFiller, which caches it
        // after one fetch; everything else is preset so no filler does RPC.
        let request = request
            .from(self.account)
            .nonce(self.nonce)
            .max_fee_per_gas(fees.max_fee)
            .max_priority_fee_per_gas(fees.tip);

        let started = Instant::now();
        let sent = tokio::time::timeout(RPC_TIMEOUT, self.provider.send_transaction(request)).await;
        let pending = match sent {
            Ok(Ok(pending)) => pending,
            Ok(Err(error)) => {
                self.note_rejection(format_args!("transaction rejected: {error}"));
                return Submitted::Rejected;
            }
            Err(_elapsed) => {
                self.note_rejection(format_args!("submission timed out"));
                return Submitted::Rejected;
            }
        };
        self.nonce += 1;
        self.rejection_streak = 0;

        let receipt_timeout = self.receipt_timeout;
        let hash = *pending.tx_hash();
        let provider = self.provider.clone();
        self.in_flight.push(
            async move {
                let status =
                    match tokio::time::timeout(receipt_timeout, pending.get_receipt()).await {
                        Ok(Ok(receipt)) => Some(receipt.status()),
                        // A failing watcher says nothing about the transaction:
                        // a flaky endpoint, or a load balancer answering from a
                        // node that has the block but not yet the receipt, looks
                        // the same as a chain that never saw it. Keep asking
                        // until the deadline instead of recording a landed
                        // transaction as dropped.
                        Ok(Err(error)) => {
                            debug!(%error, "receipt watcher failed, polling by hash");
                            poll_receipt(&provider, hash, started + receipt_timeout).await
                        }
                        Err(_elapsed) => None,
                    };
                (started.elapsed(), status)
            }
            .boxed(),
        );
        Submitted::Sent
    }

    fn note_rejection(&mut self, reason: std::fmt::Arguments<'_>) {
        if self.rejection_streak == 0 {
            warn!(account = %self.account, "{reason}");
        } else {
            debug!(account = %self.account, streak = self.rejection_streak, "{reason}");
        }
        self.rejection_streak += 1;
    }

    /// Awaits the next completed transaction. Stays pending forever when the
    /// window is empty, so it is safe to select on without busy-looping; it is
    /// cancel-safe, so a lost `select!` race drops no queued receipt.
    pub async fn next_completed(&mut self) -> (Duration, Option<bool>) {
        match self.in_flight.next().await {
            Some(outcome) => outcome,
            None => std::future::pending().await,
        }
    }
}

/// Asks for a receipt by hash until one arrives or the deadline passes.
async fn poll_receipt(
    provider: &DynProvider<Celo>,
    hash: alloy_primitives::TxHash,
    deadline: Instant,
) -> Option<bool> {
    while Instant::now() < deadline {
        match provider.get_transaction_receipt(hash).await {
            Ok(Some(receipt)) => return Some(receipt.status()),
            Ok(None) => {}
            Err(error) => debug!(%error, "receipt poll failed"),
        }
        tokio::time::sleep(RECEIPT_POLL_INTERVAL).await;
    }
    None
}

async fn pending_nonce(provider: &DynProvider<Celo>, account: Address) -> eyre::Result<u64> {
    let fetch = provider.get_transaction_count(account).pending();
    Ok(tokio::time::timeout(RPC_TIMEOUT, fetch.into_future()).await??)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, U256, address, b256};
    use alloy_provider::ProviderBuilder;
    use alloy_signer_local::PrivateKeySigner;
    use alloy_transport::mock::Asserter;
    use celo_alloy_network::EthereumWallet;
    use serde_json::json;

    use super::*;

    /// anvil dev key 0, the signer registered in the mocked wallet below.
    const ACCOUNT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    const TX_HASH: B256 = b256!("1111111111111111111111111111111111111111111111111111111111111111");

    fn mocked() -> (Asserter, DynProvider<Celo>) {
        let asserter = Asserter::new();
        let signer: PrivateKeySigner =
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
                .parse()
                .unwrap();
        let provider = ProviderBuilder::new_with_network::<Celo>()
            .wallet(EthereumWallet::new(signer))
            .connect_mocked_client(asserter.clone())
            .erased();
        (asserter, provider)
    }

    fn fees() -> FeeQuote {
        FeeQuote {
            max_fee: 1_000_000_000,
            tip: 1_000_000,
        }
    }

    fn transfer() -> CeloTransactionRequest {
        CeloTransactionRequest::default()
            .to(ACCOUNT)
            .value(U256::from(1))
            .gas_limit(21_000)
    }

    /// A receipt as the node would return it. Only the fields the Celo receipt
    /// type needs to deserialise and the status celox reads.
    fn receipt_json(status: bool) -> serde_json::Value {
        json!({
            "type": "0x2",
            "status": if status { "0x1" } else { "0x0" },
            "cumulativeGasUsed": "0x5208",
            "logs": [],
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "transactionHash": TX_HASH,
            "transactionIndex": "0x0",
            "blockHash": TX_HASH,
            "blockNumber": "0x1",
            "gasUsed": "0x5208",
            "effectiveGasPrice": "0x1",
            "from": ACCOUNT,
            "to": ACCOUNT,
            "contractAddress": null,
        })
    }

    async fn open_lane(asserter: &Asserter, provider: &DynProvider<Celo>, nonce: u64) -> Lane {
        asserter.push_success(&format!("0x{nonce:x}"));
        Lane::open(provider.clone(), ACCOUNT, 2, Duration::from_millis(200))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn open_anchors_on_the_pending_nonce() {
        let (asserter, provider) = mocked();
        let lane = open_lane(&asserter, &provider, 7).await;
        assert_eq!(lane.nonce, 7);
        assert_eq!(lane.in_flight(), 0);
        assert!(lane.has_capacity());
    }

    #[tokio::test]
    async fn a_rejected_submission_does_not_consume_the_nonce() {
        let (asserter, provider) = mocked();
        let mut lane = open_lane(&asserter, &provider, 7).await;

        asserter.push_success(&"0x1"); // eth_chainId, filled once per provider
        asserter.push_failure_msg("insufficient funds");
        assert_eq!(lane.submit(transfer(), fees()).await, Submitted::Rejected);

        // The transaction never reached the chain, so the nonce still belongs
        // to the next attempt and nothing occupies the window.
        assert_eq!(lane.nonce, 7);
        assert_eq!(lane.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_sent_transaction_advances_the_nonce_and_fills_the_window() {
        let (asserter, provider) = mocked();
        let mut lane = open_lane(&asserter, &provider, 7).await;

        asserter.push_success(&"0x1"); // eth_chainId
        asserter.push_success(&TX_HASH);
        assert_eq!(lane.submit(transfer(), fees()).await, Submitted::Sent);
        assert_eq!(lane.nonce, 8);
        assert_eq!(lane.in_flight(), 1);
        assert!(lane.has_capacity());

        asserter.push_success(&TX_HASH);
        assert_eq!(lane.submit(transfer(), fees()).await, Submitted::Sent);
        assert_eq!(lane.nonce, 9);
        assert_eq!(lane.in_flight(), 2);
        // The window is full at 2: the caller must drain before sending more.
        assert!(!lane.has_capacity());
    }

    #[tokio::test]
    async fn resync_reanchors_on_the_chain() {
        let (asserter, provider) = mocked();
        let mut lane = open_lane(&asserter, &provider, 7).await;

        asserter.push_success(&"0x2a");
        lane.resync().await.unwrap();
        assert_eq!(lane.nonce, 42);
    }

    #[tokio::test]
    async fn a_transient_receipt_error_does_not_become_a_drop() {
        // The bug this guards: any receipt error used to be reported as a
        // dropped transaction, turning a flaky endpoint into fabricated
        // failures and a needless nonce resync.
        let (asserter, provider) = mocked();
        asserter.push_failure_msg("temporarily unavailable");
        asserter.push_success(&serde_json::Value::Null); // mined, receipt not visible yet
        asserter.push_success(&receipt_json(true));

        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(poll_receipt(&provider, TX_HASH, deadline).await, Some(true));
    }

    /// The whole path, not just the helper: submit, have the receipt watcher
    /// fail, and confirm the lane still reports the transaction as confirmed.
    /// Guards the bug where any watcher error was recorded as a drop.
    #[tokio::test]
    async fn a_submitted_transaction_survives_a_failing_receipt_watcher() {
        let (asserter, provider) = mocked();
        let mut lane = open_lane(&asserter, &provider, 7).await;

        asserter.push_success(&"0x1"); // eth_chainId
        asserter.push_success(&TX_HASH); // eth_sendRawTransaction
        assert_eq!(lane.submit(transfer(), fees()).await, Submitted::Sent);

        // The watcher's first query fails, as a flaky endpoint would; the
        // by-hash fallback then finds the receipt that was there all along.
        asserter.push_failure_msg("temporarily unavailable");
        for _ in 0..4 {
            asserter.push_success(&receipt_json(true));
        }

        let (_latency, status) = lane.next_completed().await;
        assert_eq!(
            status,
            Some(true),
            "a landed transaction must not be a drop"
        );
    }

    #[tokio::test]
    async fn a_reverted_receipt_is_reported_as_such() {
        let (asserter, provider) = mocked();
        asserter.push_success(&receipt_json(false));

        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(
            poll_receipt(&provider, TX_HASH, deadline).await,
            Some(false)
        );
    }

    #[tokio::test]
    async fn a_receipt_that_never_arrives_is_a_drop() {
        let (asserter, provider) = mocked();
        for _ in 0..50 {
            asserter.push_success(&serde_json::Value::Null);
        }

        let deadline = Instant::now() + Duration::from_millis(600);
        assert_eq!(poll_receipt(&provider, TX_HASH, deadline).await, None);
    }
}
