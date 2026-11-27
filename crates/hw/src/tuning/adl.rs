//! The AMD driver's library, `atiadlxx.dll` (AMD Display Library, ADL).
//!
//! The library ships with the Radeon driver in the system directory. It is
//! loaded from there by name and from nowhere else, so nothing a user can
//! write is ever loaded in its place. A machine without the driver has no
//! such file, which is the normal case and not an error to show.
//!
//! Only the handful of Overdrive N functions [`AdlApi`] names are bound.
//! Their signatures and structures are from AMD's public ADL SDK
//! (`adl_sdk.h`, `adl_structures.h`, and the `OverdriveN` sample).

use std::ffi::{CStr, c_char, c_void};
use std::ptr;

use syspilot_proto::win::wide;

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapAlloc};

use super::radeon::{
    AdlAdapter, AdlApi, AdlError, OdnCapabilities, OdnFan, OdnLevel, OdnLevels, OdnPower,
};
use crate::os::text::clean_text;

/// The library's file name. No path: the loader is told to look in the
/// system directory only.
const LIBRARY: &str = "atiadlxx.dll";
/// `ADL_OK`.
const ADL_OK: i32 = 0;
/// `ADL_MAX_PATH`.
const ADL_MAX_PATH: usize = 256;
/// Most outputs the library is believed about. A machine has a few per card.
const MAX_ADAPTERS: usize = 256;
/// Most performance states a levels call is made for. Overdrive N cards
/// report eight.
const MAX_LEVELS: usize = 32;

/// `AdapterInfo` as the library fills it on Windows.
#[repr(C)]
#[derive(Clone, Copy)]
struct AdapterInfo {
    size: i32,
    adapter_index: i32,
    udid: [c_char; ADL_MAX_PATH],
    bus_number: i32,
    device_number: i32,
    function_number: i32,
    vendor_id: i32,
    adapter_name: [c_char; ADL_MAX_PATH],
    display_name: [c_char; ADL_MAX_PATH],
    present: i32,
    exist: i32,
    driver_path: [c_char; ADL_MAX_PATH],
    driver_path_ext: [c_char; ADL_MAX_PATH],
    pnp_string: [c_char; ADL_MAX_PATH],
    os_display_index: i32,
}

/// The fixed part of `ADLODNPerformanceLevelsX2`; the levels follow it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LevelsHeader {
    size: i32,
    mode: i32,
    count: i32,
}

/// The library's context handle.
type Context = *mut c_void;
/// `ADL_MAIN_MALLOC_CALLBACK`.
type Malloc = unsafe extern "system" fn(i32) -> *mut c_void;

type MainCreate = unsafe extern "C" fn(Malloc, i32, *mut Context) -> i32;
type MainDestroy = unsafe extern "C" fn(Context) -> i32;
type AdapterCount = unsafe extern "C" fn(Context, *mut i32) -> i32;
type AdapterInfoGet = unsafe extern "C" fn(Context, *mut AdapterInfo, i32) -> i32;
type OverdriveCaps = unsafe extern "C" fn(Context, i32, *mut i32, *mut i32, *mut i32) -> i32;
type CapabilitiesGet = unsafe extern "C" fn(Context, i32, *mut OdnCapabilities) -> i32;
type LevelsCall = unsafe extern "C" fn(Context, i32, *mut LevelsHeader) -> i32;
type FanCall = unsafe extern "C" fn(Context, i32, *mut OdnFan) -> i32;
type PowerCall = unsafe extern "C" fn(Context, i32, *mut OdnPower) -> i32;

/// Memory for the library's own allocations. None of the functions bound
/// here allocates through it; it exists because creating a context requires
/// one.
unsafe extern "system" fn allocate(size: i32) -> *mut c_void {
    let Ok(size) = usize::try_from(size) else { return ptr::null_mut() };
    // SAFETY: `GetProcessHeap` has no preconditions.
    let heap = unsafe { GetProcessHeap() };
    // SAFETY: the process heap is valid for the life of the process, and
    // `HeapAlloc` returns null when it cannot give `size` bytes.
    unsafe { HeapAlloc(heap, 0, size) }
}

