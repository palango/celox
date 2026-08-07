use std::{sync::Arc, time::Duration};

// Tokio's clock, not the system one, so the refill keeps working under a paused
// test runtime.
use tokio::{sync::Semaphore, time::Instant};

/// How often the bucket is topped up. Tokio's timer has millisecond
/// granularity, so a tick per transaction would cap the achievable rate at
/// about a thousand per second; refilling by elapsed time instead lets one
/// tick carry as many permits as the rate has earned.
const REFILL_INTERVAL: Duration = Duration::from_millis(10);

/// Global token bucket shared by all senders: one permit per transaction,
/// refilled at the target rate, with at most one second of burst.
pub struct Pacer {
    permits: Semaphore,
}

impl Pacer {
    /// Starts the refill task and returns the shared bucket.
    pub fn start(tps: f64) -> Arc<Self> {
        let burst = tps.ceil().max(1.0) as usize;
        let pacer = Arc::new(Self {
            permits: Semaphore::new(burst),
        });
        let refill = Arc::clone(&pacer);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REFILL_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // The first tick fires immediately; skip it — the bucket starts full.
            tick.tick().await;
            let mut last = Instant::now();
            // Whole permits are handed out, so carry the remainder forward
            // instead of rounding it away on every tick.
            let mut earned = 0.0;
            loop {
                tick.tick().await;
                let now = Instant::now();
                earned += now.duration_since(last).as_secs_f64() * tps;
                last = now;

                let available = refill.permits.available_permits();
                let room = burst.saturating_sub(available);
                let granting = (earned as usize).min(room);
                if granting > 0 {
                    refill.permits.add_permits(granting);
                    earned -= granting as f64;
                }
                // Never bank more than the burst allowance: an idle consumer
                // must not buy the right to a later flood.
                earned = earned.min(burst as f64);
            }
        });
        pacer
    }

    /// Waits until the rate allows one more transaction.
    pub async fn acquire(&self) {
        self.permits
            .acquire()
            .await
            .expect("pacer semaphore is never closed")
            .forget();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn refills_at_target_rate() {
        let pacer = Pacer::start(10.0);

        // Drain the initial burst allowance.
        for _ in 0..10 {
            pacer.acquire().await;
        }

        // The next permit needs a refill: at 10 tps that is 100ms away.
        let start = tokio::time::Instant::now();
        pacer.acquire().await;
        assert!(start.elapsed() >= Duration::from_millis(90));
        assert!(start.elapsed() <= Duration::from_millis(250));
    }

    #[tokio::test(start_paused = true)]
    async fn sustains_rates_above_the_timer_granularity() {
        // A tick-per-permit design caps out near 1000/s; 5000 must be servable.
        let pacer = Pacer::start(5_000.0);
        for _ in 0..5_000 {
            pacer.acquire().await;
        }

        let start = tokio::time::Instant::now();
        for _ in 0..5_000 {
            pacer.acquire().await;
        }
        // A second batch of 5000 takes about a second, not five.
        assert!(start.elapsed() <= Duration::from_millis(1_500));
    }
}
