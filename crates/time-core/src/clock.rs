//! Wall-clock time behind a trait, so a scripted day can run in seconds.
//!
//! Production code reads [`SystemClock`]. The end-to-end harness hands the
//! same code a [`ManualClock`] and advances it minute by minute, which is
//! what makes a whole day of simulated activity deterministic.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// Milliseconds since the Unix epoch.
pub type Millis = i64;

pub const MINUTE_MS: Millis = 60_000;

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> Millis;
}

/// The real clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> Millis {
        chrono::Utc::now().timestamp_millis()
    }
}

/// A clock that only moves when told to. Clones share the same time.
#[derive(Debug, Clone)]
pub struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    pub fn new(start_ms: Millis) -> Self {
        Self(Arc::new(AtomicI64::new(start_ms)))
    }

    pub fn set(&self, ms: Millis) {
        self.0.store(ms, Ordering::SeqCst);
    }

    pub fn advance(&self, ms: Millis) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> Millis {
        self.0.load(Ordering::SeqCst)
    }
}

/// The start of the minute containing `ms`.
pub fn floor_minute(ms: Millis) -> Millis {
    ms.div_euclid(MINUTE_MS) * MINUTE_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_is_shared_between_clones() {
        let a = ManualClock::new(1_000);
        let b = a.clone();
        a.advance(MINUTE_MS);
        assert_eq!(b.now_ms(), 61_000);
    }

    #[test]
    fn floor_minute_handles_negative_and_exact() {
        assert_eq!(floor_minute(120_000), 120_000);
        assert_eq!(floor_minute(179_999), 120_000);
        assert_eq!(floor_minute(-1), -60_000);
    }
}
