//! Delta math for sensors that are rates of a monotonic counter.
//!
//! Every rate is computed from the counter difference and the time that
//! really passed between two reads, not from the nominal sampling period:
//! the hub may be late, and an on-demand read may squeeze in between ticks.

use std::time::{Duration, Instant};

/// Shortest interval a rate is computed over. Two reads closer together than
/// this would turn counter granularity into wild rates, so the second one
/// reports nothing and the baseline stays where it was.
pub(crate) const MIN_INTERVAL: Duration = Duration::from_millis(50);

/// Units per second between two counter values, or `None` when the counter
/// went backwards (reset, wrap, device re-created) or `elapsed` is too short
/// to mean anything. The result is finite and never negative.
pub(crate) fn rate_per_second(previous: u64, current: u64, elapsed: Duration) -> Option<f64> {
    if current < previous || elapsed < MIN_INTERVAL {
        return None;
    }
    let rate = (current - previous) as f64 / elapsed.as_secs_f64();
    rate.is_finite().then_some(rate)
}

/// Remembers the previous counter value and when it was read.
#[derive(Debug, Default)]
pub(crate) struct RateTracker {
    previous: Option<(Instant, u64)>,
}

impl RateTracker {
    /// Feeds the counter value read at `now` and returns its rate since the
    /// previous accepted read. `None` on the first read.
    pub(crate) fn update(&mut self, now: Instant, counter: u64) -> Option<f64> {
        let Some((then, before)) = self.previous else {
            self.previous = Some((now, counter));
            return None;
        };
        let elapsed = now.saturating_duration_since(then);
        if elapsed < MIN_INTERVAL {
            return None;
        }
        // A counter that went backwards still becomes the new baseline, so
        // the next interval is measured from the reset point.
        self.previous = Some((now, counter));
        rate_per_second(before, counter, elapsed)
    }
}

/// Share of `total` that was not `idle`, in percent, clamped to 0..=100.
/// `None` when no time passed or a counter went backwards.
pub(crate) fn busy_percent(idle_delta: Option<u64>, total_delta: Option<u64>) -> Option<f64> {
    let (idle, total) = (idle_delta?, total_delta?);
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle) as f64 / total as f64 * 100.0;
    Some(busy.clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn rate_uses_the_elapsed_time_not_a_fixed_period() {
        assert_eq!(rate_per_second(1_000, 3_000, ONE_SECOND), Some(2_000.0));
        assert_eq!(rate_per_second(1_000, 3_000, Duration::from_secs(2)), Some(1_000.0));
        assert_eq!(rate_per_second(0, 500, Duration::from_millis(500)), Some(1_000.0));
    }

    #[test]
    fn rate_of_an_idle_counter_is_zero() {
        assert_eq!(rate_per_second(7, 7, ONE_SECOND), Some(0.0));
    }

    #[test]
    fn counter_that_went_backwards_has_no_rate() {
        assert_eq!(rate_per_second(10, 9, ONE_SECOND), None);
        assert_eq!(rate_per_second(u64::MAX, 0, ONE_SECOND), None);
    }

    #[test]
    fn interval_shorter_than_the_minimum_has_no_rate() {
        assert_eq!(rate_per_second(0, 100, Duration::ZERO), None);
        assert_eq!(rate_per_second(0, 100, MIN_INTERVAL - Duration::from_nanos(1)), None);
        assert!(rate_per_second(0, 100, MIN_INTERVAL).is_some());
    }

    #[test]
    fn rate_is_finite_for_the_largest_counters() {
        let rate = rate_per_second(0, u64::MAX, ONE_SECOND).unwrap();
        assert!(
            rate.is_finite() && rate > 0.0,
            "rate: expected finite and positive | received: {rate}"
        );
    }

    #[test]
    fn tracker_reports_nothing_on_the_first_read() {
        let mut tracker = RateTracker::default();
        assert_eq!(tracker.update(Instant::now(), 123), None);
    }

    #[test]
    fn tracker_measures_between_consecutive_reads() {
        let start = Instant::now();
        let mut tracker = RateTracker::default();
        tracker.update(start, 1_000);
        assert_eq!(tracker.update(start + ONE_SECOND, 1_500), Some(500.0));
        assert_eq!(tracker.update(start + 3 * ONE_SECOND, 2_500), Some(500.0));
    }

    #[test]
    fn tracker_keeps_its_baseline_when_a_read_is_too_soon() {
        let start = Instant::now();
        let mut tracker = RateTracker::default();
        tracker.update(start, 0);
        assert_eq!(tracker.update(start + Duration::from_millis(10), 50), None);
        // The rate spans the full second since the baseline, not 990 ms.
        assert_eq!(tracker.update(start + ONE_SECOND, 1_000), Some(1_000.0));
    }

    #[test]
    fn tracker_recovers_after_a_counter_reset() {
        let start = Instant::now();
        let mut tracker = RateTracker::default();
        tracker.update(start, 9_000);
        assert_eq!(tracker.update(start + ONE_SECOND, 100), None, "reset interval is unknown");
        assert_eq!(tracker.update(start + 2 * ONE_SECOND, 300), Some(200.0));
    }

    #[test]
    fn tracker_tolerates_a_clock_that_stepped_back() {
        let start = Instant::now() + ONE_SECOND;
        let mut tracker = RateTracker::default();
        tracker.update(start, 10);
        assert_eq!(tracker.update(start - ONE_SECOND, 20), None);
    }

    #[test]
    fn busy_percent_is_the_non_idle_share() {
        assert_eq!(busy_percent(Some(250), Some(1_000)), Some(75.0));
        assert_eq!(busy_percent(Some(0), Some(1_000)), Some(100.0));
        assert_eq!(busy_percent(Some(1_000), Some(1_000)), Some(0.0));
    }

    #[test]
    fn busy_percent_needs_elapsed_ticks_and_sane_counters() {
        assert_eq!(busy_percent(Some(0), Some(0)), None, "no ticks elapsed");
        assert_eq!(busy_percent(None, Some(100)), None, "idle counter went backwards");
        assert_eq!(busy_percent(Some(5), None), None, "total counter went backwards");
    }

    #[test]
    fn busy_percent_never_leaves_the_range_when_idle_exceeds_total() {
        assert_eq!(busy_percent(Some(2_000), Some(1_000)), Some(0.0));
    }
}
