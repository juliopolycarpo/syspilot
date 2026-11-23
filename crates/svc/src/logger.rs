//! Optional recording of sensor values to CSV.
//!
//! Rows are buffered in memory and written out once a minute, so logging
//! does not turn into a disk write per sample. Files rotate by size and old
//! ones are pruned.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use syspilot_proto::profile::LoggingConfig;
use syspilot_proto::sensor::SensorList;

/// Seconds between flushes of the in-memory buffer.
const FLUSH_INTERVAL_SECS: u64 = 60;
const BUFFER_BYTES: usize = 64 * 1024;
const FILE_PREFIX: &str = "sensors-";
const FILE_SUFFIX: &str = ".csv";

/// An open log file.
struct OpenLog {
    writer: BufWriter<File>,
    bytes: u64,
    /// Sensor IDs of the columns, in order.
    columns: Vec<u32>,
    /// Sensor set the columns belong to.
    generation: u32,
    last_flush_unix: u64,
}

/// Writes sensor rows according to a [`LoggingConfig`].
pub struct SensorLogger {
    config: LoggingConfig,
    directory: PathBuf,
    log: Option<OpenLog>,
    last_row_unix: Option<u64>,
}

/// Converts seconds since the Unix epoch to a UTC calendar date and time.
/// (Howard Hinnant's `civil_from_days`.)
fn civil(unix_secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (unix_secs / 86_400) as i64;
    let seconds = (unix_secs % 86_400) as u32;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = (if month_index < 10 { month_index + 3 } else { month_index - 9 }) as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day, seconds / 3600, seconds / 60 % 60, seconds % 60)
}

