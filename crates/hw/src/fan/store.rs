//! The durable store of the BIOS registers a channel had before SysPilot
//! first wrote to it. Compiled only with the `fan-control` cargo feature.
//!
//! The controller saves the original before its first write and clears it
//! once the restore is confirmed. This file is what a restart after a crash
//! or a power cut finds: without it the only copy of the BIOS mode would die
//! with the process and the channel would stay in manual mode at whatever
//! duty it held (docs/research/fan-control.md, rule 5).
//!
//! One TOML file in a directory the caller names, holding one entry per
//! channel. Every change is written to a temporary file, flushed to disk and
//! renamed over the old one, so a power cut leaves the old file or the new
//! one and never half of either. A file that does not parse is an error and
//! is never overwritten: the controller then takes no control of a channel it
//! cannot be sure to give back.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::failsafe::{ControlSnapshot, OriginalStore, StoreError};

/// The file's name inside the store's directory.
pub const STORE_FILE_NAME: &str = "fan-originals.toml";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Records {
    /// Channel id to the `(register address, value)` pairs, in restore order.
    #[serde(default)]
    channel: BTreeMap<String, Vec<(u16, u8)>>,
}

/// An [`OriginalStore`] in a file.
#[derive(Debug, Clone)]
pub struct FileOriginalStore {
    path: PathBuf,
}

impl FileOriginalStore {
    /// A store that keeps its file in `directory`, which must exist. It is
    /// not created: the service creates its data directory with the owner
    /// and permissions the security model requires.
    pub fn in_directory(directory: &Path) -> Result<Self, StoreError> {
        if !directory.is_dir() {
            // `StoreError` already says what was expected: a record that can
            // be read and written.
            return Err(StoreError(format!(
                "the directory {} does not exist",
                directory.display()
            )));
        }
        Ok(Self { path: directory.join(STORE_FILE_NAME) })
    }

    fn failure(&self, what: &str, error: impl std::fmt::Display) -> StoreError {
        StoreError(format!("{what} {}: {error}", self.path.display()))
    }

    fn read(&self) -> Result<Records, StoreError> {
        match fs::read_to_string(&self.path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|error| self.failure("could not parse", error))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Records::default()),
            Err(error) => Err(self.failure("could not read", error)),
        }
    }

    fn write(&self, records: &Records) -> Result<(), StoreError> {
        if records.channel.is_empty() {
            // Nothing saved is the same as no file.
            return match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(self.failure("could not remove", error)),
            };
        }
        let text =
            toml::to_string(records).map_err(|error| self.failure("could not encode", error))?;
        let temporary = self.path.with_extension("toml.tmp");
        let written = (|| {
            let mut file = File::create(&temporary)?;
            file.write_all(text.as_bytes())?;
            // Durable before the rename: the rename must never expose a file
            // whose contents are still in the cache.
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &self.path)
        })();
        written.map_err(|error| {
            let _ = fs::remove_file(&temporary);
            self.failure("could not write", error)
        })
    }
}

impl OriginalStore for FileOriginalStore {
    fn load(&mut self, channel: &str) -> Result<Option<ControlSnapshot>, StoreError> {
        Ok(self.read()?.channel.remove(channel).map(|registers| ControlSnapshot { registers }))
    }

    fn save(&mut self, channel: &str, original: &ControlSnapshot) -> Result<(), StoreError> {
        let mut records = self.read()?;
        records.channel.insert(channel.to_owned(), original.registers.clone());
        self.write(&records)
    }

    fn clear(&mut self, channel: &str) -> Result<(), StoreError> {
        let mut records = self.read()?;
        if records.channel.remove(channel).is_none() {
            return Ok(());
        }
        self.write(&records)
    }

