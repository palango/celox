use std::{
    future::IntoFuture,
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

/// Bound on every submission and nonce RPC. Without it a connection that
/// accepts a request but never answers blocks the sender loop outside its
/// `select!`, where the stop signal cannot reach it.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

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

/// What the sender loop does next, given its current state. Pure so the
/// break/resync/send truth table is directly testable.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    /// Stop signal seen and the window is empty: exit the loop.
    Finish,
    /// The nonce is suspect and the window has drained: refetch it.
    Resync,
    /// Only wait for receipts: stopping, resync pending, or the window is full.
    Drain,
    /// Room in the window and sending is allowed.
    Send,
}

fn step(stopped: bool, need_resync: bool, in_flight: usize, window: usize) -> Step {
    match (stopped, need_resync, in_flight) {
        (true, _, 0) => Step::Finish,
        (false, true, 0) => Step::Resync,
        (true, _, _) | (_, true, _) => Step::Drain,
        _ if in_flight >= window => Step::Drain,
        _ => Step::Send,
    }
}

impl Sender {
    pub fn spawn(self) -> JoinHandle<eyre::Result<()>> {
        tokio::spawn(self.run())
    }

    async fn run(mut self) -> eyre::Result<()> {
        let mut nonce = self.fetch_pending_nonce().await.map_err(|error| {
            eyre::eyre!(
                "sender {}: initial nonce fetch failed: {error}",
                self.address
            )
        })?;
        // Set when a transaction is dropped or rejected: the on-chain nonce is
        // then unknown, so stop sending, let the window drain, and refetch.
        let mut need_resync = false;
        let mut in_flight: FuturesOrdered<ReceiptWait> = FuturesOrdered::new();

        loop {
            let stopped = *self.stop.borrow();
            let step = step(stopped, need_resync, in_flight.len(), self.window);
            match step {
                Step::Finish => break,
                Step::Resync => match self.fetch_pending_nonce().await {
                    Ok(fresh) => {
                        debug!(sender = %self.address, nonce = fresh, "nonce resynced");
                        nonce = fresh;
                        need_resync = false;
                    }
                    Err(error) => {
                        warn!(sender = %self.address, %error, "nonce resync failed, retrying");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                },
                Step::Drain | Step::Send => {
                    tokio::select! {
                        biased;
                        _ = self.stop.changed(), if !stopped => {}
                        Some((latency, status)) = in_flight.next() => {
                            need_resync |= self.record(latency, status);
                        }
                        _ = self.pacer.acquire(), if step == Step::Send => {
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
        }
        Ok(())
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
        let sent = tokio::time::timeout(RPC_TIMEOUT, self.provider.send_transaction(request)).await;
        let pending = match sent {
            Ok(Ok(pending)) => pending,
            Ok(Err(error)) => {
                warn!(sender = %self.address, %error, "transaction rejected");
                self.report(Event::Done(Outcome::Rejected));
                return None;
            }
            Err(_elapsed) => {
                warn!(sender = %self.address, "submission timed out");
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
        let fetch = self.provider.get_transaction_count(self.address).pending();
        Ok(tokio::time::timeout(RPC_TIMEOUT, fetch.into_future()).await??)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_truth_table() {
        let window = 3;
        // Normal operation: send until the window is full, then drain.
        assert_eq!(step(false, false, 0, window), Step::Send);
        assert_eq!(step(false, false, 2, window), Step::Send);
        assert_eq!(step(false, false, 3, window), Step::Drain);
        // A suspect nonce blocks sending until the window drains, then resyncs.
        assert_eq!(step(false, true, 2, window), Step::Drain);
        assert_eq!(step(false, true, 0, window), Step::Resync);
        // Stopping drains outstanding receipts, then finishes.
        assert_eq!(step(true, false, 2, window), Step::Drain);
        assert_eq!(step(true, false, 0, window), Step::Finish);
        // Stop wins over resync: no point refetching a nonce we won't use.
        assert_eq!(step(true, true, 0, window), Step::Finish);
        assert_eq!(step(true, true, 2, window), Step::Drain);
    }
}
