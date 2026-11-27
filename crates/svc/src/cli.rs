//! The command-line interface: argument parsing, the client commands and
//! their text output.

use std::fmt::Write as _;
use std::path::PathBuf;

use syspilot_proto::catalog::{Catalog, Reboot, Tweak};
use syspilot_proto::message::{
    ActionState, DriverState, EntryStatus, Event, FanAvailability, FanPhase, FanStatus,
    HistoryEvent, Request, ServiceStatus, SystemInfo, UpdateState, UpdateStatus,
};
use syspilot_proto::profile::{Profile, UpdateMode};
use syspilot_proto::sensor::{Sample, SectionReader, SensorList};
use syspilot_proto::tuning::{TuningState, TuningStatus};
use syspilot_proto::win::MappedSection;
use syspilot_proto::{Client, ClientError, Instance, PRODUCT_NAME};

use crate::logger::iso_timestamp;

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Register and start the service.
    Install { start: bool },
    /// Remove the service. `wait_for_processes` also waits, before the data
    /// is deleted, for SysPilot's programs in this folder to exit, which only
    /// the installer asks for.
    Uninstall { keep_tweaks: bool, wait_for_processes: bool },
    /// Run in the console.
    Run,
    /// Entry point used by the service control manager.
    Service,
    /// Show service and driver status.
    Status,
    /// List tweaks and custom actions.
    List { enabled_only: bool },
    /// Show one tweak in full.
    Show { id: String },
    /// Show recent enforcement events.
    History { limit: u32 },
    /// Show current sensor readings.
    Sensors,
    /// Show the hardware and system inventory.
    Info,
    /// Turn tweaks on or off.
    SetTweaks { ids: Vec<String>, enabled: bool },
    /// Apply a tweak again now.
    Reapply { id: String },
    /// Write the profile as TOML to a file or standard output.
    Export { path: Option<PathBuf> },
    /// Replace the profile from a TOML file.
    Apply { path: PathBuf },
    /// Show how SysPilot updates itself, or choose it.
    Updates { mode: Option<UpdateMode> },
    /// Look for a new version, and install it unless only looking.
    Update { check_only: bool },
    /// Print the version.
    Version,
    /// Print the usage text.
    Help,
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Which instance to act on.
    pub instance: Instance,
    /// What to do.
    pub command: Command,
}

/// The usage text.
pub const USAGE: &str = "\
Usage: syspilot-svc <command> [options]

Look:
  status                     Service, driver and feature status
  list [--enabled]           Tweaks and custom actions with their state
  show <id>                  Everything about one tweak
  history [count]            Recent enforcement events (default 20)
  sensors                    Current sensor readings
  info                       Hardware and system inventory

Change (administrator):
  enable <id>...             Turn tweaks on
  disable <id>...            Turn tweaks off and restore the previous values
  reapply <id>               Apply a tweak again now
  export [file]              Write the profile as TOML
  apply <file>               Replace the profile with a TOML file
  updates [mode]             Show or choose how SysPilot updates itself:
                             off, notify (the default) or automatic
  update [--check]           Install the newest version now, or only look

Service (administrator):
  install [--no-start]       Register this executable as a Windows service
  uninstall [--keep-tweaks] [--wait-for-processes]
                             Restore all settings, then remove the service.
                             --wait-for-processes also waits up to 10 s for
                             SysPilot's programs in this folder to exit,
                             before the data is deleted; the installer asks
                             for it
  run                       Run in this console until Ctrl+C

Options:
  --instance <name>          Act on a named instance instead of SysPilot
  -h, --help                 Show this text
  -V, --version              Show the version
";

fn usage_error(problem: impl std::fmt::Display) -> String {
    format!("{problem}\nRun `syspilot-svc --help` for the commands.")
}

/// Parses the arguments after the program name.
pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Invocation, String> {
    let mut instance = Instance::product();
    let mut flags = Vec::new();
    let mut words = Vec::new();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        if argument == "--instance" {
            let name = arguments
                .next()
                .ok_or_else(|| usage_error("(--instance): expected a name after it"))?;
            instance = Instance::named(&name).map_err(usage_error)?;
        } else if argument.starts_with('-') && argument.len() > 1 {
            flags.push(argument);
        } else {
            words.push(argument);
        }
    }
    if flags.iter().any(|flag| flag == "-h" || flag == "--help") {
        return Ok(Invocation { instance, command: Command::Help });
    }
    if flags.iter().any(|flag| flag == "-V" || flag == "--version") {
        return Ok(Invocation { instance, command: Command::Version });
    }

    let mut words = words.into_iter();
    let Some(name) = words.next() else {
        return Ok(Invocation { instance, command: Command::Help });
    };
    let mut rest: Vec<String> = words.collect();
    // Takes a flag this command understands; whatever is left is an error.
    let mut take_flag = |wanted: &str| {
        let before = flags.len();
        flags.retain(|flag| flag != wanted);
        flags.len() != before
    };
    let one_id = |rest: &mut Vec<String>| match rest.len() {
        1 => Ok(rest.remove(0)),
        count => Err(usage_error(format!("({name}): expected one ID | received: {count}"))),
    };

    let command = match name.as_str() {
        "install" => Command::Install { start: !take_flag("--no-start") },
        "uninstall" => Command::Uninstall {
            keep_tweaks: take_flag("--keep-tweaks"),
            wait_for_processes: take_flag("--wait-for-processes"),
        },
        "run" => Command::Run,
        "service" => Command::Service,
        "status" => Command::Status,
        "list" => Command::List { enabled_only: take_flag("--enabled") },
        "show" => Command::Show { id: one_id(&mut rest)? },
        "history" => {
            let limit = match rest.pop() {
                None => 20,
                Some(text) => text.parse().map_err(|_| {
                    usage_error(format!("(history): expected a count | received: {text:?}"))
                })?,
            };
            Command::History { limit }
        }
        "sensors" => Command::Sensors,
        "info" => Command::Info,
        "enable" | "disable" => {
            if rest.is_empty() {
                return Err(usage_error(format!("({name}): expected at least one ID")));
            }
            Command::SetTweaks { ids: std::mem::take(&mut rest), enabled: name == "enable" }
        }
        "reapply" => Command::Reapply { id: one_id(&mut rest)? },
        "export" => Command::Export { path: rest.pop().map(PathBuf::from) },
        "apply" => match rest.pop() {
            Some(path) => Command::Apply { path: path.into() },
            None => return Err(usage_error("(apply): expected a profile file")),
        },
        "updates" => match rest.pop() {
            None => Command::Updates { mode: None },
            Some(key) => match UpdateMode::from_key(&key) {
                Some(mode) => Command::Updates { mode: Some(mode) },
                None => {
                    return Err(usage_error(format!(
                        "(updates): expected off | notify | automatic | received: {key:?}"
                    )));
                }
            },
        },
        "update" => Command::Update { check_only: take_flag("--check") },
        "help" => Command::Help,
        "version" => Command::Version,
        other => return Err(usage_error(format!("(command): {other:?} is not a command"))),
    };
    if let Some(extra) = flags.first().or(rest.first()) {
        return Err(usage_error(format!("({name}): unexpected argument {extra:?}")));
    }
    Ok(Invocation { instance, command })
}

