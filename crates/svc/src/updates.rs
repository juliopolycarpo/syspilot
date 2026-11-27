//! Keeping SysPilot itself up to date.
//!
//! The service decides *when*: once a day it has the updater look for a new
//! version, and depending on the profile it then only says so or has it
//! installed. *How* is the updater's business, a separate executable that
//! runs for the length of one job (see [`syspilot_proto::update`]). The
//! service holds no network code and verifies nothing itself; it starts the
//! executable next to its own and reads one line.
//!
//! [`Updates`] is the state machine, free of processes and clocks so that it
//! runs against `FakeUpdater` in tests. [`ProcessUpdater`] is the real
//! thing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use syspilot_proto::message::{UpdateState, UpdateStatus};
use syspilot_proto::profile::UpdateMode;
use syspilot_proto::update::{CHECK_COMMAND, INSTALL_COMMAND, UPDATER_EXECUTABLE, UpdaterReport};

use crate::events::{Event, EventQueue};

/// How long after the service starts the first look is made. Not at once: a
/// service start is usually a boot, when the machine is busy and may not be
/// online yet.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10 * 60);
/// Time between looks.
const CHECK_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);
/// How long Windows Installer gets to stop this service once it has been
/// started. A service that is still running after that was not updated.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// `CREATE_NO_WINDOW`: a console service must not flash a window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Longest reason kept from the updater, in characters.
const REASON_MAX: usize = 300;

/// What the updater is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Look for a new version.
    Check,
    /// Download, verify and install the newest version.
    Install,
}

/// What a finished job reported, or why it failed.
pub type Outcome = Result<UpdaterReport, String>;

/// Runs update jobs.
pub trait Updater {
    /// Starts `job` without waiting for it. When it ends, an
    /// [`Event::UpdateFinished`] reaches the event loop.
    fn start(&mut self, job: Job);
}

/// Turns what the updater process left behind into an outcome.
fn outcome_of(succeeded: bool, stdout: &str, stderr: &str) -> Outcome {
    if succeeded {
        return UpdaterReport::parse_line(stdout).ok_or_else(|| {
            format!("(updater): expected one line of its protocol | received: {stdout:?}")
        });
    }
    // The first line is the sentence; anything after it is detail.
    let reason: String =
        stderr.trim().lines().next().unwrap_or("").chars().take(REASON_MAX).collect();
    Err(if reason.is_empty() { "The updater ended without saying why.".to_owned() } else { reason })
}

/// Runs the updater executable that sits next to the service's. It is
/// addressed by its full path, never searched for, so nothing else can be
/// run in its place.
pub struct ProcessUpdater {
    queue: Arc<EventQueue>,
    /// Where a download goes: a directory only LocalSystem and
    /// administrators can write.
    directory: PathBuf,
}

impl ProcessUpdater {
    /// An updater that reports to `queue` and downloads into `directory`.
    pub fn new(queue: Arc<EventQueue>, directory: PathBuf) -> Self {
        Self { queue, directory }
    }
}

/// Removes the packages earlier installations left in `directory`, and
/// partial downloads. The installation log stays. Best effort: a file that
/// cannot be removed now is removed before a later look.
///
/// The updater only clears the directory before its next download, so
/// without this an installed package (about 20 MB) would stay until the
/// next release. It is not done when the new service starts: Windows
/// Installer starts it while it still reads the package.
fn remove_installed_packages(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for path in entries.flatten().map(|entry| entry.path()) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        if path.is_file() && (name.ends_with(".msi") || name.ends_with(".part")) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

impl Updater for ProcessUpdater {
    fn start(&mut self, job: Job) {
        use std::os::windows::process::CommandExt;
        let (queue, directory) = (self.queue.clone(), self.directory.clone());
        // A job is a download; the event loop must not wait for it.
        std::thread::spawn(move || {
            if job == Job::Check {
                remove_installed_packages(&directory);
            }
            let outcome = std::env::current_exe()
                .map(|service| service.with_file_name(UPDATER_EXECUTABLE))
                .and_then(|updater| {
                    let mut command = Command::new(updater);
                    match job {
                        Job::Check => command.arg(CHECK_COMMAND),
                        Job::Install => command.arg(INSTALL_COMMAND).arg(&directory),
                    };
                    command.stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW).output()
                })
                .map_or_else(
                    |error| Err(format!("The updater could not be started: {error}")),
                    |output| {
                        outcome_of(
                            output.status.success(),
                            &String::from_utf8_lossy(&output.stdout),
                            &String::from_utf8_lossy(&output.stderr),
                        )
                    },
                );
            queue.push(Event::UpdateFinished { job, outcome });
        });
    }
}

