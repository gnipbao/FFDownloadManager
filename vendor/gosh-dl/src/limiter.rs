//! Byte-rate limiting shared by all transfer paths.
//!
//! A single [`RateLimiter`] instance can be shared (via `Arc`) by any number
//! of concurrent transfer tasks; the aggregate throughput converges on the
//! configured limit. The engine owns one global pair (download/upload) that
//! HTTP segments, single-stream HTTP, torrent peers, and webseeds all draw
//! from, plus optional per-download instances layered on top.
//!
//! The implementation is a debt-based token bucket: an [`acquire`] call books
//! the bytes immediately (the bucket may go negative) and then sleeps until
//! the debt is repaid at the configured rate. This handles chunks of any size
//! at any rate — including chunks larger than one second's budget — which the
//! previous `governor`-based approach could not (chunks above the burst size
//! were silently let through, so limits under 16 KiB/s did nothing).
//!
//! [`acquire`]: RateLimiter::acquire

use parking_lot::Mutex;
use std::time::Duration;
use tokio::time::Instant;

/// Maximum burst: how many bytes may be sent instantly after an idle period.
/// One second's worth of budget, floored so tiny limits still make progress.
const MIN_BURST: f64 = 8.0 * 1024.0;

/// Cap on any single sleep so limit changes take effect promptly.
const MAX_SLEEP: Duration = Duration::from_millis(500);

#[derive(Debug)]
struct Inner {
    /// Bytes per second; `None` = unlimited.
    rate: Option<f64>,
    /// Current bucket level in bytes. Negative = debt to repay before the
    /// next acquire may proceed.
    tokens: f64,
    /// Last refill timestamp.
    last_refill: Instant,
}

/// A shareable byte-rate limiter. See the module docs.
#[derive(Debug)]
pub struct RateLimiter {
    inner: Mutex<Inner>,
}

impl RateLimiter {
    /// Create a limiter. `limit` is bytes/second; `None` or `Some(0)` means
    /// unlimited.
    pub fn new(limit: Option<u64>) -> Self {
        let rate = normalize(limit);
        Self {
            inner: Mutex::new(Inner {
                rate,
                tokens: rate.map(burst_for).unwrap_or(0.0),
                last_refill: Instant::now(),
            }),
        }
    }

    /// Replace the limit. `None` or `Some(0)` means unlimited. Takes effect
    /// for acquires from now on; sleepers re-check within `MAX_SLEEP` (500ms).
    pub fn set_limit(&self, limit: Option<u64>) {
        let rate = normalize(limit);
        let mut inner = self.inner.lock();
        if inner.rate == rate {
            return;
        }
        inner.rate = rate;
        // Reset accumulated state so an old debt (or hoard) from a very
        // different rate doesn't distort the new one.
        inner.tokens = rate.map(burst_for).unwrap_or(0.0);
        inner.last_refill = Instant::now();
    }

    /// Current limit in bytes/second (`None` = unlimited).
    pub fn limit(&self) -> Option<u64> {
        self.inner.lock().rate.map(|r| r as u64)
    }

    /// Account for `bytes` of transfer, sleeping as needed to keep the
    /// aggregate rate at the limit. The bytes are booked immediately, so
    /// calling this either before or after the actual I/O is fine.
    pub async fn acquire(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        {
            let mut inner = self.inner.lock();
            let Some(rate) = inner.rate else { return };
            let now = Instant::now();
            let elapsed = now.duration_since(inner.last_refill).as_secs_f64();
            inner.last_refill = now;
            inner.tokens = (inner.tokens + elapsed * rate).min(burst_for(rate));
            inner.tokens -= bytes as f64;
            if inner.tokens >= 0.0 {
                return;
            }
        }
        self.wait_out_debt().await;
    }

    /// Sleep until the bucket is non-negative (debt already booked).
    async fn wait_out_debt(&self) {
        loop {
            let wait = {
                let mut inner = self.inner.lock();
                let Some(rate) = inner.rate else { return };
                let now = Instant::now();
                let elapsed = now.duration_since(inner.last_refill).as_secs_f64();
                inner.last_refill = now;
                inner.tokens = (inner.tokens + elapsed * rate).min(burst_for(rate));
                if inner.tokens >= 0.0 {
                    return;
                }
                Duration::from_secs_f64(-inner.tokens / rate)
            };
            tokio::time::sleep(wait.min(MAX_SLEEP)).await;
        }
    }
}

fn normalize(limit: Option<u64>) -> Option<f64> {
    match limit {
        None | Some(0) => None,
        Some(n) => Some(n as f64),
    }
}

fn burst_for(rate: f64) -> f64 {
    rate.max(MIN_BURST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn unlimited_never_waits() {
        let limiter = RateLimiter::new(None);
        let start = Instant::now();
        limiter.acquire(100 * 1024 * 1024).await;
        assert!(start.elapsed() < Duration::from_millis(50));

        let limiter = RateLimiter::new(Some(0));
        limiter.acquire(100 * 1024 * 1024).await;
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test(start_paused = true)]
    async fn limits_below_chunk_size_are_enforced() {
        // 4 KiB/s with a 16 KiB chunk: the old governor implementation waved
        // any chunk above the burst straight through. After the 8 KiB initial
        // burst, a 16 KiB chunk leaves 8 KiB of debt, i.e. ~2s at 4 KiB/s.
        let limiter = RateLimiter::new(Some(4 * 1024));
        let start = Instant::now();
        limiter.acquire(16 * 1024).await;
        assert!(
            start.elapsed() >= Duration::from_millis(1500),
            "sub-chunk-size limit was not enforced (waited {:?})",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn aggregate_rate_converges() {
        // Two concurrent writers sharing one limiter at 100 KiB/s: pushing
        // 200 KiB total (beyond the 100 KiB burst) must take ~1s of virtual
        // time, not complete instantly.
        let limiter = Arc::new(RateLimiter::new(Some(100 * 1024)));
        let start = tokio::time::Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let limiter = Arc::clone(&limiter);
            tasks.push(tokio::spawn(async move {
                for _ in 0..10 {
                    limiter.acquire(10 * 1024).await;
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "200 KiB at 100 KiB/s finished too fast: {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_secs(3),
            "200 KiB at 100 KiB/s took too long: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn raising_limit_releases_sleepers_promptly() {
        let limiter = Arc::new(RateLimiter::new(Some(1024)));
        // Book a huge debt at 1 KiB/s (~64s of debt)...
        let waiter = {
            let limiter = Arc::clone(&limiter);
            tokio::spawn(async move {
                limiter.acquire(64 * 1024).await;
                limiter.acquire(1).await;
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        // ...then lift the limit; the sleeper must finish within ~MAX_SLEEP.
        limiter.set_limit(None);
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("sleeper did not react to limit change")
            .unwrap();
    }
}
