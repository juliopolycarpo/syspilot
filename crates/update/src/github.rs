//! Releases as GitHub publishes them, read over HTTPS with WinHTTP.
//!
//! Two kinds of request, both without any API or token:
//!
//! - `GET <repository>/releases/latest` answers with a redirect to the page
//!   of the newest published release, whose address ends in its tag. Drafts
//!   and pre-releases are never "latest", so publishing a release is what
//!   offers it.
//! - `GET <repository>/releases/download/<tag>/<file>` is the file, after a
//!   redirect to GitHub's storage that WinHTTP follows.
//!
//! WinHTTP validates the server's certificate and refuses a redirect from
//! HTTPS to HTTP by default; neither is switched off. What arrives is still
//! only a hint until the release signature has been verified.

use std::ffi::c_void;
use std::io::Write;
use std::ptr;

use syspilot_proto::win::wide;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_DISABLE_REDIRECTS, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetTimeouts,
};

use crate::release::Releases;

/// The host releases are read from. The repository address must be on it.
const HOST: &str = "github.com";
const HTTPS_PORT: u16 = 443;
/// Milliseconds a name lookup, a connection, a send and a receive may take.
const TIMEOUTS_MS: [i32; 4] = [15_000, 15_000, 30_000, 60_000];
/// Longest `Location` header read, in UTF-16 units.
const LOCATION_MAX: usize = 2048;

/// A WinHTTP handle, closed on drop.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is valid (checked at construction) and closed
        // exactly once.
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// Why the last WinHTTP call failed, in words. WinHTTP's own error numbers
/// have no text in the system's table.
fn last_failure() -> String {
    // SAFETY: trivially safe; reads thread-local state.
    match unsafe { GetLastError() } {
        12002 => "the server took too long to answer".to_owned(),
        12007 => "the server's name could not be found; is this PC online?".to_owned(),
        12029 | 12030 => "the connection could not be made or was cut".to_owned(),
        12175 => "the server's certificate could not be trusted".to_owned(),
        code => format!("network error {code}"),
    }
}

fn opened(handle: *mut c_void, action: &str) -> Result<Handle, String> {
    if handle.is_null() { Err(format!("{action}: {}", last_failure())) } else { Ok(Handle(handle)) }
}

/// The path of the repository on [`HOST`], from its address in the package
/// metadata: `/owner/name`.
pub fn repository_path(repository: &str) -> Result<&str, String> {
    let path = repository.strip_prefix("https://").and_then(|rest| rest.strip_prefix(HOST));
    match path {
        Some(path) if path.starts_with('/') && !path.ends_with('/') => Ok(path),
        _ => Err(format!(
            "(repository): expected https://{HOST}/<owner>/<name> | received: {repository:?}"
        )),
    }
}

/// The tag the "latest release" redirect points at.
pub fn tag_from_location<'a>(location: &'a str, repository: &str) -> Result<&'a str, String> {
    let page = format!("https://{HOST}{repository}/releases");
    let rest = location.strip_prefix(&page);
    // With no published release the redirect goes to the list itself.
    if rest == Some("") {
        return Err("SysPilot has no published release yet.".to_owned());
    }
    rest.and_then(|rest| rest.strip_prefix("/tag/")).filter(|tag| !tag.is_empty()).ok_or_else(
        || {
            format!(
                "(latest release): expected a redirect to {page}/tag/<tag> | received: {location:?}"
            )
        },
    )
}

/// The repository's releases on GitHub.
pub struct GitHubReleases {
    /// Declared before the session: a connection is closed before the
    /// session it belongs to.
    connection: Handle,
    _session: Handle,
    repository: &'static str,
}

impl GitHubReleases {
    /// Prepares a connection to the repository in the package metadata.
    /// Nothing is sent yet.
    pub fn new() -> Result<Self, String> {
        let repository = repository_path(env!("CARGO_PKG_REPOSITORY"))?;
        let agent = wide(concat!("SysPilot-update/", env!("CARGO_PKG_VERSION")));
        // SAFETY: `agent` is NUL-terminated; no proxy names are given, the
        // system's proxy settings apply.
        let session = opened(
            unsafe {
                WinHttpOpen(
                    agent.as_ptr(),
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    ptr::null(),
                    ptr::null(),
                    0,
                )
            },
            "start WinHTTP",
        )?;
        let [resolve, connect, send, receive] = TIMEOUTS_MS;
        // SAFETY: `session` is a valid session handle. A failure leaves the
        // defaults, which are longer but finite.
        unsafe { WinHttpSetTimeouts(session.0, resolve, connect, send, receive) };
        let host = wide(HOST);
        // SAFETY: `session` is valid and `host` NUL-terminated.
        let connection = opened(
            unsafe { WinHttpConnect(session.0, host.as_ptr(), HTTPS_PORT, 0) },
            "prepare the connection",
        )?;
        Ok(Self { connection, _session: session, repository })
    }

