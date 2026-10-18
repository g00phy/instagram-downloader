//! Global bandwidth cap shared by every transfer of a [`super::Downloader`].

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// Upper bound for a single wait, so absurd inputs cannot overflow a `Duration`.
const MAX_WAIT: Duration = Duration::from_secs(3600);

/// Token bucket that lets callers go into debt: every caller takes its bytes
/// immediately and sleeps until the bucket is back at zero. Concurrent callers
/// queue up behind each other's debt, so the aggregate rate stays at or below
/// the cap.
#[derive(Debug)]
pub(super) struct RateLimiter {
    /// Bytes per second.
    rate: f64,
    /// Most credit an idle period can build up (a quarter second of traffic).
    burst: f64,
    bucket: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// A limiter for `bytes_per_sec` (must be > 0).
    pub(super) fn new(bytes_per_sec: u64) -> Self {
        let rate = bytes_per_sec.max(1) as f64;
        RateLimiter {
            rate,
            burst: rate / 4.0,
            bucket: Mutex::new(Bucket {
                tokens: 0.0,
                last: Instant::now(),
            }),
        }
    }

    /// Accounts for `bytes` just transferred and sleeps as long as needed to keep
    /// the aggregate rate under the cap.
    pub(super) async fn acquire(&self, bytes: usize) {
        let wait = {
            let mut bucket = self.bucket.lock().await;
            let now = Instant::now();
            let elapsed = now.duration_since(bucket.last).as_secs_f64();
            bucket.last = now;
            bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.burst);
            bucket.tokens -= bytes as f64;
            if bucket.tokens >= 0.0 {
                Duration::ZERO
            } else {
                Duration::try_from_secs_f64(-bucket.tokens / self.rate)
                    .unwrap_or(MAX_WAIT)
                    .min(MAX_WAIT)
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waits_for_debt() {
        let limiter = RateLimiter::new(20_000);
        let start = Instant::now();
        limiter.acquire(2_000).await;
        limiter.acquire(8_000).await;
        // 10 000 bytes at 20 000 B/s.
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(450), "{elapsed:?}");
    }

    #[tokio::test]
    async fn shared_between_callers() {
        let limiter = std::sync::Arc::new(RateLimiter::new(40_000));
        let start = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let limiter = limiter.clone();
            tasks.push(tokio::spawn(async move {
                for _ in 0..10 {
                    limiter.acquire(1_000).await;
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        // 20 000 bytes at 40 000 B/s.
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(450), "{elapsed:?}");
    }
}
