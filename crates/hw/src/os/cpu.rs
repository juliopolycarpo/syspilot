//! Processor load and effective clock.
//!
//! Load comes from the per-processor kernel time counters
//! (`NtQuerySystemInformation`, class `SystemProcessorPerformanceInformation`),
//! the same source Task Manager uses. The clock is the nominal frequency
//! scaled by the PDH counter `% Processor Performance`, which sees turbo and
//! power-saving states.

use syspilot_proto::sensor::{DeviceClass, SensorKind};
use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemProcessorPerformanceInformation,
};
use windows_sys::Win32::Foundation::STATUS_INFO_LENGTH_MISMATCH;
use windows_sys::Win32::System::Threading::{ALL_PROCESSOR_GROUPS, GetActiveProcessorCount};
use windows_sys::Win32::System::WindowsProgramming::SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION;

use super::pdh::{CounterId, Query};
use super::rate::busy_percent;
use super::registry;
use crate::hub::{Provider, SensorSpec};

/// Registry key of the first logical processor; every processor reports the
/// same brand string and nominal clock.
pub(crate) const PROCESSOR_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";
const PERFORMANCE_COUNTER: &str = r"\Processor Information(_Total)\% Processor Performance";
/// Clocks outside this range come from a broken counter, not a CPU.
const PLAUSIBLE_MHZ: std::ops::RangeInclusive<f64> = 1.0..=20_000.0;

/// Cumulative kernel time of one logical processor, in 100 ns units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CpuTimes {
    /// Time spent idle.
    pub idle: u64,
    /// Kernel plus user time. The kernel figure already includes idle time,
    /// so this is the total time, not the busy time.
    pub total: u64,
}

/// Where cumulative per-processor times come from.
pub(crate) trait TimesSource: Send {
    /// Replaces `out` with one entry per logical processor, in processor
    /// order across all groups. `false` if the read failed.
    fn read(&mut self, out: &mut Vec<CpuTimes>) -> bool;
}

/// Where the clock scaling factor comes from.
pub(crate) trait PerformanceSource: Send {
    /// Current frequency as a percentage of nominal, or `None` while the
    /// source has no value yet.
    fn percent(&mut self) -> Option<f64>;
}

/// [`TimesSource`] backed by `NtQuerySystemInformation`.
struct KernelTimes {
    entries: Vec<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>,
}

const ENTRY_BYTES: usize = size_of::<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>();
// The documented x64 layout: five 64-bit times, a 32-bit count and padding.
const _: () = assert!(ENTRY_BYTES == 48);

impl KernelTimes {
    fn new() -> Self {
        // SAFETY: trivially safe; no pointers involved.
        let active = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
        let zeroed = SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION::default();
        Self { entries: vec![zeroed; (active as usize).max(1)] }
    }
}

