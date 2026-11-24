//! OpenSSH file signatures (`ssh-keygen -Y sign`), Ed25519 only.
//!
//! A release is signed with an SSH key the maintainer keeps outside the
//! build. The signature file is the armoured `SSHSIG` structure of
//! OpenSSH's `PROTOCOL.sshsig`:
//!
//! ```text
//! "SSHSIG"  uint32 version (1)
//! string publickey   { string "ssh-ed25519", string key (32 bytes) }
//! string namespace
//! string reserved
//! string hash_algorithm
//! string signature   { string "ssh-ed25519", string signature (64 bytes) }
//! ```
//!
//! What is signed is not the file but
//! `"SSHSIG" || string namespace || string reserved || string hash_algorithm
//! || string H(file)`, so a file of any size is hashed once, in a stream.
//!
//! Only what a release uses is accepted: Ed25519, SHA-512 (the default of
//! `ssh-keygen`), the namespace [`NAMESPACE`], and a key from the list the
//! updater was built with. Everything else is an error that says what was
//! expected.

use std::io::{self, Read};

use ed25519_compact::{PublicKey, Signature, sha512};

/// The namespace a release signature must carry. It keeps a signature made
/// for something else with the same key (a commit, a login) from passing as
/// a release.
pub const NAMESPACE: &str = "syspilot-update";

/// The keys that may sign a release: the file the repository keeps for
/// `ssh-keygen -Y verify`, compiled in.
const SIGNERS_FILE: &str = include_str!("../../../installer/update-signers");

const MAGIC: &[u8] = b"SSHSIG";
const KEY_TYPE: &str = "ssh-ed25519";
const HASH_ALGORITHM: &str = "sha512";
const ARMOUR_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
const ARMOUR_END: &str = "-----END SSH SIGNATURE-----";
/// Longest signature file read. A real one is about 300 bytes.
pub const SIGNATURE_FILE_MAX: u64 = 16 * 1024;

/// An Ed25519 public key.
pub type Key = [u8; 32];
/// A SHA-512 digest.
pub type Digest = [u8; 64];

/// A parsed signature file. Nothing in it is trusted until
/// [`FileSignature::verify`] has passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSignature {
    key: Key,
    namespace: String,
    reserved: Vec<u8>,
    hash_algorithm: String,
    signature: [u8; 64],
}

fn problem(expected: &str, received: impl std::fmt::Display) -> String {
    format!("(signature): expected {expected} | received: {received}")
}

/// Decodes standard base64 with padding. White space is skipped, as the
/// armour wraps its lines.
fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3);
    let (mut bits, mut count, mut padding) = (0u32, 0u8, 0u8);
    for character in text.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
        let value = match character {
            b'A'..=b'Z' => character - b'A',
            b'a'..=b'z' => character - b'a' + 26,
            b'0'..=b'9' => character - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding += 1;
                continue;
            }
            other => return Err(problem("base64 text", format!("the byte {other:#04x}"))),
        };
        if padding > 0 {
            return Err(problem("base64 padding only at the end", "data after it"));
        }
        bits = bits << 6 | u32::from(value);
        count += 1;
        if count == 4 {
            bytes.extend_from_slice(&bits.to_be_bytes()[1..]);
            (bits, count) = (0, 0);
        }
    }
    // A last group of two or three characters carries one or two bytes and
    // is padded to four.
    match (count, padding) {
        (0, 0) => {}
        (2, 2) => bytes.push((bits >> 4) as u8),
        (3, 1) => bytes.extend_from_slice(&((bits >> 2) as u16).to_be_bytes()),
        _ => return Err(problem("base64 in groups of four", "a truncated group")),
    }
    Ok(bytes)
}

/// Reads the length-prefixed fields of the SSH wire format.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn take(&mut self, count: usize, what: &str) -> Result<&'a [u8], String> {
        if self.0.len() < count {
            return Err(problem(&format!("{count} bytes of {what}"), format!("{}", self.0.len())));
        }
        let (taken, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(taken)
    }

    fn uint32(&mut self, what: &str) -> Result<u32, String> {
        let bytes = self.take(4, what)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn string(&mut self, what: &str) -> Result<&'a [u8], String> {
        let length = self.uint32(what)? as usize;
        self.take(length, what)
    }

    fn end(&self, what: &str) -> Result<(), String> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(problem(&format!("the end of {what}"), format!("{} more bytes", self.0.len())))
        }
    }
}

