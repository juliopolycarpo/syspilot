//! `cargo xtask fetch-modules`: downloads the pinned PawnIO.Modules release
//! and places the modules SysPilot uses next to their licence text.
//!
//! The blobs are LGPL-2.1 files signed by the PawnIO author. They are
//! never committed and never embedded in an executable, so the service
//! reads them from a directory at run time.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sha256::sha256_file_hex;

/// One file taken out of the release archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedFile {
    /// Name of the member inside the archive.
    pub member: &'static str,
    /// Name it gets in the destination directory.
    pub target: &'static str,
    /// Expected SHA-256, lowercase hex.
    pub sha256: &'static str,
}

/// A release archive and the files used from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pins {
    /// Download URL of the zip.
    pub archive_url: &'static str,
    /// Expected SHA-256 of the zip, lowercase hex.
    pub archive_sha256: &'static str,
    /// Files to extract.
    pub files: &'static [PinnedFile],
}

/// PawnIO.Modules 0.2.12 (see `docs/research/pawnio.md`).
pub const PINNED_RELEASE: Pins = Pins {
    archive_url: "https://github.com/namazso/PawnIO.Modules/releases/download/0.2.12/release_0_2_12.zip",
    archive_sha256: "09698897ce19b92f1043a2e322cfcf0b1fe823e0b576971b178a65244fb0af9d",
    files: &[
        PinnedFile {
            member: "IntelMSR.bin",
            target: "IntelMSR.bin",
            sha256: "30a5a302283de1e080dfee99e686eca91d7e53e911095cb91a320fb961f1ce12",
        },
        PinnedFile {
            member: "AMDFamily17.bin",
            target: "AMDFamily17.bin",
            sha256: "dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91",
        },
        PinnedFile {
            member: "LpcIO.bin",
            target: "LpcIO.bin",
            sha256: "b3896a1cab0d808fca31fe2ebcae045d59dac690da87b17c858bb8da357eb45e",
        },
        PinnedFile {
            member: "SmbusI801.bin",
            target: "SmbusI801.bin",
            sha256: "a0f7d066e7efda28c0e754c1f52dbd8dc280d388ba8575b514a80cb67490530c",
        },
        PinnedFile {
            member: "SmbusPIIX4.bin",
            target: "SmbusPIIX4.bin",
            sha256: "91f9b4b1c39e3d399ce48477a89d8f6bd3e58a2241064a4daec3a1513dff56e5",
        },
        PinnedFile {
            member: "COPYING",
            target: "LICENSE-LGPL-2.1.txt",
            sha256: "1e7e6bae5a5bde32f1ae5a7c37a082d1ab03cf89354f7f936ac40be9e39a6531",
        },
    ],
};

/// What a run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every file was already present with the pinned hash.
    UpToDate,
    /// The archive was downloaded and these targets were written.
    Fetched(Vec<&'static str>),
}

/// Downloads and unpacks; replaced in tests by a fake.
pub trait Transport {
    /// Downloads `url` to the file `destination`.
    fn download(&self, url: &str, destination: &Path) -> Result<(), String>;
    /// Extracts the named `members` of `archive` into the directory `into`.
    fn extract(&self, archive: &Path, members: &[&str], into: &Path) -> Result<(), String>;
}

/// Uses `curl.exe` and `tar.exe`, both part of Windows 10 and later.
pub struct WindowsTools;

