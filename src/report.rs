use std::time::{Duration, Instant};

use tokio::{sync::mpsc, task::JoinHandle};
use tracing::info;

/// Terminal state of one transaction.
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    /// Landed with receipt status 1.
    Confirmed { latency: Duration },
    /// Landed with receipt status 0.
    Reverted,
    /// No receipt within the timeout.
    Dropped,
    /// The node rejected it at submission.
    Rejected,
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    Sent,
    Done(Outcome),
}

/// Aggregated results of a run; the process exit code derives from it.
#[derive(Debug, Default)]
pub struct Summary {
    pub sent: u64,
    pub confirmed: u64,
    pub reverted: u64,
    pub dropped: u64,
    pub rejected: u64,
    latencies: Vec<Duration>,
}

impl Summary {
    fn record(&mut self, event: Event) {
        match event {
            Event::Sent => self.sent += 1,
            Event::Done(Outcome::Confirmed { latency }) => {
                self.confirmed += 1;
                self.latencies.push(latency);
            }
            Event::Done(Outcome::Reverted) => self.reverted += 1,
            Event::Done(Outcome::Dropped) => self.dropped += 1,
            Event::Done(Outcome::Rejected) => self.rejected += 1,
        }
    }

    /// Non-zero when any transaction failed to land cleanly — or none were
    /// sent at all, which for a spammer is a failure, not a clean run.
    pub fn exit_code(&self) -> i32 {
        let clean = self.reverted == 0 && self.dropped == 0 && self.rejected == 0;
        if self.sent > 0 && self.confirmed == self.sent && clean {
            0
        } else {
            1
        }
    }

    pub fn latency_percentile(&self, percentile: f64) -> Option<Duration> {
        if self.latencies.is_empty() {
            return None;
        }
        let mut sorted = self.latencies.clone();
        sorted.sort();
        let rank = (percentile / 100.0 * (sorted.len() - 1) as f64).round() as usize;
        Some(sorted[rank])
    }

    /// Prints the summary. `sending` is the span from the first to the last
    /// submission — the drain that follows is excluded, since dividing by it
    /// would understate the rate exactly when a stressed chain makes the drain
    /// long.
    pub fn print(&self, sending: Duration) {
        let fmt_latency = |p| match self.latency_percentile(p) {
            Some(d) => format!("{:.2}s", d.as_secs_f64()),
            None => "-".into(),
        };
        info!(
            sent = self.sent,
            confirmed = self.confirmed,
            reverted = self.reverted,
            dropped = self.dropped,
            rejected = self.rejected,
            tps = format!(
                "{:.1}",
                self.confirmed as f64 / sending.as_secs_f64().max(0.001)
            ),
            p50 = fmt_latency(50.0),
            p95 = fmt_latency(95.0),
            "run finished"
        );
    }
}

/// Consumes events until all senders hang up, printing a status line every 5s.
pub fn spawn(mut events: mpsc::UnboundedReceiver<Event>) -> JoinHandle<Summary> {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut summary = Summary::default();
        const STATUS_INTERVAL: Duration = Duration::from_secs(5);
        let mut confirmed_at_last_tick = 0;
        let mut last_tick = started;
        let mut first_sent: Option<Instant> = None;
        let mut last_sent = started;
        let mut tick = tokio::time::interval(STATUS_INTERVAL);
        tick.tick().await; // the first tick fires immediately; skip it
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => {
                        if matches!(event, Event::Sent) {
                            first_sent.get_or_insert_with(Instant::now);
                            last_sent = Instant::now();
                        }
                        summary.record(event);
                    }
                    None => break,
                },
                _ = tick.tick() => {
                    // Measure the real gap: a late tick would otherwise be
                    // divided by the nominal interval and dip the reported rate
                    // for reasons that have nothing to do with the chain.
                    let now = Instant::now();
                    let interval_tps = (summary.confirmed - confirmed_at_last_tick) as f64
                        / now.duration_since(last_tick).as_secs_f64().max(0.001);
                    confirmed_at_last_tick = summary.confirmed;
                    last_tick = now;
                    info!(
                        sent = summary.sent,
                        confirmed = summary.confirmed,
                        // Everything sent but not yet resolved. A run that is
                        // outrunning the chain shows this climbing long before
                        // the receipt timeout turns it into drops.
                        in_flight = summary.sent
                            - summary.confirmed
                            - summary.reverted
                            - summary.dropped,
                        reverted = summary.reverted,
                        dropped = summary.dropped,
                        rejected = summary.rejected,
                        tps = format!("{interval_tps:.1}"),
                        "status"
                    );
                }
            }
        }
        let sending = match first_sent {
            Some(first) => last_sent.duration_since(first),
            None => Duration::ZERO,
        };
        summary.print(sending);
        summary
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirmed(ms: u64) -> Event {
        Event::Done(Outcome::Confirmed {
            latency: Duration::from_millis(ms),
        })
    }

    #[test]
    fn clean_run_exits_zero() {
        let mut s = Summary::default();
        for event in [Event::Sent, Event::Sent, confirmed(100), confirmed(200)] {
            s.record(event);
        }
        assert_eq!(s.exit_code(), 0);
        assert_eq!(s.confirmed, 2);
    }

    #[test]
    fn any_failure_exits_nonzero() {
        for failure in [Outcome::Reverted, Outcome::Dropped, Outcome::Rejected] {
            let mut s = Summary::default();
            for event in [
                Event::Sent,
                Event::Sent,
                confirmed(100),
                Event::Done(failure),
            ] {
                s.record(event);
            }
            assert_eq!(s.exit_code(), 1, "{failure:?} must fail the run");
        }
    }

    #[test]
    fn unlanded_sends_exit_nonzero() {
        let mut s = Summary::default();
        for event in [Event::Sent, Event::Sent, confirmed(100)] {
            s.record(event);
        }
        assert_eq!(s.exit_code(), 1);
    }

    #[test]
    fn empty_run_exits_nonzero() {
        assert_eq!(Summary::default().exit_code(), 1);
    }

    #[test]
    fn percentiles() {
        let mut s = Summary::default();
        assert_eq!(s.latency_percentile(50.0), None);
        for ms in [100, 200, 300, 400, 500] {
            s.record(confirmed(ms));
        }
        assert_eq!(s.latency_percentile(50.0), Some(Duration::from_millis(300)));
        assert_eq!(s.latency_percentile(0.0), Some(Duration::from_millis(100)));
        assert_eq!(
            s.latency_percentile(100.0),
            Some(Duration::from_millis(500))
        );
    }
}
