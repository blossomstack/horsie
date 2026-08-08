//! Throttles password guessing by *delaying failures*, never by locking the
//! account.
//!
//! Per-IP lockout is the textbook answer and is wrong here: behind a reverse
//! proxy (Caddy, fly.io) every request arrives from the proxy's address, so one
//! bucket covers everybody and an attacker denies the admin their own server by
//! guessing wrong on purpose. Delaying failures throttles guessing at the same
//! rate without ever refusing the person who knows the password — a correct
//! password is answered immediately no matter how many failures preceded it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const FREE_ATTEMPTS: u32 = 3;
const MAX_DELAY_SECS: u64 = 30;

pub struct Throttle {
    consecutive_failures: AtomicU32,
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new()
    }
}

impl Throttle {
    pub fn new() -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
        }
    }

    /// How long to wait before answering the *next* failed attempt.
    pub fn delay(&self) -> Duration {
        let n = self.consecutive_failures.load(Ordering::Relaxed);
        if n < FREE_ATTEMPTS {
            return Duration::ZERO;
        }
        let steps = n - FREE_ATTEMPTS;
        let secs = 2u64
            .checked_pow(steps + 1)
            .unwrap_or(MAX_DELAY_SECS)
            .min(MAX_DELAY_SECS);
        Duration::from_secs(secs)
    }

    pub fn record_failure(&self) {
        // Saturating: an attacker running forever must not wrap the counter
        // back into the free-attempt range.
        let _ = self
            .consecutive_failures
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            });
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_first_three_failures_are_not_delayed() {
        let t = Throttle::new();
        for _ in 0..3 {
            assert_eq!(t.delay(), Duration::ZERO);
            t.record_failure();
        }
        assert_eq!(t.delay(), Duration::from_secs(2));
    }

    #[test]
    fn the_delay_doubles_and_stops_at_thirty_seconds() {
        let t = Throttle::new();
        for _ in 0..3 {
            t.record_failure();
        }
        let seen: Vec<u64> = (0..7)
            .map(|_| {
                let d = t.delay().as_secs();
                t.record_failure();
                d
            })
            .collect();
        assert_eq!(seen, vec![2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn a_success_clears_the_delay() {
        let t = Throttle::new();
        for _ in 0..6 {
            t.record_failure();
        }
        assert!(t.delay() > Duration::ZERO);
        t.record_success();
        assert_eq!(t.delay(), Duration::ZERO);
    }
}