/// ISO 8601 UTC timestamp, e.g. `2026-11-23T14:39:05Z`.
pub fn iso_timestamp(unix_secs: u64) -> String {
    let (year, month, day, hour, minute, second) = civil(unix_secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn file_name(unix_secs: u64) -> String {
    let (year, month, day, hour, minute, second) = civil(unix_secs);
    format!("{FILE_PREFIX}{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}{FILE_SUFFIX}")
}

/// Formats a value compactly: integers without a fraction, everything else
/// with up to three decimals.
fn format_value(value: f64, out: &mut String) {
    use std::fmt::Write as _;
    if !value.is_finite() {
        return;
    }
    let _ = write!(out, "{value:.3}");
    if out.contains('.') {
        let trimmed = out.trim_end_matches('0').trim_end_matches('.').len();
        out.truncate(trimmed);
    }
}

/// Quotes a CSV field when it contains a separator, quote or line break.
fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

impl SensorLogger {
    /// A logger writing to `directory`.
    pub fn new(directory: PathBuf, config: LoggingConfig) -> Self {
        Self { config, directory, log: None, last_row_unix: None }
    }

    /// Replaces the settings. The current file is closed so that a changed
    /// sensor selection starts a file with a matching header.
    pub fn configure(&mut self, config: LoggingConfig) {
        if config != self.config {
            self.close();
            self.config = config;
        }
    }

    /// Whether logging is on. While it is, the logger counts as a
    /// subscriber: the service keeps sampling.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Offers the latest sample. Writes a row when one is due.
    pub fn record(
        &mut self,
        unix_secs: u64,
        sensors: &SensorList,
        values: &[Option<f64>],
    ) -> io::Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let due = self
            .last_row_unix
            .is_none_or(|last| unix_secs >= last + u64::from(self.config.interval_secs));
        if !due {
            return Ok(());
        }
        let limit = u64::from(self.config.max_file_kib) * 1024;
        let reusable = self
            .log
            .as_ref()
            .is_some_and(|log| log.generation == sensors.generation && log.bytes < limit);
        if !reusable {
            self.close();
            self.log = Some(self.open(unix_secs, sensors)?);
        }
        let Some(log) = self.log.as_mut() else { return Ok(()) };

        let mut row = iso_timestamp(unix_secs);
        for id in &log.columns {
            row.push(',');
            if let Some(Some(value)) = values.get(*id as usize) {
                let mut field = String::new();
                format_value(*value, &mut field);
                row.push_str(&field);
            }
        }
        row.push('\n');
        log.writer.write_all(row.as_bytes())?;
        log.bytes += row.len() as u64;
        self.last_row_unix = Some(unix_secs);
        if unix_secs >= log.last_flush_unix + FLUSH_INTERVAL_SECS {
            log.writer.flush()?;
            log.last_flush_unix = unix_secs;
        }
        Ok(())
    }

    /// Flushes and closes the current file.
    pub fn close(&mut self) {
        if let Some(mut log) = self.log.take() {
            let _ = log.writer.flush();
        }
    }

    fn open(&self, unix_secs: u64, sensors: &SensorList) -> io::Result<OpenLog> {
        let selected: Vec<_> = sensors
            .sensors
            .iter()
            .filter(|sensor| {
                self.config.sensors.is_empty() || self.config.sensors.contains(&sensor.key)
            })
            .collect();
        let path = self.directory.join(file_name(unix_secs));
        let mut writer = BufWriter::with_capacity(BUFFER_BYTES, File::create(&path)?);
        let mut header = String::from("time");
        for sensor in &selected {
            header.push(',');
            header.push_str(&csv_field(&sensor.key));
        }
        header.push('\n');
        writer.write_all(header.as_bytes())?;
        prune(&self.directory, self.config.keep_files as usize);
        Ok(OpenLog {
            writer,
            bytes: header.len() as u64,
            columns: selected.iter().map(|sensor| sensor.id).collect(),
            generation: sensors.generation,
            last_flush_unix: unix_secs,
        })
    }
}

impl Drop for SensorLogger {
    fn drop(&mut self) {
        self.close();
    }
}

/// Deletes the oldest log files so that at most `keep` remain. File names
/// sort chronologically.
fn prune(directory: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(directory) else { return };
    let mut logs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX))
        })
        .collect();
    logs.sort();
    let excess = logs.len().saturating_sub(keep);
    for path in &logs[..excess] {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_proto::sensor::{DeviceClass, DeviceInfo, SensorInfo, SensorKind};

    /// 2026-11-23T14:39:05Z.
    const T0: u64 = 1_795_444_745;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("SysPilot-Test-logger-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.0.join(name)).unwrap()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sensors(generation: u32, keys: &[&str]) -> SensorList {
        SensorList {
            generation,
            devices: vec![DeviceInfo { name: "Fake".into(), class: DeviceClass::Cpu }],
            sensors: keys
                .iter()
                .enumerate()
                .map(|(id, key)| SensorInfo {
                    id: id as u32,
                    device: 0,
                    key: (*key).into(),
                    label: (*key).into(),
                    kind: SensorKind::Temperature,
                    driver: false,
                })
                .collect(),
        }
    }

    fn enabled(interval_secs: u32) -> LoggingConfig {
        LoggingConfig { enabled: true, interval_secs, ..LoggingConfig::default() }
    }

    #[test]
    fn timestamps_are_utc_iso_8601() {
        assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso_timestamp(T0), "2026-11-23T14:39:05Z");
        assert_eq!(file_name(T0), "sensors-20261123-143905.csv");
    }

    #[test]
    fn values_are_written_compactly() {
        let format = |value: f64| {
            let mut out = String::new();
            format_value(value, &mut out);
            out
        };
        assert_eq!(format(42.0), "42");
        assert_eq!(format(41.5), "41.5");
        assert_eq!(format(0.1234567), "0.123");
        assert_eq!(format(1200.0), "1200");
        assert_eq!(format(f64::NAN), "");
    }

    #[test]
    fn fields_with_separators_are_quoted() {
        assert_eq!(csv_field("cpu.temp"), "cpu.temp");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn disabled_logger_writes_nothing() {
        let dir = TempDir::new("disabled");
        let mut logger = SensorLogger::new(dir.0.clone(), LoggingConfig::default());
        logger.record(T0, &sensors(1, &["a"]), &[Some(1.0)]).unwrap();
        assert!(dir.files().is_empty());
        assert!(!logger.is_enabled());
    }

    #[test]
    fn rows_follow_the_interval_and_missing_readings_stay_empty() {
        let dir = TempDir::new("rows");
        let mut logger = SensorLogger::new(dir.0.clone(), enabled(5));
        let list = sensors(1, &["cpu.temp", "gpu.temp"]);
        for second in 0..11 {
            let values = [Some(40.0 + second as f64), (second != 5).then_some(60.5)];
            logger.record(T0 + second, &list, &values).unwrap();
        }
        logger.close();
        assert_eq!(dir.files(), ["sensors-20261123-143905.csv"]);
        assert_eq!(
            dir.read("sensors-20261123-143905.csv"),
            "time,cpu.temp,gpu.temp\n\
             2026-11-23T14:39:05Z,40,60.5\n\
             2026-11-23T14:39:10Z,45,\n\
             2026-11-23T14:39:15Z,50,60.5\n"
        );
    }

    #[test]
    fn only_selected_sensors_are_logged() {
        let dir = TempDir::new("selection");
        let config = LoggingConfig { sensors: vec!["b".into()], ..enabled(1) };
        let mut logger = SensorLogger::new(dir.0.clone(), config);
        logger
            .record(T0, &sensors(1, &["a", "b", "c"]), &[Some(1.0), Some(2.0), Some(3.0)])
            .unwrap();
        logger.close();
        assert_eq!(dir.read(&dir.files()[0]), "time,b\n2026-11-23T14:39:05Z,2\n");
    }

    #[test]
    fn a_new_sensor_set_starts_a_new_file() {
        let dir = TempDir::new("generation");
        let mut logger = SensorLogger::new(dir.0.clone(), enabled(1));
        logger.record(T0, &sensors(1, &["a"]), &[Some(1.0)]).unwrap();
        logger.record(T0 + 1, &sensors(2, &["a", "b"]), &[Some(1.0), Some(2.0)]).unwrap();
        logger.close();
        let files = dir.files();
        assert_eq!(files.len(), 2);
        assert!(dir.read(&files[1]).starts_with("time,a,b\n"));
    }

    #[test]
    fn files_rotate_by_size_and_old_ones_are_pruned() {
        let dir = TempDir::new("rotation");
        // The smallest allowed size, and room for two files.
        let config = LoggingConfig { max_file_kib: 64, keep_files: 2, ..enabled(1) };
        let mut logger = SensorLogger::new(dir.0.clone(), config);
        let keys: Vec<String> = (0..200).map(|index| format!("sensor{index}.temp")).collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let list = sensors(1, &keys);
        let values = vec![Some(123.456); 200];
        // Each row is about 1.6 KiB, so 64 KiB fills in about 40 rows.
        for second in 0..200 {
            logger.record(T0 + second, &list, &values).unwrap();
        }
        logger.close();
        let files = dir.files();
        assert_eq!(files.len(), 2, "expected the newest two files | received: {files:?}");
        for name in &files {
            let size = fs::metadata(dir.0.join(name)).unwrap().len();
            assert!(size < 70 * 1024, "{name} grew to {size} bytes");
        }
    }

    #[test]
    fn reconfiguring_closes_the_file_and_applies_the_new_settings() {
        let dir = TempDir::new("reconfigure");
        let mut logger = SensorLogger::new(dir.0.clone(), enabled(1));
        logger.record(T0, &sensors(1, &["a"]), &[Some(1.0)]).unwrap();
        logger.configure(LoggingConfig::default());
        assert!(!logger.is_enabled());
        logger.record(T0 + 1, &sensors(1, &["a"]), &[Some(1.0)]).unwrap();
        assert_eq!(dir.read(&dir.files()[0]).lines().count(), 2, "header and one row, flushed");
    }
}
