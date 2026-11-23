//! SysPilot sensor worker.
//!
//! Reading sensors pulls in Windows components that cost several megabytes
//! and never give them back, and it talks to buses and a driver that can
//! stall. The service therefore never reads hardware itself: while someone
//! needs values it runs this program, asks it for a sample once a second,
//! and lets it exit when nobody is looking.
//!
//! The protocol is in `syspilot_proto::worker`. This program only reads; it
//! takes no arguments and changes nothing.

mod machine;
mod worker;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Standard output carries frames, so nothing else may print there.
    let (requests, frames) = (std::io::stdin().lock(), std::io::stdout().lock());
    match worker::run(machine::probe, requests, frames) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