impl WindowsTools {
    /// Full path of a System32 tool. A bare name would also search the
    /// current directory, where a downloaded file could shadow it.
    fn system_tool(name: &str) -> Result<PathBuf, String> {
        let root = std::env::var_os("SystemRoot").ok_or(
            "(fetch-modules): expected the SystemRoot environment variable | received: unset",
        )?;
        let path = Path::new(&root).join("System32").join(name);
        if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "(fetch-modules): expected {name} in System32 | received: {} does not exist",
                path.display()
            ))
        }
    }

    fn run(mut command: Command, what: &str) -> Result<(), String> {
        let output = command
            .output()
            .map_err(|error| format!("(fetch-modules): {what} could not start: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        Err(format!(
            "(fetch-modules): expected {what} to succeed | received: {} {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

impl Transport for WindowsTools {
    fn download(&self, url: &str, destination: &Path) -> Result<(), String> {
        let mut command = Command::new(Self::system_tool("curl.exe")?);
        command
            .args(["--fail", "--silent", "--show-error", "--location", "--retry", "3"])
            .args(["--proto", "=https", "--output"])
            .arg(destination)
            .arg(url);
        Self::run(command, &format!("download of {url}"))
    }

    fn extract(&self, archive: &Path, members: &[&str], into: &Path) -> Result<(), String> {
        let mut command = Command::new(Self::system_tool("tar.exe")?);
        command.arg("-xf").arg(archive).arg("-C").arg(into).args(members);
        Self::run(command, "tar extraction")
    }
}

/// A fresh, empty scratch directory removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    /// `create_dir` (not `create_dir_all`) so a pre-existing directory,
    /// which someone else could have prepared, is an error.
    fn create() -> Result<Self, String> {
        let nanos =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let path = std::env::temp_dir()
            .join(format!("syspilot-fetch-modules-{}-{nanos}", std::process::id()));
        fs::create_dir(&path).map_err(|error| {
            format!("(fetch-modules): cannot create {}: {error}", path.display())
        })?;
        Ok(Self(path))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Compares two hashes, naming both on mismatch.
pub fn check_hash(what: &str, expected: &str, received: &str) -> Result<(), String> {
    if expected.eq_ignore_ascii_case(received) {
        Ok(())
    } else {
        Err(format!("({what}): expected sha256 {expected} | received: {received}"))
    }
}

/// Whether every target in `pins` already exists in `directory` with the
/// pinned hash.
pub fn is_up_to_date(pins: &Pins, directory: &Path) -> bool {
    pins.files.iter().all(|file| {
        sha256_file_hex(&directory.join(file.target))
            .is_ok_and(|hash| check_hash(file.target, file.sha256, &hash).is_ok())
    })
}

/// Fetches and verifies the files of `pins` into `directory`.
///
/// Does nothing when `force` is false and every file is already correct.
/// The archive hash is checked before extraction and each extracted file's
/// hash before it is moved into place, so a tampered archive or a changed
/// member never reaches `directory`.
pub fn fetch_modules(
    pins: &Pins,
    directory: &Path,
    force: bool,
    transport: &dyn Transport,
) -> Result<Outcome, String> {
    if !force && is_up_to_date(pins, directory) {
        return Ok(Outcome::UpToDate);
    }

    let scratch = ScratchDir::create()?;
    let archive = scratch.0.join("release.zip");
    transport.download(pins.archive_url, &archive)?;
    let archive_hash = sha256_file_hex(&archive)
        .map_err(|error| format!("(fetch-modules): cannot read {}: {error}", archive.display()))?;
    check_hash("release archive", pins.archive_sha256, &archive_hash)?;

    let extracted = scratch.0.join("extracted");
    fs::create_dir(&extracted).map_err(|error| {
        format!("(fetch-modules): cannot create {}: {error}", extracted.display())
    })?;
    let members: Vec<&str> = pins.files.iter().map(|file| file.member).collect();
    transport.extract(&archive, &members, &extracted)?;

    for file in pins.files {
        let source = extracted.join(file.member);
        let hash = sha256_file_hex(&source).map_err(|error| {
            format!("(fetch-modules): {} missing from the archive: {error}", file.member)
        })?;
        check_hash(file.member, file.sha256, &hash)?;
    }

    fs::create_dir_all(directory).map_err(|error| {
        format!("(fetch-modules): cannot create {}: {error}", directory.display())
    })?;
    for file in pins.files {
        // Copy beside the destination and rename, so an interrupted run
        // never leaves a truncated module under its final name.
        let part = directory.join(format!("{}.part", file.target));
        fs::copy(extracted.join(file.member), &part).map_err(|error| {
            format!("(fetch-modules): cannot write {}: {error}", part.display())
        })?;
        fs::rename(&part, directory.join(file.target))
            .map_err(|error| format!("(fetch-modules): cannot place {}: {error}", file.target))?;
    }
    Ok(Outcome::Fetched(pins.files.iter().map(|file| file.target).collect()))
}

/// Builds pins for test data: hashes computed from the given contents.
#[cfg(test)]
pub fn pins_for_test(archive: &[u8], files: &[(&'static str, &'static str, &[u8])]) -> Pins {
    use crate::sha256::sha256_hex;
    let files: Vec<PinnedFile> = files
        .iter()
        .map(|(member, target, content)| PinnedFile {
            member,
            target,
            sha256: Box::leak(sha256_hex(content).into_boxed_str()),
        })
        .collect();
    Pins {
        archive_url: "https://example.invalid/release.zip",
        archive_sha256: Box::leak(sha256_hex(archive).into_boxed_str()),
        files: Box::leak(files.into_boxed_slice()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256::sha256_hex;
    use std::cell::Cell;

    /// Serves a fixed archive and fixed member contents, and counts calls.
    struct FakeTransport {
        archive: Vec<u8>,
        members: Vec<(&'static str, Vec<u8>)>,
        downloads: Cell<u32>,
    }

    impl Transport for FakeTransport {
        fn download(&self, _url: &str, destination: &Path) -> Result<(), String> {
            self.downloads.set(self.downloads.get() + 1);
            fs::write(destination, &self.archive).map_err(|e| e.to_string())
        }

        fn extract(&self, _archive: &Path, members: &[&str], into: &Path) -> Result<(), String> {
            for (name, content) in &self.members {
                if members.contains(name) {
                    fs::write(into.join(name), content).map_err(|e| e.to_string())?;
                }
            }
            Ok(())
        }
    }

    struct Fixture {
        pins: Pins,
        transport: FakeTransport,
        directory: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let directory =
                std::env::temp_dir().join(format!("xtask-modules-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&directory);
            let archive = b"pretend zip".to_vec();
            let pins = pins_for_test(
                &archive,
                &[("A.bin", "A.bin", b"module a"), ("COPYING", "LICENSE.txt", b"licence text")],
            );
            let transport = FakeTransport {
                archive,
                members: vec![
                    ("A.bin", b"module a".to_vec()),
                    ("COPYING", b"licence text".to_vec()),
                ],
                downloads: Cell::new(0),
            };
            Self { pins, transport, directory }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn check_hash_names_expected_and_received() {
        let error = check_hash("IntelMSR.bin", "aa", "bb").unwrap_err();
        assert_eq!(error, "(IntelMSR.bin): expected sha256 aa | received: bb");
        assert!(check_hash("x", "AB", "ab").is_ok(), "hex case is irrelevant");
    }

    #[test]
    fn fetch_places_files_under_their_target_names() {
        let fx = Fixture::new("place");
        let outcome = fetch_modules(&fx.pins, &fx.directory, false, &fx.transport).unwrap();
        assert_eq!(outcome, Outcome::Fetched(vec!["A.bin", "LICENSE.txt"]));
        assert_eq!(fs::read(fx.directory.join("A.bin")).unwrap(), b"module a");
        assert_eq!(fs::read(fx.directory.join("LICENSE.txt")).unwrap(), b"licence text");
        assert!(!fx.directory.join("A.bin.part").exists(), "no temporary file is left");
    }

    #[test]
    fn second_run_downloads_nothing() {
        let fx = Fixture::new("idempotent");
        fetch_modules(&fx.pins, &fx.directory, false, &fx.transport).unwrap();
        let again = fetch_modules(&fx.pins, &fx.directory, false, &fx.transport).unwrap();
        assert_eq!(again, Outcome::UpToDate);
        assert_eq!(fx.transport.downloads.get(), 1);
    }

    #[test]
    fn force_downloads_again() {
        let fx = Fixture::new("force");
        fetch_modules(&fx.pins, &fx.directory, false, &fx.transport).unwrap();
        fetch_modules(&fx.pins, &fx.directory, true, &fx.transport).unwrap();
        assert_eq!(fx.transport.downloads.get(), 2);
    }

    #[test]
    fn a_modified_local_file_is_replaced() {
        let fx = Fixture::new("repair");
        fetch_modules(&fx.pins, &fx.directory, false, &fx.transport).unwrap();
        fs::write(fx.directory.join("A.bin"), b"tampered").unwrap();
        let outcome = fetch_modules(&fx.pins, &fx.directory, false, &fx.transport).unwrap();
        assert!(matches!(outcome, Outcome::Fetched(_)));
        assert_eq!(fs::read(fx.directory.join("A.bin")).unwrap(), b"module a");
    }

    #[test]
    fn a_wrong_archive_is_refused_before_anything_is_placed() {
        let mut fx = Fixture::new("bad-archive");
        fx.transport.archive = b"substituted archive".to_vec();
        let error = fetch_modules(&fx.pins, &fx.directory, false, &fx.transport).unwrap_err();
        assert!(error.starts_with("(release archive): expected sha256 "), "{error}");
        assert!(error.contains(&sha256_hex(b"substituted archive")), "{error}");
        assert!(!fx.directory.exists(), "nothing may be written for a bad archive");
    }

    #[test]
    fn a_wrong_member_is_refused_before_anything_is_placed() {
        let mut fx = Fixture::new("bad-member");
        fx.transport.members[0].1 = b"module a, patched".to_vec();
        let error = fetch_modules(&fx.pins, &fx.directory, false, &fx.transport).unwrap_err();
        assert!(error.starts_with("(A.bin): expected sha256 "), "{error}");
        assert!(!fx.directory.exists(), "nothing may be written for a bad member");
    }

    #[test]
    fn a_member_missing_from_the_archive_is_named() {
        let mut fx = Fixture::new("missing-member");
        fx.transport.members.remove(1);
        let error = fetch_modules(&fx.pins, &fx.directory, false, &fx.transport).unwrap_err();
        assert!(error.contains("COPYING missing from the archive"), "{error}");
    }

    #[test]
    fn pinned_release_lists_the_modules_syspilot_uses() {
        let targets: Vec<_> = PINNED_RELEASE.files.iter().map(|f| f.target).collect();
        assert_eq!(
            targets,
            [
                "IntelMSR.bin",
                "AMDFamily17.bin",
                "LpcIO.bin",
                "SmbusI801.bin",
                "SmbusPIIX4.bin",
                "LICENSE-LGPL-2.1.txt"
            ]
        );
        for file in PINNED_RELEASE.files {
            assert_eq!(file.sha256.len(), 64, "{}: expected a 64-digit hash", file.target);
            assert!(file.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        }
    }
}
