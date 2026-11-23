//! The pause a provider takes after its bus fails.
//!
//! A bus that just returned an error (a timeout, a busy host controller, a
//! stuck transaction) is not helped by another request a second later. After
//! each consecutive failure the provider skips twice as many samples as after
//! the one before, up to a cap, and starts again from nothing on the first
//! success.

/// Skip counter shared by the providers that touch a shared bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    cap_samples: u32,
    failures: u32,
    skip: u32,
}

impl Backoff {
    /// A backoff that never skips more than `cap_samples` samples in a row.
    pub fn new(cap_samples: u32) -> Self {
        Self { cap_samples: cap_samples.max(1), failures: 0, skip: 0 }
    }

    /// Whether this sample must be skipped. Consumes one skipped sample.
    pub fn skip_this_sample(&mut self) -> bool {
        if self.skip == 0 {
            return false;
        }
        self.skip -= 1;
        true
    }

    /// Records a failed sample: the next 1, 2, 4, ... samples are skipped,
    /// never more than the cap.
    pub fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
        let doubling = 1u32.checked_shl(self.failures - 1).unwrap_or(u32::MAX);
        self.skip = doubling.min(self.cap_samples);
    }

    /// Records a successful sample.
    pub fn record_success(&mut self) {
        self.failures = 0;
        self.skip = 0;
    }

    /// Consecutive failed samples.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Samples still to skip.
    pub fn pending_skips(&self) -> u32 {
        self.skip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_backoff_skips_nothing() {
        let mut backoff = Backoff::new(8);
        assert!(!backoff.skip_this_sample());
        assert_eq!((backoff.failures(), backoff.pending_skips()), (0, 0));
    }

    #[test]
    fn the_pause_doubles_after_each_failure_up_to_the_cap() {
        let mut backoff = Backoff::new(8);
        let mut pauses = Vec::new();
        for _ in 0..6 {
            backoff.record_failure();
            pauses.push(backoff.pending_skips());
            while backoff.skip_this_sample() {}
        }
        assert_eq!(pauses, [1, 2, 4, 8, 8, 8]);
    }

    #[test]
    fn skipped_samples_are_counted_down_one_at_a_time() {
        let mut backoff = Backoff::new(8);
        backoff.record_failure();
        backoff.record_failure();
        backoff.record_failure();
        let skipped = (0..10).filter(|_| backoff.skip_this_sample()).count();
        assert_eq!(skipped, 4, "(backoff after 3 failures): expected 4 skipped samples");
    }

    #[test]
    fn a_success_starts_the_sequence_again() {
        let mut backoff = Backoff::new(8);
        backoff.record_failure();
        backoff.record_failure();
        backoff.record_success();
        assert_eq!((backoff.failures(), backoff.pending_skips()), (0, 0));
        backoff.record_failure();
        assert_eq!(backoff.pending_skips(), 1);
    }

    #[test]
    fn a_cap_of_zero_still_pauses_one_sample() {
        let mut backoff = Backoff::new(0);
        backoff.record_failure();
        assert_eq!(backoff.pending_skips(), 1);
    }

    #[test]
    fn the_failure_count_saturates_without_overflowing_the_shift() {
        let mut backoff = Backoff::new(8);
        for _ in 0..100 {
            backoff.record_failure();
        }
        assert_eq!(backoff.pending_skips(), 8);
        assert_eq!(backoff.failures(), 100);
    }
}
