//! SysPilot service and command-line interface.
//!
//! One executable is both the resident service and the tool that installs,
//! inspects and drives it. See `syspilot-svc --help`.

mod catalog;
mod cli;
mod core;
mod events;
mod fans;
mod hardware;
mod host;
mod install;
mod logger;
mod pipe;
mod privilege;
mod runtime;
mod service;
mod storage;
mod watch;
mod worker;

use std::process::ExitCode;

use cli::Command;

/// Exit code for a command line that could not be understood.
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let invocation = match cli::parse(std::env::args().skip(1)) {
        Ok(invocation) => invocation,
        Err(problem) => {
            eprintln!("{problem}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let instance = &invocation.instance;
    let outcome = match invocation.command {
        Command::Help => Ok(cli::USAGE.to_owned()),
        Command::Version => Ok(cli::version()),
        Command::Install { start } => install::install(instance, start),
        Command::Uninstall { keep_tweaks } => install::uninstall(instance, keep_tweaks),
        Command::Run => service::run_in_console(instance).map(|()| String::new()),
        Command::Service => service::run_as_service(instance).map(|()| String::new()),
        command => cli::run_client_command(instance, command),
    };
    match outcome {
        Ok(text) => {
            let text = text.trim_end();
            if !text.is_empty() {
                println!("{text}");
            }
            ExitCode::SUCCESS
        }
        Err(problem) => {
            eprintln!("{problem}");
            ExitCode::FAILURE
        }
    }
}
