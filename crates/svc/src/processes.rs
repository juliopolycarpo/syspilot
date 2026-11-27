//! Waiting for SysPilot's own programs to end.
//!
//! Windows Installer deletes the installed files right after the service's
//! `uninstall` has run. A program that is still ending holds its files open:
//! they go on the next close, the folder is then not empty when the
//! installer tries to remove it, and the installer leaves it without a word.
//! Restart Manager reports the app closed before its process has exited, so
//! the installer cannot be trusted to have waited long enough.
//!
//! `uninstall --wait-for-processes` therefore waits, up to a bound, until no
//! SysPilot program runs from the folder of the executable that is removing
//! it. What can be seen of the system goes through [`ProcessTable`], so the
//! waiting is tested against `FakeProcessTable`, which plays a script, and
//! only a few tests start real processes.

use std::fmt;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use syspilot_proto::tuning::TUNER_EXECUTABLE;
use syspilot_proto::update::UPDATER_EXECUTABLE;
use syspilot_proto::win::OwnedHandle;
use syspilot_proto::worker::WORKER_EXECUTABLE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

/// The image file names of every program the package installs that can run
/// on its own: the app, the service (a stopped service's process may outlive
/// its stop by a moment), the sensor worker, the tuner and the updater.
const INSTALLED_PROGRAMS: [&str; 5] =
    ["syspilot.exe", "syspilot-svc.exe", WORKER_EXECUTABLE, TUNER_EXECUTABLE, UPDATER_EXECUTABLE];

/// How long `uninstall --wait-for-processes` waits at most.
///
/// Twice the 5 s the installer gives the app to close (`CloseApplication` in
/// `installer/syspilot.wxs`), and well above the 2 s a sensor worker gets to
/// exit by itself and the second a stopping service gives its last answers.
/// A program stuck for longer adds this much to a removal, no more: it is
/// reported and the removal goes on.
pub const EXIT_WAIT: Duration = Duration::from_secs(10);

/// How often the processes are looked at while waiting.
const POLL: Duration = Duration::from_millis(100);

/// The longest path Windows allows, in UTF-16 units, and so the room
/// `QueryFullProcessImageNameW` is given.
const LONGEST_PATH: usize = 32_768;

/// What the system can say about its running processes.
pub trait ProcessTable {
    /// The id and image file name (`syspilot.exe`, no directory) of every
    /// running process.
    fn processes(&self) -> Result<Vec<(u32, String)>, String>;

    /// The full path of the executable of process `pid`, or `None` when the
    /// process has ended or may not be queried.
    fn image_path(&self, pid: u32) -> Option<PathBuf>;
}

/// The processes of this machine.
pub struct SystemProcesses;

impl ProcessTable for SystemProcesses {
    fn processes(&self) -> Result<Vec<(u32, String)>, String> {
        let failed = |error: io::Error| {
            format!("(process list): expected the running processes | received: {error}")
        };
        // SAFETY: a process snapshot takes no process id and has no
        // preconditions; failure is checked by `OwnedHandle::new`.
        let snapshot = OwnedHandle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })
            .map_err(failed)?;
        let mut entry =
            PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        // SAFETY: `snapshot` is a valid snapshot handle and `entry` a live
        // PROCESSENTRY32W whose `dwSize` is set, as the call requires.
        if unsafe { Process32FirstW(snapshot.raw(), &mut entry) } == 0 {
            return Err(failed(io::Error::last_os_error()));
        }
        let mut found = Vec::new();
        loop {
            let name = &entry.szExeFile;
            let length = name.iter().position(|&unit| unit == 0).unwrap_or(name.len());
            found.push((entry.th32ProcessID, String::from_utf16_lossy(&name[..length])));
            // SAFETY: as for `Process32FirstW`.
            if unsafe { Process32NextW(snapshot.raw(), &mut entry) } == 0 {
                return Ok(found);
            }
        }
    }

    fn image_path(&self, pid: u32) -> Option<PathBuf> {
        // SAFETY: no pointers are passed; a failed open returns null, which
        // `OwnedHandle::new` rejects.
        let process =
            OwnedHandle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })
                .ok()?;
        let mut path = vec![0u16; LONGEST_PATH];
        let mut length = path.len() as u32;
        // SAFETY: `process` was opened with the right the call needs; `path`
        // has room for `length` UTF-16 units, and the call writes back how
        // many it used.
        let named = unsafe {
            QueryFullProcessImageNameW(
                process.raw(),
                PROCESS_NAME_WIN32,
                path.as_mut_ptr(),
                &mut length,
            )
        };
        if named == 0 {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsString::from_wide(path.get(..length as usize)?)))
    }
}

