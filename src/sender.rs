use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::Address;
use alloy_provider::{DynProvider, Provider};
use celo_alloy_network::{Celo, ReceiptResponse, TransactionBuilder};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesOrdered};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tracing::{debug, warn};

use crate::{
    pacer::Pacer,
    report::{Event, Outcome},
    workloads::Workload,
};

/// Fee suggestion shared by all senders, refreshed periodically in the background.
#[derive(Clone, Copy, Debug)]
pub struct FeeQuote {
    pub max_fee: u128,
    pub tip: u128,
}

/// Receipt wait result: latency plus `Some(status)`, or `None` when no receipt
/// arrived within the timeout.
type ReceiptWait = BoxFuture<'static, (Duration, Option<bool>)>;

/// One spamming account: owns its key (via the provider's wallet), a local
/// nonce counter, and a bounded window of in-flight transactions.
pub struct Sender {
    pub provider: DynProvider<Celo>,
    pub address: Address,
    pub chain_id: u64,
    pub window: usize,
    pub receipt_timeout: Duration,
    pub workload: Arc<Workload>,
    pub pacer: Arc<Pacer>,
    pub fees: watch::Receiver<FeeQuote>,
    pub events: mpsc::UnboundedSender<Event>,
    pub stop: watch::Receiver<bool>,
}

impl Sender {
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    async fn run(mut self) {
        let mut nonce = match self.fetch_pending_nonce().await {
            Ok(nonce) => nonce,
            Err(error) => {
                warn!(sender = %self.address, %error, "initial nonce fetch failed, sender not starting");
                return;
            }
        };
        // Set when a transaction is dropped or rejected: the on-chain nonce is
        // then unknown, so stop sending, let the window drain, and refetch.
        let mut need_resync = false;
        let mut in_flight: FuturesOrdered<ReceiptWait> = FuturesOrdered::new();

        loop {
            let stopped = *self.stop.borrow();
            if in_flight.is_empty() {
                if stopped {
                    break;
                }
                if need_resync {
                    match self.fetch_pending_nonce().await {
                        Ok(fresh) => {
                            debug!(sender = %self.address, nonce = fresh, "nonce resynced");
                            nonce = fresh;
                            need_resync = false;
                        }
                        Err(error) => {
                            warn!(sender = %self.address, %error, "nonce resync failed, retrying");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    }
                }
            }
            let can_send = !stopped && !need_resync && in_flight.len() < self.window;

            tokio::select! {
                biased;
                _ = self.stop.changed(), if !stopped => {}
                Some((latency, status)) = in_flight.next() => {
                    need_resync |= self.record(latency, status);
                }
                _ = self.pacer.acquire(), if can_send => {
                    match self.send_one(nonce).await {
                        Some(wait) => {
                            nonce += 1;
                            in_flight.push_back(wait);
                        }
                        None => need_resync = true,
                    }
                }
            }
        }
    }

    /// Submits one transaction; returns the receipt wait, or `None` if the
    /// node rejected it.
    async fn send_one(&self, nonce: u64) -> Option<ReceiptWait> {
        let quote = *self.fees.borrow();
        let request = self
            .workload
            .next(self.address)
            .from(self.address)
            .nonce(nonce)
            .max_fee_per_gas(quote.max_fee)
            .max_priority_fee_per_gas(quote.tip)
            .with_chain_id(self.chain_id);

        let started = Instant::now();
        let pending = match self.provider.send_transaction(request).await {
            Ok(pending) => pending,
            Err(error) => {
                warn!(sender = %self.address, %error, "transaction rejected");
                self.report(Event::Done(Outcome::Rejected));
                return None;
            }
        };
        self.report(Event::Sent);

        let timeout = self.receipt_timeout;
        Some(
            async move {
                let status = match tokio::time::timeout(timeout, pending.get_receipt()).await {
                    Ok(Ok(receipt)) => Some(receipt.status()),
                    Ok(Err(error)) => {
                        debug!(%error, "receipt fetch failed");
                        None
                    }
                    Err(_elapsed) => None,
                };
                (started.elapsed(), status)
            }
            .boxed(),
        )
    }

    /// Records one completed wait; returns true when a resync is needed.
    fn record(&self, latency: Duration, status: Option<bool>) -> bool {
        let outcome = match status {
            Some(true) => Outcome::Confirmed { latency },
            Some(false) => Outcome::Reverted,
            None => Outcome::Dropped,
        };
        self.report(Event::Done(outcome));
        matches!(outcome, Outcome::Dropped)
    }

    fn report(&self, event: Event) {
        // The reporter outlives all senders; a send only fails during shutdown.
        let _ = self.events.send(event);
    }

    async fn fetch_pending_nonce(&self) -> eyre::Result<u64> {
        Ok(self
            .provider
            .get_transaction_count(self.address)
            .pending()
            .await?)
    }
}
