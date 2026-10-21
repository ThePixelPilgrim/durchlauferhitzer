//! Monotonic time as seen by the core.
//!
//! The core never reads a clock: the driver passes the current time with every
//! event. `Instant` is a plain count of nanoseconds on an arbitrary monotonic
//! epoch chosen by the driver (the simulation starts at zero, the epoll driver
//! uses `CLOCK_MONOTONIC`). Only differences and ordering are meaningful.

use std::time::Duration;

/// A point in time: nanoseconds since the driver's monotonic epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(u64);

impl Instant {
    /// The instant `nanos` nanoseconds after the epoch.
    pub const fn from_nanos(nanos: u64) -> Instant {
        Instant(nanos)
    }

    /// Nanoseconds since the epoch.
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// `self + by`, saturating at the largest representable instant.
    pub fn saturating_add(self, by: Duration) -> Instant {
        let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        Instant(self.0.saturating_add(by))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturating_add_adds_and_saturates() {
        let t = Instant::from_nanos(10);
        assert_eq!(t.saturating_add(Duration::from_nanos(5)).as_nanos(), 15);
        assert_eq!(
            Instant::from_nanos(u64::MAX - 1)
                .saturating_add(Duration::from_secs(1))
                .as_nanos(),
            u64::MAX
        );
        assert_eq!(t.saturating_add(Duration::MAX).as_nanos(), u64::MAX);
    }

    #[test]
    fn instants_order_by_time() {
        assert!(Instant::from_nanos(1) < Instant::from_nanos(2));
    }
}