/// The text of a fixed-size C string field, cleaned like every other device
/// string (see [`clean_text`], which also drops what follows the NUL).
fn text(field: &[c_char]) -> String {
    let bytes: Vec<u8> = field.iter().map(|&byte| byte as u8).collect();
    clean_text(&String::from_utf8_lossy(&bytes))
}

/// A levels structure as the library takes it: the header, then `count`
/// states, in one buffer of 32-bit integers (every field is one).
fn levels_buffer(mode: i32, levels: &[OdnLevel]) -> Vec<i32> {
    let header = size_of::<LevelsHeader>() / size_of::<i32>();
    let per_level = size_of::<OdnLevel>() / size_of::<i32>();
    let mut buffer = vec![0i32; header + per_level * levels.len()];
    buffer[0] = i32::try_from(buffer.len() * size_of::<i32>()).unwrap_or(0);
    buffer[1] = mode;
    buffer[2] = i32::try_from(levels.len()).unwrap_or(0);
    for (index, level) in levels.iter().enumerate() {
        let at = header + per_level * index;
        buffer[at..at + per_level].copy_from_slice(&[
            level.clock,
            level.vddc,
            level.enabled,
            level.control,
        ]);
    }
    buffer
}

/// The states in a buffer [`levels_buffer`] made and the library filled.
fn levels_from(buffer: &[i32], mode: i32) -> OdnLevels {
    let header = size_of::<LevelsHeader>() / size_of::<i32>();
    // A level is four integers, as `OdnLevel` lays them out.
    let levels = buffer
        .get(header..)
        .unwrap_or_default()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&[clock, vddc, enabled, control]| OdnLevel { clock, vddc, enabled, control })
        .collect();
    // The mode asked for, whatever the library left in the field: a write of
    // what was read in default mode is how the defaults are put back.
    OdnLevels { mode, levels }
}

/// The functions bound from the library.
#[derive(Clone, Copy)]
struct Functions {
    destroy: MainDestroy,
    adapter_count: AdapterCount,
    adapter_info: AdapterInfoGet,
    overdrive_caps: OverdriveCaps,
    capabilities: CapabilitiesGet,
    system_clocks_get: LevelsCall,
    system_clocks_set: LevelsCall,
    memory_clocks_get: LevelsCall,
    memory_clocks_set: LevelsCall,
    fan_get: FanCall,
    fan_set: FanCall,
    power_get: PowerCall,
    power_set: PowerCall,
}

/// The loaded library with an open context.
pub struct AdlLibrary {
    /// Unloads the library after [`Drop`] has destroyed the context: a
    /// struct's fields drop once its own `drop` has run.
    _module: ModuleGuard,
    context: Context,
    functions: Functions,
}

/// Unloads the library when dropped: when binding it fails half way, and once
/// an [`AdlLibrary`] owns it, after the context is destroyed.
struct ModuleGuard(HMODULE);

impl Drop for ModuleGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from `LoadLibraryExW` and is freed once.
        unsafe { FreeLibrary(self.0) };
    }
}

/// The address of `name` in `module` as a function pointer of type `F`.
///
/// # Safety
///
/// `module` must be the loaded ADL library and `F` the `extern "C"` function
/// pointer type the SDK declares for `name`.
unsafe fn export<F: Copy>(module: HMODULE, name: &'static CStr) -> Result<F, AdlError> {
    // SAFETY: `module` is a loaded module and `name` is NUL-terminated.
    let address = unsafe { GetProcAddress(module, name.as_ptr().cast()) };
    let Some(address) = address else {
        return Err(AdlError::MissingExport(name.to_str().unwrap_or("?")));
    };
    assert_eq!(size_of::<F>(), size_of_val(&address), "F is a function pointer type");
    // SAFETY: both are function pointers of the same size; the caller
    // guarantees `F` is the function's real signature.
    Ok(unsafe { std::mem::transmute_copy(&address) })
}

fn checked(function: &'static str, code: i32) -> Result<(), AdlError> {
    if code == ADL_OK { Ok(()) } else { Err(AdlError::Call { function, code }) }
}

