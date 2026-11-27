//! End-to-end tests of the real executable, in a console and as an
//! installed Windows service.
//!
//! They need an elevated process, use the instance names `SysPilot-Test*`,
//! change nothing but scratch registry keys named `SysPilot-Test`, and
//! remove the test service, its data and the scratch keys when they finish.

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::ptr;
use std::time::{Duration, Instant};

use syspilot_engine::os::{Hive, RegValue, Registry, Sessions, reg_type};
use syspilot_engine::win::{WinRegistry, WinSessions, process_user_sid};
use syspilot_proto::message::{ErrorCode, Request};
use syspilot_proto::profile::Profile;
use syspilot_proto::{Client, ClientError, Instance};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, ImpersonateLoggedOnUser, LUA_TOKEN, RevertToSelf, TOKEN_ALL_ACCESS,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, GetCurrentProcess, OpenProcessToken,
};

const EXECUTABLE: &str = env!("CARGO_BIN_EXE_syspilot-svc");
const SCRATCH_ROOT: &str = r"Software\SysPilot-Test";
const PATIENCE: Duration = Duration::from_secs(20);

fn require_elevation() {
    // Only an elevated process may write the machine hive.
    let probe = format!("Probe-{}", std::process::id());
    let elevated = WinRegistry.write(&Hive::Machine, SCRATCH_ROOT, &probe, &dword(1)).is_ok();
    let _ = WinRegistry.delete_value(&Hive::Machine, SCRATCH_ROOT, &probe);
    let _ = WinRegistry.delete_key_if_empty(&Hive::Machine, SCRATCH_ROOT);
    assert!(
        elevated,
        "(test environment): expected an elevated process (the tests install a service) | received: a standard user"
    );
}

/// Runs the executable against `instance` and returns its output.
fn run(instance: &Instance, arguments: &[&str]) -> Output {
    Command::new(EXECUTABLE)
        .args(["--instance", instance.name()])
        .args(arguments)
        .output()
        .expect("the executable starts")
}

/// Runs a command that must succeed and returns what it printed.
fn succeed(instance: &Instance, arguments: &[&str]) -> String {
    let output = run(instance, arguments);
    let (stdout, stderr) =
        (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "expected `{}` to succeed | received: {} with output {stdout:?} and error {stderr:?}",
        arguments.join(" "),
        output.status
    );
    stdout.into_owned()
}

/// Polls until `condition` holds. Fails with `what` after [`PATIENCE`].
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "expected {what} within {} seconds | received: still not the case",
            PATIENCE.as_secs()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn dword(value: u32) -> RegValue {
    RegValue { kind: reg_type::DWORD, data: value.to_le_bytes().to_vec() }
}

/// A scratch key under `Software\SysPilot-Test` in one hive.
struct Scratch {
    hive: Hive,
    key: String,
}

impl Scratch {
    fn machine(tag: &str) -> Self {
        Self { hive: Hive::Machine, key: format!(r"{SCRATCH_ROOT}\{tag}") }
    }

    fn user(tag: &str) -> Self {
        let sid = process_user_sid().expect("the test user has a SID");
        Self { hive: Hive::User(sid), key: format!(r"{SCRATCH_ROOT}\{tag}") }
    }

    fn read(&self, name: &str) -> Option<RegValue> {
        WinRegistry.read(&self.hive, &self.key, name).expect("the scratch key is readable")
    }

    fn write(&self, name: &str, value: u32) {
        WinRegistry.write(&self.hive, &self.key, name, &dword(value)).expect("scratch write");
    }

    fn exists(&self) -> bool {
        WinRegistry.key_exists(&self.hive, &self.key).expect("the scratch hive is readable")
    }
}

impl Drop for Scratch {
    /// Removes whatever a failed test left behind.
    fn drop(&mut self) {
        for name in ["Kept", "Seed", "FromCommand"] {
            let _ = WinRegistry.delete_value(&self.hive, &self.key, name);
        }
        let _ = WinRegistry.delete_key_if_empty(&self.hive, &self.key);
        let _ = WinRegistry.delete_key_if_empty(&self.hive, SCRATCH_ROOT);
    }
}

/// A profile file in the temp directory, removed on drop.
struct ProfileFile(PathBuf);

impl ProfileFile {
    fn write(tag: &str, text: &str) -> Self {
        Profile::parse(text).expect("the test profile is valid");
        let path =
            std::env::temp_dir().join(format!("SysPilot-Test-{tag}-{}.toml", std::process::id()));
        std::fs::write(&path, text).expect("the temp directory is writable");
        Self(path)
    }

    fn path(&self) -> &str {
        self.0.to_str().expect("the temp path is Unicode")
    }
}

