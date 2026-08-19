//! Adaptive concurrency limiter for network scans (AIMD, congestion-control style).
//!
//! Starts at a target concurrency and probes upward while healthy, backing off fast
//! when the source shows stress. "Stress" = an SMB I/O error (a shared counter the
//! SMB layer increments — never bumped by tag-parse failures) or per-file latency
//! climbing well above its observed baseline. NASes happily serve many parallel
//! readers, so it usually settles near the ceiling; a flaky one gets throttled.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Grow the limit by one after this many consecutive healthy completions.
const GROW_AFTER_SUCCESSES: usize = 30;
/// Treat a completion as "stressed" (hold growth) when its latency exceeds the
/// observed minimum by this factor.
const LATENCY_STRESS_FACTOR: u128 = 4;

pub struct AdaptiveLimiter {
    sem: Arc<Semaphore>,
    limit: AtomicUsize,
    min: usize,
    max: usize,
    /// Cumulative SMB I/O errors, shared with the filesystem layer.
    io_errors: Arc<AtomicU64>,
    last_io_errors: AtomicU64,
    successes: AtomicUsize,
    /// Smallest per-file latency seen (ms), the healthy baseline. `u64::MAX` until set.
    min_latency_ms: AtomicU64,
    /// Serializes the limit/semaphore read-modify-write so concurrent `increase`/`decrease`
    /// calls can't lose an update and drift the logical limit from the actual permit count.
    adjust: std::sync::Mutex<()>,
}

impl AdaptiveLimiter {
    pub fn new(initial: usize, min: usize, max: usize, io_errors: Arc<AtomicU64>) -> Arc<Self> {
        let initial = initial.clamp(min, max);
        let sem = Arc::new(Semaphore::new(max));
        // Only `initial` permits available to start; the rest are released as we grow.
        sem.forget_permits(max - initial);
        Arc::new(Self {
            sem,
            limit: AtomicUsize::new(initial),
            min,
            max,
            last_io_errors: AtomicU64::new(io_errors.load(Ordering::Relaxed)),
            io_errors,
            successes: AtomicUsize::new(0),
            min_latency_ms: AtomicU64::new(u64::MAX),
            adjust: std::sync::Mutex::new(()),
        })
    }

    pub fn current(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// Acquire a slot; the returned permit gates one in-flight file read.
    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        // `max` permits exist for the lifetime, so acquire never errors.
        self.sem
            .clone()
            .acquire_owned()
            .await
            .expect("scan semaphore closed")
    }

    /// Record a completed file read and adjust the target concurrency.
    pub fn record(&self, latency: Duration) {
        let errors = self.io_errors.load(Ordering::Relaxed);
        let previous = self.last_io_errors.swap(errors, Ordering::Relaxed);
        if errors > previous {
            self.decrease();
            return;
        }

        let latency_ms = latency.as_millis().max(1);
        let baseline = self
            .min_latency_ms
            .fetch_min(latency_ms as u64, Ordering::Relaxed);
        let stressed =
            baseline != u64::MAX && latency_ms > baseline as u128 * LATENCY_STRESS_FACTOR;
        if stressed {
            // Hold: don't grow while latency is elevated (the source is loaded).
            self.successes.store(0, Ordering::Relaxed);
            return;
        }

        if self.successes.fetch_add(1, Ordering::Relaxed) + 1 >= GROW_AFTER_SUCCESSES {
            self.increase();
            self.successes.store(0, Ordering::Relaxed);
        }
    }

    fn increase(&self) {
        let _guard = self
            .adjust
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.limit.load(Ordering::Relaxed);
        if current < self.max {
            self.sem.add_permits(1);
            self.limit.store(current + 1, Ordering::Relaxed);
        }
    }

    fn decrease(&self) {
        let _guard = self
            .adjust
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.limit.load(Ordering::Relaxed);
        let target = (current / 2).max(self.min);
        if target < current {
            self.sem.forget_permits(current - target);
            self.limit.store(target, Ordering::Relaxed);
        }
        self.successes.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn grows_when_healthy_and_backs_off_on_errors() {
        let io_errors = Arc::new(AtomicU64::new(0));
        let limiter = AdaptiveLimiter::new(10, 4, 50, io_errors.clone());
        assert_eq!(limiter.current(), 10);

        // Healthy completions grow the limit (one step per GROW_AFTER_SUCCESSES).
        for _ in 0..GROW_AFTER_SUCCESSES {
            limiter.record(Duration::from_millis(5));
        }
        assert_eq!(limiter.current(), 11);

        // An I/O error halves it (down toward the floor).
        io_errors.fetch_add(1, Ordering::Relaxed);
        limiter.record(Duration::from_millis(5));
        assert_eq!(limiter.current(), 5);

        // Never drops below the floor.
        io_errors.fetch_add(1, Ordering::Relaxed);
        limiter.record(Duration::from_millis(5));
        assert_eq!(limiter.current(), 4);
    }

    #[tokio::test]
    async fn never_exceeds_max() {
        let io_errors = Arc::new(AtomicU64::new(0));
        let limiter = AdaptiveLimiter::new(49, 4, 50, io_errors);
        for _ in 0..(GROW_AFTER_SUCCESSES * 5) {
            limiter.record(Duration::from_millis(1));
        }
        assert_eq!(limiter.current(), 50);
    }
}
