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
    use alloy_primitives::{address, b256};
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_signer_local::PrivateKeySigner;
    use alloy_transport::mock::Asserter;
    use celo_alloy_network::EthereumWallet;

    use super::*;
    use crate::{report::Outcome, workloads::Transfer};

    /// anvil dev key 0, the signer registered in the mocked wallet.
    const ACCOUNT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    struct Harness {
        sender: Sender,
        asserter: Asserter,
        events: mpsc::UnboundedReceiver<Event>,
        stop: watch::Sender<bool>,
    }

    fn harness(window: usize, receipt_timeout: Duration) -> Harness {
        let asserter = Asserter::new();
        let signer: PrivateKeySigner =
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
                .parse()
                .unwrap();
        let provider = ProviderBuilder::new_with_network::<Celo>()
            .wallet(EthereumWallet::new(signer))
            .connect_mocked_client(asserter.clone())
            .erased();
        let (event_tx, events) = mpsc::unbounded_channel();
        let (stop, stop_rx) = watch::channel(false);
        let (_fee_tx, fees) = watch::channel(FeeQuote {
            max_fee: 1_000_000_000,
            tip: 1_000_000,
        });
        // Leak the fee sender: the receiver must stay live for the run.
        std::mem::forget(_fee_tx);
        Harness {
            sender: Sender {
                provider,
                address: ACCOUNT,
                window,
                receipt_timeout,
                workload: Arc::new(crate::workloads::Workload::Transfer(Transfer)),
                pacer: Pacer::start(1_000.0),
                fees,
                events: event_tx,
                stop: stop_rx,
            },
            asserter,
            events,
            stop,
        }
    }

    fn drain(events: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        let mut collected = Vec::new();
        while let Ok(event) = events.try_recv() {
            collected.push(event);
        }
        collected
    }

    #[tokio::test]
    async fn a_confirmed_receipt_reports_confirmed_and_needs_no_resync() {
        let mut h = harness(1, Duration::from_millis(50));
        let resync = h.sender.record(Duration::from_millis(10), Some(true));
        assert!(!resync);
        assert!(matches!(
            drain(&mut h.events)[..],
            [Event::Done(Outcome::Confirmed { .. })]
        ));
    }

    #[tokio::test]
    async fn a_status_zero_receipt_reports_reverted_and_needs_no_resync() {
        let mut h = harness(1, Duration::from_millis(50));
        // A revert still consumed the nonce, so the lane stays anchored.
        let resync = h.sender.record(Duration::from_millis(10), Some(false));
        assert!(!resync);
        assert!(matches!(
            drain(&mut h.events)[..],
            [Event::Done(Outcome::Reverted)]
        ));
    }

    #[tokio::test]
    async fn a_missing_receipt_reports_dropped_and_forces_a_resync() {
        let mut h = harness(1, Duration::from_millis(50));
        // The transaction's fate is unknown, so the local nonce is suspect.
        let resync = h.sender.record(Duration::from_millis(10), None);
        assert!(resync);
        assert!(matches!(
            drain(&mut h.events)[..],
            [Event::Done(Outcome::Dropped)]
        ));
    }

    #[tokio::test]
    async fn the_loop_sends_and_then_stops_when_told() {
        let mut h = harness(1, Duration::from_millis(50));
        h.asserter.push_success(&"0x0"); // pending nonce
        h.asserter.push_success(&"0x1"); // chain id
        h.asserter.push_success(&b256!(
            "1111111111111111111111111111111111111111111111111111111111111111"
        ));

        let task = h.sender.spawn();
        // Give the loop time to send, then wait out the receipt timeout so the
        // window drains rather than the task being killed mid-flight.
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.stop.send(true).unwrap();

        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("a stopped sender exits within the drain window")
            .unwrap()
            .unwrap();

        let events = drain(&mut h.events);
        assert!(
            events.iter().any(|e| matches!(e, Event::Sent)),
            "expected a Sent event, got {events:?}"
        );
    }

    #[tokio::test]
    async fn a_sender_that_cannot_read_its_nonce_fails_rather_than_spamming() {
        let h = harness(1, Duration::from_millis(50));
        // Empty asserter queue: the opening nonce fetch errors.
        let error = h.sender.spawn().await.unwrap().unwrap_err();
        assert!(error.to_string().contains(&ACCOUNT.to_string()));
    }

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
