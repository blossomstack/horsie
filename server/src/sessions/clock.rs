//! An injectable clock, so idle-offload behaviour is testable without sleeping.
//!
//! The idle timer decides when a session is unloaded and its runtime
//! hibernated. Tested against the wall clock, every one of those tests would
//! either take the whole timeout or be a race; with this, they take
//! microseconds and are exact.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The real clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A clock that only moves when a test says so.
pub struct TestClock {
    base: Instant,
    offset: Mutex<Duration>,
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl TestClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Mutex::new(Duration::ZERO),
        }
    }

    /// Move time forward.
    pub fn advance(&self, by: Duration) {
        let mut offset = self.offset.lock().unwrap_or_else(|e| e.into_inner());
        *offset += by;
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        let offset = *self.offset.lock().unwrap_or_else(|e| e.into_inner());
        self.base + offset
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_test_clock_moves_only_when_told() {
        let clock = TestClock::new();
        let t0 = clock.now();
        assert_eq!(clock.now(), t0, "time must not pass on its own");
        clock.advance(Duration::from_secs(600));
        assert!(clock.now() > t0);
        assert_eq!(clock.now().duration_since(t0), Duration::from_secs(600));
    }
}
