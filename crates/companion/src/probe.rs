//! Looking at the machine: the service's pipe and Windows Installer.

use syspilot_proto::message::UpdateState;
use syspilot_proto::win::{OwnedHandle, wide};
use syspilot_proto::{Client, ClientError, Instance};
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    MUTEX_MODIFY_STATE, OpenMutexW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};

use crate::watch::{Observation, Service};

/// The mutex Windows Installer holds while it runs an installation. See
/// "_MSIExecute Mutex" in the Windows Installer documentation.
const INSTALLER_MUTEX: &str = r"Global\_MSIExecute";

/// Something that can take one look at the machine.
pub trait Probe {
    /// What answers on the pipe and whether Windows Installer is busy.
    fn observe(&mut self) -> Observation;
}

/// The real machine, for the service of one instance.
pub struct MachineProbe {
    instance: Instance,
    /// The version that was running when the update was started: its
    /// status is the only one worth asking for.
    from: String,
}

impl MachineProbe {
    /// A probe for the service of `instance`, updating away from `from`.
    pub fn new(instance: Instance, from: &str) -> Self {
        Self { instance, from: from.to_owned() }
    }

    fn service(&self) -> Service {
        let mut client = match Client::connect(&self.instance) {
            Ok(client) => client,
            Err(ClientError::ProtocolMismatch { .. }) => return Service::OtherProtocol,
            Err(ClientError::NotRunning) => return Service::Absent,
            // A busy pipe, a service stopping halfway through the handshake,
            // or a pipe that is not the service's: no answer either way.
            Err(_) => return Service::Unknown,
        };
        let version = client.welcome().version.clone();
        if version != self.from {
            // A new service's first status waits for its first look at the
            // hardware; the handshake already says all that is needed.
            return Service::Answering { version, update: UpdateState::Idle };
        }
        match client.status() {
            Ok(status) => Service::Answering { version, update: status.update.state },
            Err(_) => Service::Unknown,
        }
    }
}

/// Whether Windows Installer runs an installation.
fn installer_busy() -> bool {
    is_held(INSTALLER_MUTEX)
}

/// Whether another thread holds the mutex called `name`. Taking it is the
/// only way to ask; it is given back at once, so a thread that wants it
/// waits a few microseconds at most.
fn is_held(name: &str) -> bool {
    let name = wide(name);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let handle =
        unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE, 0, name.as_ptr()) };
    let mutex = match OwnedHandle::new(handle) {
        Ok(mutex) => mutex,
        // Absent: no installation runs.
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => return false,
        // Without the right to give it back it must not be taken, and then
        // nothing is known. Busy is the answer that waits: idle would call
        // a new version ready before Windows Installer has committed it, and
        // start the restart timeout halfway through the installation. The
        // watch's timeouts bound the wait.
        Err(_) => return true,
    };
    // SAFETY: `mutex` is an open mutex handle with SYNCHRONIZE access.
    match unsafe { WaitForSingleObject(mutex.raw(), 0) } {
        WAIT_TIMEOUT => true,
        WAIT_OBJECT_0 | WAIT_ABANDONED => {
            // SAFETY: this thread owns the mutex since the wait above, and
            // the handle has MUTEX_MODIFY_STATE access.
            unsafe { ReleaseMutex(mutex.raw()) };
            false
        }
        _ => false,
    }
}

impl Probe for MachineProbe {
    fn observe(&mut self) -> Observation {
        Observation { service: self.service(), installer_busy: installer_busy() }
    }
}

#[cfg(test)]
pub use fake::ScriptedProbe;

#[cfg(test)]
mod fake {
    use super::*;

    /// A probe that plays back a list of looks, then repeats the last one.
    pub struct ScriptedProbe(pub Vec<Observation>);

    impl Probe for ScriptedProbe {
        fn observe(&mut self) -> Observation {
            if self.0.len() > 1 { self.0.remove(0) } else { self.0[0].clone() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_that_is_not_there_is_absent() {
        let instance = Instance::named("SysPilot-Test.companion-absent").unwrap();
        let mut probe = MachineProbe::new(instance, "0.3.0");
        assert_eq!(probe.observe().service, Service::Absent);
    }

    /// `is_held(name)` asked from a thread that stays alive until the
    /// answer has been used: a mutex it kept would stay kept meanwhile.
    fn ask_from_another_thread(name: &str, then: impl FnOnce(bool)) {
        let (answer_sender, answer) = std::sync::mpsc::channel();
        let (done, done_receiver) = std::sync::mpsc::channel::<()>();
        let name = name.to_owned();
        let asker = std::thread::spawn(move || {
            answer_sender.send(is_held(&name)).unwrap();
            let _ = done_receiver.recv();
        });
        then(answer.recv().unwrap());
        drop(done);
        asker.join().unwrap();
    }

    #[test]
    fn a_held_mutex_is_busy_and_a_free_one_is_given_back() {
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name = format!(r"Local\SysPilot-Test.companion-mutex-{}", std::process::id());
        assert!(!is_held(&name), "(absent mutex): expected not held");

        let wide_name = wide(&name);
        // SAFETY: `wide_name` is NUL-terminated; no security attributes, not
        // owned on creation.
        let created = unsafe { CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr()) };
        let mutex = OwnedHandle::new(created).unwrap();
        ask_from_another_thread(&name, |held| {
            assert!(!held, "(free mutex): expected not held");
            // SAFETY: `mutex` is an open mutex handle.
            let taken = unsafe { WaitForSingleObject(mutex.raw(), 0) };
            assert_eq!(taken, WAIT_OBJECT_0, "(free mutex): expected the asker to give it back");
        });
        // This thread holds it now.
        ask_from_another_thread(&name, |held| assert!(held, "(held mutex): expected held"));
        // SAFETY: this thread took the mutex above.
        unsafe { ReleaseMutex(mutex.raw()) };
    }

    #[test]
    fn a_mutex_that_cannot_be_given_back_counts_as_busy() {
        use syspilot_proto::win::SecurityDescriptor;
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name = format!(r"Local\SysPilot-Test.companion-denied-{}", std::process::id());
        // SYNCHRONIZE for everyone and nothing else: opening it with the
        // right to release it is refused.
        let descriptor = SecurityDescriptor::from_sddl("D:P(A;;0x100000;;;WD)").unwrap();
        let attributes = descriptor.attributes();
        let wide_name = wide(&name);
        // SAFETY: `wide_name` is NUL-terminated and `attributes` points at
        // `descriptor`, both alive for the call; not owned on creation.
        let created = unsafe { CreateMutexW(&attributes, 0, wide_name.as_ptr()) };
        let _mutex = OwnedHandle::new(created).unwrap();
        assert!(is_held(&name), "(mutex without release right): expected busy | received: idle");
    }

    #[test]
    fn the_scripted_probe_repeats_its_last_look() {
        let gone = Observation { service: Service::Absent, installer_busy: true };
        let back = Observation { service: Service::OtherProtocol, installer_busy: false };
        let mut probe = ScriptedProbe(vec![gone.clone(), back.clone()]);
        assert_eq!([probe.observe(), probe.observe(), probe.observe()], [gone, back.clone(), back]);
    }
}