/// When and whether SysPilot updates itself.
pub struct Updates {
    /// `None` for a copy that does not update itself: a console run or a
    /// test instance, which no release replaces.
    updater: Option<Box<dyn Updater>>,
    mode: UpdateMode,
    state: UpdateState,
    checked_unix: Option<u64>,
    /// The job under way. One at a time.
    running: Option<Job>,
    /// When the next look is due. `None` when updates are off.
    next_check: Option<Instant>,
    /// Until when a started installation may take to stop this service.
    install_deadline: Option<Instant>,
    /// Where Windows Installer's log is, for the message after a failure.
    log: String,
}

/// What a request for an update is answered with when there is no updater.
pub const UNAVAILABLE: &str =
    "This copy of SysPilot does not update itself. Install a release to get updates.";

impl Updates {
    /// The state at service start. With updates on, the first look is due
    /// [`FIRST_CHECK_DELAY`] from `now`.
    pub fn new(
        updater: Option<Box<dyn Updater>>,
        mode: UpdateMode,
        log: String,
        now: Instant,
    ) -> Self {
        let scheduled = updater.is_some() && mode != UpdateMode::Off;
        Self {
            updater,
            mode,
            state: UpdateState::Idle,
            checked_unix: None,
            running: None,
            next_check: scheduled.then(|| now + FIRST_CHECK_DELAY),
            install_deadline: None,
            log,
        }
    }

    /// What clients are told.
    pub fn status(&self) -> UpdateStatus {
        if self.updater.is_none() {
            return UpdateStatus {
                mode: self.mode,
                state: UpdateState::Unavailable,
                checked_unix: None,
            };
        }
        UpdateStatus { mode: self.mode, state: self.state.clone(), checked_unix: self.checked_unix }
    }

    fn start(&mut self, job: Job) {
        if let Some(updater) = self.updater.as_mut() {
            updater.start(job);
            self.running = Some(job);
        }
    }

    /// Whether an installation was started and has neither failed nor run
    /// out of time. Nothing else is started meanwhile: the service is about
    /// to be stopped.
    fn installing(&self) -> bool {
        self.state == UpdateState::Installing
    }

    /// Takes a new mode from the profile.
    pub fn set_mode(&mut self, mode: UpdateMode, clients: usize, now: Instant) {
        let was = std::mem::replace(&mut self.mode, mode);
        if self.updater.is_none() || was == mode {
            return;
        }
        if mode == UpdateMode::Off {
            self.next_check = None;
            // Off also means not mentioning what an earlier look found.
            if matches!(self.state, UpdateState::Available { .. } | UpdateState::Failed { .. }) {
                self.state = UpdateState::Idle;
            }
        } else if was == UpdateMode::Off {
            // Switched on: the user expects to learn soon where they stand.
            self.next_check = Some(now);
        }
        self.install_if_wanted(clients);
    }

