//! Byte-rate limiter shared by all transfers of one direction (upload or download).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Token bucket in bytes. A limit of 0 means unlimited; the limit can be
/// changed at runtime from the settings.
pub struct RateLimiter {
    limit_bps: AtomicU64,
    state: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(limit_bytes_per_sec: u64) -> Self {
        Self {
            limit_bps: AtomicU64::new(limit_bytes_per_sec),
            state: Mutex::new(Bucket { tokens: 0.0, last: Instant::now() }),
        }
    }

    pub fn unlimited() -> Self {
        Self::new(0)
    }

    pub fn set_limit(&self, bytes_per_sec: u64) {
        self.limit_bps.store(bytes_per_sec, Ordering::Relaxed);
    }

    pub fn limit(&self) -> u64 {
        self.limit_bps.load(Ordering::Relaxed)
    }

    /// Wait until `bytes` may be transferred.
    pub async fn acquire(&self, bytes: usize) {
        let limit = self.limit();
        if limit == 0 || bytes == 0 {
            return;
        }
        let wait = {
            let mut b = self.state.lock().await;
            let now = Instant::now();
            let elapsed = now.duration_since(b.last).as_secs_f64();
            b.last = now;
            // Allow at most one second of burst.
            b.tokens = (b.tokens + elapsed * limit as f64).min(limit as f64);
            b.tokens -= bytes as f64;
            if b.tokens >= 0.0 { None } else { Some(Duration::from_secs_f64(-b.tokens / limit as f64)) }
        };
        if let Some(d) = wait {
            tokio::time::sleep(d).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn limits_throughput() {
        let rl = RateLimiter::new(1000);
        let start = Instant::now();
        for _ in 0..5 {
            rl.acquire(1000).await;
        }
        // 5000 bytes at 1000 B/s with an empty initial bucket ≈ 5 s.
        let secs = start.elapsed().as_secs_f64();
        assert!((4.5..=5.5).contains(&secs), "took {secs}");

        let free = RateLimiter::unlimited();
        let t = Instant::now();
        free.acquire(10_000_000).await;
        assert!(t.elapsed() < Duration::from_millis(1));
    }
}
