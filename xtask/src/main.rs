//! Developer tasks for the SysPilot workspace, run as `cargo xtask <task>`.

mod modules;
mod sha256;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <task>

tasks:
  fetch-modules [--dir <path>] [--force]
      Download the pinned PawnIO.Modules release, verify its SHA-256 and that
      of every module used, and place them in <repo>/pawnio-modules/
      (or <path>). Does nothing when the files are already correct, unless
      --force is given.";

/// A parsed command line.
#[derive(Debug, PartialEq, Eq)]
enum Task {
    FetchModules { directory: Option<PathBuf>, force: bool },
    Help,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Task, String> {
    let mut args = args.into_iter();
    let Some(task) = args.next() else {
        return Ok(Task::Help);
    };
    match task.as_str() {
        "help" | "--help" | "-h" => Ok(Task::Help),
        "fetch-modules" => {
            let mut directory = None;
            let mut force = false;
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--force" => force = true,
                    "--dir" => {
                        let value = args.next().ok_or("(fetch-modules): expected a path after --dir | received: end of arguments")?;
                        directory = Some(PathBuf::from(value));
                    }
                    other => {
                        return Err(format!(
                            "(fetch-modules): expected --dir <path> or --force | received: {other:?}"
                        ));
                    }
                }
            }
            Ok(Task::FetchModules { directory, force })
        }
        other => Err(format!("(xtask): expected a task name | received: {other:?}\n\n{USAGE}")),
    }
}

/// The workspace root: the parent of this crate's directory.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf).unwrap_or_default()
}

fn main() -> ExitCode {
    let task = match parse_args(std::env::args().skip(1)) {
        Ok(task) => task,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    match task {
        Task::Help => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Task::FetchModules { directory, force } => {
            let directory = directory.unwrap_or_else(|| repository_root().join("pawnio-modules"));
            match modules::fetch_modules(
                &modules::PINNED_RELEASE,
                &directory,
                force,
                &modules::WindowsTools,
            ) {
                Ok(modules::Outcome::UpToDate) => {
                    println!("pawnio-modules: already up to date in {}", directory.display());
                    ExitCode::SUCCESS
                }
                Ok(modules::Outcome::Fetched(targets)) => {
                    println!("pawnio-modules: verified and placed in {}", directory.display());
                    for target in targets {
                        println!("  {target}");
                    }
                    ExitCode::SUCCESS
                }
                Err(message) => {
                    eprintln!("{message}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Task, String> {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_arguments_print_help() {
        assert_eq!(parse(&[]), Ok(Task::Help));
        assert_eq!(parse(&["--help"]), Ok(Task::Help));
    }

    #[test]
    fn fetch_modules_defaults() {
        assert_eq!(
            parse(&["fetch-modules"]),
            Ok(Task::FetchModules { directory: None, force: false })
        );
    }

    #[test]
    fn fetch_modules_flags() {
        assert_eq!(
            parse(&["fetch-modules", "--force", "--dir", "out"]),
            Ok(Task::FetchModules { directory: Some(PathBuf::from("out")), force: true })
        );
    }

    #[test]
    fn dir_needs_a_value() {
        let error = parse(&["fetch-modules", "--dir"]).unwrap_err();
        assert!(error.contains("expected a path after --dir"), "{error}");
    }

    #[test]
    fn unknown_flags_and_tasks_are_named() {
        let error = parse(&["fetch-modules", "--fast"]).unwrap_err();
        assert!(error.contains("received: \"--fast\""), "{error}");
        let error = parse(&["build"]).unwrap_err();
        assert!(error.contains("received: \"build\""), "{error}");
    }

    #[test]
    fn repository_root_holds_the_workspace_manifest() {
        assert!(repository_root().join("Cargo.toml").is_file());
    }
}