/// Reads a blob made of a key type and one value of `N` bytes: a public key
/// or a signature.
fn typed_value<const N: usize>(blob: &[u8], what: &str) -> Result<[u8; N], String> {
    let mut fields = Fields(blob);
    let kind = fields.string(what)?;
    if kind != KEY_TYPE.as_bytes() {
        let received = String::from_utf8_lossy(kind).into_owned();
        return Err(problem(&format!("an {KEY_TYPE} {what}"), format!("{received:?}")));
    }
    let value = fields.string(what)?;
    fields.end(what)?;
    value
        .try_into()
        .map_err(|_| problem(&format!("{N} bytes of {what}"), format!("{}", value.len())))
}

/// Appends an SSH `string`: a big-endian length, then the bytes.
fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

impl FileSignature {
    /// Parses an armoured signature file.
    pub fn parse(armoured: &str) -> Result<Self, String> {
        let body = armoured
            .trim()
            .strip_prefix(ARMOUR_BEGIN)
            .and_then(|rest| rest.strip_suffix(ARMOUR_END))
            .ok_or_else(|| problem("an SSH signature file", "something else"))?;
        let blob = decode_base64(body)?;
        let mut fields = Fields(&blob);
        if fields.take(MAGIC.len(), "magic")? != MAGIC {
            return Err(problem("the SSHSIG magic", "other bytes"));
        }
        let version = fields.uint32("version")?;
        if version != 1 {
            return Err(problem("version 1", version));
        }
        let key = typed_value(fields.string("public key")?, "public key")?;
        let namespace = String::from_utf8_lossy(fields.string("namespace")?).into_owned();
        let reserved = fields.string("reserved")?.to_vec();
        let hash_algorithm = String::from_utf8_lossy(fields.string("hash algorithm")?).into_owned();
        let signature = typed_value(fields.string("signature")?, "signature")?;
        fields.end("the signature file")?;
        Ok(Self { key, namespace, reserved, hash_algorithm, signature })
    }

    /// Checks what can be checked without the file: that the signature is
    /// of the kind a release carries and names one of `trusted`. A release
    /// whose signature fails this is not offered at all.
    pub fn check_shape(&self, trusted: &[Key]) -> Result<(), String> {
        if self.namespace != NAMESPACE {
            return Err(problem(
                &format!("the namespace {NAMESPACE}"),
                format!("{:?}", self.namespace),
            ));
        }
        if self.hash_algorithm != HASH_ALGORITHM {
            return Err(problem(
                &format!("the hash algorithm {HASH_ALGORITHM}"),
                format!("{:?}", self.hash_algorithm),
            ));
        }
        if !trusted.contains(&self.key) {
            return Err(problem("a key SysPilot releases are signed with", "another key"));
        }
        Ok(())
    }

    /// Verifies that one of `trusted` signed the file whose SHA-512 is
    /// `digest`.
    pub fn verify(&self, trusted: &[Key], digest: &Digest) -> Result<(), String> {
        self.check_shape(trusted)?;
        let mut signed = MAGIC.to_vec();
        put_string(&mut signed, self.namespace.as_bytes());
        put_string(&mut signed, &self.reserved);
        put_string(&mut signed, self.hash_algorithm.as_bytes());
        put_string(&mut signed, digest);
        PublicKey::new(self.key)
            .verify(&signed, &Signature::new(self.signature))
            .map_err(|_| problem("a signature that matches the file", "one that does not"))
    }
}

/// Parses an allowed-signers file: every `ssh-ed25519 <base64>` pair on a
/// line that is not a comment is a key.
fn parse_signers(text: &str) -> Result<Vec<Key>, String> {
    let mut keys = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.starts_with('#')) {
        let mut words = line.split_whitespace().skip_while(|word| *word != KEY_TYPE);
        if words.next().is_none() {
            continue;
        }
        let encoded = words.next().ok_or_else(|| problem("a key after ssh-ed25519", "nothing"))?;
        keys.push(typed_value(&decode_base64(encoded)?, "public key")?);
    }
    Ok(keys)
}