/// `3d 4h`, `2h 5m`, `41s`: the two most significant units.
fn human_duration(seconds: u64) -> String {
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3600 % 24, seconds / 60 % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds}s"),
        (0, 0, _) => format!("{minutes}m {}s", seconds % 60),
        (0, _, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

fn driver_line(driver: &DriverState, url: &str) -> String {
    match driver {
        DriverState::Missing => format!("PawnIO is not installed. Get it from {url}"),
        DriverState::Ready { version } => format!("PawnIO {version}"),
        DriverState::TooOld { version, required } => {
            format!(
                "PawnIO {version} is too old; {required} or later is needed. Update it from {url}"
            )
        }
        DriverState::Failed { error } => format!("PawnIO is installed but not usable: {error}"),
    }
}

/// How SysPilot updates itself and where that stands, in one line.
fn update_line(update: &UpdateStatus) -> String {
    let state = match &update.state {
        UpdateState::Unavailable => return "this copy does not update itself".into(),
        UpdateState::Idle if update.checked_unix.is_some() => "up to date".to_owned(),
        UpdateState::Idle if update.mode == UpdateMode::Off => "not looking".to_owned(),
        UpdateState::Idle => "has not looked yet".to_owned(),
        UpdateState::Checking => "looking for a new version".to_owned(),
        UpdateState::Available { version } => format!("version {version} is available"),
        UpdateState::Installing => "installing a new version".to_owned(),
        UpdateState::Failed { reason } => format!("failed: {reason}"),
    };
    format!("{}, {state}", update.mode.key())
}

/// Asks the service to look for a new version and waits for what it finds.
fn look_for_update(client: &mut Client) -> Result<String, String> {
    let problem = |error: ClientError| error.to_string();
    client.command(&Request::CheckForUpdate).map_err(problem)?;
    // The look is a network request of the updater; the service answers at
    // once and the outcome shows in its status.
    for _ in 0..UPDATE_LOOK_POLLS {
        let update = client.status().map_err(problem)?.update;
        if update.state != UpdateState::Checking {
            return Ok(format!("{}\n", update_line(&update)));
        }
        std::thread::sleep(UPDATE_LOOK_POLL);
    }
    Ok("Still looking. See `syspilot-svc status` in a moment.\n".into())
}

/// How often and how many times the outcome of a look is asked for: up to
/// two minutes, the updater's own timeouts taken together.
const UPDATE_LOOK_POLL: std::time::Duration = std::time::Duration::from_millis(500);
const UPDATE_LOOK_POLLS: u32 = 240;

/// Renders the status view.
pub fn render_status(status: &ServiceStatus) -> String {
    let mode = if status.console { "console" } else { "service" };
    let mut out = format!(
        "{} {} ({mode}, up {})\n",
        status.instance,
        status.version,
        human_duration(status.uptime_secs)
    );
    let _ = writeln!(out, "  Windows build  {}", status.os_build);
    let _ = writeln!(out, "  Driver         {}", driver_line(&status.driver, &status.driver_url));
    let sampling = if status.sampling { "sampling sensors" } else { "not sampling" };
    let _ = writeln!(out, "  Clients        {} subscribed, {sampling}", status.subscribers);
    let _ = writeln!(out, "  Sensor logs    {}", status.log_dir);
    let _ = writeln!(out, "  Updates        {}", update_line(&status.update));
    if !status.features.is_empty() {
        out.push_str("Features\n");
    }
    for feature in &status.features {
        let mark = if feature.available { "yes" } else { " no" };
        let _ = write!(out, "  {mark}  {}", feature.title);
        if !feature.detail.is_empty() {
            let _ = write!(out, " ({})", feature.detail);
        }
        out.push('\n');
    }
    out.push_str(&fans_section(&status.fans));
    out.push_str(&tuning_section(&status.tuning));
    if !status.problems.is_empty() {
        out.push_str("Problems\n");
    }
    for problem in &status.problems {
        let _ = writeln!(out, "  ! {problem}");
    }
    out
}

/// What the status says about the fans: whether curves are followed, and for
/// each channel what it is doing. Empty for a build with no fan control and
/// a board the catalog does not know, which has nothing to say.
fn fans_section(fans: &FanStatus) -> String {
    if fans.availability == FanAvailability::NotInBuild && fans.channels.is_empty() {
        return String::new();
    }
    let board = if fans.board.is_empty() { String::new() } else { format!(" ({})", fans.board) };
    let mut out = format!("Fans{board}\n");
    let control = match &fans.availability {
        FanAvailability::NotInBuild => "this build leaves fan control out".to_owned(),
        FanAvailability::ReadOnly { reason } => format!("read-only: {reason}"),
        FanAvailability::Controllable => "curves in the profile are followed".to_owned(),
    };
    let _ = writeln!(out, "  Control        {control}");
    for channel in &fans.channels {
        let state = match channel.phase {
            FanPhase::Manual => {
                format!("following its curve, asking {}", percent(channel.target_duty_pct))
            }
            FanPhase::FailSafe => {
                format!(
                    "full speed because a sensor failed, asking {}",
                    percent(channel.target_duty_pct)
                )
            }
            FanPhase::ReleasePending => "handing back to the BIOS did not work, retrying".into(),
            FanPhase::Blocked => "left alone: found in manual mode with no saved setting".into(),
            FanPhase::Bios if channel.active => "BIOS control, curve waiting for sensors".into(),
            FanPhase::Bios if !channel.curve.is_empty() => {
                "BIOS control, curve not followed".into()
            }
            FanPhase::Bios => "BIOS control".into(),
        };
        let _ = write!(out, "  {:<12}  {} - {state}", channel.channel, channel.label);
        if let Some(problem) = &channel.problem {
            let _ = write!(out, " ({problem})");
        }
        out.push('\n');
    }
    out
}

/// What the status says about tuning: each device that can be tuned, where
/// its setting stands and the values that are not at the driver's default.
/// Empty on a machine with nothing to tune and nothing to report.
fn tuning_section(tuning: &TuningStatus) -> String {
    if tuning.devices.is_empty() && tuning.problem.is_none() {
        return String::new();
    }
    let mut out = "Tuning\n".to_owned();
    if let Some(problem) = &tuning.problem {
        let _ = writeln!(out, "  ! {problem}");
    }
    for device in &tuning.devices {
        let state = match &device.state {
            TuningState::Unmanaged => "left to its driver".to_owned(),
            TuningState::Applied => "setting applied".to_owned(),
            TuningState::Different => "setting not in place yet".to_owned(),
            TuningState::Contested => {
                "setting given up: something else keeps changing the values".to_owned()
            }
            TuningState::Held => {
                "setting held back: the machine stopped after it was last applied".to_owned()
            }
            TuningState::Failed { reason } => format!("setting failed: {reason}"),
            TuningState::Missing => "setting for a device that is not there".to_owned(),
        };
        let _ = writeln!(out, "  {} - {state}", device.name);
        for control in device.controls.iter().filter(|control| control.current != control.default) {
            let suffix = control.unit.suffix();
            let _ = writeln!(
                out,
                "    {:<18}  {} {suffix} (default {} {suffix})",
                control.key, control.current, control.default
            );
        }
    }
    out
}

fn percent(duty: Option<u8>) -> String {
    duty.map_or_else(|| "an unknown duty".to_owned(), |duty| format!("{duty} %"))
}

/// One-word state of an entry for the list view. Checks only report, so
/// they read as a finding rather than as on or off.
fn state_label(status: &EntryStatus, audit: bool) -> String {
    match (&status.state, status.enabled, audit) {
        (ActionState::NotApplicable { .. }, _, _) => "n/a".into(),
        (ActionState::Compliant, _, true) => "ok".into(),
        (ActionState::Different, _, true) => "attention".into(),
        (_, _, true) => "unknown".into(),
        (ActionState::Compliant, true, _) => "on".into(),
        (ActionState::Compliant, false, _) => "off (already set)".into(),
        (_, false, _) => "off".into(),
        (ActionState::Different, true, _) => "on (not applied yet)".into(),
        (ActionState::Contested { retry_in_secs }, true, _) => {
            format!("contested (retry in {})", human_duration(u64::from(*retry_in_secs)))
        }
        (ActionState::NotEnforceable { .. }, true, _) => "blocked by Windows".into(),
        (ActionState::Unknown, true, _) => "on (not verifiable)".into(),
        (ActionState::Failed { .. }, true, _) => "failed".into(),
    }
}

/// Renders tweaks and custom actions with their state.
pub fn render_list(
    catalog: &Catalog,
    states: &[EntryStatus],
    custom_titles: &[(String, String)],
    enabled_only: bool,
) -> String {
    let rows: Vec<(String, &str, &str)> = states
        .iter()
        .filter(|status| status.enabled || !enabled_only)
        .map(|status| {
            let tweak = catalog.get(&status.id);
            let title = tweak.map(|tweak| tweak.title.as_str()).or_else(|| {
                custom_titles
                    .iter()
                    .find(|(id, _)| *id == status.id)
                    .map(|(_, title)| title.as_str())
            });
            let audit = tweak.is_some_and(|tweak| tweak.audit);
            (state_label(status, audit), status.id.as_str(), title.unwrap_or(""))
        })
        .collect();
    if rows.is_empty() {
        return if enabled_only { "Nothing is turned on.\n" } else { "The catalog is empty.\n" }
            .into();
    }
    let id_width = rows.iter().map(|row| row.1.len()).max().unwrap_or(0);
    let state_width = rows.iter().map(|row| row.0.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (state, id, title) in rows {
        let _ = writeln!(out, "{id:id_width$}  {state:state_width$}  {title}");
    }
    out
}

/// Renders everything known about one tweak.
pub fn render_tweak(tweak: &Tweak, status: Option<&EntryStatus>) -> String {
    let mut out = format!("{}\n  {}\n", tweak.title, tweak.summary);
    if !tweak.detail.is_empty() {
        let _ = writeln!(out, "  {}", tweak.detail);
    }
    let mut field = |name: &str, value: &str| {
        if !value.is_empty() {
            let _ = writeln!(out, "  {name:13}{value}");
        }
    };
    field("ID", &tweak.id);
    field("Level", &format!("{:?}", tweak.level).to_lowercase());
    field("Category", &format!("{:?}", tweak.category).to_lowercase());
    if let Some(direction) = tweak.direction {
        field("Security", &format!("{direction:?}").to_lowercase());
    }
    field("Risk", tweak.risk.as_deref().unwrap_or(""));
    field("Side effects", &tweak.side_effects);
    match tweak.reboot {
        Reboot::None => {}
        Reboot::SignOut => field("Takes effect", "after you sign out and back in"),
        Reboot::Restart => field("Takes effect", "after a restart"),
    }
    if let Some(build) = tweak.min_build {
        field("Needs build", &build.to_string());
    }
    field("Source", &tweak.source);
    if let Some(status) = status {
        field("State", &state_label(status, tweak.audit));
        if status.drift_count > 0 {
            field(
                "Restored",
                &format!("{} time(s) after something changed it", status.drift_count),
            );
        }
    }
    out.push_str("Settings\n");
    for (index, action) in tweak.actions.iter().enumerate() {
        let _ = writeln!(out, "  {}", action.describe());
        let current = status.and_then(|status| status.actions.get(index));
        if let Some(current) = current.filter(|current| !current.current.is_empty()) {
            let _ = writeln!(out, "    now: {}", current.current);
        }
    }
    out
}

/// Renders enforcement events, newest first.
pub fn render_history(events: &[HistoryEvent]) -> String {
    if events.is_empty() {
        return "No events yet.\n".into();
    }
    let mut out = String::new();
    for event in events {
        let kind = format!("{:?}", event.kind).to_lowercase();
        let _ = writeln!(
            out,
            "{}  {kind:9}  {}  {}",
            iso_timestamp(event.unix_secs),
            event.id,
            event.detail
        );
    }
    out
}

/// Formats a reading with at most one decimal and its unit. Byte counts
/// and rates are scaled to the largest binary prefix that keeps the number
/// at 1 or above, so memory reads "13.9 GB" and not fourteen digits.
fn reading(value: Option<f64>, unit: &str) -> String {
    let Some(mut value) = value else { return "-".into() };
    let mut prefix = "";
    if unit.starts_with('B') {
        for larger in ["K", "M", "G", "T"] {
            if value.abs() < 1024.0 {
                break;
            }
            value /= 1024.0;
            prefix = larger;
        }
    }
    if value.abs() >= 100.0 || (unit.starts_with('B') && prefix.is_empty()) {
        format!("{value:.0} {prefix}{unit}")
    } else {
        format!("{value:.1} {prefix}{unit}")
    }
}

/// Renders sensor readings grouped by device.
pub fn render_sensors(list: &SensorList, samples: &[Sample]) -> String {
    if list.sensors.is_empty() {
        return "No sensors were found.\n".into();
    }
    let width = list.sensors.iter().map(|sensor| sensor.label.chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    for (index, device) in list.devices.iter().enumerate() {
        let _ = writeln!(out, "{}", device.name);
        for sensor in list.sensors.iter().filter(|sensor| sensor.device as usize == index) {
            let value = samples.iter().find(|sample| sample.id == sensor.id).and_then(|s| s.value);
            let label = &sensor.label;
            let _ = writeln!(out, "  {label:width$}  {:>12}", reading(value, sensor.kind.unit()));
        }
    }
    out
}

/// Renders the inventory.
pub fn render_info(info: &SystemInfo) -> String {
    let summary = &info.summary;
    let mut out = String::new();
    let gpus = summary.gpus.join(", ");
    let memory = match summary.memory_bytes {
        0 => String::new(),
        bytes => format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30)),
    };
    for (name, value) in [
        ("Computer", summary.computer.as_str()),
        ("Model", &summary.model),
        ("Processor", &summary.cpu),
        ("Graphics", &gpus),
        ("Memory", &memory),
        ("Windows", &summary.os),
        ("Firmware", &summary.bios),
    ] {
        if !value.is_empty() {
            let _ = writeln!(out, "{name:10} {value}");
        }
    }
    for section in &info.sections {
        let _ = writeln!(out, "\n{}", section.title);
        let width = section.rows.iter().map(|row| row.0.chars().count()).max().unwrap_or(0);
        for (name, value) in &section.rows {
            let _ = writeln!(out, "  {name:width$}  {value}");
        }
    }
    out
}

fn connect(instance: &Instance) -> Result<Client, String> {
    Client::connect(instance).map_err(|error| match error {
        ClientError::NotRunning => format!(
            "{} is not running. Start the service, or run `syspilot-svc run` in an elevated terminal.",
            instance.name()
        ),
        other => other.to_string(),
    })
}

/// Reads the sensor list and one settled sample.
fn read_sensors(instance: &Instance) -> Result<(SensorList, Vec<Sample>), String> {
    let problem = |error: ClientError| error.to_string();
    // Subscribing is what makes the service sample at all.
    let mut events = connect(instance)?.subscribe(true).map_err(problem)?;
    // Rates need two samples; the first one only sets their baseline.
    let mut ticks = 0;
    while ticks < 2 {
        if events.next_event().map_err(problem)? == Event::Sampled {
            ticks += 1;
        }
    }
    let list = connect(instance)?.sensors().map_err(problem)?;
    let section = MappedSection::open(&instance.sensor_section_name())
        .map_err(|error| format!("open the sensor section: {error}"))?;
    let reader = SectionReader::new(section.words()).map_err(|error| error.to_string())?;
    let mut samples = Vec::new();
    let header = reader.read(&mut samples).map_err(|error| error.to_string())?;
    if header.generation != list.generation {
        return Err("The set of sensors changed while reading. Run the command again.".into());
    }
    Ok((list, samples))
}

/// Runs a command that talks to the service and returns what to print.
pub fn run_client_command(instance: &Instance, command: Command) -> Result<String, String> {
    let problem = |error: ClientError| error.to_string();
    if command == Command::Sensors {
        let (list, samples) = read_sensors(instance)?;
        return Ok(render_sensors(&list, &samples));
    }
    let mut client = connect(instance)?;
    match command {
        Command::Status => Ok(render_status(&client.status().map_err(problem)?)),
        Command::List { enabled_only } => {
            let catalog = client.catalog().map_err(problem)?;
            let states = client.states().map_err(problem)?;
            // Custom action titles live in the profile, which only an
            // administrator may read; without it they are listed by ID.
            let custom_titles = client.profile().map_or_else(
                |_| Vec::new(),
                |profile| profile.custom.into_iter().map(|c| (c.id, c.title)).collect(),
            );
            Ok(render_list(&catalog, &states, &custom_titles, enabled_only))
        }
        Command::Show { id } => {
            let catalog = client.catalog().map_err(problem)?;
            let tweak = catalog
                .get(&id)
                .ok_or_else(|| format!("There is no tweak with the ID {id}. See `list`."))?;
            let states = client.states().map_err(problem)?;
            Ok(render_tweak(tweak, states.iter().find(|status| status.id == id)))
        }
        Command::History { limit } => Ok(render_history(&client.history(limit).map_err(problem)?)),
        Command::Info => Ok(render_info(&client.system_info().map_err(problem)?)),
        Command::SetTweaks { ids, enabled } => {
            for id in &ids {
                client.command(&Request::SetTweak { id: id.clone(), enabled }).map_err(problem)?;
            }
            let verb = if enabled { "Turned on" } else { "Turned off" };
            Ok(format!("{verb}: {}\n", ids.join(", ")))
        }
        Command::Reapply { id } => {
            client.command(&Request::Reapply { id: id.clone() }).map_err(problem)?;
            Ok(format!("Applied again: {id}\n"))
        }
        Command::Export { path } => {
            let text = client.profile().map_err(problem)?.to_toml();
            match path {
                None => Ok(text),
                Some(path) => std::fs::write(&path, text)
                    .map(|()| format!("Profile written to {}\n", path.display()))
                    .map_err(|error| format!("write {}: {error}", path.display())),
            }
        }
        Command::Apply { path } => {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            let profile = Profile::parse(&text).map_err(|error| error.to_string())?;
            let summary = format!(
                "Profile applied: {} tweak(s) on, {} custom action(s)\n",
                profile.enabled.len(),
                profile.custom.len()
            );
            client.command(&Request::SetProfile(profile)).map_err(problem)?;
            Ok(summary)
        }
        Command::Updates { mode: None } => {
            Ok(format!("{}\n", update_line(&client.status().map_err(problem)?.update)))
        }
        Command::Updates { mode: Some(mode) } => {
            client.command(&Request::SetUpdates(mode)).map_err(problem)?;
            Ok(format!("Updates: {}\n", mode.key()))
        }
        Command::Update { check_only: true } => look_for_update(&mut client),
        Command::Update { check_only: false } => {
            client.command(&Request::InstallUpdate).map_err(problem)?;
            Ok("The newest version is being downloaded, verified and installed. The service \
                restarts when it is in place; `syspilot-svc status` shows how it went.\n"
                .into())
        }
        other => Err(format!("(command): {other:?} does not talk to the service")),
    }
}

/// The version line.
pub fn version() -> String {
    format!("{PRODUCT_NAME} service {}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_proto::message::{
        ActionStatus, FanChannelStatus, Feature, HistoryKind, InfoSection, SystemSummary,
    };
    use syspilot_proto::sensor::{DeviceClass, DeviceInfo, SensorInfo, SensorKind};

    fn parse_words(line: &str) -> Result<Invocation, String> {
        parse(line.split_whitespace().map(str::to_owned))
    }

    fn command(line: &str) -> Command {
        parse_words(line).unwrap().command
    }

    #[test]
    fn commands_parse_with_their_defaults() {
        assert_eq!(command(""), Command::Help);
        assert_eq!(command("status"), Command::Status);
        assert_eq!(command("install"), Command::Install { start: true });
        assert_eq!(command("install --no-start"), Command::Install { start: false });
        assert_eq!(
            command("uninstall"),
            Command::Uninstall { keep_tweaks: false, wait_for_processes: false }
        );
        assert_eq!(
            command("uninstall --keep-tweaks"),
            Command::Uninstall { keep_tweaks: true, wait_for_processes: false }
        );
        assert_eq!(command("list --enabled"), Command::List { enabled_only: true });
        assert_eq!(command("history"), Command::History { limit: 20 });
        assert_eq!(command("history 5"), Command::History { limit: 5 });
        assert_eq!(command("export"), Command::Export { path: None });
        assert_eq!(command("apply p.toml"), Command::Apply { path: "p.toml".into() });
        assert_eq!(
            command("enable a.b c.d"),
            Command::SetTweaks { ids: vec!["a.b".into(), "c.d".into()], enabled: true }
        );
        assert_eq!(
            command("disable a.b"),
            Command::SetTweaks { ids: vec!["a.b".into()], enabled: false }
        );
        assert_eq!(command("updates"), Command::Updates { mode: None });
        assert_eq!(
            command("updates automatic"),
            Command::Updates { mode: Some(UpdateMode::Automatic) }
        );
        assert_eq!(command("update"), Command::Update { check_only: false });
        assert_eq!(command("update --check"), Command::Update { check_only: true });
        assert_eq!(command("--version"), Command::Version);
        assert_eq!(command("status --help"), Command::Help);
    }

    #[test]
    fn uninstall_waits_for_processes_only_when_asked() {
        let both = Command::Uninstall { keep_tweaks: true, wait_for_processes: true };
        assert_eq!(
            command("uninstall --wait-for-processes"),
            Command::Uninstall { keep_tweaks: false, wait_for_processes: true }
        );
        assert_eq!(command("uninstall --keep-tweaks --wait-for-processes"), both);
        assert_eq!(command("uninstall --wait-for-processes --keep-tweaks"), both);
        let invocation = parse_words("--instance SysPilot-Test.msi uninstall --wait-for-processes");
        assert_eq!(invocation.unwrap().instance.name(), "SysPilot-Test.msi");
        for line in ["install", "status", "run"] {
            let problem = parse_words(&format!("{line} --wait-for-processes")).unwrap_err();
            let expected = format!(r#"({line}): unexpected argument "--wait-for-processes""#);
            assert!(problem.starts_with(&expected), "{line}: {problem}");
        }
        assert!(USAGE.contains("[--wait-for-processes]"), "(usage): expected the flag listed");
    }

    #[test]
    fn instance_option_works_before_and_after_the_command() {
        for line in ["--instance SysPilot-Test status", "status --instance SysPilot-Test"] {
            let invocation = parse_words(line).unwrap();
            assert_eq!(invocation.instance.name(), "SysPilot-Test", "{line}");
            assert_eq!(invocation.command, Command::Status);
        }
        assert!(parse_words("status").unwrap().instance.is_product());
    }

    #[test]
    fn mistakes_are_named() {
        let cases = [
            ("frobnicate", r#"(command): "frobnicate" is not a command"#),
            ("status extra", r#"(status): unexpected argument "extra""#),
            ("list --bogus", r#"(list): unexpected argument "--bogus""#),
            ("enable", "(enable): expected at least one ID"),
            ("show", "(show): expected one ID | received: 0"),
            ("history many", r#"(history): expected a count | received: "many""#),
            ("apply", "(apply): expected a profile file"),
            (
                "updates sometimes",
                r#"(updates): expected off | notify | automatic | received: "sometimes""#,
            ),
            ("status --instance", "(--instance): expected a name after it"),
        ];
        for (line, expected) in cases {
            let problem = parse_words(line).unwrap_err();
            assert!(problem.starts_with(expected), "{line}: {problem}");
            assert!(problem.ends_with("for the commands."), "{line}: {problem}");
        }
        assert!(parse_words("status --instance bad/name").is_err());
    }

    #[test]
    fn durations_show_two_units() {
        assert_eq!(human_duration(41), "41s");
        assert_eq!(human_duration(125), "2m 5s");
        assert_eq!(human_duration(2 * 3600 + 300), "2h 5m");
        assert_eq!(human_duration(3 * 86_400 + 4 * 3600 + 59), "3d 4h");
    }

    fn status() -> ServiceStatus {
        ServiceStatus {
            version: "0.1.0".into(),
            instance: "SysPilot".into(),
            uptime_secs: 125,
            console: false,
            driver: DriverState::Missing,
            driver_url: "https://pawnio.eu/".into(),
            features: vec![Feature {
                key: "fans".into(),
                title: "Fan speeds".into(),
                available: false,
                detail: "needs PawnIO".into(),
            }],
            subscribers: 1,
            sampling: true,
            os_build: 26100,
            log_dir: r"C:\ProgramData\SysPilot\logs".into(),
            fan_control: false,
            fans: Default::default(),
            update: UpdateStatus::default(),
            tuning: TuningStatus::default(),
            problems: vec!["Settings could not be saved to disk: disk full".into()],
        }
    }

    #[test]
    fn status_view_points_to_the_driver_download() {
        assert_eq!(
            render_status(&status()),
            "SysPilot 0.1.0 (service, up 2m 5s)\n\
             \x20 Windows build  26100\n\
             \x20 Driver         PawnIO is not installed. Get it from https://pawnio.eu/\n\
             \x20 Clients        1 subscribed, sampling sensors\n\
             \x20 Sensor logs    C:\\ProgramData\\SysPilot\\logs\n\
             \x20 Updates        notify, has not looked yet\n\
             Features\n\
             \x20  no  Fan speeds (needs PawnIO)\n\
             Problems\n\
             \x20 ! Settings could not be saved to disk: disk full\n"
        );
    }

    #[test]
    fn the_update_line_says_the_mode_and_where_things_stand() {
        let line =
            |mode, state, checked_unix| update_line(&UpdateStatus { mode, state, checked_unix });
        let found = UpdateState::Available { version: "0.2.1".into() };
        let failed = UpdateState::Failed { reason: "github.com could not be reached".into() };
        assert_eq!(line(UpdateMode::Notify, UpdateState::Idle, Some(1)), "notify, up to date");
        assert_eq!(line(UpdateMode::Off, UpdateState::Idle, None), "off, not looking");
        assert_eq!(line(UpdateMode::Off, UpdateState::Idle, Some(1)), "off, up to date");
        assert_eq!(
            line(UpdateMode::Automatic, UpdateState::Checking, None),
            "automatic, looking for a new version"
        );
        assert_eq!(line(UpdateMode::Notify, found, Some(1)), "notify, version 0.2.1 is available");
        assert_eq!(
            line(UpdateMode::Automatic, UpdateState::Installing, Some(1)),
            "automatic, installing a new version"
        );
        assert_eq!(
            line(UpdateMode::Notify, failed, None),
            "notify, failed: github.com could not be reached"
        );
        assert_eq!(
            line(UpdateMode::Automatic, UpdateState::Unavailable, None),
            "this copy does not update itself"
        );
    }

    fn fan_channel(pwm: u8, label: &str, phase: FanPhase, duty: Option<u8>) -> FanChannelStatus {
        FanChannelStatus {
            channel: format!("board.pwm{pwm}"),
            label: label.into(),
            curve: vec![syspilot_proto::profile::CurvePoint(30, 20)],
            active: true,
            target_duty_pct: duty,
            phase,
            ..FanChannelStatus::default()
        }
    }

    #[test]
    fn status_view_says_nothing_about_fans_a_build_without_control_cannot_describe() {
        assert!(!render_status(&status()).contains("Fans"));
    }

    #[test]
    fn status_view_lists_each_tuned_device_with_its_changed_values() {
        use syspilot_proto::tuning::{TuningControl, TuningDevice, TuningKind, TuningUnit};
        assert!(!render_status(&status()).contains("Tuning"), "nothing to tune, nothing said");

        let control = |key: &str, unit, default, current| TuningControl {
            key: key.into(),
            kind: TuningKind::PowerLimit,
            state: None,
            unit,
            min: -2000,
            max: 2000,
            step: 1,
            default,
            current,
            wanted: None,
        };
        let device = |name: &str, state, controls| TuningDevice {
            key: "gpu.amd-699f.bus-3".into(),
            name: name.into(),
            interface: "AMD Radeon driver (Overdrive N)".into(),
            controls,
            enabled: true,
            state,
        };
        let mut view = status();
        view.tuning = TuningStatus {
            devices: vec![
                device(
                    "Radeon RX 550 Series",
                    TuningState::Applied,
                    vec![
                        control("power-limit", TuningUnit::Percent, 0, -20),
                        control("core-p7-clock", TuningUnit::Megahertz, 1183, 1183),
                        control("core-p7-voltage", TuningUnit::Millivolt, 1050, 1000),
                    ],
                ),
                device("gpu.amd-0000.bus-9", TuningState::Missing, vec![]),
            ],
            problem: Some("(tuner): expected its report | received: nothing".into()),
        };
        let text = render_status(&view);
        let expected = "Tuning\n\
             \x20 ! (tuner): expected its report | received: nothing\n\
             \x20 Radeon RX 550 Series - setting applied\n\
             \x20   power-limit         -20 % (default 0 %)\n\
             \x20   core-p7-voltage     1000 mV (default 1050 mV)\n\
             \x20 gpu.amd-0000.bus-9 - setting for a device that is not there\n\
             Problems\n";
        assert!(text.contains(expected), "{text}");
    }

    #[test]
    fn status_view_lists_each_fan_with_what_it_is_doing() {
        let mut view = status();
        let failing = FanChannelStatus {
            problem: Some("cpu.temp has no reading".into()),
            ..fan_channel(3, "Case fan", FanPhase::FailSafe, Some(100))
        };
        view.fans = FanStatus {
            availability: FanAvailability::Controllable,
            board: "ACME B550".into(),
            channels: vec![
                fan_channel(1, "CPU fan", FanPhase::Manual, Some(40)),
                fan_channel(2, "Rear fan", FanPhase::Bios, None),
                failing,
            ],
        };
        let text = render_status(&view);
        assert!(
            text.contains(
                "Fans (ACME B550)\n  Control        curves in the profile are followed\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("  board.pwm1    CPU fan - following its curve, asking 40 %\n"),
            "{text}"
        );
        assert!(
            text.contains("  board.pwm2    Rear fan - BIOS control, curve waiting for sensors\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "Case fan - full speed because a sensor failed, asking 100 % (cpu.temp has no reading)\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn status_view_gives_the_reason_a_board_is_read_only() {
        let mut view = status();
        view.fans = FanStatus {
            availability: FanAvailability::ReadOnly {
                reason: "the board's entry has not been validated".into(),
            },
            board: "ACME A320".into(),
            channels: vec![FanChannelStatus {
                curve: Vec::new(),
                active: false,
                ..fan_channel(1, "CPU fan", FanPhase::Bios, None)
            }],
        };
        let text = render_status(&view);
        assert!(
            text.contains("  Control        read-only: the board's entry has not been validated\n"),
            "{text}"
        );
        assert!(text.contains("CPU fan - BIOS control\n"), "{text}");
    }

    #[test]
    fn driver_states_read_as_sentences() {
        let url = "https://pawnio.eu/";
        assert_eq!(
            driver_line(&DriverState::Ready { version: "2.2.0".into() }, url),
            "PawnIO 2.2.0"
        );
        let old = DriverState::TooOld { version: "1.0".into(), required: "2.0".into() };
        assert!(driver_line(&old, url).contains("2.0 or later is needed"));
        let failed = DriverState::Failed { error: "access denied".into() };
        assert_eq!(driver_line(&failed, url), "PawnIO is installed but not usable: access denied");
    }

    fn entry(id: &str, enabled: bool, state: ActionState) -> EntryStatus {
        EntryStatus {
            id: id.into(),
            enabled,
            state,
            actions: vec![ActionStatus { state: ActionState::Compliant, current: "1".into() }],
            drift_count: 0,
            last_drift_unix: None,
        }
    }

    const CATALOG: &str = r#"
[[tweak]]
id = "privacy.ads"
title = "Turn off the advertising ID"
summary = "Stops apps from using your advertising ID."
level = "simple"
category = "privacy"
source = "https://learn.microsoft.com/windows/"
[[tweak.action]]
registry = { scope = "user", key = 'Software\Example', name = "Enabled", data = { dword = 0 } }

[[tweak]]
id = "security.check"
title = "Secure Boot is on"
summary = "Reports whether Secure Boot is on."
level = "advanced"
category = "security"
direction = "hardens"
audit = true
source = "https://learn.microsoft.com/windows/"
[[tweak.action]]
registry = { scope = "machine", key = 'SYSTEM\Example', name = "Enabled", data = { dword = 1 } }
"#;

    #[test]
    fn list_aligns_columns_and_names_custom_actions() {
        let catalog = Catalog::parse(CATALOG).unwrap();
        let states = [
            entry("privacy.ads", true, ActionState::Compliant),
            entry("security.check", false, ActionState::Different),
            entry("custom.mine", false, ActionState::Unknown),
        ];
        let titles = [("custom.mine".to_owned(), "My action".to_owned())];
        assert_eq!(
            render_list(&catalog, &states, &titles, false),
            "privacy.ads     on         Turn off the advertising ID\n\
             security.check  attention  Secure Boot is on\n\
             custom.mine     off        My action\n"
        );
        assert_eq!(
            render_list(&catalog, &states, &titles, true),
            "privacy.ads  on  Turn off the advertising ID\n"
        );
        assert_eq!(render_list(&catalog, &states[1..], &titles, true), "Nothing is turned on.\n");
    }

    #[test]
    fn every_state_has_a_label() {
        let label = |enabled, state| state_label(&entry("x.y", enabled, state), false);
        assert_eq!(label(false, ActionState::Compliant), "off (already set)");
        assert_eq!(label(true, ActionState::Different), "on (not applied yet)");
        assert_eq!(
            label(true, ActionState::Contested { retry_in_secs: 125 }),
            "contested (retry in 2m 5s)"
        );
        assert_eq!(
            label(true, ActionState::NotEnforceable { reason: "x".into() }),
            "blocked by Windows"
        );
        assert_eq!(label(true, ActionState::Failed { error: "x".into() }), "failed");
        assert_eq!(label(true, ActionState::Unknown), "on (not verifiable)");
        assert_eq!(label(false, ActionState::NotApplicable { reason: "x".into() }), "n/a");
        assert_eq!(state_label(&entry("x.y", false, ActionState::Compliant), true), "ok");
    }

    #[test]
    fn tweak_view_lists_fields_and_current_values() {
        let catalog = Catalog::parse(CATALOG).unwrap();
        let tweak = catalog.get("privacy.ads").unwrap();
        let mut status = entry("privacy.ads", true, ActionState::Compliant);
        status.drift_count = 2;
        let text = render_tweak(tweak, Some(&status));
        assert!(text.starts_with("Turn off the advertising ID\n  Stops apps from"), "{text}");
        assert!(text.contains("  ID           privacy.ads\n"), "{text}");
        assert!(text.contains("  Level        simple\n"), "{text}");
        assert!(text.contains("  State        on\n"), "{text}");
        assert!(text.contains("  Restored     2 time(s) after something changed it\n"), "{text}");
        assert!(text.contains("Settings\n  "), "{text}");
        assert!(text.ends_with("    now: 1\n"), "{text}");
        assert!(!text.contains("Risk"), "empty fields are left out: {text}");
        assert!(!text.contains("Takes effect"), "no restart is not worth a line: {text}");

        let mut needs_restart = tweak.clone();
        needs_restart.reboot = Reboot::Restart;
        let text = render_tweak(&needs_restart, None);
        assert!(text.contains("  Takes effect after a restart\n"), "{text}");
    }

    #[test]
    fn history_lines_have_time_kind_id_and_detail() {
        assert_eq!(render_history(&[]), "No events yet.\n");
        let events = [HistoryEvent {
            unix_secs: 1_795_444_745,
            id: "privacy.ads".into(),
            kind: HistoryKind::Restored,
            detail: "was 1".into(),
        }];
        assert_eq!(
            render_history(&events),
            "2026-11-23T14:39:05Z  restored   privacy.ads  was 1\n"
        );
    }

    #[test]
    fn sensors_are_grouped_by_device_with_units() {
        let sensor = |id, device, label: &str, kind| SensorInfo {
            id,
            device,
            key: format!("k{id}"),
            label: label.into(),
            kind,
            driver: false,
        };
        let list = SensorList {
            generation: 1,
            devices: vec![
                DeviceInfo { name: "Example CPU".into(), class: DeviceClass::Cpu },
                DeviceInfo { name: "Example board".into(), class: DeviceClass::Board },
            ],
            sensors: vec![
                sensor(0, 0, "Package", SensorKind::Temperature),
                sensor(1, 1, "Fan 1", SensorKind::Fan),
                sensor(2, 0, "Load", SensorKind::Load),
            ],
        };
        let samples = [
            Sample { id: 0, value: Some(45.3) },
            Sample { id: 1, value: Some(1234.4) },
            Sample { id: 2, value: None },
        ];
        // Labels are padded to the longest one; readings are right-aligned
        // in twelve columns.
        let expected = [
            "Example CPU",
            "  Package       45.3 °C",
            "  Load                -",
            "Example board",
            "  Fan 1        1234 RPM",
            "",
        ]
        .join("\n");
        assert_eq!(render_sensors(&list, &samples), expected);
        assert_eq!(render_sensors(&SensorList::default(), &[]), "No sensors were found.\n");
    }

    #[test]
    fn byte_readings_are_scaled_and_other_units_are_not() {
        assert_eq!(reading(Some(14_913_462_272.0), "B"), "13.9 GB");
        assert_eq!(reading(Some(968_900_608.0), "B"), "924 MB");
        assert_eq!(reading(Some(170.0), "B/s"), "170 B/s");
        assert_eq!(reading(Some(0.0), "B/s"), "0 B/s");
        assert_eq!(reading(Some(3_500_000.0), "B/s"), "3.3 MB/s");
        assert_eq!(reading(Some(2045.0), "MHz"), "2045 MHz");
        assert_eq!(reading(Some(41.0), "°C"), "41.0 °C");
        assert_eq!(reading(None, "B"), "-");
    }

    #[test]
    fn info_view_of_an_empty_inventory_is_empty() {
        assert_eq!(render_info(&SystemInfo::default()), "");
    }

    #[test]
    fn info_view_skips_empty_summary_fields() {
        let info = SystemInfo {
            summary: SystemSummary {
                computer: "DESK".into(),
                cpu: "Example CPU".into(),
                memory_bytes: 32 << 30,
                ..SystemSummary::default()
            },
            sections: vec![InfoSection {
                title: "Memory".into(),
                rows: vec![("Slot 1".into(), "16 GiB".into()), ("Slot 12".into(), "16 GiB".into())],
            }],
        };
        assert_eq!(
            render_info(&info),
            "Computer   DESK\n\
             Processor  Example CPU\n\
             Memory     32.0 GiB\n\
             \n\
             Memory\n\
             \x20 Slot 1   16 GiB\n\
             \x20 Slot 12  16 GiB\n"
        );
    }
}