impl AdlLibrary {
    /// Loads the driver's library from the system directory and opens a
    /// context for the cards that are present.
    pub fn load() -> Result<Self, AdlError> {
        let name = wide(LIBRARY);
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the
        // call; the flag restricts the search to the system directory.
        let module =
            unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return Err(AdlError::NotInstalled);
        }
        // Owns the module until the library value takes it over below, so
        // that a binding that fails on the way unloads it.
        let guard = ModuleGuard(module);
        // In each binding below the annotated type is the function pointer
        // type `adl_sdk.h` or the Overdrive N sample declares for that export.
        // SAFETY: `module` is the ADL library, loaded above, and `MainCreate`
        // is the signature of `ADL2_Main_Control_Create`.
        let create: MainCreate = unsafe { export(module, c"ADL2_Main_Control_Create") }?;
        // SAFETY: `module` is still that library; `MainDestroy` is the
        // signature of `ADL2_Main_Control_Destroy`.
        let destroy: MainDestroy = unsafe { export(module, c"ADL2_Main_Control_Destroy") }?;
        // SAFETY: `AdapterCount` is the signature of
        // `ADL2_Adapter_NumberOfAdapters_Get`.
        let adapter_count: AdapterCount =
            unsafe { export(module, c"ADL2_Adapter_NumberOfAdapters_Get") }?;
        // SAFETY: `AdapterInfoGet` is the signature of
        // `ADL2_Adapter_AdapterInfo_Get`.
        let adapter_info: AdapterInfoGet =
            unsafe { export(module, c"ADL2_Adapter_AdapterInfo_Get") }?;
        // SAFETY: `OverdriveCaps` is the signature of `ADL2_Overdrive_Caps`.
        let overdrive_caps: OverdriveCaps = unsafe { export(module, c"ADL2_Overdrive_Caps") }?;
        // SAFETY: `CapabilitiesGet` is the signature of
        // `ADL2_OverdriveN_CapabilitiesX2_Get`.
        let capabilities: CapabilitiesGet =
            unsafe { export(module, c"ADL2_OverdriveN_CapabilitiesX2_Get") }?;
        // SAFETY: `LevelsCall` is the signature of
        // `ADL2_OverdriveN_SystemClocksX2_Get`.
        let system_clocks_get: LevelsCall =
            unsafe { export(module, c"ADL2_OverdriveN_SystemClocksX2_Get") }?;
        // SAFETY: `LevelsCall` is also the signature of
        // `ADL2_OverdriveN_SystemClocksX2_Set`.
        let system_clocks_set: LevelsCall =
            unsafe { export(module, c"ADL2_OverdriveN_SystemClocksX2_Set") }?;
        // SAFETY: `LevelsCall` is the signature of
        // `ADL2_OverdriveN_MemoryClocksX2_Get`.
        let memory_clocks_get: LevelsCall =
            unsafe { export(module, c"ADL2_OverdriveN_MemoryClocksX2_Get") }?;
        // SAFETY: `LevelsCall` is also the signature of
        // `ADL2_OverdriveN_MemoryClocksX2_Set`.
        let memory_clocks_set: LevelsCall =
            unsafe { export(module, c"ADL2_OverdriveN_MemoryClocksX2_Set") }?;
        // SAFETY: `FanCall` is the signature of `ADL2_OverdriveN_FanControl_Get`.
        let fan_get: FanCall = unsafe { export(module, c"ADL2_OverdriveN_FanControl_Get") }?;
        // SAFETY: `FanCall` is also the signature of
        // `ADL2_OverdriveN_FanControl_Set`.
        let fan_set: FanCall = unsafe { export(module, c"ADL2_OverdriveN_FanControl_Set") }?;
        // SAFETY: `PowerCall` is the signature of
        // `ADL2_OverdriveN_PowerLimit_Get`.
        let power_get: PowerCall = unsafe { export(module, c"ADL2_OverdriveN_PowerLimit_Get") }?;
        // SAFETY: `PowerCall` is also the signature of
        // `ADL2_OverdriveN_PowerLimit_Set`.
        let power_set: PowerCall = unsafe { export(module, c"ADL2_OverdriveN_PowerLimit_Set") }?;
        let functions = Functions {
            destroy,
            adapter_count,
            adapter_info,
            overdrive_caps,
            capabilities,
            system_clocks_get,
            system_clocks_set,
            memory_clocks_get,
            memory_clocks_set,
            fan_get,
            fan_set,
            power_get,
            power_set,
        };
        let mut context: Context = ptr::null_mut();
        // SAFETY: `allocate` matches the callback type, 1 asks for present
        // adapters only, and `context` is valid for the write.
        let code = unsafe { create(allocate, 1, &mut context) };
        checked("ADL2_Main_Control_Create", code)?;
        // The library value owns the module from here and frees it on drop.
        Ok(Self { _module: guard, context, functions })
    }

    fn levels(
        &mut self,
        call: LevelsCall,
        function: &'static str,
        adapter: i32,
        mode: i32,
        count: usize,
    ) -> Result<OdnLevels, AdlError> {
        let blank = vec![OdnLevel::default(); count.min(MAX_LEVELS)];
        let mut buffer = levels_buffer(mode, &blank);
        // SAFETY: the buffer starts with a header whose size field counts the
        // whole buffer, which is what the library is told it may fill.
        let code = unsafe { call(self.context, adapter, buffer.as_mut_ptr().cast()) };
        checked(function, code)?;
        Ok(levels_from(&buffer, mode))
    }

    fn set_levels(
        &mut self,
        call: LevelsCall,
        function: &'static str,
        adapter: i32,
        levels: &OdnLevels,
    ) -> Result<(), AdlError> {
        let mut buffer = levels_buffer(levels.mode, &levels.levels);
        // SAFETY: as in `levels`; the library reads the buffer it is given.
        let code = unsafe { call(self.context, adapter, buffer.as_mut_ptr().cast()) };
        checked(function, code)
    }
}

