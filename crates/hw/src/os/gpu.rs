//! Graphics adapter temperature, fan, utilisation and dedicated memory,
//! without vendor SDKs.
//!
//! * Temperature, fan speed and power come from the kernel-mode thunk
//!   `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERPERFDATA)`. Drivers that have
//!   no such sensor report zero, and then the sensor is not declared.
//! * Utilisation is the PDH counter `\GPU Engine(*)\Utilization Percentage`,
//!   reduced the way Task Manager does: sum the instances of each engine
//!   across processes, then take the busiest engine of the adapter.
//! * Dedicated memory in use is `\GPU Adapter Memory(*)\Dedicated Usage`.

use std::collections::HashSet;
use std::ptr;

use syspilot_proto::sensor::{DeviceClass, SensorKind};
use windows_sys::Wdk::Graphics::Direct3D::{
    D3DKMT_ADAPTER_PERFDATA, D3DKMT_ADAPTER_PERFDATACAPS, D3DKMT_ADAPTERINFO,
    D3DKMT_ADAPTERREGISTRYINFO, D3DKMT_ADAPTERTYPE, D3DKMT_CLOSEADAPTER, D3DKMT_DRIVER_DESCRIPTION,
    D3DKMT_ENUMADAPTERS2, D3DKMT_QUERYADAPTERINFO, D3DKMTCloseAdapter, D3DKMTEnumAdapters2,
    D3DKMTQueryAdapterInfo, KMTQAITYPE_ADAPTERPERFDATA, KMTQAITYPE_ADAPTERPERFDATA_CAPS,
    KMTQAITYPE_ADAPTERREGISTRYINFO, KMTQAITYPE_ADAPTERTYPE, KMTQAITYPE_DRIVER_DESCRIPTION,
    KMTQUERYADAPTERINFOTYPE,
};
use windows_sys::Win32::Foundation::{LUID, STATUS_BUFFER_TOO_SMALL};

use super::pdh::{CounterId, Query};
use super::text::clean_utf16;
use crate::hub::{Provider, SensorSpec};

const ENGINE_COUNTER: &str = r"\GPU Engine(*)\Utilization Percentage";
const MEMORY_COUNTER: &str = r"\GPU Adapter Memory(*)\Dedicated Usage";
/// Bit of `D3DKMT_ADAPTERTYPE::Value` that marks a software rasteriser
/// (`SoftwareDevice`, bit 2 in `d3dkmthk.h`), such as the Microsoft Basic
/// Render Driver.
const ADAPTER_TYPE_SOFTWARE_DEVICE: u32 = 1 << 2;
/// Name of the WARP software adapter, in case a driver omits the flag.
const BASIC_RENDER_DRIVER: &str = "Microsoft Basic Render Driver";
/// Times adapter enumeration retries when adapters appear between the size
/// query and the data call.
const ENUM_ATTEMPTS: usize = 3;

/// Locally unique identifier of an adapter, as it appears in PDH instance
/// names. Changes at every boot, so it is never used as a sensor key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Luid {
    pub high: u32,
    pub low: u32,
}

impl From<LUID> for Luid {
    fn from(luid: LUID) -> Self {
        Self { high: luid.HighPart as u32, low: luid.LowPart }
    }
}

/// A PDH `GPU Engine` instance:
/// `pid_<pid>_luid_0x<high>_0x<low>_phys_<n>_eng_<n>_engtype_<type>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EngineInstance {
    luid: Luid,
    physical_adapter: u32,
    engine: u32,
}

/// Parses a `GPU Engine` instance name. The process ID and the engine type
/// are deliberately dropped: engines are identified by adapter, physical
/// node and index, and the type is a label that does not split an engine.
fn parse_engine_instance(name: &str) -> Option<EngineInstance> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, rest) = rest.split_once('_')?;
    pid.parse::<u32>().ok()?;
    let (luid, rest) = take_luid(rest)?;
    let rest = rest.strip_prefix("_phys_")?;
    let (physical_adapter, rest) = rest.split_once('_')?;
    let rest = rest.strip_prefix("eng_")?;
    let (engine, rest) = rest.split_once('_')?;
    rest.strip_prefix("engtype_")?;
    Some(EngineInstance {
        luid,
        physical_adapter: physical_adapter.parse().ok()?,
        engine: engine.parse().ok()?,
    })
}