impl TimesSource for KernelTimes {
    fn read(&mut self, out: &mut Vec<CpuTimes>) -> bool {
        out.clear();
        // The second attempt covers processors that came online since the
        // buffer was sized.
        for _ in 0..2 {
            let capacity = u32::try_from(self.entries.len() * ENTRY_BYTES).unwrap_or(u32::MAX);
            let mut returned = 0u32;
            // SAFETY: `entries` is valid for writes of `capacity` bytes;
            // `returned` is a valid out-pointer.
            let status = unsafe {
                NtQuerySystemInformation(
                    SystemProcessorPerformanceInformation,
                    self.entries.as_mut_ptr().cast(),
                    capacity,
                    &mut returned,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH {
                let wanted = (returned as usize).div_ceil(ENTRY_BYTES).max(self.entries.len() * 2);
                self.entries.resize(wanted, SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION::default());
                continue;
            }
            if status < 0 {
                return false;
            }
            let count = (returned as usize / ENTRY_BYTES).min(self.entries.len());
            out.extend(self.entries[..count].iter().map(|entry| {
                CpuTimes {
                    idle: u64::try_from(entry.IdleTime).unwrap_or(0),
                    total: u64::try_from(entry.KernelTime)
                        .unwrap_or(0)
                        .saturating_add(u64::try_from(entry.UserTime).unwrap_or(0)),
                }
            }));
            return !out.is_empty();
        }
        false
    }
}

/// [`PerformanceSource`] backed by a PDH counter.
struct PdhPerformance {
    query: Query,
    counter: CounterId,
}

impl PdhPerformance {
    fn open() -> Option<Self> {
        let mut query = Query::open()?;
        let counter = query.add_counter(PERFORMANCE_COUNTER)?;
        Some(Self { query, counter })
    }
}

impl PerformanceSource for PdhPerformance {
    fn percent(&mut self) -> Option<f64> {
        if !self.query.collect() {
            return None;
        }
        self.query.value(self.counter)
    }
}

/// How `cpu.clock` is produced.
struct ClockModel {
    nominal_mhz: f64,
    /// `None` when the PDH counter does not exist: the clock then stays at
    /// nominal. A counter that exists but has no value yet (before its
    /// second collection) reports no reading instead; claiming nominal
    /// would show 2400 MHz for a second on a CPU that is boosting.
    performance: Option<Box<dyn PerformanceSource>>,
}

impl ClockModel {
    fn read(&mut self) -> Option<f64> {
        let Some(source) = self.performance.as_mut() else {
            return Some(self.nominal_mhz);
        };
        let mhz = self.nominal_mhz * source.percent()? / 100.0;
        PLAUSIBLE_MHZ.contains(&mhz).then_some(mhz)
    }
}

/// Load of the whole CPU and of every logical processor, plus the clock.
pub struct CpuProvider {
    device: String,
    times: Box<dyn TimesSource>,
    clock: Option<ClockModel>,
    /// Number of per-thread sensors, fixed at construction.
    threads: usize,
    previous: Vec<CpuTimes>,
    current: Vec<CpuTimes>,
}

impl CpuProvider {
    fn new(
        device: String,
        threads: usize,
        times: Box<dyn TimesSource>,
        clock: Option<ClockModel>,
    ) -> Self {
        Self { device, times, clock, threads, previous: Vec::new(), current: Vec::new() }
    }
}

/// Load over the interval between two reads of every processor, or `None`
/// if the processor sets differ.
///
/// A processor whose counters went backwards contributes nothing to the
/// interval instead of voiding it. This happens on a healthy machine: the
/// kernel accounts a processor's time in clock ticks (15.6 ms) and a read
/// taken mid-tick can come out one or more ticks below the previous one, on
/// idle and total alike (about one sample in thirty on a 28-thread CPU,
/// measured at 60 ms intervals). Dropping the whole sample for that would
/// leave the total load blinking out.
fn total_load(previous: &[CpuTimes], current: &[CpuTimes]) -> Option<f64> {
    if previous.is_empty() || previous.len() != current.len() {
        return None;
    }
    let (mut idle, mut total) = (0u64, 0u64);
    for (before, now) in previous.iter().zip(current) {
        idle = idle.saturating_add(now.idle.saturating_sub(before.idle));
        total = total.saturating_add(now.total.saturating_sub(before.total));
    }
    busy_percent(Some(idle), Some(total))
}

fn thread_load(before: &CpuTimes, now: &CpuTimes) -> Option<f64> {
    busy_percent(now.idle.checked_sub(before.idle), now.total.checked_sub(before.total))
}

impl Provider for CpuProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        let spec = |key: String, label: String, kind| SensorSpec {
            device: self.device.clone(),
            class: DeviceClass::Cpu,
            key,
            label,
            kind,
        };
        let mut specs = vec![spec("cpu.load".into(), "Total".into(), SensorKind::Load)];
        if self.clock.is_some() {
            specs.push(spec("cpu.clock".into(), "Clock".into(), SensorKind::Frequency));
        }
        for thread in 0..self.threads {
            specs.push(spec(
                format!("cpu.thread{thread}.load"),
                format!("Thread {thread}"),
                SensorKind::Load,
            ));
        }
        specs
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        let read_ok = self.times.read(&mut self.current);
        let comparable = read_ok && !self.previous.is_empty();

        out.push(if comparable { total_load(&self.previous, &self.current) } else { None });
        if let Some(clock) = self.clock.as_mut() {
            out.push(clock.read());
        }
        for thread in 0..self.threads {
            let pair = self.previous.get(thread).zip(self.current.get(thread));
            out.push(
                pair.filter(|_| comparable).and_then(|(before, now)| thread_load(before, now)),
            );
        }

        // A failed read leaves `current` empty; keeping the older baseline
        // would span the gap, which is still a valid (longer) interval.
        if read_ok {
            std::mem::swap(&mut self.previous, &mut self.current);
        }
    }
}