/// The keys this build accepts a release from.
pub fn trusted_keys() -> Result<Vec<Key>, String> {
    let keys = parse_signers(SIGNERS_FILE)?;
    if keys.is_empty() {
        return Err(problem("at least one release key in this build", "none"));
    }
    Ok(keys)
}

/// The SHA-512 of everything `source` yields.
pub fn digest_of(mut source: impl Read) -> io::Result<Digest> {
    let mut hash = sha512::Hash::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match source.read(&mut buffer)? {
            0 => return Ok(hash.finalize()),
            read => hash.update(&buffer[..read]),
        }
    }
}

#[cfg(test)]
pub mod fixtures {
    //! A signer for tests: makes the signature files `ssh-keygen` would.

    use super::*;
    use ed25519_compact::{KeyPair, Seed};

    /// A key pair that exists only in tests.
    pub struct TestSigner(KeyPair);

    fn encode_base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut text = String::new();
        for chunk in bytes.chunks(3) {
            let mut group = [0u8; 3];
            group[..chunk.len()].copy_from_slice(chunk);
            let bits = u32::from_be_bytes([0, group[0], group[1], group[2]]);
            for index in 0..4 {
                if index <= chunk.len() {
                    text.push(ALPHABET[(bits >> (18 - 6 * index) & 63) as usize] as char);
                } else {
                    text.push('=');
                }
            }
        }
        text
    }

    impl TestSigner {
        /// The signer whose secret is `seed` repeated.
        pub fn new(seed: u8) -> Self {
            Self(KeyPair::from_seed(Seed::new([seed; 32])))
        }

        /// The public key.
        pub fn key(&self) -> Key {
            *self.0.pk
        }

        /// A signature file over `content`, as `ssh-keygen -Y sign` with
        /// `namespace` and `hash_algorithm` writes it.
        pub fn sign_as(&self, content: &[u8], namespace: &str, hash_algorithm: &str) -> String {
            let digest = sha512::Hash::hash(content);
            let mut signed = MAGIC.to_vec();
            put_string(&mut signed, namespace.as_bytes());
            put_string(&mut signed, b"");
            put_string(&mut signed, hash_algorithm.as_bytes());
            put_string(&mut signed, &digest);
            let signature = self.0.sk.sign(&signed, None);

            let typed = |value: &[u8]| {
                let mut blob = Vec::new();
                put_string(&mut blob, KEY_TYPE.as_bytes());
                put_string(&mut blob, value);
                blob
            };
            let mut blob = MAGIC.to_vec();
            blob.extend_from_slice(&1u32.to_be_bytes());
            put_string(&mut blob, &typed(&self.key()));
            put_string(&mut blob, namespace.as_bytes());
            put_string(&mut blob, b"");
            put_string(&mut blob, hash_algorithm.as_bytes());
            put_string(&mut blob, &typed(&*signature));
            format!("{ARMOUR_BEGIN}\n{}\n{ARMOUR_END}\n", encode_base64(&blob))
        }

        /// A release signature over `content`.
        pub fn sign(&self, content: &[u8]) -> String {
            self.sign_as(content, NAMESPACE, HASH_ALGORITHM)
        }
    }

    #[test]
    fn the_test_encoder_and_the_decoder_agree_on_every_length() {
        for length in 0..=9usize {
            let bytes: Vec<u8> = (0..length as u8).map(|index| index.wrapping_mul(83)).collect();
            assert_eq!(decode_base64(&encode_base64(&bytes)), Ok(bytes), "{length} bytes");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::TestSigner;
    use super::*;

    /// A file signed with a throwaway key by OpenSSH itself:
    /// `ssh-keygen -Y sign -f key -n syspilot-update sample.txt`.
    const OPENSSH_CONTENT: &[u8] = include_bytes!("../tests/fixtures/sample.txt");
    const OPENSSH_SIGNATURE: &str = include_str!("../tests/fixtures/sample.txt.sig");
    const OPENSSH_SIGNERS: &str = include_str!("../tests/fixtures/signers");

    fn digest(content: &[u8]) -> Digest {
        digest_of(content).unwrap()
    }

    #[test]
    fn a_signature_made_by_openssh_verifies() {
        let trusted = parse_signers(OPENSSH_SIGNERS).unwrap();
        assert_eq!(trusted.len(), 1);
        let signature = FileSignature::parse(OPENSSH_SIGNATURE).unwrap();
        assert_eq!(signature.verify(&trusted, &digest(OPENSSH_CONTENT)), Ok(()));
    }

    #[test]
    fn a_changed_file_does_not_verify() {
        let trusted = parse_signers(OPENSSH_SIGNERS).unwrap();
        let signature = FileSignature::parse(OPENSSH_SIGNATURE).unwrap();
        let mut changed = OPENSSH_CONTENT.to_vec();
        changed[0] ^= 1;
        assert_eq!(
            signature.verify(&trusted, &digest(&changed)).unwrap_err(),
            "(signature): expected a signature that matches the file | received: one that does not"
        );
    }

    #[test]
    fn a_signature_from_a_key_that_is_not_listed_is_refused() {
        let (signer, stranger) = (TestSigner::new(1), TestSigner::new(2));
        let signature = FileSignature::parse(&stranger.sign(b"release")).unwrap();
        assert_eq!(
            signature.verify(&[signer.key()], &digest(b"release")).unwrap_err(),
            "(signature): expected a key SysPilot releases are signed with | received: another key"
        );
        assert_eq!(signature.verify(&[signer.key(), stranger.key()], &digest(b"release")), Ok(()));
    }

    #[test]
    fn a_signature_made_for_something_else_is_refused() {
        let signer = TestSigner::new(1);
        let commit = FileSignature::parse(&signer.sign_as(b"release", "git", "sha512")).unwrap();
        assert_eq!(
            commit.verify(&[signer.key()], &digest(b"release")).unwrap_err(),
            "(signature): expected the namespace syspilot-update | received: \"git\""
        );
        let weaker =
            FileSignature::parse(&signer.sign_as(b"release", NAMESPACE, "sha256")).unwrap();
        assert_eq!(
            weaker.check_shape(&[signer.key()]).unwrap_err(),
            "(signature): expected the hash algorithm sha512 | received: \"sha256\""
        );
    }

    #[test]
    fn a_tampered_signature_does_not_verify() {
        let signer = TestSigner::new(1);
        let mut parsed = FileSignature::parse(&signer.sign(b"release")).unwrap();
        parsed.signature[10] ^= 0x40;
        assert!(parsed.verify(&[signer.key()], &digest(b"release")).is_err());
    }

    #[test]
    fn files_that_are_not_signatures_say_what_was_expected() {
        let cases = [
            ("", "an SSH signature file"),
            ("<html>Not Found</html>", "an SSH signature file"),
            ("-----BEGIN SSH SIGNATURE-----\n!!!!\n-----END SSH SIGNATURE-----", "base64 text"),
            (
                "-----BEGIN SSH SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----",
                "6 bytes of magic",
            ),
            (
                "-----BEGIN SSH SIGNATURE-----\nU1NIU0lHAAAAAg==\n-----END SSH SIGNATURE-----",
                "version 1",
            ),
            (
                "-----BEGIN SSH SIGNATURE-----\nU1NIU0lHAAAAAf////8=\n-----END SSH SIGNATURE-----",
                "4294967295 bytes of public key",
            ),
        ];
        for (text, expected) in cases {
            let error = FileSignature::parse(text).unwrap_err();
            assert!(
                error.starts_with(&format!("(signature): expected {expected}")),
                "{text:?}: {error}"
            );
        }
    }

    #[test]
    fn base64_is_strict_about_its_shape() {
        assert_eq!(decode_base64("TQ=="), Ok(b"M".to_vec()));
        assert_eq!(decode_base64("TWE="), Ok(b"Ma".to_vec()));
        assert_eq!(decode_base64("TW\r\nFu"), Ok(b"Man".to_vec()));
        for bad in ["TQ", "TQ=", "T", "TQ==TQ==", "TW=u", "TWFu*"] {
            assert!(decode_base64(bad).is_err(), "(base64): expected {bad:?} to be refused");
        }
    }

    #[test]
    fn this_build_carries_at_least_one_release_key() {
        let keys = trusted_keys().unwrap();
        assert!(!keys.is_empty());
        // Comments and lines without a key are skipped; a key that is cut
        // short is an error, not a missing key.
        assert_eq!(parse_signers("# nothing\n\nname only\n"), Ok(Vec::new()));
        assert!(parse_signers("name ssh-ed25519 AAAA").is_err());
        assert!(parse_signers("name ssh-ed25519").is_err());
    }
}
