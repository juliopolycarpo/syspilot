//! Turning RAPL energy counters into watts.
//!
//! Intel and AMD both expose cumulative energy in a 32-bit counter that
//! wraps. Power is the counter's difference between two reads times the
//! energy per count, divided by the time between them.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Source of time for power calculations. A trait so tests control it.
pub trait Clock: Send {
    /// The current instant.
    fn now(&self) -> Instant;

    /// Lets `duration` pass. Real clocks sleep; a test clock moves its time
    /// forward, so code that waits for a counter to count stays testable.
    fn wait(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The system's monotonic clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A clock that moves only when told to. Clones share the same time.
#[derive(Debug, Clone)]
pub struct ManualClock {
    origin: Instant,
    elapsed: Arc<Mutex<Duration>>,
}

impl ManualClock {
    /// A clock at an arbitrary starting instant.
    pub fn new() -> Self {
        Self { origin: Instant::now(), elapsed: Arc::default() }
    }

    /// Moves the clock forward.
    pub fn advance(&self, by: Duration) {
        if let Ok(mut elapsed) = self.elapsed.lock() {
            *elapsed += by;
        }
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.origin + self.elapsed.lock().map(|elapsed| *elapsed).unwrap_or_default()
    }

    fn wait(&self, duration: Duration) {
        self.advance(duration);
    }
}

/// Shortest interval a power reading is computed over. Shorter intervals
/// quantise badly: one count is a visible fraction of the result.
pub const MIN_INTERVAL: Duration = Duration::from_millis(200);

/// Longest interval a reading may span. A 32-bit counter at 2^-14 J per
/// count wraps after 262 144 J, which a 1 kW part reaches in 262 s; past
/// this limit a second wrap cannot be ruled out, so the meter starts over.
pub const MAX_INTERVAL: Duration = Duration::from_secs(60);

/// Above this a result is a counter glitch, not a measurement.
pub const MAX_PLAUSIBLE_WATTS: f64 = 2000.0;

/// The width of the energy counters, in bits.
const COUNTER_BITS: u32 = 32;

/// Converts successive counter reads into watts.
#[derive(Debug, Clone)]
pub struct EnergyMeter {
    joules_per_count: f64,
    last: Option<(Instant, u32)>,
}

impl EnergyMeter {
    /// A meter for a counter that advances by one every `joules_per_count`.
    pub fn new(joules_per_count: f64) -> Self {
        Self { joules_per_count, last: None }
    }

    /// Records `counter` read at `now` and returns the average power since
    /// the previous accepted read.
    ///
    /// `None` on the first read (nothing to compare with), when less than
    /// [`MIN_INTERVAL`] has passed (the baseline stays), when more than
    /// [`MAX_INTERVAL`] has passed or the result is implausible (the
    /// baseline restarts).
    pub fn watts(&mut self, now: Instant, counter: u64) -> Option<f64> {
        // Only the low 32 bits are the counter; the rest is reserved.
        let counter = (counter & ((1u64 << COUNTER_BITS) - 1)) as u32;
        let Some((then, before)) = self.last else {
            self.last = Some((now, counter));
            return None;
        };
        let elapsed = now.checked_duration_since(then).unwrap_or_default();
        if elapsed < MIN_INTERVAL {
            return None;
        }
        self.last = Some((now, counter));
        if elapsed > MAX_INTERVAL {
            return None;
        }
        let counts = counter.wrapping_sub(before);
        let watts = f64::from(counts) * self.joules_per_count / elapsed.as_secs_f64();
        (watts <= MAX_PLAUSIBLE_WATTS).then_some(watts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIT_2_14: f64 = 1.0 / 16384.0;

    fn meter_and_clock() -> (EnergyMeter, ManualClock) {
        (EnergyMeter::new(UNIT_2_14), ManualClock::new())
    }

    #[test]
    fn the_first_read_has_no_value() {
        let (mut meter, clock) = meter_and_clock();
        assert_eq!(meter.watts(clock.now(), 1000), None);
    }

    #[test]
    fn power_is_energy_over_time() {
        let (mut meter, clock) = meter_and_clock();
        meter.watts(clock.now(), 1_000);
        clock.advance(Duration::from_secs(2));
        // 16 W for 2 s is 32 J, which is 32 * 2^14 counts.
        let watts = meter.watts(clock.now(), 1_000 + 16 * 2 * 16384).unwrap();
        assert!((watts - 16.0).abs() < 1e-9, "{watts}");
    }

    #[test]
    fn a_wrap_of_the_32_bit_counter_is_handled() {
        let (mut meter, clock) = meter_and_clock();
        meter.watts(clock.now(), 0xFFFF_FFF0);
        clock.advance(Duration::from_secs(1));
        // 0x10 counts to the wrap plus 0x1000 after it.
        let watts = meter.watts(clock.now(), 0x1000).unwrap();
        let expected = f64::from(0x1010u32) * UNIT_2_14;
        assert!((watts - expected).abs() < 1e-9, "{watts} != {expected}");
    }

    #[test]
    fn reserved_upper_bits_are_ignored() {
        let (mut meter, clock) = meter_and_clock();
        meter.watts(clock.now(), 0xDEAD_0000_0000_0100);
        clock.advance(Duration::from_secs(1));
        let watts = meter.watts(clock.now(), 0xBEEF_0000_0000_0100 + 16384 * 10).unwrap();
        assert!((watts - 10.0).abs() < 1e-9, "{watts}");
    }

    #[test]
    fn a_too_short_interval_keeps_the_baseline() {
        let (mut meter, clock) = meter_and_clock();
        meter.watts(clock.now(), 0);
        clock.advance(Duration::from_millis(50));
        assert_eq!(meter.watts(clock.now(), 5000), None);
        clock.advance(Duration::from_secs(1));
        // Measured from the first read, 1.05 s ago.
        let watts = meter.watts(clock.now(), 16384 * 21).unwrap();
        assert!((watts - 20.0).abs() < 1e-9, "{watts}");
    }

    #[test]
    fn a_long_gap_restarts_the_meter() {
        let (mut meter, clock) = meter_and_clock();
        meter.watts(clock.now(), 0);
        clock.advance(Duration::from_secs(61));
        assert_eq!(meter.watts(clock.now(), 123), None, "more than one wrap cannot be excluded");
        clock.advance(Duration::from_secs(1));
        assert!(meter.watts(clock.now(), 123 + 16384).is_some(), "the next read measures again");
    }

    #[test]
    fn an_implausible_jump_is_dropped_and_the_baseline_restarts() {
        let (mut meter, clock) = meter_and_clock();
        meter.watts(clock.now(), 0);
        clock.advance(Duration::from_secs(1));
        assert_eq!(meter.watts(clock.now(), 0x4000_0000), None);
        clock.advance(Duration::from_secs(1));
        assert!(meter.watts(clock.now(), 0x4000_0000 + 16384 * 30).is_some());
    }

    #[test]
    fn a_standing_counter_is_zero_watts() {
        let (mut meter, clock) = meter_and_clock();
        meter.watts(clock.now(), 777);
        clock.advance(Duration::from_secs(2));
        assert_eq!(meter.watts(clock.now(), 777), Some(0.0));
    }

    #[test]
    fn the_manual_clock_is_shared_by_clones() {
        let clock = ManualClock::new();
        let copy = clock.clone();
        let before = copy.now();
        clock.advance(Duration::from_secs(3));
        assert_eq!(copy.now() - before, Duration::from_secs(3));
    }

    #[test]
    fn waiting_on_the_manual_clock_moves_it_forward_without_sleeping() {
        let clock = ManualClock::new();
        let before = clock.now();
        let started = Instant::now();
        clock.wait(Duration::from_secs(3600));
        assert_eq!(clock.now() - before, Duration::from_secs(3600));
        assert!(started.elapsed() < Duration::from_secs(5), "expected no real sleep");
    }

    #[test]
    fn waiting_on_the_system_clock_takes_real_time() {
        let clock = SystemClock;
        let before = clock.now();
        clock.wait(Duration::from_millis(20));
        assert!(clock.now() - before >= Duration::from_millis(20));
    }

    #[test]
    fn the_system_clock_moves_forward() {
        let clock = SystemClock;
        let first = clock.now();
        assert!(clock.now() >= first);
    }
}