impl Drop for AdlLibrary {
    fn drop(&mut self) {
        // SAFETY: the context was created by this library and is destroyed
        // once. The module is still loaded: `_module` is dropped, and so
        // unloads it, only after this body.
        unsafe { (self.functions.destroy)(self.context) };
    }
}

impl AdlApi for AdlLibrary {
    fn adapters(&mut self) -> Result<Vec<AdlAdapter>, AdlError> {
        let mut count = 0i32;
        // SAFETY: `count` is valid for the write.
        let code = unsafe { (self.functions.adapter_count)(self.context, &mut count) };
        checked("ADL2_Adapter_NumberOfAdapters_Get", code)?;
        let count = usize::try_from(count).unwrap_or(0).min(MAX_ADAPTERS);
        if count == 0 {
            return Ok(Vec::new());
        }
        // SAFETY: `AdapterInfo` is integers and byte arrays, for which all
        // zeroes is a valid value.
        let blank: AdapterInfo = unsafe { std::mem::zeroed() };
        let mut records = vec![blank; count];
        let bytes = i32::try_from(size_of_val(records.as_slice())).unwrap_or(0);
        // SAFETY: `records` holds `count` records and `bytes` is their size.
        let code =
            unsafe { (self.functions.adapter_info)(self.context, records.as_mut_ptr(), bytes) };
        checked("ADL2_Adapter_AdapterInfo_Get", code)?;
        Ok(records
            .iter()
            .map(|record| AdlAdapter {
                index: record.adapter_index,
                bus: record.bus_number,
                device: record.device_number,
                function: record.function_number,
                name: text(&record.adapter_name),
                pnp: text(&record.pnp_string),
            })
            .collect())
    }

    fn overdrive_version(&mut self, adapter: i32) -> Option<i32> {
        let (mut supported, mut enabled, mut version) = (0i32, 0i32, 0i32);
        // SAFETY: the three integers are valid for the writes.
        let code = unsafe {
            (self.functions.overdrive_caps)(
                self.context,
                adapter,
                &mut supported,
                &mut enabled,
                &mut version,
            )
        };
        (code == ADL_OK && supported != 0 && enabled != 0).then_some(version)
    }

