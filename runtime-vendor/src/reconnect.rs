//! The delay schedule a vendor agent waits out between connection attempts.
//!
//! Split out as a value rather than left inline in the reconnect loop because
//! the schedule is the part with rules worth pinning down — it grows, it is
//! capped, and it starts over once a link was real. A loop that computed its
//! own delays could only be tested by sleeping through them.

use std::time::Duration;

/// Exponential backoff with a ceiling, advanced one attempt at a time.
///
/// Pure: it never sleeps and never reads the clock, so the caller decides how
/// a delay is waited out (and can abandon the wait on cancellation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    next: Duration,
}

impl Backoff {
    /// Short enough that a server restarting under a process manager is barely
    /// noticed.
    pub const DEFAULT_BASE: Duration = Duration::from_secs(1);
    /// Long enough that an agent left pointing at a server that is gone for the
    /// weekend costs nothing, short enough that a human waiting on it does not
    /// give up first.
    pub const DEFAULT_CAP: Duration = Duration::from_secs(30);

    /// A cap below the base would make the first wait longer than the last, so
    /// it is raised to the base rather than rejected — there is no useful
    /// failure for a caller to handle here.
    #[must_use]
    pub fn new(base: Duration, cap: Duration) -> Self {
        let cap = cap.max(base);
        Self {
            base,
            cap,
            next: base,
        }
    }

    /// How long to wait before the next attempt, doubling each call up to the
    /// cap.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.cap);
        delay
    }

    /// Start the schedule over.
    ///
    /// Called once a connection got far enough to be real: a link that
    /// handshook and later died is a fresh incident, not the continuation of
    /// whatever streak preceded it, and making it wait out an inherited 30s is
    /// how a one-second server restart turns into a half-minute outage.
    pub fn reset(&mut self) {
        self.next = self.base;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Self::DEFAULT_BASE, Self::DEFAULT_CAP)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    fn secs(backoff: &mut Backoff, n: usize) -> Vec<u64> {
        (0..n).map(|_| backoff.next_delay().as_secs()).collect()
    }

    #[test]
    fn the_delay_doubles_from_the_base_and_stops_at_the_cap() {
        let mut backoff = Backoff::default();
        assert_eq!(secs(&mut backoff, 8), vec![1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn a_reconnect_that_worked_starts_the_schedule_over() {
        let mut backoff = Backoff::default();
        let _ = secs(&mut backoff, 5);
        backoff.reset();
        assert_eq!(
            secs(&mut backoff, 3),
            vec![1, 2, 4],
            "a link that handshook must not inherit the previous streak's delay"
        );
    }

    #[test]
    fn the_first_delay_is_always_the_base() {
        let mut backoff = Backoff::new(Duration::from_millis(20), Duration::from_millis(100));
        assert_eq!(backoff.next_delay(), Duration::from_millis(20));
    }

    #[test]
    fn a_cap_below_the_base_never_shortens_the_first_wait() {
        let mut backoff = Backoff::new(Duration::from_secs(5), Duration::from_secs(1));
        assert_eq!(secs(&mut backoff, 3), vec![5, 5, 5]);
    }
}