/// A PDH `GPU Adapter Memory` instance: `luid_0x<high>_0x<low>_phys_<n>`.
/// Returns the adapter; memory of every physical node is summed.
fn parse_memory_instance(name: &str) -> Option<Luid> {
    let (luid, rest) = take_luid(name)?;
    let node = rest.strip_prefix("_phys_")?;
    node.parse::<u32>().ok()?;
    Some(luid)
}

/// Takes `luid_0x<high>_0x<low>` from the start of `text`. Hex digits may be
/// upper or lower case.
fn take_luid(text: &str) -> Option<(Luid, &str)> {
    let rest = text.strip_prefix("luid_0x")?;
    let (high, rest) = rest.split_once("_0x")?;
    let digits = rest.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(rest.len());
    let (low, rest) = rest.split_at(digits);
    let parse = |hex: &str| (hex.len() <= 8).then(|| u32::from_str_radix(hex, 16).ok()).flatten();
    Some((Luid { high: parse(high)?, low: parse(low)? }, rest))
}

/// Whether the adapter is a software rasteriser that has no sensors.
fn is_software_adapter(type_flags: u32, name: &str) -> bool {
    type_flags & ADAPTER_TYPE_SOFTWARE_DEVICE != 0 || name == BASIC_RENDER_DRIVER
}

/// Makes names unique by numbering repeats: two identical cards become
/// `X` and `X (2)`, so the hub does not merge them into one device.
fn disambiguate_names(names: &mut [String]) {
    let mut seen: HashSet<String> = HashSet::new();
    for name in names.iter_mut() {
        let base = name.clone();
        let mut number = 2;
        while !seen.insert(name.clone()) {
            *name = format!("{base} ({number})");
            number += 1;
        }
    }
}

/// An open adapter handle from `D3DKMTEnumAdapters2`, closed on drop.
#[derive(Debug)]
pub(crate) struct AdapterHandle(u32);

impl Drop for AdapterHandle {
    fn drop(&mut self) {
        let request = D3DKMT_CLOSEADAPTER { hAdapter: self.0 };
        // SAFETY: `request` is a valid close request for a handle this
        // wrapper owns and closes exactly once.
        unsafe { D3DKMTCloseAdapter(&request) };
    }
}

/// Reads `T` from `D3DKMTQueryAdapterInfo`. `T` is one of the plain C
/// structures of `d3dkmthk.h`, for which every bit pattern is valid.
fn query_adapter_info<T: Copy>(
    adapter: &AdapterHandle,
    kind: KMTQUERYADAPTERINFOTYPE,
    data: &mut T,
) -> bool {
    let mut request = D3DKMT_QUERYADAPTERINFO {
        hAdapter: adapter.0,
        Type: kind,
        pPrivateDriverData: ptr::from_mut(data).cast(),
        PrivateDriverDataSize: size_of::<T>() as u32,
    };
    // SAFETY: `request` points at `data`, which is valid for writes of the
    // `size_of::<T>()` bytes declared in the request, and outlives the call.
    unsafe { D3DKMTQueryAdapterInfo(&mut request) >= 0 }
}

/// What the display driver reports about an adapter.
#[derive(Debug)]
pub(crate) struct Adapter {
    pub name: String,
    pub luid: Luid,
    pub software: bool,
    handle: AdapterHandle,
}

/// Every adapter the graphics kernel knows, including software ones.
/// Handles are closed when the returned values drop.
pub(crate) fn adapters() -> Vec<Adapter> {
    let mut found = Vec::new();
    for raw in enumerate_raw() {
        // Wrap first: the handle is closed even for adapters skipped below.
        let handle = AdapterHandle(raw.hAdapter);
        let mut kind = D3DKMT_ADAPTERTYPE::default();
        let type_flags = if query_adapter_info(&handle, KMTQAITYPE_ADAPTERTYPE, &mut kind) {
            // SAFETY: the union is plain 32-bit data; `Value` is always valid.
            unsafe { kind.Anonymous.Value }
        } else {
            0
        };
        let name = adapter_name(&handle).unwrap_or_else(|| "Graphics adapter".to_owned());
        found.push(Adapter {
            software: is_software_adapter(type_flags, &name),
            name,
            luid: raw.AdapterLuid.into(),
            handle,
        });
    }
    found
}