    fn capabilities(&mut self, adapter: i32) -> Result<OdnCapabilities, AdlError> {
        let mut capabilities = OdnCapabilities::default();
        // SAFETY: `OdnCapabilities` has the layout of `ADLODNCapabilitiesX2`
        // and is valid for the write.
        let code =
            unsafe { (self.functions.capabilities)(self.context, adapter, &mut capabilities) };
        checked("ADL2_OverdriveN_CapabilitiesX2_Get", code)?;
        Ok(capabilities)
    }

    fn system_clocks(
        &mut self,
        adapter: i32,
        mode: i32,
        count: usize,
    ) -> Result<OdnLevels, AdlError> {
        let call = self.functions.system_clocks_get;
        self.levels(call, "ADL2_OverdriveN_SystemClocksX2_Get", adapter, mode, count)
    }

    fn set_system_clocks(&mut self, adapter: i32, levels: &OdnLevels) -> Result<(), AdlError> {
        let call = self.functions.system_clocks_set;
        self.set_levels(call, "ADL2_OverdriveN_SystemClocksX2_Set", adapter, levels)
    }

    fn memory_clocks(
        &mut self,
        adapter: i32,
        mode: i32,
        count: usize,
    ) -> Result<OdnLevels, AdlError> {
        let call = self.functions.memory_clocks_get;
        self.levels(call, "ADL2_OverdriveN_MemoryClocksX2_Get", adapter, mode, count)
    }

    fn set_memory_clocks(&mut self, adapter: i32, levels: &OdnLevels) -> Result<(), AdlError> {
        let call = self.functions.memory_clocks_set;
        self.set_levels(call, "ADL2_OverdriveN_MemoryClocksX2_Set", adapter, levels)
    }

    fn fan(&mut self, adapter: i32) -> Result<OdnFan, AdlError> {
        let mut fan = OdnFan::default();
        // SAFETY: `OdnFan` has the layout of `ADLODNFanControl`.
        let code = unsafe { (self.functions.fan_get)(self.context, adapter, &mut fan) };
        checked("ADL2_OverdriveN_FanControl_Get", code)?;
        Ok(fan)
    }

    fn set_fan(&mut self, adapter: i32, fan: &OdnFan) -> Result<(), AdlError> {
        let mut fan = *fan;
        // SAFETY: as in `fan`; the library reads the structure.
        let code = unsafe { (self.functions.fan_set)(self.context, adapter, &mut fan) };
        checked("ADL2_OverdriveN_FanControl_Set", code)
    }

    fn power(&mut self, adapter: i32) -> Result<OdnPower, AdlError> {
        let mut power = OdnPower::default();
        // SAFETY: `OdnPower` has the layout of `ADLODNPowerLimitSetting`.
        let code = unsafe { (self.functions.power_get)(self.context, adapter, &mut power) };
        checked("ADL2_OverdriveN_PowerLimit_Get", code)?;
        Ok(power)
    }

