use std::{sync::Arc, time::Duration};

use tokio::sync::Semaphore;

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
            let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / tps));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // The first tick fires immediately; skip it — the bucket starts full.
            tick.tick().await;
            loop {
                tick.tick().await;
                if refill.permits.available_permits() < burst {
                    refill.permits.add_permits(1);
                }
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

        // The next permit needs a refill tick: at 10 tps that is 100ms away.
        let start = tokio::time::Instant::now();
        pacer.acquire().await;
        assert!(start.elapsed() >= Duration::from_millis(90));
        assert!(start.elapsed() <= Duration::from_millis(250));
    }
}