/// The adapter list from `D3DKMTEnumAdapters2`, as raw records.
fn enumerate_raw() -> Vec<D3DKMT_ADAPTERINFO> {
    let blank = D3DKMT_ADAPTERINFO {
        hAdapter: 0,
        AdapterLuid: LUID { LowPart: 0, HighPart: 0 },
        NumOfSources: 0,
        bPrecisePresentRegionsPreferred: 0,
    };
    // The first pass has no buffer and only learns the count; a later pass
    // retries with a bigger buffer if an adapter appeared in between.
    let mut capacity = 0usize;
    for _ in 0..ENUM_ATTEMPTS {
        let mut records = vec![blank; capacity];
        let mut request = D3DKMT_ENUMADAPTERS2 {
            NumAdapters: u32::try_from(capacity).unwrap_or(0),
            pAdapters: if capacity == 0 { ptr::null_mut() } else { records.as_mut_ptr() },
        };
        // SAFETY: `pAdapters` is null (a size query) or points at `records`,
        // which holds `NumAdapters` records.
        let status = unsafe { D3DKMTEnumAdapters2(&mut request) };
        let reported = request.NumAdapters as usize;
        if status >= 0 && reported <= capacity {
            records.truncate(reported);
            return records;
        }
        if status >= 0 || status == STATUS_BUFFER_TOO_SMALL {
            capacity = reported;
            continue;
        }
        return Vec::new();
    }
    Vec::new()
}

/// The marketing name of the adapter: the driver description, or the
/// registry adapter string on drivers that do not answer that query.
fn adapter_name(handle: &AdapterHandle) -> Option<String> {
    let mut description = Box::<D3DKMT_DRIVER_DESCRIPTION>::default();
    if query_adapter_info(handle, KMTQAITYPE_DRIVER_DESCRIPTION, &mut *description) {
        let name = clean_utf16(&description.DriverDescription);
        if !name.is_empty() {
            return Some(name);
        }
    }
    let mut registry = Box::<D3DKMT_ADAPTERREGISTRYINFO>::default();
    if query_adapter_info(handle, KMTQAITYPE_ADAPTERREGISTRYINFO, &mut *registry) {
        let name = clean_utf16(&registry.AdapterString);
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Sensor data of `KMTQAITYPE_ADAPTERPERFDATA`, in the driver's units.
///
/// The structure also has a `Power` field, in tenths of a percent of an
/// unspecified maximum. It is left out on purpose: the value (7, i.e. 0.7 %,
/// on an idle RX 550) cannot be cross-checked against any other source, and
/// a percentage of an unknown maximum is not a power reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PerfReading {
    /// Tenths of a degree Celsius; zero means the driver has no sensor.
    pub deci_celsius: u32,
    /// Revolutions per minute. Zero is either "no fan sensor" or a fan that
    /// is stopped (zero-RPM mode); [`declares_fan`] tells them apart.
    pub fan_rpm: u32,
}

impl PerfReading {
    fn temperature(&self) -> Option<f64> {
        (1..=1_250).contains(&self.deci_celsius).then(|| f64::from(self.deci_celsius) / 10.0)
    }

    /// The fan speed. Only meaningful for an adapter that declares a fan.
    fn fan(&self) -> Option<f64> {
        (self.fan_rpm <= 30_000).then(|| f64::from(self.fan_rpm))
    }
}

/// Whether the adapter has a fan sensor. A stopped fan reads 0 RPM, the same
/// as a missing sensor, so the adapter's capabilities (`MaxFanRPM`) decide.
fn declares_fan(reading: &PerfReading, max_fan_rpm: u32) -> bool {
    reading.fan_rpm > 0 || max_fan_rpm > 0
}

/// `MaxFanRPM` from `KMTQAITYPE_ADAPTERPERFDATA_CAPS`, zero if unavailable.
fn max_fan_rpm(handle: &AdapterHandle) -> u32 {
    let mut caps = D3DKMT_ADAPTER_PERFDATACAPS { PhysicalAdapterIndex: 0, ..Default::default() };
    if query_adapter_info(handle, KMTQAITYPE_ADAPTERPERFDATA_CAPS, &mut caps) {
        caps.MaxFanRPM
    } else {
        0
    }
}

/// Where an adapter's perf data comes from.
pub(crate) trait PerfSource: Send {
    /// `None` if the query failed.
    fn read(&mut self) -> Option<PerfReading>;
}

/// [`PerfSource`] backed by `D3DKMTQueryAdapterInfo`.
struct KernelPerf {
    handle: AdapterHandle,
}

impl PerfSource for KernelPerf {
    fn read(&mut self) -> Option<PerfReading> {
        // `PhysicalAdapterIndex` is an input: zero selects the first (and,
        // outside linked-adapter setups, only) physical adapter.
        let mut data = D3DKMT_ADAPTER_PERFDATA { PhysicalAdapterIndex: 0, ..Default::default() };
        query_adapter_info(&self.handle, KMTQAITYPE_ADAPTERPERFDATA, &mut data)
            .then_some(PerfReading { deci_celsius: data.Temperature, fan_rpm: data.FanRPM })
    }
}

/// Where utilisation and memory instances come from.
pub(crate) trait UsageSource: Send {
    /// Takes one sample of the counters. `false` if it failed.
    fn collect(&mut self) -> bool;
    /// Visits `GPU Engine` instances that have valid data.
    fn for_each_engine(&mut self, visit: &mut dyn FnMut(&str, f64));
    /// Visits `GPU Adapter Memory` instances that have valid data.
    fn for_each_memory(&mut self, visit: &mut dyn FnMut(&str, f64));
}

/// [`UsageSource`] backed by PDH. Either counter may be missing.
struct PdhUsage {
    query: Query,
    engine: Option<CounterId>,
    memory: Option<CounterId>,
}

impl PdhUsage {
    fn open() -> Option<Self> {
        let mut query = Query::open()?;
        let engine = query.add_counter(ENGINE_COUNTER);
        let memory = query.add_counter(MEMORY_COUNTER);
        (engine.is_some() || memory.is_some()).then_some(Self { query, engine, memory })
    }
}

impl UsageSource for PdhUsage {
    fn collect(&mut self) -> bool {
        self.query.collect()
    }

    fn for_each_engine(&mut self, visit: &mut dyn FnMut(&str, f64)) {
        if let Some(counter) = self.engine {
            self.query.for_each_instance(counter, visit);
        }
    }

    fn for_each_memory(&mut self, visit: &mut dyn FnMut(&str, f64)) {
        if let Some(counter) = self.memory {
            self.query.for_each_instance(counter, visit);
        }
    }
}

/// Which sensors an adapter declares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Declared {
    temperature: bool,
    fan: bool,
    load: bool,
    memory: bool,
}