    /// Looks for a new version now, whatever the mode: the user asked.
    pub fn check_now(&mut self) -> Result<(), &'static str> {
        if self.updater.is_none() {
            return Err(UNAVAILABLE);
        }
        if self.running.is_none() && !self.installing() {
            self.state = UpdateState::Checking;
            self.start(Job::Check);
        }
        Ok(())
    }

    /// Installs the newest version now, whatever the mode and whoever is
    /// connected: the user asked. The updater looks again by itself, so this
    /// needs no earlier check.
    pub fn install_now(&mut self) -> Result<(), &'static str> {
        if self.updater.is_none() {
            return Err(UNAVAILABLE);
        }
        match self.running {
            Some(Job::Install) => {}
            // The look under way ends in a moment; its report then finds
            // the state at Installing and starts the installation.
            Some(Job::Check) => self.state = UpdateState::Installing,
            None if self.installing() => {}
            None => {
                self.state = UpdateState::Installing;
                self.start(Job::Install);
            }
        }
        Ok(())
    }

    /// In automatic mode, installs what was found while no app is open: an
    /// installation closes the app, which must not happen under the user's
    /// hands.
    fn install_if_wanted(&mut self, clients: usize) {
        let found = matches!(self.state, UpdateState::Available { .. });
        if self.mode == UpdateMode::Automatic && found && clients == 0 && self.running.is_none() {
            self.state = UpdateState::Installing;
            self.start(Job::Install);
        }
    }

    /// The number of connected clients changed.
    pub fn clients_changed(&mut self, clients: usize) {
        self.install_if_wanted(clients);
    }

    /// A job ended. `unix_secs` is the time for "last checked".
    pub fn finished(
        &mut self,
        job: Job,
        outcome: Outcome,
        clients: usize,
        now: Instant,
        unix_secs: u64,
    ) {
        self.running = None;
        // The user asked to install while a look was under way.
        let install_asked = job == Job::Check && self.installing();
        if job == Job::Check && self.mode != UpdateMode::Off {
            self.next_check = Some(now + CHECK_PERIOD);
        }
        match outcome {
            Ok(UpdaterReport::Current) => {
                self.checked_unix = Some(unix_secs);
                self.state = UpdateState::Idle;
            }
            Ok(UpdaterReport::Available(version)) => {
                self.checked_unix = Some(unix_secs);
                self.state = UpdateState::Available { version };
                if install_asked {
                    self.state = UpdateState::Installing;
                    self.start(Job::Install);
                } else {
                    self.install_if_wanted(clients);
                }
            }
            // Windows Installer runs now and will stop this service. If it
            // has not by the deadline, it failed.
            Ok(UpdaterReport::Installing(_)) => {
                self.state = UpdateState::Installing;
                self.install_deadline = Some(now + INSTALL_TIMEOUT);
            }
            Err(reason) => self.state = UpdateState::Failed { reason },
        }
    }

    /// Does what has become due: the daily look, and giving up on an
    /// installation that never stopped the service.
    pub fn run_due(&mut self, now: Instant) {
        if self.install_deadline.is_some_and(|deadline| deadline <= now) {
            self.install_deadline = None;
            self.state = UpdateState::Failed {
                reason: format!(
                    "The update was started but did not install. Windows Installer's log is {}.",
                    self.log
                ),
            };
        }
        let due = self.next_check.is_some_and(|due| due <= now);
        if due && self.running.is_none() && !self.installing() {
            // A version already found stays on show while it is looked for
            // again.
            if !matches!(self.state, UpdateState::Available { .. }) {
                self.state = UpdateState::Checking;
            }
            self.start(Job::Check);
        }
    }

    /// How long until [`Updates::run_due`] has something to do.
    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        // While a job runs or an installation is under way the look waits;
        // the job's end wakes the loop.
        let check = self.next_check.filter(|_| self.running.is_none() && !self.installing());
        [check, self.install_deadline]
            .into_iter()
            .flatten()
            .map(|at| at.saturating_duration_since(now))
            .min()
    }
}

#[cfg(test)]
pub use fake::FakeUpdater;

#[cfg(test)]
mod fake {
    use super::*;
    use std::sync::{Mutex, PoisonError};

    /// An updater that only records the jobs it was given. The test plays
    /// the outcome back through [`Updates::finished`] or the event queue.
    #[derive(Clone, Default)]
    pub struct FakeUpdater(Arc<Mutex<Vec<Job>>>);

