//! SysPilot updater.
//!
//! Started by the service, as LocalSystem, with one command; see
//! [`syspilot_proto::update`] for what it prints. It is a separate
//! executable so that the resident service carries no network code, and so
//! that everything that reads bytes from the internet lives in a process
//! that runs for seconds a day.
//!
//! - `check`: is there a signed release newer than this build?
//! - `install <directory>`: download it into `directory`, verify it, start
//!   Windows Installer and exit. The directory must be one only LocalSystem
//!   and administrators can write; the service passes its own.
//! - `verify <file> [signature]`: would this build accept `file` as a
//!   release? For the maintainer, before publishing.

mod github;
mod msi;
mod release;
mod sshsig;
mod version;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use syspilot_proto::update::{CHECK_COMMAND, INSTALL_COMMAND};

use crate::github::GitHubReleases;
use crate::msi::WindowsInstaller;
use crate::version::Version;

const USAGE: &str = "\
Usage: syspilot-update <command>

  check                      Say whether a newer signed release is published
  install <directory>        Download it there, verify it and install it
  verify <file> [signature]  Say whether this build would accept the file
                             (the signature defaults to <file>.sig)
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Check,
    Install { directory: PathBuf },
    Verify { file: PathBuf, signature: PathBuf },
}

fn parse(arguments: &[String]) -> Result<Command, String> {
    let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    match words[..] {
        [CHECK_COMMAND] => Ok(Command::Check),
        [INSTALL_COMMAND, directory] => Ok(Command::Install { directory: directory.into() }),
        ["verify", file] => {
            Ok(Command::Verify { file: file.into(), signature: format!("{file}.sig").into() })
        }
        ["verify", file, signature] => {
            Ok(Command::Verify { file: file.into(), signature: signature.into() })
        }
        _ => Err(format!("(arguments): expected a command | received: {arguments:?}\n{USAGE}")),
    }
}

/// The version of this build, which is the version of the installed product.
fn current_version() -> Result<Version, String> {
    let text = env!("CARGO_PKG_VERSION");
    Version::parse(text)
        .ok_or_else(|| format!("(this build): expected a version like 0.2.1 | received: {text:?}"))
}

fn run(command: Command) -> Result<String, String> {
    let trusted = sshsig::trusted_keys()?;
    match command {
        Command::Check => {
            let mut releases = GitHubReleases::new()?;
            Ok(release::check(&mut releases, &trusted, current_version()?)?.to_line())
        }
        Command::Install { directory } => {
            if !Path::new(&directory).is_dir() {
                return Err(format!(
                    "(install): expected an existing directory | received: {}",
                    directory.display()
                ));
            }
            let mut releases = GitHubReleases::new()?;
            let report = release::install(
                &mut releases,
                &WindowsInstaller,
                &trusted,
                current_version()?,
                &directory,
            )?;
            Ok(report.to_line())
        }
        Command::Verify { file, signature } => {
            release::verify_file(&file, &signature, &trusted)?;
            Ok(format!("{} is signed with a SysPilot release key.", file.display()))
        }
    }
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let outcome = parse(&arguments).and_then(run);
    // The service may be gone by the time an installation has been started;
    // a closed pipe must not turn a success into a panic.
    use std::io::Write as _;
    match outcome {
        Ok(line) => {
            let _ = writeln!(std::io::stdout(), "{line}");
            ExitCode::SUCCESS
        }
        Err(problem) => {
            let _ = writeln!(std::io::stderr(), "{problem}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(line: &str) -> Result<Command, String> {
        parse(&line.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
    }

    #[test]
    fn commands_parse() {
        assert_eq!(command("check"), Ok(Command::Check));
        assert_eq!(
            command(r"install C:\ProgramData\SysPilot\updates"),
            Ok(Command::Install { directory: r"C:\ProgramData\SysPilot\updates".into() })
        );
        assert_eq!(
            command("verify a.msi"),
            Ok(Command::Verify { file: "a.msi".into(), signature: "a.msi.sig".into() })
        );
        assert_eq!(
            command("verify a.msi b.sig"),
            Ok(Command::Verify { file: "a.msi".into(), signature: "b.sig".into() })
        );
    }

    #[test]
    fn anything_else_shows_what_was_received_and_the_usage() {
        for line in ["", "install", "check now", "update", "verify"] {
            let problem = command(line).unwrap_err();
            assert!(
                problem.starts_with("(arguments): expected a command | received: ")
                    && problem.contains("Usage: syspilot-update"),
                "{line:?}: {problem}"
            );
        }
    }

    #[test]
    fn this_build_has_a_version_the_updater_can_compare() {
        assert_eq!(current_version().unwrap().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
