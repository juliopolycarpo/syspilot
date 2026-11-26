//! Prints what the tuning interfaces of this machine offer: every device,
//! its controls with their ranges and defaults, and the values in effect.
//!
//! Reads only. `cargo run -p syspilot-hw --example tuning`

use syspilot_hw::tuning::{run, system_backends};
use syspilot_proto::tuning::TuneJob;

fn main() {
    let mut backends = system_backends();
    let report = run(&TuneJob::default(), &mut backends);
    if let Some(problem) = &report.problem {
        println!("An interface could not list its devices: {problem}");
    }
    if report.devices.is_empty() {
        println!("No device with a tuning interface.");
    }
    for device in &report.devices {
        println!("{} ({})", device.name, device.key);
        println!("  {}", device.interface);
        if let Some(problem) = &device.problem {
            println!("  problem: {problem}");
        }
        for control in &device.controls {
            let unit = control.unit.suffix();
            println!(
                "  {:<24} {:>6} {unit:<3}  default {:>6}  range {}..={} step {}",
                control.key,
                control.current,
                control.default,
                control.min,
                control.max,
                control.step
            );
        }
    }
}