/// A program that is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    /// Its process id.
    pub pid: u32,
    /// Its image file name.
    pub name: String,
}

impl fmt::Display for Running {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.name, self.pid)
    }
}

/// Whether two folders are the same one. Windows compares paths without
/// regard to case. A path that reaches a folder another way, such as a short
/// `PROGRA~1` name, does not match, which only means that nothing is waited
/// for.
fn same_folder(left: &Path, right: &Path) -> bool {
    left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
}

/// The SysPilot programs that run from `directory` itself, not a folder
/// beside or inside it, leaving out process `own_pid`: the one that asks.
fn running_from(
    table: &impl ProcessTable,
    directory: &Path,
    own_pid: u32,
) -> Result<Vec<Running>, String> {
    let mut found = Vec::new();
    for (pid, name) in table.processes()? {
        let listed = INSTALLED_PROGRAMS.iter().any(|program| program.eq_ignore_ascii_case(&name));
        if pid == own_pid || !listed {
            continue;
        }
        let in_directory = table.image_path(pid).is_some_and(|image| {
            image.parent().is_some_and(|parent| same_folder(parent, directory))
        });
        if in_directory {
            found.push(Running { pid, name });
        }
    }
    Ok(found)
}

/// Waits until no SysPilot program runs from `directory`, for at most
/// `bound`, looking every `poll`. `own_pid` is not waited for.
///
/// Returns `None` when nothing was running at the first look, and how long
/// it waited otherwise. Fails, naming what still runs, when `bound` passes,
/// and when the processes cannot be listed.
pub fn wait_for_exit(
    table: &impl ProcessTable,
    directory: &Path,
    own_pid: u32,
    bound: Duration,
    poll: Duration,
) -> Result<Option<Duration>, String> {
    let start = Instant::now();
    let mut waited = false;
    loop {
        let running = running_from(table, directory, own_pid)?;
        if running.is_empty() {
            return Ok(waited.then(|| start.elapsed()));
        }
        let elapsed = start.elapsed();
        if elapsed >= bound {
            let names: Vec<String> = running.iter().map(Running::to_string).collect();
            return Err(format!(
                "(processes in {}): expected none within {} s | received: {}",
                directory.display(),
                bound.as_secs_f64(),
                names.join(", ")
            ));
        }
        waited = true;
        // Not past the bound: the last look is at the bound itself.
        std::thread::sleep(poll.min(bound - elapsed));
    }
}

/// What to tell on the error output about a wait: how long it took when it
/// had to, why it gave up, and nothing when there was nothing to wait for.
fn describe(outcome: Result<Option<Duration>, String>) -> Option<String> {
    match outcome {
        Ok(None) => None,
        Ok(Some(waited)) => Some(format!(
            "Waited {:.1} s for the SysPilot programs in this folder to exit.",
            waited.as_secs_f64()
        )),
        Err(problem) => Some(format!("{problem}\nContinuing: the files may stay in use.")),
    }
}

/// Waits, for up to [`EXIT_WAIT`], for the SysPilot programs that run from
/// the folder of this executable, and returns what to tell about it, if
/// anything. Never fails: a program that cannot be waited for must not keep
/// SysPilot from being removed.
pub fn wait_for_own_folder() -> Option<String> {
    let folder = std::env::current_exe().map_err(|error| {
        format!("(own folder): expected the path of this executable | received: {error}")
    });
    let outcome = folder.and_then(|executable| {
        let directory = executable.parent().ok_or_else(|| {
            format!("(own folder): expected a folder | received: {}", executable.display())
        })?;
        wait_for_exit(&SystemProcesses, directory, std::process::id(), EXIT_WAIT, POLL)
    });
    describe(outcome)
}

#[cfg(test)]
pub use fake::FakeProcessTable;

#[cfg(test)]
mod fake {
    use super::*;
    use std::cell::Cell;

    /// One scripted process.
    struct FakeProcess {
        pid: u32,
        name: String,
        image: Option<PathBuf>,
        /// For how many looks at the process list it is there.
        looks: usize,
    }

    /// A process table that plays a script: each process is listed for a
    /// number of looks, then it has ended.
    #[derive(Default)]
    pub struct FakeProcessTable {
        scripted: Vec<FakeProcess>,
        looked: Cell<usize>,
        listing_fails: Option<String>,
    }