    impl FakeUpdater {
        /// Every job started, oldest first.
        pub fn jobs(&self) -> Vec<Job> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }
    }

    impl Updater for FakeUpdater {
        fn start(&mut self, job: Job) {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).push(job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = r"C:\ProgramData\SysPilot\updates\install.log";
    const UNIX: u64 = 1_800_000_000;

    struct Fixture {
        updater: FakeUpdater,
        updates: Updates,
        start: Instant,
    }

    impl Fixture {
        fn new(mode: UpdateMode) -> Self {
            let (updater, start) = (FakeUpdater::default(), Instant::now());
            let updates = Updates::new(Some(Box::new(updater.clone())), mode, LOG.into(), start);
            Self { updater, updates, start }
        }

        fn state(&self) -> UpdateState {
            self.updates.status().state
        }

        fn finish(&mut self, job: Job, outcome: Outcome, clients: usize) {
            self.updates.finished(job, outcome, clients, self.start, UNIX);
        }

        /// Runs the first scheduled look and reports `version` as found.
        fn find(&mut self, version: &str, clients: usize) {
            self.updates.run_due(self.start + FIRST_CHECK_DELAY);
            self.finish(Job::Check, Ok(UpdaterReport::Available(version.into())), clients);
        }
    }

    fn available(version: &str) -> UpdateState {
        UpdateState::Available { version: version.into() }
    }

    /// A directory under the temp directory, removed on drop.
    struct DownloadDir(PathBuf);

    impl Drop for DownloadDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_look_first_removes_the_packages_earlier_installations_left() {
        let directory = DownloadDir(
            std::env::temp_dir().join(format!("SysPilot-Test-updates-{}", std::process::id())),
        );
        std::fs::create_dir_all(directory.0.join("kept-folder")).unwrap();
        for name in ["SysPilot-0.2.0-x64.MSI", "SysPilot-0.2.1-x64.msi.part", "install.log"] {
            std::fs::write(directory.0.join(name), b"x").unwrap();
        }
        remove_installed_packages(&directory.0);
        let mut left: Vec<String> = std::fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["install.log", "kept-folder"],
            "(updates directory): expected only the log and folders to stay"
        );
        // A directory that does not exist is no error.
        remove_installed_packages(&directory.0.join("missing"));
    }

    #[test]
    fn the_first_look_waits_and_then_repeats_daily() {
        let mut fixture = Fixture::new(UpdateMode::Notify);
        let start = fixture.start;
        assert_eq!(fixture.updates.next_wake(start), Some(FIRST_CHECK_DELAY));
        fixture.updates.run_due(start + FIRST_CHECK_DELAY - Duration::from_secs(1));
        assert_eq!(fixture.updater.jobs(), [], "nothing is due yet");

        let due = start + FIRST_CHECK_DELAY;
        fixture.updates.run_due(due);
        assert_eq!(fixture.updater.jobs(), [Job::Check]);
        assert_eq!(fixture.state(), UpdateState::Checking);
        assert_eq!(fixture.updates.next_wake(due), None, "the job's end wakes the loop");
        fixture.updates.run_due(due + CHECK_PERIOD);
        assert_eq!(fixture.updater.jobs(), [Job::Check], "one job at a time");

        fixture.updates.finished(Job::Check, Ok(UpdaterReport::Current), 0, due, UNIX);
        let status = fixture.updates.status();
        assert_eq!((status.state, status.checked_unix), (UpdateState::Idle, Some(UNIX)));
        assert_eq!(fixture.updates.next_wake(due), Some(CHECK_PERIOD));
    }

    #[test]
    fn telling_mode_shows_a_new_version_and_installs_nothing() {
        let mut fixture = Fixture::new(UpdateMode::Notify);
        fixture.find("0.2.1", 0);
        assert_eq!(fixture.state(), available("0.2.1"));
        fixture.updates.clients_changed(0);
        assert_eq!(fixture.updater.jobs(), [Job::Check]);

        // The next daily look keeps it on show.
        fixture.updates.run_due(fixture.start + CHECK_PERIOD);
        assert_eq!(fixture.state(), available("0.2.1"));
        assert_eq!(fixture.updater.jobs(), [Job::Check, Job::Check]);
    }

    #[test]
    fn automatic_mode_installs_what_it_finds_while_no_app_is_open() {
        let mut fixture = Fixture::new(UpdateMode::Automatic);
        fixture.find("0.2.1", 0);
        assert_eq!(fixture.updater.jobs(), [Job::Check, Job::Install]);
        assert_eq!(fixture.state(), UpdateState::Installing);
    }

    #[test]
    fn automatic_mode_waits_for_the_app_to_close() {
        let mut fixture = Fixture::new(UpdateMode::Automatic);
        fixture.find("0.2.1", 1);
        assert_eq!(fixture.state(), available("0.2.1"), "an open app is not closed unasked");
        fixture.updates.clients_changed(1);
        assert_eq!(fixture.updater.jobs(), [Job::Check]);
        fixture.updates.clients_changed(0);
        assert_eq!(fixture.updater.jobs(), [Job::Check, Job::Install]);
    }

    #[test]
    fn switching_to_automatic_installs_what_was_already_found() {
        let mut fixture = Fixture::new(UpdateMode::Notify);
        fixture.find("0.2.1", 0);
        fixture.updates.set_mode(UpdateMode::Automatic, 0, fixture.start);
        assert_eq!(fixture.updater.jobs(), [Job::Check, Job::Install]);
    }

    #[test]
    fn off_neither_looks_nor_mentions_anything() {
        let mut fixture = Fixture::new(UpdateMode::Off);
        assert_eq!(fixture.updates.next_wake(fixture.start), None);
        fixture.updates.run_due(fixture.start + CHECK_PERIOD);
        assert_eq!(fixture.updater.jobs(), [], "(off): expected no look");

        // Switching off drops what an earlier look found.
        let mut fixture = Fixture::new(UpdateMode::Notify);
        fixture.find("0.2.1", 0);
        fixture.updates.set_mode(UpdateMode::Off, 0, fixture.start);
        assert_eq!(fixture.state(), UpdateState::Idle);
        assert_eq!(fixture.updates.next_wake(fixture.start), None);
        assert_eq!(fixture.updates.status().mode, UpdateMode::Off);

        // Switching on again looks at once.
        fixture.updates.set_mode(UpdateMode::Notify, 0, fixture.start);
        assert_eq!(fixture.updates.next_wake(fixture.start), Some(Duration::ZERO));
    }

    #[test]
    fn the_user_can_look_and_install_in_any_mode() {
        let mut fixture = Fixture::new(UpdateMode::Off);
        assert_eq!(fixture.updates.check_now(), Ok(()));
        assert_eq!(fixture.state(), UpdateState::Checking);
        fixture.finish(Job::Check, Ok(UpdaterReport::Available("0.2.1".into())), 1);
        assert_eq!(fixture.state(), available("0.2.1"));
        assert_eq!(fixture.updates.next_wake(fixture.start), None, "off schedules no further look");

        // With the app open, because it is the app that asked.
        assert_eq!(fixture.updates.install_now(), Ok(()));
        assert_eq!(fixture.updates.install_now(), Ok(()), "asking twice starts one job");
        assert_eq!(fixture.updater.jobs(), [Job::Check, Job::Install]);
        assert_eq!(fixture.state(), UpdateState::Installing);
    }

    #[test]
    fn asking_to_install_during_a_look_installs_when_the_look_ends() {
        let mut fixture = Fixture::new(UpdateMode::Notify);
        fixture.updates.check_now().unwrap();
        fixture.updates.install_now().unwrap();
        assert_eq!(fixture.updater.jobs(), [Job::Check], "the look is not interrupted");
        fixture.finish(Job::Check, Ok(UpdaterReport::Available("0.2.1".into())), 1);
        assert_eq!(fixture.updater.jobs(), [Job::Check, Job::Install]);

        // And when the look finds nothing, there is nothing to install.
        let mut fixture = Fixture::new(UpdateMode::Notify);
        fixture.updates.check_now().unwrap();
        fixture.updates.install_now().unwrap();
        fixture.finish(Job::Check, Ok(UpdaterReport::Current), 1);
        assert_eq!(fixture.state(), UpdateState::Idle);
        assert_eq!(fixture.updater.jobs(), [Job::Check]);
    }

    #[test]
    fn a_started_installation_that_never_stops_the_service_is_a_failure() {
        let mut fixture = Fixture::new(UpdateMode::Automatic);
        fixture.find("0.2.1", 0);
        fixture.finish(Job::Install, Ok(UpdaterReport::Installing("0.2.1".into())), 0);
        let start = fixture.start;
        assert_eq!(fixture.state(), UpdateState::Installing);
        assert_eq!(fixture.updates.next_wake(start), Some(INSTALL_TIMEOUT));

        // Nothing else starts while Windows Installer may still be at work.
        fixture.updates.run_due(start + INSTALL_TIMEOUT - Duration::from_secs(1));
        fixture.updates.check_now().unwrap();
        assert_eq!(fixture.updater.jobs(), [Job::Check, Job::Install]);

        fixture.updates.run_due(start + INSTALL_TIMEOUT);
        assert_eq!(
            fixture.state(),
            UpdateState::Failed {
                reason: format!(
                    "The update was started but did not install. Windows Installer's log is {LOG}."
                )
            }
        );
    }

    #[test]
    fn a_failed_job_says_why_and_the_daily_look_goes_on() {
        let mut fixture = Fixture::new(UpdateMode::Notify);
        fixture.updates.run_due(fixture.start + FIRST_CHECK_DELAY);
        fixture.finish(Job::Check, Err("github.com could not be reached".into()), 0);
        assert_eq!(
            fixture.state(),
            UpdateState::Failed { reason: "github.com could not be reached".into() }
        );
        assert_eq!(fixture.updates.status().checked_unix, None, "a failed look is not a look");
        assert_eq!(fixture.updates.next_wake(fixture.start), Some(CHECK_PERIOD));
    }

    #[test]
    fn a_copy_without_an_updater_says_so_and_does_nothing() {
        let start = Instant::now();
        let mut updates = Updates::new(None, UpdateMode::Automatic, LOG.into(), start);
        assert_eq!(updates.status().state, UpdateState::Unavailable);
        assert_eq!(updates.next_wake(start), None);
        assert_eq!(updates.check_now(), Err(UNAVAILABLE));
        assert_eq!(updates.install_now(), Err(UNAVAILABLE));
        updates.run_due(start + CHECK_PERIOD);
        updates.set_mode(UpdateMode::Notify, 0, start);
        assert_eq!(updates.status().mode, UpdateMode::Notify, "the choice is still kept");
        assert_eq!(updates.status().state, UpdateState::Unavailable);
    }

    #[test]
    fn the_updaters_output_becomes_an_outcome() {
        assert_eq!(outcome_of(true, "current\r\n", ""), Ok(UpdaterReport::Current));
        assert_eq!(
            outcome_of(true, "available 0.2.1\n", "ignored"),
            Ok(UpdaterReport::Available("0.2.1".into()))
        );
        assert_eq!(
            outcome_of(true, "<html>", "").unwrap_err(),
            "(updater): expected one line of its protocol | received: \"<html>\""
        );
        assert_eq!(
            outcome_of(false, "available 0.2.1", "github.com could not be reached\ndetail\n"),
            Err("github.com could not be reached".into()),
            "(updater): expected a failed run to be a failure whatever it printed"
        );
        assert_eq!(
            outcome_of(false, "", "  \n").unwrap_err(),
            "The updater ended without saying why."
        );
        let long = "x".repeat(REASON_MAX + 50);
        assert_eq!(outcome_of(false, "", &long).unwrap_err().len(), REASON_MAX);
    }
}