/// The CPU provider, if the kernel reports processor times.
pub fn probe() -> Vec<Box<dyn Provider>> {
    let mut times = KernelTimes::new();
    let mut first = Vec::new();
    if !times.read(&mut first) {
        return Vec::new();
    }
    let device = registry::read_string(PROCESSOR_KEY, "ProcessorNameString")
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "CPU".to_owned());
    let clock =
        registry::read_u32(PROCESSOR_KEY, "~MHz").filter(|mhz| *mhz > 0).map(|mhz| ClockModel {
            nominal_mhz: f64::from(mhz),
            performance: PdhPerformance::open()
                .map(|source| Box::new(source) as Box<dyn PerformanceSource>),
        });
    vec![Box::new(CpuProvider::new(device, first.len(), Box::new(times), clock))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashSet, VecDeque};

    /// Replays scripted reads; `None` is a failed read.
    struct ScriptedTimes {
        reads: VecDeque<Option<Vec<CpuTimes>>>,
    }

    impl ScriptedTimes {
        fn new(reads: Vec<Option<Vec<CpuTimes>>>) -> Box<Self> {
            Box::new(Self { reads: reads.into() })
        }
    }

    impl TimesSource for ScriptedTimes {
        fn read(&mut self, out: &mut Vec<CpuTimes>) -> bool {
            out.clear();
            match self.reads.pop_front().flatten() {
                Some(frame) => {
                    out.extend(frame);
                    true
                }
                None => false,
            }
        }
    }

    /// Replays scripted `% Processor Performance` values.
    struct ScriptedPerformance {
        values: VecDeque<Option<f64>>,
    }

    impl PerformanceSource for ScriptedPerformance {
        fn percent(&mut self) -> Option<f64> {
            self.values.pop_front().flatten()
        }
    }

    fn times(idle: u64, total: u64) -> CpuTimes {
        CpuTimes { idle, total }
    }

    fn provider(
        reads: Vec<Option<Vec<CpuTimes>>>,
        threads: usize,
        clock: Option<ClockModel>,
    ) -> CpuProvider {
        CpuProvider::new("Test CPU".into(), threads, ScriptedTimes::new(reads), clock)
    }

    fn sample(provider: &mut CpuProvider) -> Vec<Option<f64>> {
        let mut out = Vec::new();
        provider.sample(&mut out);
        out
    }

    #[test]
    fn sensors_cover_total_clock_and_every_thread_with_unique_keys() {
        let clock = ClockModel { nominal_mhz: 2400.0, performance: None };
        let specs = provider(vec![], 3, Some(clock)).sensors();
        let keys: Vec<_> = specs.iter().map(|spec| spec.key.as_str()).collect();
        assert_eq!(
            keys,
            ["cpu.load", "cpu.clock", "cpu.thread0.load", "cpu.thread1.load", "cpu.thread2.load"]
        );
        assert_eq!(keys.iter().collect::<HashSet<_>>().len(), keys.len());
        assert!(
            specs.iter().all(|spec| spec.device == "Test CPU" && spec.class == DeviceClass::Cpu)
        );
    }

    #[test]
    fn clock_sensor_is_omitted_without_a_nominal_frequency() {
        let keys: Vec<_> = provider(vec![], 1, None).sensors().into_iter().map(|s| s.key).collect();
        assert_eq!(keys, ["cpu.load", "cpu.thread0.load"]);
    }

    #[test]
    fn first_sample_has_no_load() {
        let mut cpu = provider(vec![Some(vec![times(0, 100), times(0, 100)])], 2, None);
        assert_eq!(sample(&mut cpu), [None, None, None]);
    }

    #[test]
    fn load_is_the_busy_share_of_the_interval() {
        let reads = vec![
            Some(vec![times(0, 0), times(0, 0)]),
            // Thread 0: 25 of 100 ticks idle; thread 1: all idle.
            Some(vec![times(25, 100), times(100, 100)]),
        ];
        let mut cpu = provider(reads, 2, None);
        sample(&mut cpu);
        // Total: 125 idle of 200 ticks.
        assert_eq!(sample(&mut cpu), [Some(37.5), Some(75.0), Some(0.0)]);
    }

    #[test]
    fn load_uses_only_the_latest_interval() {
        let reads =
            vec![Some(vec![times(0, 0)]), Some(vec![times(0, 100)]), Some(vec![times(100, 200)])];
        let mut cpu = provider(reads, 1, None);
        sample(&mut cpu);
        assert_eq!(sample(&mut cpu), [Some(100.0), Some(100.0)]);
        assert_eq!(sample(&mut cpu), [Some(0.0), Some(0.0)]);
    }

    #[test]
    fn thread_whose_counter_went_backwards_has_no_load_but_others_and_the_total_do() {
        let reads = vec![
            Some(vec![times(50, 100), times(50, 100)]),
            Some(vec![times(10, 50), times(60, 200)]),
        ];
        let mut cpu = provider(reads, 2, None);
        sample(&mut cpu);
        // The total ignores the processor that stepped back: 10 idle of 100.
        assert_eq!(sample(&mut cpu), [Some(90.0), None, Some(90.0)]);
    }

    #[test]
    fn one_processor_stepping_back_a_tick_does_not_blank_the_total() {
        // Measured on a real machine: processor 17 read 156 250 units (one
        // 15.6 ms tick) lower on both idle and total, 27 others advanced.
        const TICK: u64 = 156_250;
        let before: Vec<CpuTimes> = (0..28).map(|_| times(1_000_000, 1_100_000)).collect();
        let mut after: Vec<CpuTimes> = (0..28).map(|_| times(1_900_000, 2_000_000)).collect();
        after[17] = times(1_000_000 - TICK, 1_100_000 - TICK);
        // 27 processors: 900 000 idle of 900 000 total.
        assert_eq!(total_load(&before, &after), Some(0.0));
        // Without the saturation the whole sample would have been None.
        assert_eq!(thread_load(&before[17], &after[17]), None);
        assert_eq!(thread_load(&before[0], &after[0]), Some(0.0));
    }

    #[test]
    fn total_load_of_an_interval_where_every_counter_stepped_back_is_unknown() {
        let before = [times(100, 200), times(100, 200)];
        let after = [times(50, 150), times(50, 150)];
        assert_eq!(total_load(&before, &after), None, "no elapsed ticks at all");
    }

    #[test]
    fn failed_read_gives_no_values_and_keeps_the_baseline() {
        let reads = vec![Some(vec![times(0, 0)]), None, Some(vec![times(50, 100)])];
        let mut cpu = provider(reads, 1, None);
        sample(&mut cpu);
        assert_eq!(sample(&mut cpu), [None, None]);
        assert_eq!(
            sample(&mut cpu),
            [Some(50.0), Some(50.0)],
            "the interval spans the failed read"
        );
    }

    #[test]
    fn processor_that_appears_later_has_no_baseline_and_drops_the_total() {
        let reads = vec![Some(vec![times(0, 0)]), Some(vec![times(0, 100), times(0, 100)])];
        let mut cpu = provider(reads, 2, None);
        sample(&mut cpu);
        assert_eq!(sample(&mut cpu), [None, Some(100.0), None]);
    }

    #[test]
    fn sample_always_returns_one_value_per_sensor() {
        let clock = ClockModel { nominal_mhz: 2400.0, performance: None };
        let mut cpu = provider(vec![None, Some(vec![times(0, 1)])], 4, Some(clock));
        let expected = cpu.sensors().len();
        assert_eq!(sample(&mut cpu).len(), expected, "failed read");
        assert_eq!(sample(&mut cpu).len(), expected, "one processor reported, four declared");
    }

    #[test]
    fn clock_scales_nominal_by_processor_performance() {
        let performance =
            ScriptedPerformance { values: [None, Some(100.0), Some(137.5), Some(40.0)].into() };
        let clock = ClockModel { nominal_mhz: 2400.0, performance: Some(Box::new(performance)) };
        let mut cpu = provider(vec![], 0, Some(clock));
        let clocks: Vec<_> = (0..4).map(|_| sample(&mut cpu)[1]).collect();
        assert_eq!(clocks, [None, Some(2400.0), Some(3300.0), Some(960.0)]);
    }

    #[test]
    fn clock_without_a_performance_counter_stays_at_nominal() {
        let clock = ClockModel { nominal_mhz: 2400.0, performance: None };
        let mut cpu = provider(vec![], 0, Some(clock));
        assert_eq!(sample(&mut cpu)[1], Some(2400.0));
    }

    #[test]
    fn implausible_performance_is_not_a_clock() {
        let performance = ScriptedPerformance { values: [Some(0.0), Some(-5.0), Some(1e9)].into() };
        let clock = ClockModel { nominal_mhz: 2400.0, performance: Some(Box::new(performance)) };
        let mut cpu = provider(vec![], 0, Some(clock));
        let clocks: Vec<_> = (0..3).map(|_| sample(&mut cpu)[1]).collect();
        assert_eq!(clocks, [None, None, None]);
    }

    #[test]
    fn total_load_sums_idle_and_total_across_processors_before_dividing() {
        // Averaging the per-processor percentages (100 and 0) would give 50.
        // Weighted by elapsed ticks it is 10 busy ticks out of 110.
        let previous = [times(0, 0), times(0, 0)];
        let current = [times(0, 10), times(100, 100)];
        let load = total_load(&previous, &current).unwrap();
        assert!(
            (load - 100.0 * 10.0 / 110.0).abs() < 1e-9,
            "total load: expected 9.0909 | received: {load}"
        );
    }
}
