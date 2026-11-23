//! A small wrapper over the Performance Data Helper (PDH) API.
//!
//! PDH is the only supported way to read some Windows counters (GPU engines,
//! GPU adapter memory, processor performance, ACPI thermal zones). The
//! wrapper keeps the usage rules in one place:
//!
//! * counters are added by their English path, so a localised Windows works;
//! * reads use plain [`Query::collect`], which samples on the calling thread
//!   (the `Ex` variant would start a timer thread);
//! * a counter that needs two collections (a rate) reports `None` until the
//!   second one.

use std::ptr;

use syspilot_proto::win::wide;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
    PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
    PdhGetFormattedCounterValue, PdhOpenQueryW,
};

/// `PDH_FMT_NOCAP100` from `pdh.h`: do not clamp percentage counters at 100.
/// `% Processor Performance` exceeds 100 when the CPU runs above its
/// nominal clock.
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;
/// Format used for every read.
const FORMAT: u32 = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;

/// Identifies a counter added to a [`Query`]. Only valid for that query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CounterId(usize);

/// A PDH query: a set of counters collected together.
pub(crate) struct Query {
    handle: PDH_HQUERY,
    counters: Vec<PDH_HCOUNTER>,
}

// SAFETY: a PDH query handle is a pointer to PDH-internal state that is not
// tied to the creating thread; every call that uses it here takes `&self` or
// `&mut self`, so it is never used from two threads at once.
unsafe impl Send for Query {}

impl Query {
    /// Opens a query on the local machine's real-time data.
    pub(crate) fn open() -> Option<Self> {
        let mut handle: PDH_HQUERY = ptr::null_mut();
        // SAFETY: a null data source means real-time data; `handle` is a
        // valid out-pointer.
        let status = unsafe { PdhOpenQueryW(ptr::null(), 0, &mut handle) };
        if status != ERROR_SUCCESS || handle.is_null() {
            return None;
        }
        Some(Self { handle, counters: Vec::new() })
    }

    /// Adds the counter with the English `path`, e.g.
    /// `\Processor Information(_Total)\% Processor Performance`. `None` if
    /// the counter set or counter does not exist on this machine.
    pub(crate) fn add_counter(&mut self, path: &str) -> Option<CounterId> {
        let path = wide(path);
        let mut counter: PDH_HCOUNTER = ptr::null_mut();
        // SAFETY: `self.handle` is an open query; `path` is NUL-terminated
        // and outlives the call; `counter` is a valid out-pointer.
        let status = unsafe { PdhAddEnglishCounterW(self.handle, path.as_ptr(), 0, &mut counter) };
        if status != ERROR_SUCCESS || counter.is_null() {
            return None;
        }
        self.counters.push(counter);
        Some(CounterId(self.counters.len() - 1))
    }

    /// Takes one sample of every counter. `false` if nothing was collected.
    pub(crate) fn collect(&mut self) -> bool {
        // SAFETY: `self.handle` is an open query.
        unsafe { PdhCollectQueryData(self.handle) == ERROR_SUCCESS }
    }

    /// The formatted value of a single-instance counter, `None` while the
    /// counter has no valid data (a rate before its second collection).
    pub(crate) fn value(&self, id: CounterId) -> Option<f64> {
        let counter = *self.counters.get(id.0)?;
        let mut formatted = PDH_FMT_COUNTERVALUE::default();
        // SAFETY: `counter` belongs to `self.handle`, which is open;
        // `formatted` is a valid out-pointer; the type out-pointer is null.
        let status = unsafe {
            PdhGetFormattedCounterValue(counter, FORMAT, ptr::null_mut(), &mut formatted)
        };
        if status != ERROR_SUCCESS || !is_valid_status(formatted.CStatus) {
            return None;
        }
        // SAFETY: `FORMAT` requested `PDH_FMT_DOUBLE`, so the double member
        // of the union is the one PDH wrote.
        let value = unsafe { formatted.Anonymous.doubleValue };
        value.is_finite().then_some(value)
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is an open query closed exactly once; this
        // also releases every counter added to it.
        unsafe { PdhCloseQuery(self.handle) };
    }
}

/// Whether a counter status means the value can be used.
fn is_valid_status(status: u32) -> bool {
    status == PDH_CSTATUS_VALID_DATA || status == PDH_CSTATUS_NEW_DATA
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_valid_and_new_data_statuses_are_usable() {
        assert!(is_valid_status(PDH_CSTATUS_VALID_DATA));
        assert!(is_valid_status(PDH_CSTATUS_NEW_DATA));
        assert!(!is_valid_status(0xC0000BBA), "PDH_CSTATUS_INVALID_DATA");
    }

    #[test]
    fn unknown_counter_is_not_added() {
        let mut query = Query::open().unwrap();
        assert_eq!(query.add_counter(r"\SysPilot No Such Object(*)\No Such Counter"), None);
    }

    #[test]
    fn rate_counter_has_no_value_after_one_collection() {
        let mut query = Query::open().unwrap();
        let counter = query.add_counter(r"\Processor Information(_Total)\% Processor Performance");
        let counter = counter.expect("the processor performance counter exists on Windows 10+");
        assert!(query.collect());
        // A rate has no value after one collection; it must say so, not read zero.
        assert_eq!(query.value(counter), None);
    }

    #[test]
    fn counter_that_was_never_added_reads_nothing() {
        let query = Query::open().unwrap();
        assert_eq!(query.value(CounterId(3)), None);
    }
}