    fn set_power(&mut self, adapter: i32, power: &OdnPower) -> Result<(), AdlError> {
        let mut power = *power;
        // SAFETY: as in `power`; the library reads the structure.
        let code = unsafe { (self.functions.power_set)(self.context, adapter, &mut power) };
        checked("ADL2_OverdriveN_PowerLimit_Set", code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuning::radeon::{OdnRange, mode};

    #[test]
    fn the_structures_have_the_sizes_the_sdk_declares() {
        // adl_structures.h: every field is an `int` or a 256-byte string.
        assert_eq!(size_of::<AdapterInfo>(), 4 * 9 + ADL_MAX_PATH * 6);
        assert_eq!(size_of::<OdnRange>(), 4 * 5);
        assert_eq!(size_of::<OdnCapabilities>(), 4 * 2 + size_of::<OdnRange>() * 10);
        assert_eq!(size_of::<OdnLevel>(), 4 * 4);
        assert_eq!(size_of::<OdnFan>(), 4 * 8);
        assert_eq!(size_of::<OdnPower>(), 4 * 3);
        assert_eq!(size_of::<LevelsHeader>(), 4 * 3);
    }

    #[test]
    fn a_levels_buffer_carries_its_size_mode_and_states_and_reads_back() {
        let levels = [
            OdnLevel { clock: 21_400, vddc: 700, enabled: 1, control: 4 },
            OdnLevel { clock: 118_300, vddc: 1050, enabled: 1, control: 7 },
        ];
        let buffer = levels_buffer(mode::MANUAL, &levels);
        // sizeof(ADLODNPerformanceLevelsX2) + one more level, as the sample
        // computes it: 28 + 16 bytes.
        assert_eq!(buffer[..3], [44, mode::MANUAL, 2]);
        assert_eq!(buffer[3..], [21_400, 700, 1, 4, 118_300, 1050, 1, 7]);
        assert_eq!(
            levels_from(&buffer, mode::DEFAULT),
            OdnLevels { mode: mode::DEFAULT, levels: levels.to_vec() }
        );
        assert_eq!(levels_from(&[], mode::CURRENT).levels, Vec::new());
        assert_eq!(levels_buffer(mode::CURRENT, &[]), [12, 0, 0]);
    }

    #[test]
    fn a_string_field_ends_at_its_terminator() {
        let mut field: [c_char; 16] = [0; 16];
        for (slot, byte) in field.iter_mut().zip(b" RX 550 \0junk") {
            *slot = *byte as c_char;
        }
        assert_eq!(text(&field), "RX 550");
        assert_eq!(text(&[0; 4]), "");
        let mut padded: [c_char; 16] = [0; 16];
        for (slot, byte) in padded.iter_mut().zip(b"Radeon  RX\t550") {
            *slot = *byte as c_char;
        }
        assert_eq!(text(&padded), "Radeon RX 550", "runs of whitespace collapse");
    }

    #[test]
    fn a_failed_call_names_its_function() {
        assert_eq!(checked("ADL2_Main_Control_Create", ADL_OK), Ok(()));
        assert_eq!(
            checked("ADL2_Main_Control_Create", -1),
            Err(AdlError::Call { function: "ADL2_Main_Control_Create", code: -1 })
        );
    }

    #[test]
    fn the_allocator_gives_memory_and_refuses_a_negative_size() {
        // SAFETY: `allocate` has no requirement beyond being called.
        let memory = unsafe { allocate(16) };
        // SAFETY: as above; a negative size is refused before the heap is
        // touched.
        let refused = unsafe { allocate(-1) };
        assert!(!memory.is_null());
        assert!(refused.is_null());
        // SAFETY: `GetProcessHeap` has no preconditions.
        let heap = unsafe { GetProcessHeap() };
        // SAFETY: `memory` is 16 bytes from the process heap, freed once.
        unsafe { windows_sys::Win32::System::Memory::HeapFree(heap, 0, memory) };
    }

    /// Reads only. On a machine without the AMD driver the library is not
    /// there, which is what `load` must say; with it, every card the driver
    /// lists must be describable.
    #[test]
    fn this_machines_library_loads_and_describes_its_cards_or_is_not_installed() {
        let mut library = match AdlLibrary::load() {
            Ok(library) => library,
            Err(error) => {
                assert_eq!(error, AdlError::NotInstalled, "(load): expected no library at all");
                return;
            }
        };
        let adapters = library.adapters().unwrap();
        for adapter in &adapters {
            assert!(!adapter.name.is_empty(), "(adapter {}): expected a name", adapter.index);
            if library.overdrive_version(adapter.index) != Some(crate::tuning::radeon::OVERDRIVE_N)
            {
                continue;
            }
            let capabilities = library.capabilities(adapter.index).unwrap();
            let count = usize::try_from(capabilities.maximum_levels).unwrap();
            assert!(
                (1..=MAX_LEVELS).contains(&count),
                "(maximum levels): expected 1..={MAX_LEVELS} | received: {count}"
            );
            let current = library.system_clocks(adapter.index, mode::CURRENT, count).unwrap();
            let defaults = library.system_clocks(adapter.index, mode::DEFAULT, count).unwrap();
            assert_eq!((current.levels.len(), defaults.levels.len()), (count, count));
            assert!(
                defaults.levels[0].clock > 0,
                "(default state 0): expected a clock | received: {:?}",
                defaults.levels[0]
            );
            library.memory_clocks(adapter.index, mode::CURRENT, count).unwrap();
            library.fan(adapter.index).unwrap();
            library.power(adapter.index).unwrap();
        }
    }
}