    impl FakeProcessTable {
        /// A process that runs `image`, listed for `looks` looks.
        pub fn running(mut self, pid: u32, image: &str, looks: usize) -> Self {
            let image = PathBuf::from(image);
            let name = image.file_name().map(|name| name.to_string_lossy().into_owned());
            self.scripted.push(FakeProcess {
                pid,
                name: name.unwrap_or_default(),
                image: Some(image),
                looks,
            });
            self
        }

        /// A process of `name` that refuses to say where it runs from.
        pub fn unreadable(mut self, pid: u32, name: &str, looks: usize) -> Self {
            self.scripted.push(FakeProcess { pid, name: name.into(), image: None, looks });
            self
        }

        /// A table that cannot list its processes.
        pub fn failing(mut self, reason: &str) -> Self {
            self.listing_fails = Some(reason.into());
            self
        }

        /// How many times the list was asked for.
        pub fn looks(&self) -> usize {
            self.looked.get()
        }
    }

    impl ProcessTable for FakeProcessTable {
        fn processes(&self) -> Result<Vec<(u32, String)>, String> {
            self.looked.set(self.looked.get() + 1);
            if let Some(reason) = &self.listing_fails {
                return Err(format!(
                    "(process list): expected the running processes | received: {reason}"
                ));
            }
            Ok(self
                .scripted
                .iter()
                .filter(|process| self.looked.get() <= process.looks)
                .map(|process| (process.pid, process.name.clone()))
                .collect())
        }

        fn image_path(&self, pid: u32) -> Option<PathBuf> {
            self.scripted.iter().find(|process| process.pid == pid)?.image.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::{Child, Command, Stdio};

    const FOLDER: &str = r"C:\Program Files\SysPilot-Test.msi";
    const OWN_PID: u32 = 100;
    /// A bound a test never reaches unless it is testing the bound.
    const FAR: Duration = Duration::from_secs(60);
    const QUICK: Duration = Duration::from_millis(1);

    fn folder() -> &'static Path {
        Path::new(FOLDER)
    }

    fn image(name: &str) -> String {
        format!(r"{FOLDER}\{name}")
    }

    fn names(running: &[Running]) -> Vec<String> {
        running.iter().map(Running::to_string).collect()
    }

    #[test]
    fn nothing_to_wait_for_returns_at_the_first_look() {
        let table = FakeProcessTable::default();
        let outcome = wait_for_exit(&table, folder(), OWN_PID, FAR, QUICK);
        assert_eq!(outcome, Ok(None), "(wait for none): expected no wait");
        assert_eq!(table.looks(), 1, "(looks at the process list): expected 1");
    }

    #[test]
    fn waits_until_the_programs_have_ended() {
        let table = FakeProcessTable::default().running(4120, &image("syspilot.exe"), 3).running(
            4188,
            &image("syspilot-sensors.exe"),
            5,
        );
        let outcome = wait_for_exit(&table, folder(), OWN_PID, FAR, QUICK);
        assert!(
            matches!(outcome, Ok(Some(_))),
            "(wait): expected Ok(Some(waited)) | received: {outcome:?}"
        );
        // The app is there for looks 1 to 3 and the worker for 1 to 5; the
        // sixth look finds neither.
        assert_eq!(table.looks(), 6, "(looks at the process list): expected 6");
    }

    #[test]
    fn gives_up_at_the_bound_and_names_what_still_runs() {
        let table = FakeProcessTable::default()
            .running(4120, &image("syspilot.exe"), usize::MAX)
            .running(4188, &image("syspilot-sensors.exe"), usize::MAX);
        let started = Instant::now();
        let outcome = wait_for_exit(&table, folder(), OWN_PID, Duration::from_millis(50), QUICK);
        assert!(started.elapsed() >= Duration::from_millis(50), "(wait): expected the whole bound");
        assert_eq!(
            outcome,
            Err(format!(
                "(processes in {FOLDER}): expected none within 0.05 s | received: \
                 syspilot.exe (4120), syspilot-sensors.exe (4188)"
            ))
        );
        assert!(
            table.looks() > 2,
            "(looks at the process list): expected several | received: {}",
            table.looks()
        );
    }

