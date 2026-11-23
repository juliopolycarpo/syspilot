//! Manual check of the driverless providers: probes the machine, samples
//! twice one second apart (rates need two reads) and prints every sensor,
//! then the system information.
//!
//! Run with `cargo run -p syspilot-hw --release --example sensors`.

use std::time::{Duration, Instant};

use syspilot_hw::Hub;
use syspilot_hw::os;
use syspilot_hw::sysinfo;
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
    println!("private memory at start: {} KiB", private_bytes() / 1024);

    // Probe each provider on its own to see where the time goes.
    let mut providers = Vec::new();
    let started = Instant::now();
    for &(name, probe) in os::PROBES {
        let provider_started = Instant::now();
        let found = probe();
        println!(
            "probe {name}: {:?} ({} provider(s)), private memory {} KiB",
            provider_started.elapsed(),
            found.len(),
            private_bytes() / 1024
        );
        providers.extend(found);
    }
    let probe_time = started.elapsed();

    let mut hub = Hub::new(providers, 1);
    let started = Instant::now();
    hub.sample();
    let first_sample_time = started.elapsed();

    std::thread::sleep(Duration::from_secs(1));
    let started = Instant::now();
    hub.sample();
    let second_sample_time = started.elapsed();

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

    println!("\nprobe (all): {probe_time:?}");
    println!("first sample: {first_sample_time:?}");
    println!("second sample: {second_sample_time:?}");
    println!("private memory at end: {} KiB", private_bytes() / 1024);
    // The service drops its hub when the last subscriber leaves; check that
    // the providers give their memory back.
    drop(hub);
    println!("private memory after dropping the hub: {} KiB", private_bytes() / 1024);

    let started = Instant::now();
    let info = sysinfo::collect();
    let collect_time = started.elapsed();
    let summary = &info.summary;
    println!("\n=== system information ===");
    println!("computer: {}", summary.computer);
    println!("model:    {}", summary.model);
    println!("cpu:      {}", summary.cpu);
    println!("gpus:     {}", summary.gpus.join("; "));
    println!("memory:   {} bytes", summary.memory_bytes);
    println!("os:       {}", summary.os);
    println!("bios:     {}", summary.bios);
    for section in &info.sections {
        println!("\n[{}]", section.title);
        for (label, value) in &section.rows {
            println!("  {label:<22} {value}");
        }
    }
    println!("\nsysinfo::collect: {collect_time:?}, private memory {} KiB", private_bytes() / 1024);
}
