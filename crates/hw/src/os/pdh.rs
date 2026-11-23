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
//!   second one;
//! * wildcard instances are returned through one reusable buffer.
//!
//! The first counter added in a process makes PDH load the system's counter
//! name tables, which takes about a second and keeps several MB resident for
//! the life of the process. Later counters are cheap.

use std::ptr;

use syspilot_proto::win::wide;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhGetFormattedCounterValue, PdhOpenQueryW,
};

/// `PDH_FMT_NOCAP100` from `pdh.h`: do not clamp percentage counters at 100.
/// `% Processor Performance` exceeds 100 when the CPU runs above its
/// nominal clock, and a sum over instances may too.
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;
/// Format used for every read.
const FORMAT: u32 = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;
/// Times the array read retries when the instance set grows between the
/// size query and the data call.
const ARRAY_ATTEMPTS: usize = 4;

/// Identifies a counter added to a [`Query`]. Only valid for that query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CounterId(usize);

/// A PDH query: a set of counters collected together.
pub(crate) struct Query {
    handle: PDH_HQUERY,
    counters: Vec<PDH_HCOUNTER>,
    /// Array results; `u64` elements keep the items 8-byte aligned.
    buffer: Vec<u64>,
    /// Decoded instance name, reused for every item.
    name: String,
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
        Some(Self { handle, counters: Vec::new(), buffer: Vec::new(), name: String::new() })
    }

    /// Adds the counter with the English `path`, e.g.
    /// `\GPU Engine(*)\Utilization Percentage`. `None` if the counter set or
    /// counter does not exist on this machine.
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

    /// Calls `visit(instance_name, value)` for every instance of a wildcard
    /// counter that currently has valid data. Returns `false` if the counter
    /// could not be read at all (distinct from "no instance has data").
    pub(crate) fn for_each_instance(
        &mut self,
        id: CounterId,
        visit: &mut dyn FnMut(&str, f64),
    ) -> bool {
        let Some(&counter) = self.counters.get(id.0) else { return false };
        for _ in 0..ARRAY_ATTEMPTS {
            // The buffer persists between reads, so a stable instance set
            // costs one PDH call. An empty buffer sends a null pointer and
            // size zero, which makes PDH answer with the size it needs.
            let mut bytes = u32::try_from(self.buffer.len() * 8).unwrap_or(u32::MAX);
            let data =
                if self.buffer.is_empty() { ptr::null_mut() } else { self.buffer.as_mut_ptr() };
            let mut items = 0u32;
            // SAFETY: `counter` belongs to the open query; `data` is null or
            // points at `self.buffer`, which holds at least `bytes` bytes.
            let status = unsafe {
                PdhGetFormattedCounterArrayW(counter, FORMAT, &mut bytes, &mut items, data.cast())
            };
            if status == PDH_MORE_DATA {
                // A quarter of slack avoids growing again when a process
                // starts and adds instances before the next read.
                let wanted = bytes as usize + bytes as usize / 4;
                self.buffer.resize(wanted.div_ceil(8), 0);
                continue;
            }
            if status != ERROR_SUCCESS {
                return false;
            }
            self.visit_items(items as usize, bytes as usize, visit);
            return true;
        }
        false
    }

    /// Walks the items PDH wrote to `self.buffer`.
    fn visit_items(&mut self, count: usize, bytes: usize, visit: &mut dyn FnMut(&str, f64)) {
        let buffer_units = self.buffer.len() * 4;
        // SAFETY: `self.buffer` is a live allocation of `buffer_units`
        // 16-bit units; `u64` alignment satisfies `u16`.
        let units: &[u16] =
            unsafe { std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u16>(), buffer_units) };
        let base = self.buffer.as_ptr() as usize;
        let item_size = size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        // Never trust the count beyond what fits in the bytes PDH reported.
        let count = count.min(bytes / item_size).min(self.buffer.len() * 8 / item_size);
        for index in 0..count {
            // SAFETY: `index * item_size` lies inside the buffer (checked by
            // the `count` bound above) and the buffer start is 8-byte
            // aligned, as the item type requires; PDH initialised these
            // items.
            let item =
                unsafe { &*self.buffer.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>().add(index) };
            if !is_valid_status(item.FmtValue.CStatus) {
                continue;
            }
            // The name pointer targets the string area of the same buffer.
            let Some(offset) = (item.szName as usize).checked_sub(base) else { continue };
            if !decode_name(units, offset / 2, &mut self.name) {
                continue;
            }
            // SAFETY: `FORMAT` requested `PDH_FMT_DOUBLE`.
            let value = unsafe { item.FmtValue.Anonymous.doubleValue };
            if value.is_finite() {
                visit(&self.name, value);
            }
        }
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

/// Decodes the NUL-terminated name starting at `start` (in UTF-16 units)
/// into `out`. `false` if `start` is outside `units` or no terminator
/// follows, which would mean the item points at memory we do not own.
fn decode_name(units: &[u16], start: usize, out: &mut String) -> bool {
    let Some(tail) = units.get(start..) else { return false };
    let Some(length) = tail.iter().position(|&unit| unit == 0) else { return false };
    out.clear();
    out.extend(char::decode_utf16(tail[..length].iter().copied()).map(|c| c.unwrap_or('\u{FFFD}')));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[test]
    fn name_is_decoded_up_to_its_terminator() {
        let mut buffer = units("ab");
        buffer.extend(units("pid_4_luid_0x00000000_0x0000d5b5"));
        let mut out = String::from("stale");
        assert!(decode_name(&buffer, 3, &mut out));
        assert_eq!(out, "pid_4_luid_0x00000000_0x0000d5b5");
        assert!(decode_name(&buffer, 0, &mut out));
        assert_eq!(out, "ab");
    }

    #[test]
    fn name_outside_the_buffer_is_rejected() {
        let buffer = units("abc");
        let mut out = String::new();
        assert!(!decode_name(&buffer, 10, &mut out), "start beyond the buffer");
    }

    #[test]
    fn name_without_a_terminator_is_rejected() {
        let buffer: Vec<u16> = "abc".encode_utf16().collect();
        let mut out = String::new();
        assert!(!decode_name(&buffer, 0, &mut out), "no NUL before the end of the buffer");
    }

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
        let mut query = Query::open().unwrap();
        assert_eq!(query.value(CounterId(3)), None);
        assert!(!query.for_each_instance(CounterId(3), &mut |_, _| {}));
    }

    #[test]
    fn wildcard_counter_lists_its_instances() {
        let mut query = Query::open().unwrap();
        let counter = query.add_counter(r"\Processor Information(*)\Processor Frequency").unwrap();
        assert!(query.collect());
        let mut names = Vec::new();
        assert!(query.for_each_instance(counter, &mut |name, _| names.push(name.to_owned())));
        assert!(
            names.iter().any(|name| name == "_Total"),
            "processor instances: expected \"_Total\" among them | received: {names:?}"
        );
    }

    #[test]
    fn instance_values_are_repeatable_with_a_reused_buffer() {
        let mut query = Query::open().unwrap();
        let counter = query.add_counter(r"\Processor Information(*)\Processor Frequency").unwrap();
        let mut counts = Vec::new();
        for _ in 0..3 {
            assert!(query.collect());
            let mut count = 0;
            assert!(query.for_each_instance(counter, &mut |_, _| count += 1));
            counts.push(count);
        }
        assert!(counts[0] > 0, "instances per read: expected > 0 | received: {counts:?}");
        assert_eq!(counts[0], counts[1]);
        assert_eq!(counts[1], counts[2]);
    }
}
