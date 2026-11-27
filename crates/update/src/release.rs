//! Finding a new release, and getting it onto the machine verified.
//!
//! Everything outside the process sits behind two traits, so the whole path
//! runs against fakes in tests: [`Releases`] is where releases are
//! published, [`Packages`] is Windows Installer.
//!
//! The order of [`install`] is the security of the updater. The package is
//! downloaded into a directory only LocalSystem and administrators can
//! write, its signature is verified against a key compiled into this
//! executable, and only then is anything in it read or run. The release tag
//! and every byte that arrives before that point are hints, never facts.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use syspilot_proto::update::{INSTALL_LOG, UpdaterReport};

use crate::sshsig::{self, FileSignature, Key, SIGNATURE_FILE_MAX};
use crate::version::Version;

/// Largest package downloaded. A release is about 30 MiB.
const PACKAGE_MAX: u64 = 512 * 1024 * 1024;
/// What a download is called until it has been verified.
const PARTIAL_SUFFIX: &str = ".part";

/// File name of the package of `version` in a release.
pub fn package_name(version: Version) -> String {
    format!("SysPilot-{version}-x64.msi")
}

/// File name of the signature of the package of `version`.
pub fn signature_name(version: Version) -> String {
    format!("{}.sig", package_name(version))
}

/// Where releases are published.
pub trait Releases {
    /// The tag of the newest published release, e.g. `v0.2.1`.
    fn latest_tag(&mut self) -> Result<String, String>;

    /// Writes the file `asset` of the release `tag` to `sink`. `Ok(false)`
    /// when the release has no such file.
    fn fetch(&mut self, tag: &str, asset: &str, sink: &mut dyn Write) -> Result<bool, String>;
}

/// Windows Installer, as far as the updater needs it.
pub trait Packages {
    /// The `ProductVersion` a package declares.
    fn product_version(&self, package: &Path) -> Result<String, String>;

    /// Starts installing `package` without any window and returns at once.
    /// The installation replaces this executable, so nothing waits for it;
    /// what happened is in `log`.
    fn install(&self, package: &Path, log: &Path) -> Result<(), String>;
}

/// A sink that takes at most `left` bytes, so that nothing from the network
/// can fill the disk or the memory.
struct Limited<W> {
    inner: W,
    left: u64,
}

impl<W: Write> Write for Limited<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.left {
            return Err(io::Error::other("the download is larger than a release can be"));
        }
        self.left -= bytes.len() as u64;
        self.inner.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The newest release if it is newer than `current` and carries a signature
/// of the right kind from a trusted key. A release that is published but not
/// signed yet is not offered: `None`, as if it were not there.
fn newer_signed_release(
    releases: &mut dyn Releases,
    trusted: &[Key],
    current: Version,
) -> Result<Option<(Version, FileSignature)>, String> {
    let tag = releases.latest_tag()?;
    let version = Version::from_tag(&tag).ok_or_else(|| {
        format!("(release tag): expected v<major>.<minor>.<patch> | received: {tag:?}")
    })?;
    if version <= current {
        return Ok(None);
    }
    let mut text = Limited { inner: Vec::new(), left: SIGNATURE_FILE_MAX };
    if !releases.fetch(&version.tag(), &signature_name(version), &mut text)? {
        return Ok(None);
    }
    let signature = FileSignature::parse(&String::from_utf8_lossy(&text.inner))?;
    signature.check_shape(trusted)?;
    Ok(Some((version, signature)))
}

/// Looks for a release newer than `current`.
pub fn check(
    releases: &mut dyn Releases,
    trusted: &[Key],
    current: Version,
) -> Result<UpdaterReport, String> {
    Ok(match newer_signed_release(releases, trusted, current)? {
        Some((version, _)) => UpdaterReport::Available(version.to_string()),
        None => UpdaterReport::Current,
    })
}