impl Drop for ProfileFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// While alive, the calling thread acts as a standard user: the same token
/// Windows gives an administrator who has not elevated.
struct StandardUser(HANDLE);

impl StandardUser {
    fn impersonate() -> Self {
        let (mut process_token, mut filtered): (HANDLE, HANDLE) =
            (ptr::null_mut(), ptr::null_mut());
        // SAFETY: takes no arguments and only returns the pseudo-handle of
        // this process, which needs no closing.
        let current_process = unsafe { GetCurrentProcess() };
        // SAFETY: `current_process` is that always-valid pseudo-handle and the
        // out-pointer `process_token` is valid for writes.
        let opened =
            unsafe { OpenProcessToken(current_process, TOKEN_ALL_ACCESS, &mut process_token) };
        assert_ne!(opened, 0, "open the process token: {}", std::io::Error::last_os_error());
        // SAFETY: `process_token` is valid; no SIDs or privileges are
        // listed explicitly; the flag asks for the standard-user variant.
        let created = unsafe {
            CreateRestrictedToken(
                process_token,
                LUA_TOKEN,
                0,
                ptr::null(),
                0,
                ptr::null(),
                0,
                ptr::null(),
                &mut filtered,
            )
        };
        assert_ne!(created, 0, "create the filtered token: {}", std::io::Error::last_os_error());
        // SAFETY: `process_token` was opened above, is no longer needed and is
        // closed exactly once.
        unsafe { CloseHandle(process_token) };
        // SAFETY: `filtered` is a valid token, created by the call above.
        let impersonating = unsafe { ImpersonateLoggedOnUser(filtered) };
        assert_ne!(impersonating, 0, "impersonate: {}", std::io::Error::last_os_error());
        Self(filtered)
    }
}

impl Drop for StandardUser {
    fn drop(&mut self) {
        // SAFETY: takes no arguments; ends the impersonation started in
        // `impersonate` on this thread.
        unsafe { RevertToSelf() };
        // SAFETY: `self.0` is the token `impersonate` created and owns; it is
        // closed exactly once, after the impersonation has ended.
        unsafe { CloseHandle(self.0) };
    }
}

/// Stops a console run if the test fails before it does so itself.
struct ConsoleRun(Child);

impl Drop for ConsoleRun {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn data_dir(instance: &Instance) -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").expect("ProgramData is set"))
        .join(instance.name())
}

/// A folder under the temp directory that holds a copy of the executable
/// and, as a stand-in for the app, a copy of `ping.exe` named `syspilot.exe`.
/// Removed on drop.
struct InstalledFolder(PathBuf);

