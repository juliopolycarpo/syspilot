//! Delta math for sensors computed from cumulative counters.
//!
//! Every figure is derived from the counter difference over the interval
//! that really elapsed, not from the nominal sampling period: the hub may be
//! late, and an on-demand read may squeeze in between ticks.

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