    #[test]
    fn with_no_time_to_wait_there_is_one_look() {
        // A program that is gone at that look is fine, one that is not is
        // reported.
        let table = FakeProcessTable::default().running(4120, &image("syspilot.exe"), 0);
        assert_eq!(wait_for_exit(&table, folder(), OWN_PID, Duration::ZERO, QUICK), Ok(None));
        assert_eq!(table.looks(), 1, "(looks at the process list): expected 1");
        let table = FakeProcessTable::default().running(4120, &image("syspilot.exe"), 1);
        let outcome = wait_for_exit(&table, folder(), OWN_PID, Duration::ZERO, QUICK);
        assert_eq!(
            outcome,
            Err(format!(
                "(processes in {FOLDER}): expected none within 0 s | received: syspilot.exe (4120)"
            ))
        );
    }

    #[test]
    fn the_process_that_asks_is_not_waited_for() {
        // The uninstall itself is a syspilot-svc.exe in the folder.
        let table =
            FakeProcessTable::default().running(OWN_PID, &image("syspilot-svc.exe"), usize::MAX);
        assert_eq!(wait_for_exit(&table, folder(), OWN_PID, FAR, QUICK), Ok(None));
        // Another one is: a service that was stopped moments ago.
        let table = FakeProcessTable::default()
            .running(OWN_PID, &image("syspilot-svc.exe"), usize::MAX)
            .running(7, &image("syspilot-svc.exe"), 2);
        let outcome = wait_for_exit(&table, folder(), OWN_PID, FAR, QUICK);
        assert!(
            matches!(outcome, Ok(Some(_))),
            "(wait): expected a wait for pid 7 | received: {outcome:?}"
        );
        assert_eq!(table.looks(), 3);
    }

    #[test]
    fn only_listed_programs_in_exactly_this_folder_count() {
        let table = FakeProcessTable::default()
            // Counted: each listed name, one only in another case.
            .running(1, &image("syspilot.exe"), 1)
            .running(2, &image("SYSPILOT-SENSORS.EXE"), 1)
            .running(3, &image("syspilot-tune.exe"), 1)
            .running(4, &image("syspilot-update.exe"), 1)
            .running(5, &image("syspilot-svc.exe"), 1)
            // A folder written in other case is the same folder.
            .running(6, r"c:\program files\syspilot-test.msi\syspilot.exe", 1)
            // Not counted: a folder beside this one with the same start...
            .running(10, &format!("{FOLDER}.old\\syspilot.exe"), 1)
            // ... one inside it ...
            .running(11, &image(r"en-us\syspilot.exe"), 1)
            // ... the folder above it ...
            .running(12, r"C:\Program Files\syspilot.exe", 1)
            // ... a program that is not SysPilot's, from this folder ...
            .running(13, &image("notepad.exe"), 1)
            .running(14, &image("syspilot-ui.exe"), 1)
            // ... and one that cannot say where it runs from.
            .unreadable(15, "syspilot.exe", 1);
        let running = running_from(&table, folder(), OWN_PID).unwrap();
        assert_eq!(
            names(&running),
            [
                "syspilot.exe (1)",
                "SYSPILOT-SENSORS.EXE (2)",
                "syspilot-tune.exe (3)",
                "syspilot-update.exe (4)",
                "syspilot-svc.exe (5)",
                "syspilot.exe (6)",
            ],
            "(programs in {FOLDER}): expected the six listed names there"
        );
    }

    #[test]
    fn a_list_that_cannot_be_read_ends_the_wait_with_the_reason() {
        let table = FakeProcessTable::default().failing("access denied");
        let outcome = wait_for_exit(&table, folder(), OWN_PID, FAR, QUICK);
        assert_eq!(
            outcome,
            Err("(process list): expected the running processes | received: access denied".into())
        );
        assert_eq!(table.looks(), 1, "(looks at the process list): expected 1");
    }

    #[test]
    fn the_wait_is_told_in_one_message_and_silence_is_none() {
        assert_eq!(describe(Ok(None)), None);
        assert_eq!(
            describe(Ok(Some(Duration::from_millis(1250)))),
            Some("Waited 1.2 s for the SysPilot programs in this folder to exit.".into())
        );
        let told = describe(Err(
            "(processes in C:\\x): expected none within 10 s | received: syspilot.exe (4120)"
                .into(),
        ));
        assert_eq!(
            told.as_deref(),
            Some(
                "(processes in C:\\x): expected none within 10 s | received: syspilot.exe (4120)\n\
                 Continuing: the files may stay in use."
            )
        );
    }

