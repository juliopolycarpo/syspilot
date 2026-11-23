//! SysPilot sensor worker.
//!
//! Reading sensors pulls in Windows components that cost several megabytes
//! and never give them back, and it talks to buses and a driver that can
//! stall. The service therefore never reads hardware itself: while someone
//! needs values it runs this program, asks it for a sample once a second,
//! and lets it exit when nobody is looking.
//!
//! The protocol is in `syspilot_proto::worker`. This program takes no
//! arguments. It only reads, except in a build with the `fan-control` cargo
//! feature, which can also follow the profile's fan curves on a board whose
//! catalog entry a human validated; see `fans`. Releases are built without
//! that feature.

#[cfg(all(test, feature = "fan-control"))]
mod fan_fakes;
mod fans;
mod machine;
mod worker;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Standard output carries frames, so nothing else may print there.
    // The reading thread owns standard input, which is why this is `Stdin`
    // and not its lock: a lock cannot move to another thread.
    let (requests, frames) = (std::io::stdin(), std::io::stdout().lock());
    match worker::run(machine::probe, requests, frames) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