    fn channels(&mut self) -> Result<Vec<String>, StoreError> {
        Ok(self.read()?.channel.into_keys().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("syspilot-fan-store-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn store(&self) -> FileOriginalStore {
            FileOriginalStore::in_directory(&self.0).unwrap()
        }

        fn file(&self) -> PathBuf {
            self.0.join(STORE_FILE_NAME)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn snapshot(duty: u8, mode: u8) -> ControlSnapshot {
        ControlSnapshot { registers: vec![(0x109, duty), (0x102, mode)] }
    }

    #[test]
    fn an_original_survives_a_new_store_over_the_same_file_in_restore_order() {
        let scratch = Scratch::new("survives");
        scratch.store().save("pwm1", &snapshot(0x66, 0x45)).unwrap();
        // A restart builds a new store; nothing is held in memory.
        let loaded = scratch.store().load("pwm1").unwrap();
        assert_eq!(loaded, Some(snapshot(0x66, 0x45)));
    }

    #[test]
    fn nothing_saved_loads_as_none_and_creates_no_file() {
        let scratch = Scratch::new("absent");
        assert_eq!(scratch.store().load("pwm1").unwrap(), None);
        assert!(!scratch.file().exists());
    }

    #[test]
    fn channels_are_kept_apart_and_clearing_one_keeps_the_other() {
        let scratch = Scratch::new("apart");
        let mut store = scratch.store();
        store.save("pwm1", &snapshot(1, 2)).unwrap();
        store.save("pwm3", &snapshot(3, 4)).unwrap();
        store.clear("pwm1").unwrap();
        assert_eq!(store.load("pwm1").unwrap(), None);
        assert_eq!(store.load("pwm3").unwrap(), Some(snapshot(3, 4)));
    }

    #[test]
    fn clearing_the_last_original_removes_the_file_and_clearing_nothing_is_fine() {
        let scratch = Scratch::new("clear");
        let mut store = scratch.store();
        store.clear("pwm1").unwrap();
        store.save("pwm1", &snapshot(1, 2)).unwrap();
        assert!(scratch.file().exists());
        store.clear("pwm1").unwrap();
        assert!(!scratch.file().exists());
        store.clear("pwm1").unwrap();
    }

    #[test]
    fn saving_twice_replaces_the_record_and_leaves_no_temporary_file() {
        let scratch = Scratch::new("replace");
        let mut store = scratch.store();
        store.save("pwm1", &snapshot(1, 2)).unwrap();
        store.save("pwm1", &snapshot(5, 6)).unwrap();
        assert_eq!(store.load("pwm1").unwrap(), Some(snapshot(5, 6)));
        let names: Vec<_> = fs::read_dir(&scratch.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [STORE_FILE_NAME]);
    }

    #[test]
    fn a_file_that_does_not_parse_is_an_error_and_is_not_overwritten() {
        let scratch = Scratch::new("corrupt");
        fs::write(scratch.file(), "channel = 5 this is not toml").unwrap();
        let mut store = scratch.store();
        let load = store.load("pwm1").unwrap_err().to_string();
        assert!(load.contains("could not parse") && load.contains(STORE_FILE_NAME), "{load}");
        assert!(store.save("pwm1", &snapshot(1, 2)).is_err());
        assert!(store.clear("pwm1").is_err());
        assert_eq!(
            fs::read_to_string(scratch.file()).unwrap(),
            "channel = 5 this is not toml",
            "(original store): expected the unreadable file to be left for inspection"
        );
    }

    #[test]
    fn every_channel_with_a_record_is_listed_and_a_cleared_one_is_not() {
        let scratch = Scratch::new("listed");
        let mut store = scratch.store();
        assert_eq!(store.channels().unwrap(), Vec::<String>::new(), "no file, no channels");
        store.save("pwm3", &snapshot(3, 4)).unwrap();
        store.save("pwm1", &snapshot(1, 2)).unwrap();
        assert_eq!(store.channels().unwrap(), ["pwm1", "pwm3"]);
        store.clear("pwm1").unwrap();
        assert_eq!(store.channels().unwrap(), ["pwm3"]);
    }

    #[test]
    fn listing_a_file_that_does_not_parse_is_an_error() {
        let scratch = Scratch::new("list-corrupt");
        fs::write(scratch.file(), "channel = 5 this is not toml").unwrap();
        let error = scratch.store().channels().unwrap_err().to_string();
        assert!(error.contains("could not parse"), "{error}");
    }

    #[test]
    fn a_missing_directory_is_refused_and_named() {
        let absent = std::env::temp_dir().join("syspilot-fan-store-absent-directory");
        let error = FileOriginalStore::in_directory(&absent).unwrap_err().to_string();
        assert!(error.contains("does not exist") && error.contains("absent-directory"), "{error}");
    }
}
