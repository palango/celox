use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_provider::DynProvider;
use celo_alloy_network::Celo;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tracing::warn;

use crate::{
    fees::FeeQuote,
    lane::{Lane, Submitted},
    pacer::Pacer,
    report::{Event, Outcome},
    workloads::Workload,
};

/// One spamming account: a workload-driven, paced producer for its own [`Lane`].
pub struct Sender {
    pub provider: DynProvider<Celo>,
    pub address: Address,
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
        let mut lane = Lane::open(
            self.provider.clone(),
            self.address,
            self.window,
            self.receipt_timeout,
        )
        .await
        .map_err(|error| eyre::eyre!("sender {}: {error}", self.address))?;

        // Set when a transaction is dropped or rejected: the on-chain nonce is
        // then unknown, so stop sending, let the window drain, and refetch.
        let mut need_resync = false;

        loop {
            let stopped = *self.stop.borrow();
            let step = step(stopped, need_resync, lane.in_flight(), self.window);
            match step {
                Step::Finish => break,
                Step::Resync => match lane.resync().await {
                    Ok(()) => need_resync = false,
                    Err(error) => {
                        warn!(sender = %self.address, %error, "nonce resync failed, retrying");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                },
                Step::Drain | Step::Send => {
                    tokio::select! {
                        biased;
                        _ = self.stop.changed(), if !stopped => {}
                        (latency, status) = lane.next_completed() => {
                            need_resync |= self.record(latency, status);
                        }
                        _ = self.pacer.acquire(), if step == Step::Send => {
                            let request = self.workload.next(self.address);
                            // Copy the quote out before awaiting; the guard is not Send.
                            let fees = *self.fees.borrow();
                            match lane.submit(request, fees).await {
                                Submitted::Sent => self.report(Event::Sent),
                                Submitted::Rejected => {
                                    self.report(Event::Done(Outcome::Rejected));
                                    need_resync = true;
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
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
