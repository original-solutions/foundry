//! The clock that counts through sleep: every age and wait the factory measures.
//!
//! Rust's [`std::time::Instant`] is not used, and neither is `tokio::time`, which is built on
//! it. On macOS `Instant` reads `CLOCK_UPTIME_RAW`, which stops while the machine sleeps, so a
//! Mac that slept for an hour would wake believing its lease, a cut-off or an unreachable
//! machine had aged by none of it. The factory's ages must count that hour: a Mac that slept
//! through its lease wakes with it expired. [`SystemClock`] therefore reads a clock that keeps
//! counting through sleep, `CLOCK_MONOTONIC` on macOS and `CLOCK_BOOTTIME` on Linux (where
//! `CLOCK_MONOTONIC` stops during suspend). Both are monotonic, so a wall clock stepped back
//! moves no age.
//!
//! A reading is the time since an arbitrary start on this machine. It means nothing on
//! another machine, is never stored in a record and is never compared with another
//! machine's: only differences between two readings of the same clock are ages.
//!
//! Code that measures an age takes an [`Arc<dyn Clock>`]: [`system()`] in the daemon, a
//! [`FakeClock`] that the test moves in tests.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nix::time::{ClockId, clock_gettime};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("foundryd knows a clock that counts through sleep only on Linux and macOS");

/// A clock whose readings never go back. Only the difference between two readings means
/// anything.
pub trait Clock: Send + Sync {
    /// The time since this clock's arbitrary start.
    fn now(&self) -> Duration;
}

/// This machine's clock that keeps counting while it sleeps.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl SystemClock {
    /// The clock read: one that counts through sleep on this OS.
    #[cfg(target_os = "linux")]
    pub const ID: ClockId = ClockId::CLOCK_BOOTTIME;
    /// The clock read: one that counts through sleep on this OS.
    #[cfg(target_os = "macos")]
    pub const ID: ClockId = ClockId::CLOCK_MONOTONIC;
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        // The call fails only for a clock id the kernel does not know, and both ids above
        // have been known for over a decade on the OSes they are read on.
        let now = clock_gettime(Self::ID).expect("the sleep-counting clock can be read");
        Duration::from(now)
    }
}

/// The daemon's clock.
pub fn system() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}

/// A clock that stands still until it is told to move, for tests. It starts at zero.
#[derive(Debug, Default)]
pub struct FakeClock {
    now: Mutex<Duration>,
}

impl FakeClock {
    /// A clock at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the clock forward by `by`. It stops at [`Duration::MAX`] rather than wrap back.
    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        *now = now.saturating_add(by);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use nix::time::ClockId;

    use super::{Clock, FakeClock, SystemClock, system};

    #[test]
    fn two_reads_of_the_system_clock_never_go_back() {
        let clock = system();
        let mut last = clock.now();
        for _ in 0..10_000 {
            let now = clock.now();
            assert!(now >= last, "{now:?} is before {last:?}");
            last = now;
        }
    }

    #[test]
    fn the_system_clock_moves_forward_while_time_passes() {
        let clock = system();
        let before = clock.now();
        std::thread::sleep(Duration::from_millis(20));
        assert!(clock.now() - before >= Duration::from_millis(20));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reads_the_boot_time_clock() {
        assert_eq!(SystemClock::ID, ClockId::CLOCK_BOOTTIME);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reads_the_monotonic_clock() {
        assert_eq!(SystemClock::ID, ClockId::CLOCK_MONOTONIC);
    }

    #[test]
    fn the_system_clock_reads_the_clock_it_names() {
        let named = Duration::from(nix::time::clock_gettime(SystemClock::ID).unwrap());
        let read = SystemClock.now();
        let named_after = Duration::from(nix::time::clock_gettime(SystemClock::ID).unwrap());
        assert!(named <= read && read <= named_after);
    }

    #[test]
    fn a_fake_clock_starts_at_zero_and_stands_still() {
        let clock = FakeClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
        assert_eq!(clock.now(), Duration::ZERO);
    }

    #[test]
    fn a_fake_clock_advances_by_what_it_is_told() {
        let clock = FakeClock::new();
        clock.advance(Duration::from_secs(60));
        assert_eq!(clock.now(), Duration::from_secs(60));
        clock.advance(Duration::from_millis(1_500));
        assert_eq!(clock.now(), Duration::from_millis(61_500));
        clock.advance(Duration::ZERO);
        assert_eq!(clock.now(), Duration::from_millis(61_500));
    }

    #[test]
    fn a_fake_clock_moved_by_the_test_is_seen_through_the_trait() {
        let fake = Arc::new(FakeClock::new());
        let clock: Arc<dyn Clock> = fake.clone();
        fake.advance(Duration::from_secs(180));
        assert_eq!(clock.now(), Duration::from_secs(180));
    }

    #[test]
    fn a_fake_clock_never_goes_back_at_its_limit() {
        let clock = FakeClock::new();
        clock.advance(Duration::MAX);
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.now(), Duration::MAX);
    }
}
