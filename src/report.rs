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

    /// Non-zero when any transaction failed to land cleanly.
    pub fn exit_code(&self) -> i32 {
        let all_landed = self.confirmed == self.sent;
        let clean = self.reverted == 0 && self.dropped == 0 && self.rejected == 0;
        if all_landed && clean { 0 } else { 1 }
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

    pub fn print(&self, elapsed: Duration) {
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
                self.confirmed as f64 / elapsed.as_secs_f64().max(0.001)
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
        let mut confirmed_at_last_tick = 0;
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.tick().await; // the first tick fires immediately; skip it
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => summary.record(event),
                    None => break,
                },
                _ = tick.tick() => {
                    let interval_tps = (summary.confirmed - confirmed_at_last_tick) as f64 / 5.0;
                    confirmed_at_last_tick = summary.confirmed;
                    info!(
                        sent = summary.sent,
                        confirmed = summary.confirmed,
                        reverted = summary.reverted,
                        dropped = summary.dropped,
                        rejected = summary.rejected,
                        tps = format!("{interval_tps:.1}"),
                        "status"
                    );
                }
            }
        }
        summary.print(started.elapsed());
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