    #[test]
    fn folders_are_compared_without_regard_to_case() {
        assert!(same_folder(Path::new(r"C:\A\B"), Path::new(r"c:\a\b")));
        assert!(!same_folder(Path::new(r"C:\A\B"), Path::new(r"C:\A\B\C")));
        assert!(!same_folder(Path::new(r"C:\A\B"), Path::new(r"C:\A\B2")));
    }

    #[test]
    fn each_program_the_service_starts_is_listed() {
        for program in [WORKER_EXECUTABLE, TUNER_EXECUTABLE, UPDATER_EXECUTABLE] {
            assert!(
                INSTALLED_PROGRAMS.contains(&program),
                "(INSTALLED_PROGRAMS): expected {program}"
            );
        }
    }

    /// A copy of `ping.exe` named `name` in a folder of its own under the
    /// system temp directory, running for a long time until it is ended.
    struct RealProgram {
        folder: PathBuf,
        child: Child,
    }

    impl RealProgram {
        /// The folder as the system reports it for the running process: the
        /// temp directory may be written with 8.3 names that it spells out.
        fn folder_as_reported(&self) -> PathBuf {
            let image = SystemProcesses.image_path(self.child.id());
            let parent = image.as_deref().and_then(Path::parent).map(Path::to_owned);
            parent.unwrap_or_else(|| self.folder.clone())
        }
    }

    impl RealProgram {
        fn start(tag: &str, name: &str) -> Self {
            use std::os::windows::process::CommandExt;
            let folder = std::env::temp_dir()
                .join(format!("SysPilot-Test-processes-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&folder);
            fs::create_dir_all(&folder).unwrap();
            let system = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap();
            let copy = folder.join(name);
            fs::copy(system.join(r"System32\PING.EXE"), &copy).unwrap();
            let child = Command::new(&copy)
                .args(["-n", "120", "127.0.0.1"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
                .spawn()
                .unwrap();
            Self { folder, child }
        }
    }

    impl Drop for RealProgram {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = fs::remove_dir_all(&self.folder);
        }
    }

    #[test]
    fn the_system_lists_this_process_and_where_it_runs_from() {
        let pid = std::process::id();
        let executable = std::env::current_exe().unwrap();
        let name = executable.file_name().unwrap().to_string_lossy().into_owned();
        let listed = SystemProcesses.processes().unwrap();
        assert!(
            listed
                .iter()
                .any(|(listed_pid, listed_name)| *listed_pid == pid && *listed_name == name),
            "(process list): expected {name} ({pid}) | received: {} processes",
            listed.len()
        );
        let image = SystemProcesses.image_path(pid);
        assert!(
            image.as_deref().is_some_and(|image| same_folder(image, &executable)),
            "(image path of {pid}): expected {} | received: {image:?}",
            executable.display()
        );
    }

    #[test]
    fn a_process_that_does_not_exist_has_no_image_path() {
        assert_eq!(SystemProcesses.image_path(u32::MAX), None);
    }

    #[test]
    fn a_real_program_of_the_folder_is_waited_for_and_one_of_another_name_is_not() {
        let app = RealProgram::start("real", "syspilot.exe");
        let other = RealProgram::start("other", "notepad.exe");
        let own = std::process::id();

        // Held for its bound: reported with its real id.
        let folder = app.folder_as_reported();
        let bound = Duration::from_millis(300);
        let outcome = wait_for_exit(&SystemProcesses, &folder, own, bound, POLL);
        assert_eq!(
            outcome,
            Err(format!(
                "(processes in {}): expected none within 0.3 s | received: syspilot.exe ({})",
                folder.display(),
                app.child.id()
            ))
        );
        // Not a SysPilot name: nothing to wait for.
        let folder = other.folder_as_reported();
        assert_eq!(wait_for_exit(&SystemProcesses, &folder, own, FAR, POLL), Ok(None));
    }

    #[test]
    fn a_real_program_that_ends_while_waiting_lets_the_wait_finish() {
        let mut app = RealProgram::start("ends", "syspilot.exe");
        let folder = app.folder_as_reported();
        let own = std::process::id();
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                wait_for_exit(&SystemProcesses, &folder, own, Duration::from_secs(30), POLL)
            });
            std::thread::sleep(Duration::from_millis(300));
            app.child.kill().unwrap();
            let outcome = waiting.join().unwrap();
            assert!(
                matches!(outcome, Ok(Some(_))),
                "(wait for the program to end): expected Ok(Some(waited)) | received: {outcome:?}"
            );
        });
    }
}
