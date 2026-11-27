//! SysPilot update companion.
//!
//! When the user asks for an update, the app starts this and closes: Windows
//! Installer is about to replace its files. The companion shows the update
//! as it runs and opens the app again once the new version is in place, or
//! says why it is not.
//!
//! It is a small executable with no toolkit, and the app runs it from a
//! copy outside the installation folder, so nothing it holds is a file the
//! update replaces. It changes nothing: it only looks at the service's pipe
//! and at Windows Installer (see [`watch`]), and starts the app.
//!
//! `syspilot-companion --instance <name> --from <version> --app <path> [--to <version>]`
#![windows_subsystem = "windows"]

mod dialog;
mod probe;
mod progress;
mod watch;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use syspilot_proto::Instance;
use syspilot_proto::update::INSTALL_LOG;

use crate::dialog::{Choice, ProgressEnd};
use crate::probe::{MachineProbe, Probe};
use crate::watch::{Phase, Step, Watch};

/// Time between two looks.
const LOOK_INTERVAL: Duration = Duration::from_millis(500);

/// What the command line says.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    /// The service instance being updated.
    instance: Instance,
    /// The version running when the update was started.
    from: String,
    /// The version being installed, when the app knew it.
    to: Option<String>,
    /// The app to open again.
    app: PathBuf,
}

fn parse(arguments: &[String]) -> Result<Options, String> {
    let (mut instance, mut from, mut to, mut app) = (None, None, None, None);
    let mut rest = arguments.iter();
    while let Some(argument) = rest.next() {
        let value = rest
            .next()
            .cloned()
            .ok_or_else(|| format!("({argument}): expected a value | received: nothing"))?;
        match argument.as_str() {
            "--instance" => {
                instance = Some(Instance::named(&value).map_err(|error| error.to_string())?);
            }
            "--from" => from = Some(value),
            "--to" => to = Some(value),
            "--app" => app = Some(PathBuf::from(value)),
            other => {
                return Err(format!(
                    "(argument): expected --instance, --from, --to or --app | received: {other}"
                ));
            }
        }
    }
    let missing = |name: &str| format!("({name}): expected a value | received: no {name}");
    Ok(Options {
        instance: instance.ok_or_else(|| missing("--instance"))?,
        from: from.ok_or_else(|| missing("--from"))?,
        to,
        app: app.ok_or_else(|| missing("--app"))?,
    })
}

/// Where Windows Installer logs the update of `instance`: the service's
/// update directory.
fn install_log(instance: &Instance) -> String {
    let program_data = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    Path::new(&program_data)
        .join(instance.name())
        .join("updates")
        .join(INSTALL_LOG)
        .display()
        .to_string()
}

/// Looks until the update is over, handing every phase to `publish`, and
/// returns the last one. `elapsed` is the time since the update was
/// started; `pause` waits between two looks.
fn follow(
    probe: &mut dyn Probe,
    watch: &mut Watch,
    mut elapsed: impl FnMut() -> Duration,
    mut pause: impl FnMut(),
    mut publish: impl FnMut(&Phase),
) -> Phase {
    loop {
        let phase = watch.see(&probe.observe(), elapsed());
        publish(&phase);
        if phase.is_final() {
            return phase;
        }
        pause();
    }
}

/// The arguments the app is opened with: the instance, unless it is the
/// product. Nothing else, so that a new app that no longer knows an
/// argument does not fall back to the product's service.
fn app_arguments(instance: &Instance) -> Vec<String> {
    if instance.is_product() {
        Vec::new()
    } else {
        vec!["--instance".to_owned(), instance.name().to_owned()]
    }
}

/// Opens the app. It inherits this process's rights, which are the old
/// app's, so it does not ask for elevation again.
fn open_app(options: &Options) -> Result<(), String> {
    let mut command = Command::new(&options.app);
    command.args(app_arguments(&options.instance));
    if let Some(folder) = options.app.parent() {
        command.current_dir(folder);
    }
    command
        .spawn()
        .map(drop)
        .map_err(|error| format!("{} could not be started: {error}", options.app.display()))
}