    /// Sends `GET path` and waits for the response headers.
    fn get(&self, path: &str, follow_redirects: bool) -> Result<Handle, String> {
        let (verb, object) = (wide("GET"), wide(path));
        // SAFETY: the connection is valid; `verb` and `object` are
        // NUL-terminated; the defaults are taken for version, referrer and
        // accepted types.
        let request = opened(
            unsafe {
                WinHttpOpenRequest(
                    self.connection.0,
                    verb.as_ptr(),
                    object.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    WINHTTP_FLAG_SECURE,
                )
            },
            "prepare the request",
        )?;
        if !follow_redirects {
            let feature = WINHTTP_DISABLE_REDIRECTS;
            // SAFETY: `request` is valid; the buffer is a `u32`, as the
            // option requires, and its size is given.
            let set = unsafe {
                WinHttpSetOption(
                    request.0,
                    WINHTTP_OPTION_DISABLE_FEATURE,
                    ptr::from_ref(&feature).cast(),
                    size_of::<u32>() as u32,
                )
            };
            if set == 0 {
                return Err(format!("prepare the request: {}", last_failure()));
            }
        }
        // SAFETY: `request` is valid; there are no extra headers and no body.
        let sent = unsafe { WinHttpSendRequest(request.0, ptr::null(), 0, ptr::null(), 0, 0, 0) };
        // SAFETY: `request` is valid; the reserved pointer is null.
        if sent == 0 || unsafe { WinHttpReceiveResponse(request.0, ptr::null_mut()) } == 0 {
            return Err(format!("{HOST} could not be reached: {}", last_failure()));
        }
        Ok(request)
    }
}

fn status_of(request: &Handle) -> Result<u32, String> {
    let (mut status, mut size) = (0u32, size_of::<u32>() as u32);
    // SAFETY: `request` has received its response; with the number flag the
    // buffer is a `u32` and `size` says so.
    let ok = unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            ptr::null(),
            ptr::from_mut(&mut status).cast(),
            &mut size,
            ptr::null_mut(),
        )
    };
    if ok == 0 { Err(format!("read the response: {}", last_failure())) } else { Ok(status) }
}

fn location_of(request: &Handle) -> Result<String, String> {
    let mut buffer = [0u16; LOCATION_MAX];
    let mut size = size_of_val(&buffer) as u32;
    // SAFETY: `request` has received its response; `buffer` is `size` bytes
    // long, and on success `size` is the length written, in bytes, without
    // the terminator.
    let ok = unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_LOCATION,
            ptr::null(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(format!("read where the latest release is: {}", last_failure()));
    }
    Ok(String::from_utf16_lossy(&buffer[..size as usize / 2]))
}

impl Releases for GitHubReleases {
    fn latest_tag(&mut self) -> Result<String, String> {
        let request = self.get(&format!("{}/releases/latest", self.repository), false)?;
        match status_of(&request)? {
            301 | 302 | 303 | 307 | 308 => {
                tag_from_location(&location_of(&request)?, self.repository).map(str::to_owned)
            }
            status => {
                Err(format!("(latest release): expected a redirect | received: HTTP {status}"))
            }
        }
    }

    fn fetch(&mut self, tag: &str, asset: &str, sink: &mut dyn Write) -> Result<bool, String> {
        let path = format!("{}/releases/download/{tag}/{asset}", self.repository);
        let request = self.get(&path, true)?;
        match status_of(&request)? {
            200 => {}
            404 => return Ok(false),
            status => {
                return Err(format!("(download {asset}): expected HTTP 200 | received: {status}"));
            }
        }
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            // SAFETY: `request` has received its response; `buffer` is as
            // long as the count given, and `read` receives how much of it
            // was filled.
            let ok = unsafe {
                WinHttpReadData(
                    request.0,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut read,
                )
            };
            if ok == 0 {
                return Err(format!("download {asset}: {}", last_failure()));
            }
            if read == 0 {
                return Ok(true);
            }
            sink.write_all(&buffer[..read as usize])
                .map_err(|error| format!("download {asset}: {error}"))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPOSITORY: &str = "/juliopolycarpo/syspilot";

    #[test]
    fn the_package_metadata_names_a_repository_on_the_release_host() {
        assert_eq!(repository_path(env!("CARGO_PKG_REPOSITORY")), Ok(REPOSITORY));
        for elsewhere in [
            "http://github.com/a/b",
            "https://github.com.evil.example/a/b",
            "https://example.com/a/b",
            "https://github.com",
            "https://github.com/a/b/",
        ] {
            let problem = repository_path(elsewhere).unwrap_err();
            assert!(problem.ends_with(&format!("received: {elsewhere:?}")), "{problem}");
        }
    }

    #[test]
    fn the_latest_redirect_ends_in_the_tag() {
        let location = "https://github.com/juliopolycarpo/syspilot/releases/tag/v0.2.1";
        assert_eq!(tag_from_location(location, REPOSITORY), Ok("v0.2.1"));
    }

    #[test]
    fn a_repository_without_releases_says_so() {
        let location = "https://github.com/juliopolycarpo/syspilot/releases";
        assert_eq!(
            tag_from_location(location, REPOSITORY).unwrap_err(),
            "SysPilot has no published release yet."
        );
    }

    #[test]
    fn a_redirect_to_anywhere_else_is_refused() {
        for location in [
            "https://github.com/someone/else/releases/tag/v9.9.9",
            "https://example.com/juliopolycarpo/syspilot/releases/tag/v9.9.9",
            "https://github.com/juliopolycarpo/syspilot/releases/tag/",
            "https://github.com/juliopolycarpo/syspilot/releases-old/tag/v9.9.9",
            "https://github.com/login",
            "",
        ] {
            let problem = tag_from_location(location, REPOSITORY).unwrap_err();
            assert!(
                problem.starts_with("(latest release): expected a redirect to"),
                "{location:?}: {problem}"
            );
        }
    }
}
