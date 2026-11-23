//! Manual check of the PawnIO-backed providers: probes the driver, prints the
//! state, the features and the memory module sections, samples the hub three
//! times one second apart (power needs two reads of its provider, which the
//! hub makes every second tick) and prints every sensor, then the probe time
//! and the process's private memory.
//!
//! It only reads. Run it from an elevated shell, with the modules fetched by
//! `cargo xtask fetch-modules`:
//! `cargo run -p syspilot-hw --release --example driver_sensors [modules-dir]`.
//! Do not build it with the `spd-paging` feature on a machine that has memory
//! modules: that feature adds an SMBus write.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use syspilot_hw::Hub;
use syspilot_hw::driver;
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Private (committed, not shared) bytes of this process.
fn private_bytes() -> usize {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        PageFaultCount: 0,
        PeakWorkingSetSize: 0,
        WorkingSetSize: 0,
        QuotaPeakPagedPoolUsage: 0,
        QuotaPagedPoolUsage: 0,
        QuotaPeakNonPagedPoolUsage: 0,
        QuotaNonPagedPoolUsage: 0,
        PagefileUsage: 0,
        PeakPagefileUsage: 0,
        PrivateUsage: 0,
    };
    // SAFETY: the pseudo-handle is always valid; `counters` is a valid
    // `PROCESS_MEMORY_COUNTERS_EX` whose size is passed in `cb`, which the
    // API accepts through the base `PROCESS_MEMORY_COUNTERS` pointer.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            std::ptr::from_mut(&mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if ok == 0 { 0 } else { counters.PrivateUsage }
}

fn main() {
    let modules_dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../pawnio-modules"));
    println!("private memory at start: {} KiB", private_bytes() / 1024);

    let started = Instant::now();
    let probed = driver::probe(&modules_dir);
    let probe_time = started.elapsed();

    println!("\ndriver: {:?}", probed.state);
    for feature in &probed.features {
        println!(
            "  {:<15} {:<12} {}\n  {:<15} {}",
            feature.key,
            if feature.available { "available" } else { "unavailable" },
            feature.title,
            "",
            feature.detail
        );
    }
    for section in &probed.info {
        println!("\n[{}]", section.title);
        for (label, value) in &section.rows {
            println!("  {label:<22} {value}");
        }
    }

    let mut hub = Hub::new(probed.providers, 1);
    // The processor and the Super I/O are read every second tick, so three
    // ticks give power its two reads.
    for tick in 0..3 {
        if tick > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        hub.sample();
    }

    let list = hub.list();
    println!("\n{} sensors on {} devices", list.sensors.len(), list.devices.len());
    for (index, device) in list.devices.iter().enumerate() {
        println!("\n[{:?}] {}", device.class, device.name);
        for sensor in list.sensors.iter().filter(|sensor| sensor.device as usize == index) {
            let value = match hub.values()[sensor.id as usize] {
                Some(value) => format!("{value:.2} {}", sensor.kind.unit()),
                None => "n/a".to_owned(),
            };
            println!("  {:<24} {:<28} {value}", sensor.key, sensor.label);
        }
    }

    println!("\nprobe time: {probe_time:?}");
    println!("private memory with the hub: {} KiB", private_bytes() / 1024);
    drop(hub);
    println!("private memory after dropping the hub: {} KiB", private_bytes() / 1024);
}