fn run(options: &Options) {
    let shared = Arc::new(Mutex::new(Phase::Working(Step::Preparing)));
    let published = shared.clone();
    let (instance, from) = (options.instance.clone(), options.from.clone());
    let watcher = std::thread::spawn(move || {
        let started = Instant::now();
        let mut probe = MachineProbe::new(instance.clone(), &from);
        let mut watch = Watch::new(&from, &install_log(&instance));
        follow(
            &mut probe,
            &mut watch,
            || started.elapsed(),
            || std::thread::sleep(LOOK_INTERVAL),
            |phase| *published.lock().unwrap_or_else(PoisonError::into_inner) = phase.clone(),
        )
    });

    let last = match dialog::show_progress(shared, options.to.as_deref()) {
        ProgressEnd::Ended(phase) => phase,
        // Closed by the user: the update goes on, and nobody waits for it.
        ProgressEnd::Closed => return,
        // Without a window the update is still followed, unseen, so that
        // the app comes back all the same.
        ProgressEnd::NotShown => watcher
            .join()
            .unwrap_or_else(|_| Phase::Failed("The update could not be followed.".to_owned())),
    };
    let open = match last {
        Phase::Ready | Phase::AlreadyCurrent => true,
        Phase::Failed(reason) => {
            dialog::show_failure("The update did not finish", &reason, true) == Choice::OpenApp
        }
        Phase::Working(_) => false,
    };
    if open && let Err(problem) = open_app(options) {
        dialog::show_failure("SysPilot did not open", &problem, false);
    }
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match parse(&arguments) {
        Ok(options) => run(&options),
        // Only the app starts this; a bad command line is a bug in it.
        Err(problem) => _ = dialog::show_failure("The update cannot be followed", &problem, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::ScriptedProbe;
    use crate::watch::{Observation, Service};
    use syspilot_proto::message::UpdateState;

    fn arguments(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn every_option_is_understood() {
        let options = parse(&arguments(
            r"--instance SysPilot-Test.ui --from 0.3.0 --to 0.3.1 --app C:\SysPilot\syspilot.exe",
        ))
        .unwrap();
        assert_eq!(options.instance.name(), "SysPilot-Test.ui");
        assert_eq!((options.from.as_str(), options.to.as_deref()), ("0.3.0", Some("0.3.1")));
        assert_eq!(options.app, PathBuf::from(r"C:\SysPilot\syspilot.exe"));
        let without_target = parse(&arguments("--instance SysPilot --from 0.3.0 --app a.exe"));
        assert_eq!(without_target.unwrap().to, None);
    }

    #[test]
    fn mistakes_name_what_was_received() {
        assert_eq!(
            parse(&arguments("--from 0.3.0 --app a.exe")).unwrap_err(),
            "(--instance): expected a value | received: no --instance"
        );
        assert_eq!(
            parse(&arguments("--instance SysPilot --from")).unwrap_err(),
            "(--from): expected a value | received: nothing"
        );
        assert!(
            parse(&arguments("--colour red")).unwrap_err().ends_with("received: --colour"),
            "(unknown option): expected it named"
        );
        assert!(parse(&arguments("--instance a/b --from 1 --app a.exe")).is_err());
    }

    #[test]
    fn the_app_is_opened_for_its_instance_and_nothing_else() {
        assert!(app_arguments(&Instance::product()).is_empty());
        let test = Instance::named("SysPilot-Test.ui").unwrap();
        assert_eq!(app_arguments(&test), ["--instance", "SysPilot-Test.ui"]);
    }

    #[test]
    fn the_log_is_in_the_instances_update_directory() {
        let log = install_log(&Instance::named("SysPilot-Test.ui").unwrap());
        assert!(log.ends_with(r"SysPilot-Test.ui\updates\install.log"), "{log}");
    }

    #[test]
    fn following_publishes_every_phase_and_stops_at_the_final_one() {
        let mut probe = ScriptedProbe(vec![
            Observation {
                service: Service::Answering {
                    version: "0.3.0".into(),
                    update: UpdateState::Installing,
                },
                installer_busy: false,
            },
            Observation { service: Service::Absent, installer_busy: true },
            Observation { service: Service::OtherProtocol, installer_busy: false },
        ]);
        let mut watch = Watch::new("0.3.0", "install.log");
        let (mut seconds, mut pauses, mut published) = (0, 0, Vec::new());
        let last = follow(
            &mut probe,
            &mut watch,
            || {
                seconds += 1;
                Duration::from_secs(seconds)
            },
            || pauses += 1,
            |phase| published.push(phase.clone()),
        );
        assert_eq!(last, Phase::Ready);
        assert_eq!(
            published,
            [Phase::Working(Step::Preparing), Phase::Working(Step::Installing), Phase::Ready]
        );
        assert_eq!(pauses, 2, "(pauses): expected one between two looks");
    }
}
