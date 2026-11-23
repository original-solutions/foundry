//! The clock that counts through sleep: every age and wait the factory measures.

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
        let clock = SystemClock;
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