impl Declared {
    fn any(&self) -> bool {
        self.temperature || self.fan || self.load || self.memory
    }

    fn needs_perf(&self) -> bool {
        self.temperature || self.fan
    }
}

struct GpuAdapter {
    name: String,
    luid: Luid,
    perf: Box<dyn PerfSource>,
    declared: Declared,
    /// Sum of the instance values of each engine in the current sample,
    /// keyed by (physical node, engine index). Reused between samples.
    engine_sums: Vec<((u32, u32), f64)>,
    /// Dedicated memory in use in the current sample.
    memory_used: Option<f64>,
}

impl GpuAdapter {
    /// Busiest engine, as Task Manager's headline GPU figure.
    fn busiest_engine(&self) -> f64 {
        self.engine_sums.iter().map(|&(_, sum)| sum).fold(0.0, f64::max).clamp(0.0, 100.0)
    }
}

/// Temperature, fan, utilisation and dedicated memory of every hardware
/// graphics adapter.
pub struct GpuProvider {
    adapters: Vec<GpuAdapter>,
    usage: Option<Box<dyn UsageSource>>,
}

impl GpuProvider {
    /// Reads the PDH counters once and distributes the values to adapters.
    /// Returns whether the engine counter had valid data for any adapter at
    /// all: before its second collection it has none, which means "unknown",
    /// not "idle".
    fn refresh_usage(&mut self) -> bool {
        for adapter in &mut self.adapters {
            adapter.engine_sums.clear();
            adapter.memory_used = None;
        }
        let Some(usage) = self.usage.as_mut() else { return false };
        if !usage.collect() {
            return false;
        }
        let adapters = &mut self.adapters;
        let mut engines_seen = false;
        usage.for_each_engine(&mut |name, value| {
            let Some(engine) = parse_engine_instance(name) else { return };
            engines_seen = true;
            let Some(adapter) = adapters.iter_mut().find(|a| a.luid == engine.luid) else { return };
            let key = (engine.physical_adapter, engine.engine);
            match adapter.engine_sums.iter_mut().find(|(known, _)| *known == key) {
                Some((_, sum)) => *sum += value,
                None => adapter.engine_sums.push((key, value)),
            }
        });
        usage.for_each_memory(&mut |name, value| {
            let Some(luid) = parse_memory_instance(name) else { return };
            let Some(adapter) = adapters.iter_mut().find(|a| a.luid == luid) else { return };
            adapter.memory_used = Some(adapter.memory_used.unwrap_or(0.0) + value);
        });
        engines_seen
    }
}

