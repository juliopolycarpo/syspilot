//! Who is an elevated administrator.
//!
//! The answer gates every request that changes or reveals configuration.
//! Membership is tested with `CheckTokenMembership`, which reports `false`
//! for the filtered token of an administrator who has not elevated (the
//! group is present but deny-only), so "is in Administrators" here means
//! "is elevated".

use std::ptr;

use syspilot_proto::win::PipeStream;
use windows_sys::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, RevertToSelf, SECURITY_MAX_SID_SIZE,
    WinBuiltinAdministratorsSid,
};
use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;

/// Whether the calling thread's effective token has the Administrators
/// group enabled. Any failure reads as "no".
fn effective_token_is_administrator() -> bool {
    // `u32` elements give the buffer the alignment a SID needs.
    let mut sid = [0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];
    let mut sid_size = size_of_val(&sid) as u32;
    // SAFETY: `sid` is writable for `sid_size` bytes; no domain SID is
    // needed for a built-in group.
    let created = unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut sid_size,
        )
    };
    if created == 0 {
        return false;
    }
    let mut is_member = 0;
    // SAFETY: a null token handle means "the calling thread's effective
    // token"; `sid` holds the SID created above; `is_member` is a valid
    // out-pointer.
    let checked =
        unsafe { CheckTokenMembership(ptr::null_mut(), sid.as_mut_ptr().cast(), &mut is_member) };
    checked != 0 && is_member != 0
}

/// Whether this process runs as an elevated administrator (or LocalSystem).
pub fn is_process_elevated() -> bool {
    effective_token_is_administrator()
}

/// Whether the client on the other end of `pipe` is an elevated
/// administrator. Any failure reads as "no".
///
/// At least one message must have been read from the pipe before this is
/// called; Windows refuses to impersonate a client that has sent nothing.
pub fn pipe_client_is_elevated(pipe: &PipeStream) -> bool {
    // SAFETY: the handle is the server end of a connected pipe, owned by
    // `pipe` for the duration of the call.
    if unsafe { ImpersonateNamedPipeClient(pipe.handle().raw()) } == 0 {
        return false;
    }
    let elevated = effective_token_is_administrator();
    // SAFETY: trivially safe; drops the impersonation started above.
    let reverted = unsafe { RevertToSelf() } != 0;
    // If the thread could not return to its own identity, the answer
    // obtained under the client's identity is not trusted either.
    elevated && reverted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevation_of_this_process_is_a_stable_answer() {
        assert_eq!(is_process_elevated(), is_process_elevated());
    }
}