/// Removes what earlier downloads left in `directory`: packages and partial
/// downloads. The log of the last installation stays until the next one.
fn clear_downloads(directory: &Path) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        if path.is_file() && (name.ends_with(".msi") || name.ends_with(PARTIAL_SUFFIX)) {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Downloads the package of `version` into `directory` and returns it once
/// `signature` has been verified over it. A package that does not verify is
/// deleted.
fn download_verified(
    releases: &mut dyn Releases,
    trusted: &[Key],
    version: Version,
    signature: &FileSignature,
    directory: &Path,
) -> Result<PathBuf, String> {
    let name = package_name(version);
    let package = directory.join(&name);
    let partial = directory.join(format!("{name}{PARTIAL_SUFFIX}"));
    let failure =
        |action: &str, error: io::Error| format!("{action} {}: {error}", partial.display());

    let file = File::create(&partial).map_err(|error| failure("create", error))?;
    let mut sink = Limited { inner: file, left: PACKAGE_MAX };
    let found = releases.fetch(&version.tag(), &name, &mut sink);
    let synced = sink.inner.sync_all();
    drop(sink);
    let verified = (|| {
        if !found? {
            return Err(format!(
                "(release {version}): expected the file {name} | received: no such file"
            ));
        }
        synced.map_err(|error| failure("write", error))?;
        let content = File::open(&partial).map_err(|error| failure("open", error))?;
        let digest = sshsig::digest_of(content).map_err(|error| failure("read", error))?;
        signature.verify(trusted, &digest)
    })();
    if let Err(problem) = verified {
        let _ = fs::remove_file(&partial);
        return Err(problem);
    }
    fs::rename(&partial, &package).map_err(|error| failure("rename", error))?;
    Ok(package)
}

/// Installs the newest release if it is newer than `current`: downloads it
/// into `directory`, verifies it and hands it to Windows Installer.
pub fn install(
    releases: &mut dyn Releases,
    packages: &dyn Packages,
    trusted: &[Key],
    current: Version,
    directory: &Path,
) -> Result<UpdaterReport, String> {
    let Some((version, signature)) = newer_signed_release(releases, trusted, current)? else {
        return Ok(UpdaterReport::Current);
    };
    clear_downloads(directory)
        .map_err(|error| format!("clear {}: {error}", directory.display()))?;
    let package = download_verified(releases, trusted, version, &signature, directory)?;

    // The tag said which version this is; the signed package has the last
    // word. A signed package of another version under this tag is a replay
    // of an old release, and Windows Installer would refuse it later with
    // less to say.
    let declared = packages.product_version(&package)?;
    if Version::parse(&declared) != Some(version) {
        let _ = fs::remove_file(&package);
        return Err(format!("(package version): expected {version} | received: {declared:?}"));
    }
    packages.install(&package, &directory.join(INSTALL_LOG))?;
    Ok(UpdaterReport::Installing(version.to_string()))
}

/// Verifies `signature_file` over `file` with the keys of this build: what
/// the updater will do with a release, for whoever is about to publish one.
pub fn verify_file(file: &Path, signature_file: &Path, trusted: &[Key]) -> Result<(), String> {
    let text = fs::read_to_string(signature_file)
        .map_err(|error| format!("read {}: {error}", signature_file.display()))?;
    let content = File::open(file).map_err(|error| format!("open {}: {error}", file.display()))?;
    let digest =
        sshsig::digest_of(content).map_err(|error| format!("read {}: {error}", file.display()))?;
    FileSignature::parse(&text)?.verify(trusted, &digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshsig::fixtures::TestSigner;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A release host with one newest tag and the files put into it.
    #[derive(Default)]
    struct FakeReleases {
        latest: String,
        assets: HashMap<(String, String), Vec<u8>>,
        /// Every asset asked for, in order.
        fetched: Vec<String>,
    }

    impl FakeReleases {
        fn with_latest(tag: &str) -> Self {
            Self { latest: tag.to_owned(), ..Self::default() }
        }

        fn publish(&mut self, tag: &str, asset: &str, content: &[u8]) {
            self.assets.insert((tag.to_owned(), asset.to_owned()), content.to_vec());
        }

        /// A release with a package signed by `signer`.
        fn signed(tag: &str, package: &[u8], signer: &TestSigner) -> Self {
            let version = Version::from_tag(tag).unwrap();
            let mut releases = Self::with_latest(tag);
            releases.publish(tag, &package_name(version), package);
            releases.publish(tag, &signature_name(version), signer.sign(package).as_bytes());
            releases
        }
    }

    impl Releases for FakeReleases {
        fn latest_tag(&mut self) -> Result<String, String> {
            Ok(self.latest.clone())
        }

        fn fetch(&mut self, tag: &str, asset: &str, sink: &mut dyn Write) -> Result<bool, String> {
            self.fetched.push(asset.to_owned());
            match self.assets.get(&(tag.to_owned(), asset.to_owned())) {
                None => Ok(false),
                Some(content) => {
                    // In pieces, as a network delivers.
                    for piece in content.chunks(7) {
                        sink.write_all(piece).map_err(|error| error.to_string())?;
                    }
                    Ok(true)
                }
            }
        }
    }

    /// An installer that reports a fixed version and records what it was
    /// asked to install.
    struct FakePackages {
        version: String,
        installed: RefCell<Vec<(PathBuf, PathBuf)>>,
    }

    impl FakePackages {
        fn declaring(version: &str) -> Self {
            Self { version: version.to_owned(), installed: RefCell::default() }
        }
    }

    impl Packages for FakePackages {
        fn product_version(&self, _package: &Path) -> Result<String, String> {
            Ok(self.version.clone())
        }

        fn install(&self, package: &Path, log: &Path) -> Result<(), String> {
            self.installed.borrow_mut().push((package.to_owned(), log.to_owned()));
            Ok(())
        }
    }

    /// A directory under the system temp directory, removed on drop.
    struct ScratchDirectory(PathBuf);

    impl ScratchDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("SysPilot-Test.update-{name}-{}", std::process::id()));
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
    }

    impl Drop for ScratchDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    const PACKAGE: &[u8] = b"not really a Windows Installer package";

    #[test]
    fn a_newer_signed_release_is_available() {
        let signer = TestSigner::new(1);
        let mut releases = FakeReleases::signed("v0.2.1", PACKAGE, &signer);
        let report = check(&mut releases, &[signer.key()], version("0.2.0"));
        assert_eq!(report, Ok(UpdaterReport::Available("0.2.1".into())));
        assert_eq!(
            releases.fetched,
            ["SysPilot-0.2.1-x64.msi.sig"],
            "a check downloads no package"
        );
    }

    #[test]
    fn the_same_or_an_older_release_is_not_an_update() {
        let signer = TestSigner::new(1);
        for current in ["0.2.1", "0.3.0", "1.0.0"] {
            let mut releases = FakeReleases::signed("v0.2.1", PACKAGE, &signer);
            let report = check(&mut releases, &[signer.key()], version(current));
            assert_eq!(report, Ok(UpdaterReport::Current), "{current}");
            assert!(releases.fetched.is_empty(), "nothing is fetched for {current}");
        }
    }

    #[test]
    fn a_release_that_is_not_signed_yet_is_not_offered() {
        let signer = TestSigner::new(1);
        let mut releases = FakeReleases::with_latest("v0.2.1");
        releases.publish("v0.2.1", "SysPilot-0.2.1-x64.msi", PACKAGE);
        assert_eq!(
            check(&mut releases, &[signer.key()], version("0.2.0")),
            Ok(UpdaterReport::Current)
        );
        let scratch = ScratchDirectory::new("unsigned");
        let packages = FakePackages::declaring("0.2.1");
        let report =
            install(&mut releases, &packages, &[signer.key()], version("0.2.0"), &scratch.0);
        assert_eq!(report, Ok(UpdaterReport::Current));
        assert!(packages.installed.borrow().is_empty() && scratch.files().is_empty());
    }

    #[test]
    fn a_release_signed_by_someone_else_is_an_error_at_the_check_already() {
        let (signer, stranger) = (TestSigner::new(1), TestSigner::new(2));
        let mut releases = FakeReleases::signed("v0.2.1", PACKAGE, &stranger);
        let problem = check(&mut releases, &[signer.key()], version("0.2.0")).unwrap_err();
        assert!(problem.contains("expected a key SysPilot releases are signed with"), "{problem}");
    }

    #[test]
    fn a_tag_that_is_not_a_version_is_refused_before_anything_is_fetched() {
        let signer = TestSigner::new(1);
        for tag in ["latest", "v0.2", "v0.2.1/../../x", ""] {
            let mut releases = FakeReleases::with_latest(tag);
            let problem = check(&mut releases, &[signer.key()], version("0.2.0")).unwrap_err();
            assert_eq!(
                problem,
                format!("(release tag): expected v<major>.<minor>.<patch> | received: {tag:?}")
            );
            assert!(releases.fetched.is_empty());
        }
    }

    #[test]
    fn a_verified_release_is_handed_to_the_installer() {
        let signer = TestSigner::new(1);
        let mut releases = FakeReleases::signed("v0.2.1", PACKAGE, &signer);
        let scratch = ScratchDirectory::new("install");
        // What an earlier attempt left behind goes; the old log stays until
        // the installer writes the new one.
        for stale in ["SysPilot-0.2.0-x64.msi", "SysPilot-0.2.1-x64.msi.part", INSTALL_LOG] {
            fs::write(scratch.0.join(stale), b"stale").unwrap();
        }
        let packages = FakePackages::declaring("0.2.1");

        let report =
            install(&mut releases, &packages, &[signer.key()], version("0.2.0"), &scratch.0);
        assert_eq!(report, Ok(UpdaterReport::Installing("0.2.1".into())));
        let package = scratch.0.join("SysPilot-0.2.1-x64.msi");
        assert_eq!(fs::read(&package).unwrap(), PACKAGE);
        assert_eq!(*packages.installed.borrow(), [(package, scratch.0.join(INSTALL_LOG))]);
        assert_eq!(scratch.files(), ["SysPilot-0.2.1-x64.msi", INSTALL_LOG]);
    }

    #[test]
    fn a_package_that_was_changed_is_deleted_and_never_installed() {
        let signer = TestSigner::new(1);
        let mut releases = FakeReleases::signed("v0.2.1", PACKAGE, &signer);
        releases.publish("v0.2.1", "SysPilot-0.2.1-x64.msi", b"something else entirely");
        let scratch = ScratchDirectory::new("tampered");
        let packages = FakePackages::declaring("0.2.1");

        let problem =
            install(&mut releases, &packages, &[signer.key()], version("0.2.0"), &scratch.0)
                .unwrap_err();
        assert_eq!(
            problem,
            "(signature): expected a signature that matches the file | received: one that does not"
        );
        assert!(packages.installed.borrow().is_empty(), "nothing was installed");
        assert!(scratch.files().is_empty(), "{:?}", scratch.files());
    }

    #[test]
    fn a_signed_package_of_another_version_is_not_installed() {
        let signer = TestSigner::new(1);
        // An old, genuinely signed package republished under a new tag.
        let mut releases = FakeReleases::signed("v0.2.1", PACKAGE, &signer);
        let scratch = ScratchDirectory::new("replay");
        let packages = FakePackages::declaring("0.1.0");

        let problem =
            install(&mut releases, &packages, &[signer.key()], version("0.2.0"), &scratch.0)
                .unwrap_err();
        assert_eq!(problem, "(package version): expected 0.2.1 | received: \"0.1.0\"");
        assert!(packages.installed.borrow().is_empty());
        assert!(scratch.files().is_empty(), "{:?}", scratch.files());
    }

    #[test]
    fn a_release_without_its_package_says_so() {
        let signer = TestSigner::new(1);
        let mut releases = FakeReleases::signed("v0.2.1", PACKAGE, &signer);
        releases.assets.remove(&("v0.2.1".to_owned(), "SysPilot-0.2.1-x64.msi".to_owned()));
        let scratch = ScratchDirectory::new("no-package");
        let packages = FakePackages::declaring("0.2.1");
        let problem =
            install(&mut releases, &packages, &[signer.key()], version("0.2.0"), &scratch.0)
                .unwrap_err();
        assert_eq!(
            problem,
            "(release 0.2.1): expected the file SysPilot-0.2.1-x64.msi | received: no such file"
        );
        assert!(scratch.files().is_empty());
    }

    #[test]
    fn a_download_cannot_grow_past_its_limit() {
        let mut sink = Limited { inner: Vec::new(), left: 10 };
        sink.write_all(b"12345678").unwrap();
        let error = sink.write_all(b"901").unwrap_err();
        assert_eq!(error.to_string(), "the download is larger than a release can be");
        assert_eq!(sink.inner, b"12345678");

        // A signature file that never ends is cut off the same way.
        let signer = TestSigner::new(1);
        let mut releases = FakeReleases::with_latest("v0.2.1");
        let endless = vec![b'A'; SIGNATURE_FILE_MAX as usize + 1];
        releases.publish("v0.2.1", "SysPilot-0.2.1-x64.msi.sig", &endless);
        let problem = check(&mut releases, &[signer.key()], version("0.2.0")).unwrap_err();
        assert!(problem.contains("larger than a release can be"), "{problem}");
    }

    #[test]
    fn a_file_and_its_signature_verify_from_disk() {
        let signer = TestSigner::new(1);
        let scratch = ScratchDirectory::new("verify");
        let (file, signature) = (scratch.0.join("package.msi"), scratch.0.join("package.msi.sig"));
        fs::write(&file, PACKAGE).unwrap();
        fs::write(&signature, signer.sign(PACKAGE)).unwrap();
        assert_eq!(verify_file(&file, &signature, &[signer.key()]), Ok(()));

        fs::write(&file, b"changed").unwrap();
        assert!(verify_file(&file, &signature, &[signer.key()]).is_err());
        let missing = verify_file(&file, &scratch.0.join("absent.sig"), &[signer.key()]);
        assert!(missing.unwrap_err().starts_with("read "), "the missing file is named");
    }
}