impl Provider for GpuProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        let mut specs = Vec::new();
        for (index, adapter) in self.adapters.iter().enumerate() {
            let mut add = |suffix: &str, label: &str, kind| {
                specs.push(SensorSpec {
                    device: adapter.name.clone(),
                    class: DeviceClass::Gpu,
                    key: format!("gpu{index}.{suffix}"),
                    label: label.to_owned(),
                    kind,
                });
            };
            let declared = adapter.declared;
            if declared.temperature {
                add("temp", "Temperature", SensorKind::Temperature);
            }
            if declared.fan {
                add("fan", "Fan", SensorKind::Fan);
            }
            if declared.load {
                add("load", "Load", SensorKind::Load);
            }
            if declared.memory {
                add("memory.used", "Dedicated memory used", SensorKind::Data);
            }
        }
        specs
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        // With valid engine data, an adapter that owns no engine instance has
        // no work (0 %); without any, its load is unknown.
        let counter_has_data = self.refresh_usage();
        for adapter in &mut self.adapters {
            let declared = adapter.declared;
            let perf = if declared.needs_perf() { adapter.perf.read() } else { None };
            if declared.temperature {
                out.push(perf.and_then(|reading| reading.temperature()));
            }
            if declared.fan {
                out.push(perf.and_then(|reading| reading.fan()));
            }
            if declared.load {
                out.push(counter_has_data.then(|| adapter.busiest_engine()));
            }
            if declared.memory {
                out.push(adapter.memory_used);
            }
        }
    }
}