impl InstalledFolder {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("SysPilot-Test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the temp directory is writable");
        // The spelling the system gives a running program's path: long names
        // and no `\\?\` prefix, which `canonicalize` adds.
        let path = std::fs::canonicalize(&path).expect("the folder exists");
        let path = PathBuf::from(path.to_string_lossy().trim_start_matches(r"\\?\"));
        let system = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set"));
        std::fs::copy(EXECUTABLE, path.join("syspilot-svc.exe")).expect("copy the executable");
        std::fs::copy(system.join(r"System32\PING.EXE"), path.join("syspilot.exe"))
            .expect("copy ping.exe");
        Self(path)
    }

    /// Starts the stand-in for the app. It ends by itself after about
    /// `seconds` seconds.
    fn start_app(&self, seconds: u32) -> ConsoleRun {
        use std::os::windows::process::CommandExt;
        let child = Command::new(self.0.join("syspilot.exe"))
            .args(["-n", &(seconds + 1).to_string(), "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("the stand-in for the app starts");
        ConsoleRun(child)
    }

    /// Runs `uninstall` from the copy of the executable in this folder.
    fn uninstall(&self, instance: &Instance, flags: &[&str]) -> Output {
        Command::new(self.0.join("syspilot-svc.exe"))
            .args(["--instance", instance.name(), "uninstall"])
            .args(flags)
            .output()
            .expect("the executable starts")
    }
}

impl Drop for InstalledFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const PROFILE_HEADER: &str = "schema = 1\n\n";

/// A custom action that keeps the scratch value `Kept` at 1.
fn keep_value_action(scratch: &Scratch, scope: &str) -> String {
    format!(
        "[[custom]]\nid = \"custom.keep-{scope}\"\ntitle = \"Keep the {scope} scratch value\"\n\
         action = {{ registry = {{ scope = \"{scope}\", key = '{}', name = \"Kept\", data = {{ dword = 1 }} }} }}\n",
        scratch.key
    )
}

#[test]
fn console_run_enforces_a_profile_and_refuses_standard_users() {
    require_elevation();
    let instance = Instance::named("SysPilot-Test.console").unwrap();
    let scratch = Scratch::machine("console");
    // A value that exists before, so that turning the action off has
    // something to put back.
    scratch.write("Kept", 7);
    let _ = run(&instance, &["uninstall"]);

    let child = Command::new(EXECUTABLE)
        .args(["--instance", instance.name(), "run"])
        .stdout(Stdio::null())
        .spawn()
        .expect("the executable starts");
    let mut console = ConsoleRun(child);
    wait_until("the console run to accept connections", || Client::connect(&instance).is_ok());

    let status = succeed(&instance, &["status"]);
    assert!(status.starts_with("SysPilot-Test.console"), "{status}");
    assert!(status.contains("(console, up "), "{status}");

    // A second copy must not start.
    let second = run(&instance, &["run"]);
    assert!(!second.status.success());
    let complaint = String::from_utf8_lossy(&second.stderr);
    assert!(complaint.contains("already running"), "{complaint}");

    let profile_text = format!("{PROFILE_HEADER}{}", keep_value_action(&scratch, "machine"));
    let profile = ProfileFile::write("console", &profile_text);
    let applied = succeed(&instance, &["apply", profile.path()]);
    assert!(applied.contains("1 custom action(s)"), "{applied}");
    assert_eq!(scratch.read("Kept"), Some(dword(1)));

    // Something else changes the value: it is put back.
    scratch.write("Kept", 0);
    wait_until("the changed value to be restored", || scratch.read("Kept") == Some(dword(1)));
    let history = succeed(&instance, &["history"]);
    assert!(history.contains("restored") && history.contains("custom.keep-machine"), "{history}");
    let list = succeed(&instance, &["list", "--enabled"]);
    assert!(list.starts_with("custom.keep-machine  on  Keep the machine scratch value"), "{list}");
    assert!(succeed(&instance, &["export"]).contains("custom.keep-machine"));

    // The same user without elevation may look but not touch.
    {
        let _standard_user = StandardUser::impersonate();
        let mut client = Client::connect(&instance).unwrap();
        assert!(!client.welcome().elevated, "a filtered token was treated as elevated");
        client.status().expect("status is open to everyone");
        for request in [Request::SetProfile(Profile::default()), Request::Profile] {
            match client.request(&request) {
                Err(ClientError::Service(error)) => assert_eq!(error.code, ErrorCode::AccessDenied),
                other => panic!("expected AccessDenied for {request:?} | received: {other:?}"),
            }
        }
    }
    assert_eq!(scratch.read("Kept"), Some(dword(1)), "the refused request changed nothing");

    // Turning it off restores the value from before.
    let empty = ProfileFile::write("console-empty", "schema = 1\n");
    succeed(&instance, &["apply", empty.path()]);
    assert_eq!(scratch.read("Kept"), Some(dword(7)));

    Client::connect(&instance).unwrap().command(&Request::Shutdown).unwrap();
    let exit = console.0.wait().expect("the console run exits");
    assert!(exit.success(), "{exit}");

    let removed = succeed(&instance, &["uninstall"]);
    assert!(removed.contains("was not installed as a service"), "{removed}");
    assert!(!data_dir(&instance).exists(), "the data directory was left behind");
}

#[test]
fn installed_service_enforces_restores_and_uninstalls_without_a_trace() {
    require_elevation();
    let instance = Instance::named("SysPilot-Test").unwrap();
    let machine = Scratch::machine("service");
    let user = Scratch::user("service");
    // The command writes to a key of its own: `reg delete /v` leaves the
    // key behind, which must not be mistaken for a failed registry revert.
    let command = Scratch::machine("service-command");
    // Clears what an interrupted earlier run may have left installed.
    succeed(&instance, &["uninstall"]);
    assert!(!machine.exists() && !user.exists(), "scratch keys from an earlier run remain");

    let installed = succeed(&instance, &["install"]);
    assert_eq!(installed.trim(), "SysPilot-Test service installed and running.");
    let status = succeed(&instance, &["status"]);
    assert!(status.contains("(service, up "), "{status}");

    // A machine setting, a setting of the signed-in user, and a command run
    // as LocalSystem that has an undo.
    let reg_exe = r"%SystemRoot%\System32\reg.exe";
    let command_key = format!(r"HKLM\{}", command.key);
    let profile_text = format!(
        "{PROFILE_HEADER}{}{}[[custom]]\nid = \"custom.command\"\ntitle = \"Write through a command\"\n\
         triggers = [\"boot\"]\n\
         action = {{ command = {{ apply = {{ program = '{reg_exe}', args = [\"add\", '{command_key}', \"/v\", \"FromCommand\", \"/t\", \"REG_DWORD\", \"/d\", \"5\", \"/f\"] }}, \
         undo = {{ program = '{reg_exe}', args = [\"delete\", '{command_key}', \"/v\", \"FromCommand\", \"/f\"] }}, \
         run-as = \"system\", timeout-secs = 30 }} }}\n",
        keep_value_action(&machine, "machine"),
        keep_value_action(&user, "user"),
    );
    let profile = ProfileFile::write("service", &profile_text);
    succeed(&instance, &["apply", profile.path()]);
    assert_eq!(machine.read("Kept"), Some(dword(1)));
    // Per-user settings reach users with an interactive session. A test run
    // from a service session (continuous integration) has none; the
    // setting then waits for a logon and is reported as not verifiable.
    let own_sid = process_user_sid().expect("the test user has a SID");
    if WinSessions.interactive_user_sids().contains(&own_sid) {
        assert_eq!(user.read("Kept"), Some(dword(1)), "the signed-in user's hive was not written");
    } else {
        assert_eq!(user.read("Kept"), None, "a hive without a session was written");
        let list = succeed(&instance, &["list"]);
        assert!(list.contains("custom.keep-user     on (not verifiable)"), "{list}");
    }
    wait_until("the command to have run", || command.read("FromCommand") == Some(dword(5)));

    // Installing again updates the registration and keeps it running.
    let updated = succeed(&instance, &["install"]);
    assert_eq!(updated.trim(), "SysPilot-Test service updated and running.");

    // Something keeps changing the value. It is restored a few times, then
    // the service stops fighting and reports the conflict.
    let contested = || succeed(&instance, &["list"]).contains("contested");
    for _ in 0..6 {
        if contested() {
            break;
        }
        machine.write("Kept", 0);
        wait_until("the value to be restored or reported as contested", || {
            machine.read("Kept") == Some(dword(1)) || contested()
        });
    }
    assert!(contested(), "{}", succeed(&instance, &["list"]));
    assert_eq!(machine.read("Kept"), Some(dword(0)), "a contested value is left alone");
    let history = succeed(&instance, &["history"]);
    assert!(history.contains("restored") && history.contains("contested"), "{history}");

    let removed = succeed(&instance, &["uninstall"]);
    assert!(removed.contains("service removed and settings restored"), "{removed}");
    assert!(!machine.exists(), "the machine scratch key was not restored to absent");
    assert!(!user.exists(), "the user scratch key was not restored to absent");
    assert_eq!(command.read("FromCommand"), None, "the command's undo did not run");
    assert!(!data_dir(&instance).exists(), "the data directory was left behind");
    let gone = run(&instance, &["status"]);
    assert!(!gone.status.success());
    assert!(String::from_utf8_lossy(&gone.stderr).contains("is not running"));
    // Removing what is not there is not an error.
    succeed(&instance, &["uninstall"]);
}

#[test]
fn uninstall_waits_for_the_programs_of_its_folder_only_when_asked() {
    require_elevation();
    let instance = Instance::named("SysPilot-Test.waits").unwrap();
    let folder = InstalledFolder::new("waits");

    // Not asked: it returns with the app still running, and says nothing.
    let mut app = folder.start_app(30);
    let plain = folder.uninstall(&instance, &[]);
    let stderr = String::from_utf8_lossy(&plain.stderr);
    assert!(plain.status.success(), "(uninstall): expected success | received: {plain:?}");
    assert_eq!(stderr, "", "(uninstall): expected nothing on stderr | received: {stderr:?}");
    let state = app.0.try_wait().expect("the stand-in can be asked");
    assert_eq!(state, None, "(uninstall): expected the app still running | received: {state:?}");
    drop(app);

    // Asked: it returns only once the app has ended by itself. The app lives
    // for five seconds, well beyond what the command needs to get to its
    // first look; if a slow machine gets there after the app ended, there is
    // nothing to wait for and nothing to say.
    let mut app = folder.start_app(5);
    let waited = folder.uninstall(&instance, &["--wait-for-processes"]);
    let stderr = String::from_utf8_lossy(&waited.stderr);
    assert!(waited.status.success(), "(uninstall): expected success | received: {waited:?}");
    assert!(
        stderr.is_empty() || stderr.starts_with("Waited "),
        "(uninstall --wait-for-processes): expected nothing or `Waited ...` on stderr | received: {stderr:?}"
    );
    let state = app.0.try_wait().expect("the stand-in can be asked");
    assert!(
        state.is_some(),
        "(uninstall --wait-for-processes): expected the app ended when it returned | received: still running"
    );
}