/// The GPU provider, if the machine has a hardware adapter with at least one
/// readable sensor.
pub fn probe() -> Vec<Box<dyn Provider>> {
    let mut hardware: Vec<Adapter> =
        adapters().into_iter().filter(|adapter| !adapter.software).collect();
    // Stable keys: boot-dependent LUIDs only break ties between identical cards.
    hardware.sort_by(|a, b| a.name.cmp(&b.name).then(a.luid.cmp(&b.luid)));
    if hardware.is_empty() {
        return Vec::new();
    }
    let usage = PdhUsage::open();
    let (has_load, has_memory) =
        usage.as_ref().map_or((false, false), |u| (u.engine.is_some(), u.memory.is_some()));

    let mut names: Vec<String> = hardware.iter().map(|a| a.name.clone()).collect();
    disambiguate_names(&mut names);
    let mut adapters = Vec::new();
    for (adapter, name) in hardware.into_iter().zip(names) {
        let max_fan_rpm = max_fan_rpm(&adapter.handle);
        let mut perf = KernelPerf { handle: adapter.handle };
        let reading = perf.read().unwrap_or_default();
        let declared = Declared {
            temperature: reading.temperature().is_some(),
            fan: declares_fan(&reading, max_fan_rpm),
            load: has_load,
            memory: has_memory,
        };
        if !declared.any() {
            continue;
        }
        adapters.push(GpuAdapter {
            name,
            luid: adapter.luid,
            perf: Box::new(perf),
            declared,
            engine_sums: Vec::new(),
            memory_used: None,
        });
    }
    if adapters.is_empty() {
        return Vec::new();
    }
    let usage = usage.map(|u| Box::new(u) as Box<dyn UsageSource>);
    vec![Box::new(GpuProvider { adapters, usage })]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const LUID_A: Luid = Luid { high: 0, low: 0xD5B5 };
    const LUID_B: Luid = Luid { high: 0, low: 0x1234 };

    fn engine_name(pid: u32, luid: &str, phys: u32, engine: u32, kind: &str) -> String {
        format!("pid_{pid}_{luid}_phys_{phys}_eng_{engine}_engtype_{kind}")
    }

    const A: &str = "luid_0x00000000_0x0000D5B5";
    const B: &str = "luid_0x00000000_0x00001234";

    #[test]
    fn engine_instance_is_parsed_into_adapter_node_and_engine() {
        let parsed = parse_engine_instance(&engine_name(1234, A, 0, 3, "3D")).unwrap();
        assert_eq!(parsed, EngineInstance { luid: LUID_A, physical_adapter: 0, engine: 3 });
    }

    #[test]
    fn engine_type_may_contain_underscores_and_digits() {
        let parsed = parse_engine_instance(&engine_name(4, A, 1, 12, "VideoCodec_0")).unwrap();
        assert_eq!(parsed.engine, 12);
        assert_eq!(parsed.physical_adapter, 1);
    }

    #[test]
    fn luid_hex_is_case_insensitive_and_keeps_the_high_part() {
        let lower =
            parse_engine_instance("pid_1_luid_0x00000001_0x0000d5b5_phys_0_eng_0_engtype_3D");
        let upper =
            parse_engine_instance("pid_1_luid_0x00000001_0x0000D5B5_phys_0_eng_0_engtype_3D");
        assert_eq!(lower, upper);
        assert_eq!(lower.unwrap().luid, Luid { high: 1, low: 0xD5B5 });
    }

    #[test]
    fn malformed_engine_instances_are_rejected() {
        for bad in [
            "",
            "_Total",
            "pid_x_luid_0x0_0x0_phys_0_eng_0_engtype_3D",
            "pid_1_luid_0x00000000_phys_0_eng_0_engtype_3D",
            "pid_1_luid_0xZZ_0x00_phys_0_eng_0_engtype_3D",
            "pid_1_luid_0x000000001_0x00_phys_0_eng_0_engtype_3D",
            "pid_1_luid_0x00000000_0x0000d5b5_eng_0_engtype_3D",
            "pid_1_luid_0x00000000_0x0000d5b5_phys_a_eng_0_engtype_3D",
            "pid_1_luid_0x00000000_0x0000d5b5_phys_0_eng_0",
            "pid_1_luid_0x00000000_0x0000d5b5_phys_-1_eng_0_engtype_3D",
        ] {
            assert_eq!(parse_engine_instance(bad), None, "instance {bad:?}: expected None");
        }
    }

    #[test]
    fn memory_instance_is_parsed_into_its_adapter() {
        assert_eq!(parse_memory_instance("luid_0x00000000_0x0000D5B5_phys_0"), Some(LUID_A));
        assert_eq!(parse_memory_instance("luid_0x00000000_0x0000d5b5_phys_1"), Some(LUID_A));
        assert_eq!(parse_memory_instance("luid_0x00000000_0x0000d5b5"), None);
        assert_eq!(parse_memory_instance("luid_0x00000000_0x0000d5b5_phys_x"), None);
        assert_eq!(parse_memory_instance("pid_4_luid_0x0_0x0_phys_0"), None);
    }

    #[test]
    fn software_adapters_are_recognised_by_flag_or_name() {
        assert!(is_software_adapter(ADAPTER_TYPE_SOFTWARE_DEVICE, "Anything"));
        assert!(is_software_adapter(0, "Microsoft Basic Render Driver"));
        assert!(!is_software_adapter(0b11, "Radeon RX 550/550 Series"), "render + display only");
    }

    #[test]
    fn identical_adapter_names_are_numbered() {
        let mut names =
            vec!["RTX".to_owned(), "RTX".to_owned(), "Radeon".to_owned(), "RTX".to_owned()];
        disambiguate_names(&mut names);
        assert_eq!(names, ["RTX", "RTX (2)", "Radeon", "RTX (3)"]);
    }

    #[test]
    fn perf_reading_converts_units_and_rejects_zero_and_garbage() {
        let reading = PerfReading { deci_celsius: 457, fan_rpm: 1200 };
        assert_eq!(reading.temperature(), Some(45.7));
        assert_eq!(reading.fan(), Some(1200.0));
        assert_eq!(PerfReading::default().temperature(), None, "zero means no temperature sensor");
        let garbage = PerfReading { deci_celsius: 5_000, fan_rpm: 4_000_000_000 };
        assert_eq!((garbage.temperature(), garbage.fan()), (None, None));
    }

    #[test]
    fn stopped_fan_reads_zero_rpm_when_the_adapter_has_a_fan_sensor() {
        let stopped = PerfReading { deci_celsius: 420, fan_rpm: 0 };
        assert!(declares_fan(&stopped, 6_900), "zero RPM with a 6900 RPM maximum is a stopped fan");
        assert_eq!(stopped.fan(), Some(0.0));
    }

    #[test]
    fn adapter_without_any_fan_information_declares_no_fan() {
        let reading = PerfReading { deci_celsius: 500, fan_rpm: 0 };
        assert!(!declares_fan(&reading, 0));
        assert!(declares_fan(&PerfReading { deci_celsius: 0, fan_rpm: 800 }, 0), "spinning fan");
    }

    /// Perf data that never changes, or fails when `reading` is `None`.
    struct FixedPerf {
        reading: Option<PerfReading>,
    }

    impl PerfSource for FixedPerf {
        fn read(&mut self) -> Option<PerfReading> {
            self.reading
        }
    }

    /// One scripted PDH collection.
    struct Frame {
        collected: bool,
        engines: Vec<(String, f64)>,
        memory: Vec<(String, f64)>,
    }

    /// Replays scripted PDH collections.
    struct ScriptedUsage {
        frames: VecDeque<Frame>,
        current: Option<Frame>,
    }

    impl ScriptedUsage {
        fn new(frames: Vec<Frame>) -> Box<Self> {
            Box::new(Self { frames: frames.into(), current: None })
        }
    }

    impl UsageSource for ScriptedUsage {
        fn collect(&mut self) -> bool {
            self.current = self.frames.pop_front();
            self.current.as_ref().is_some_and(|frame| frame.collected)
        }

        fn for_each_engine(&mut self, visit: &mut dyn FnMut(&str, f64)) {
            for (name, value) in self.current.iter().flat_map(|f| &f.engines) {
                visit(name, *value);
            }
        }

        fn for_each_memory(&mut self, visit: &mut dyn FnMut(&str, f64)) {
            for (name, value) in self.current.iter().flat_map(|f| &f.memory) {
                visit(name, *value);
            }
        }
    }

    fn frame(engines: &[(String, f64)], memory: &[(&str, f64)]) -> Frame {
        Frame {
            collected: true,
            engines: engines.to_vec(),
            memory: memory.iter().map(|(name, value)| ((*name).to_owned(), *value)).collect(),
        }
    }

    fn adapter(
        name: &str,
        luid: Luid,
        declared: Declared,
        reading: Option<PerfReading>,
    ) -> GpuAdapter {
        GpuAdapter {
            name: name.to_owned(),
            luid,
            perf: Box::new(FixedPerf { reading }),
            declared,
            engine_sums: Vec::new(),
            memory_used: None,
        }
    }

    fn all_sensors() -> Declared {
        Declared { temperature: true, fan: true, load: true, memory: true }
    }

    fn sample(provider: &mut GpuProvider) -> Vec<Option<f64>> {
        let mut out = Vec::new();
        provider.sample(&mut out);
        out
    }

    #[test]
    fn engines_are_summed_across_processes_then_the_busiest_wins() {
        let engines = [
            (engine_name(10, A, 0, 0, "3D"), 30.0),
            (engine_name(11, A, 0, 0, "3D"), 25.0), // same engine, other process: 55
            (engine_name(10, A, 0, 1, "Copy"), 40.0),
            (engine_name(12, B, 0, 0, "3D"), 99.0), // another adapter
        ];
        let usage = ScriptedUsage::new(vec![frame(&engines, &[])]);
        let reading = PerfReading::default();
        let declared = Declared { load: true, ..Declared::default() };
        let mut gpu = GpuProvider {
            adapters: vec![adapter("A", LUID_A, declared, Some(reading))],
            usage: Some(usage),
        };
        assert_eq!(sample(&mut gpu), [Some(55.0)]);
    }

    #[test]
    fn engine_identity_ignores_the_engine_type_label() {
        let engines =
            [(engine_name(10, A, 0, 0, "3D"), 20.0), (engine_name(11, A, 0, 0, "Compute_0"), 30.0)];
        let usage = ScriptedUsage::new(vec![frame(&engines, &[])]);
        let declared = Declared { load: true, ..Declared::default() };
        let mut gpu = GpuProvider {
            adapters: vec![adapter("A", LUID_A, declared, None)],
            usage: Some(usage),
        };
        assert_eq!(sample(&mut gpu), [Some(50.0)]);
    }

    #[test]
    fn load_is_capped_at_one_hundred() {
        let engines =
            [(engine_name(1, A, 0, 0, "3D"), 80.0), (engine_name(2, A, 0, 0, "3D"), 70.0)];
        let usage = ScriptedUsage::new(vec![frame(&engines, &[])]);
        let declared = Declared { load: true, ..Declared::default() };
        let mut gpu = GpuProvider {
            adapters: vec![adapter("A", LUID_A, declared, None)],
            usage: Some(usage),
        };
        assert_eq!(sample(&mut gpu), [Some(100.0)]);
    }

    #[test]
    fn load_is_unknown_until_the_counter_has_data_and_zero_for_an_idle_adapter() {
        let busy_elsewhere = [(engine_name(1, B, 0, 0, "3D"), 10.0)];
        let usage = ScriptedUsage::new(vec![
            frame(&[], &[]),             // first collection: no valid instance
            frame(&busy_elsewhere, &[]), // data exists, none for adapter A
        ]);
        let declared = Declared { load: true, ..Declared::default() };
        let mut gpu = GpuProvider {
            adapters: vec![
                adapter("A", LUID_A, declared, None),
                adapter("B", LUID_B, declared, None),
            ],
            usage: Some(usage),
        };
        assert_eq!(sample(&mut gpu), [None, None], "before the counter has data");
        assert_eq!(sample(&mut gpu), [Some(0.0), Some(10.0)], "A has no engines in use");
    }

    #[test]
    fn dedicated_memory_is_summed_over_physical_nodes_of_the_matching_adapter() {
        let memory = [
            ("luid_0x00000000_0x0000d5b5_phys_0", 1_000.0),
            ("luid_0x00000000_0x0000d5b5_phys_1", 500.0),
            ("luid_0x00000000_0x00001234_phys_0", 7.0),
        ];
        let usage = ScriptedUsage::new(vec![frame(&[], &memory)]);
        let declared = Declared { memory: true, ..Declared::default() };
        let mut gpu = GpuProvider {
            adapters: vec![adapter("A", LUID_A, declared, None)],
            usage: Some(usage),
        };
        assert_eq!(sample(&mut gpu), [Some(1_500.0)]);
    }

    #[test]
    fn failed_collection_gives_no_usage_values() {
        let mut failed = frame(
            &[(engine_name(1, A, 0, 0, "3D"), 50.0)],
            &[("luid_0x00000000_0x0000d5b5_phys_0", 9.0)],
        );
        failed.collected = false;
        let usage = ScriptedUsage::new(vec![failed]);
        let declared = Declared { load: true, memory: true, ..Declared::default() };
        let mut gpu = GpuProvider {
            adapters: vec![adapter("A", LUID_A, declared, None)],
            usage: Some(usage),
        };
        assert_eq!(sample(&mut gpu), [None, None]);
    }

    #[test]
    fn sensors_follow_what_the_adapter_declared_with_stable_keys() {
        let declared = Declared { temperature: true, fan: false, load: true, memory: true };
        let gpu = GpuProvider {
            adapters: vec![
                adapter("Radeon", LUID_A, declared, None),
                adapter("Intel", LUID_B, Declared { load: true, ..Declared::default() }, None),
            ],
            usage: None,
        };
        let keys: Vec<_> = gpu.sensors().into_iter().map(|s| s.key).collect();
        assert_eq!(keys, ["gpu0.temp", "gpu0.load", "gpu0.memory.used", "gpu1.load"]);
        assert!(gpu.sensors().iter().all(|s| s.class == DeviceClass::Gpu));
    }

    #[test]
    fn sample_returns_one_value_per_declared_sensor_in_order() {
        let reading = PerfReading { deci_celsius: 412, fan_rpm: 900 };
        let usage = ScriptedUsage::new(vec![frame(
            &[(engine_name(1, A, 0, 0, "3D"), 12.5)],
            &[("luid_0x00000000_0x0000d5b5_phys_0", 2_048.0)],
        )]);
        let mut gpu = GpuProvider {
            adapters: vec![adapter("A", LUID_A, all_sensors(), Some(reading))],
            usage: Some(usage),
        };
        let expected = gpu.sensors().len();
        assert_eq!(sample(&mut gpu), [Some(41.2), Some(900.0), Some(12.5), Some(2_048.0)]);
        // A second sample with the script exhausted still keeps the count.
        assert_eq!(sample(&mut gpu).len(), expected);
    }

    #[test]
    fn failed_perf_query_leaves_perf_sensors_empty_but_not_usage() {
        let usage =
            ScriptedUsage::new(vec![frame(&[], &[("luid_0x00000000_0x0000d5b5_phys_0", 64.0)])]);
        let mut gpu = GpuProvider {
            adapters: vec![adapter("A", LUID_A, all_sensors(), None)],
            usage: Some(usage),
        };
        assert_eq!(sample(&mut gpu), [None, None, None, Some(64.0)]);
    }

    #[test]
    fn provider_without_pdh_still_reports_perf_sensors() {
        let reading = PerfReading { deci_celsius: 600, fan_rpm: 0 };
        let declared = Declared { temperature: true, ..Declared::default() };
        let mut gpu = GpuProvider {
            adapters: vec![adapter("A", LUID_A, declared, Some(reading))],
            usage: None,
        };
        assert_eq!(sample(&mut gpu), [Some(60.0)]);
    }
}
