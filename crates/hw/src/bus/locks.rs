//! The mutexes monitoring tools use to take turns on shared buses.
//!
//! Super I/O configuration mode, the SMBus controller and PCI configuration
//! space are all stateful multi-step interfaces. HWiNFO, LibreHardwareMonitor,
//! vendor utilities and some firmware services serialise their access with
//! named global mutexes; a program that skips them corrupts the others'
//! transactions (and its own). Every driver-backed read in this crate runs
//! inside a [`BusGuard`].
//!
//! Mutex ownership belongs to a thread, so the guard is `!Send`: it is
//! taken and dropped inside one `sample()` call, on one thread.

use std::fmt;
use std::marker::PhantomData;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use syspilot_proto::win::{OwnedHandle, SecurityDescriptor, wide};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, GetLastError, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, MUTEX_MODIFY_STATE, OpenMutexW, ReleaseMutex, WaitForSingleObject,
};

use super::error::BusError;

/// SDDL for "everyone may do anything": other tools run as other users, and
/// the mutex must be usable by all of them.
const EVERYONE_SDDL: &str = "D:(A;;GA;;;WD)";
/// `SYNCHRONIZE`: the right to wait on an object.
const SYNCHRONIZE_RIGHT: u32 = 0x0010_0000;

/// Longest a sample waits for a bus another program holds.
pub const DEFAULT_WAIT: Duration = Duration::from_millis(100);

/// A bus that has a shared mutex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BusKind {
    /// Super I/O configuration ports and hardware-monitor index/data ports.
    Isa,
    /// The SMBus host controller.
    Smbus,
    /// PCI configuration space, which the AMD SMN window goes through.
    Pci,
}

impl BusKind {
    /// Every bus, in the order [`SystemBusLocks`] stores them.
    pub const ALL: [BusKind; 3] = [BusKind::Isa, BusKind::Smbus, BusKind::Pci];

    /// The well-known global mutex name other tools use for this bus.
    pub fn mutex_name(self) -> &'static str {
        match self {
            Self::Isa => r"Global\Access_ISABUS.HTP.Method",
            Self::Smbus => r"Global\Access_SMBUS.HTP.Method",
            Self::Pci => r"Global\Access_PCI",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Isa => 0,
            Self::Smbus => 1,
            Self::Pci => 2,
        }
    }
}

impl fmt::Display for BusKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Isa => "isa bus",
            Self::Smbus => "smbus",
            Self::Pci => "pci bus",
        })
    }
}

/// Proof that the calling thread owns a bus mutex. Releases it on drop.
pub struct BusGuard<'a> {
    release: Release<'a>,
    /// Mutex ownership is per thread: the guard must be dropped on the
    /// thread that took it, so it must not be `Send`.
    _same_thread: PhantomData<*const ()>,
}

enum Release<'a> {
    Mutex(&'a OwnedHandle),
    Counter(Arc<AtomicUsize>),
}

impl Drop for BusGuard<'_> {
    fn drop(&mut self) {
        match &self.release {
            Release::Mutex(handle) => {
                // SAFETY: `handle` is a live mutex handle and this thread
                // owns the mutex (the guard is `!Send`, so it is dropped on
                // the thread that waited).
                let released = unsafe { ReleaseMutex(handle.raw()) };
                debug_assert_ne!(
                    released,
                    0,
                    "ReleaseMutex failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            Release::Counter(live) => {
                live.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

/// Source of bus guards.
pub trait BusLocks: Send + Sync {
    /// Takes the mutex of `bus`, waiting a bounded time.
    ///
    /// # Errors
    ///
    /// [`BusError::Contended`] when another program holds it for longer
    /// than the wait limit; the caller skips its sample.
    fn acquire(&self, bus: BusKind) -> Result<BusGuard<'_>, BusError>;
}

/// The real global mutexes.
pub struct SystemBusLocks {
    names: [String; 3],
    wait: Duration,
    handles: [OnceLock<OwnedHandle>; 3],
}

impl SystemBusLocks {
    /// The well-known global names, waiting up to [`DEFAULT_WAIT`].
    pub fn new() -> Self {
        Self::with_names(BusKind::ALL.map(|bus| bus.mutex_name().to_owned()), DEFAULT_WAIT)
    }

    /// Custom mutex names (in [`BusKind::ALL`] order) and wait limit. Tests
    /// use private `Local\` names so they never touch other programs' locks.
    pub fn with_names(names: [String; 3], wait: Duration) -> Self {
        Self { names, wait, handles: Default::default() }
    }

    /// Creates the mutex with an ACL open to everyone, or opens the existing
    /// one when its owner's ACL does not allow full access.
    fn create_or_open(&self, bus: BusKind) -> Result<OwnedHandle, BusError> {
        let name = wide(&self.names[bus.index()]);
        let lock_error = |win32| BusError::Lock { bus, win32 };
        let descriptor = SecurityDescriptor::from_sddl(EVERYONE_SDDL)
            .map_err(|error| lock_error(error.raw_os_error().unwrap_or(0) as u32))?;
        let attributes = descriptor.attributes();
        // SAFETY: `attributes` points at `descriptor`, which outlives the
        // call; `name` is NUL-terminated.
        let created = unsafe { CreateMutexW(&attributes, 0, name.as_ptr()) };
        if let Ok(handle) = OwnedHandle::new(created) {
            return Ok(handle);
        }
        // SAFETY: reads the calling thread's last error.
        let win32 = unsafe { GetLastError() };
        if win32 != ERROR_ACCESS_DENIED {
            return Err(lock_error(win32));
        }
        // The mutex exists and does not grant full access: ask only for the
        // two rights waiting and releasing need.
        // SAFETY: `name` is NUL-terminated.
        let opened =
            unsafe { OpenMutexW(SYNCHRONIZE_RIGHT | MUTEX_MODIFY_STATE, 0, name.as_ptr()) };
        OwnedHandle::new(opened)
            .map_err(|error| lock_error(error.raw_os_error().unwrap_or(0) as u32))
    }

    fn handle(&self, bus: BusKind) -> Result<&OwnedHandle, BusError> {
        let slot = &self.handles[bus.index()];
        if let Some(handle) = slot.get() {
            return Ok(handle);
        }
        // A concurrent first use may create a second handle to the same
        // mutex; `set` keeps one and drops the other.
        let created = self.create_or_open(bus)?;
        let _ = slot.set(created);
        slot.get().ok_or(BusError::Lock { bus, win32: 0 })
    }
}

impl Default for SystemBusLocks {
    fn default() -> Self {
        Self::new()
    }
}

impl BusLocks for SystemBusLocks {
    fn acquire(&self, bus: BusKind) -> Result<BusGuard<'_>, BusError> {
        let handle = self.handle(bus)?;
        let wait_ms = u32::try_from(self.wait.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `handle` is a live mutex handle.
        let waited = unsafe { WaitForSingleObject(handle.raw(), wait_ms) };
        match waited {
            // An abandoned mutex (its owner died) is ours now; the hardware
            // state the dead owner left is re-established by every
            // transaction starting from scratch.
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                Ok(BusGuard { release: Release::Mutex(handle), _same_thread: PhantomData })
            }
            WAIT_TIMEOUT => Err(BusError::Contended(bus)),
            _ => {
                // SAFETY: reads the calling thread's last error.
                let win32 = unsafe { GetLastError() };
                Err(BusError::Lock { bus, win32 })
            }
        }
    }
}

/// Locks that never block and count the guards alive, so tests can assert
/// that every guard is released on every path. Buses listed in
/// `held_elsewhere` report contention.
#[derive(Debug, Default)]
pub struct FakeBusLocks {
    held_elsewhere: Vec<BusKind>,
    live: Arc<AtomicUsize>,
    history: Mutex<Vec<BusKind>>,
}

impl FakeBusLocks {
    /// Locks that are all free.
    pub fn free() -> Self {
        Self::default()
    }

    /// Locks where another program holds `buses`.
    pub fn held_by_another_program(buses: &[BusKind]) -> Self {
        Self { held_elsewhere: buses.to_vec(), ..Self::default() }
    }

    /// Guards currently alive.
    pub fn live_guards(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Every bus acquired so far, in order.
    pub fn acquired(&self) -> Vec<BusKind> {
        self.history.lock().map(|history| history.clone()).unwrap_or_default()
    }
}

impl BusLocks for FakeBusLocks {
    fn acquire(&self, bus: BusKind) -> Result<BusGuard<'_>, BusError> {
        if self.held_elsewhere.contains(&bus) {
            return Err(BusError::Contended(bus));
        }
        if let Ok(mut history) = self.history.lock() {
            history.push(bus);
        }
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(BusGuard { release: Release::Counter(self.live.clone()), _same_thread: PhantomData })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// Private names: a test must never contend with a real monitoring tool.
    fn private_locks(wait: Duration) -> SystemBusLocks {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        SystemBusLocks::with_names(
            ["isa", "smbus", "pci"].map(|bus| format!(r"Local\syspilot-hw-test-{pid}-{id}-{bus}")),
            wait,
        )
    }

    #[test]
    fn well_known_names_match_the_other_tools() {
        assert_eq!(BusKind::Isa.mutex_name(), r"Global\Access_ISABUS.HTP.Method");
        assert_eq!(BusKind::Smbus.mutex_name(), r"Global\Access_SMBUS.HTP.Method");
        assert_eq!(BusKind::Pci.mutex_name(), r"Global\Access_PCI");
    }

    #[test]
    fn a_second_thread_times_out_while_the_first_holds_the_mutex() {
        let locks = private_locks(Duration::from_millis(40));
        let first = locks.acquire(BusKind::Isa).unwrap();
        std::thread::scope(|scope| {
            let outcome = scope.spawn(|| locks.acquire(BusKind::Isa).map(|_| ())).join().unwrap();
            assert_eq!(outcome, Err(BusError::Contended(BusKind::Isa)));
        });
        drop(first);
        assert!(locks.acquire(BusKind::Isa).is_ok(), "the guard must release on drop");
    }

    #[test]
    fn buses_have_independent_mutexes() {
        let locks = private_locks(Duration::from_millis(40));
        let _isa = locks.acquire(BusKind::Isa).unwrap();
        let _smbus = locks.acquire(BusKind::Smbus).unwrap();
        let _pci = locks.acquire(BusKind::Pci).unwrap();
    }

    #[test]
    fn a_mutex_abandoned_by_a_dead_thread_is_acquired() {
        let locks = private_locks(Duration::from_millis(200));
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let guard = locks.acquire(BusKind::Smbus).unwrap();
                    // The thread ends while owning the mutex: the kernel
                    // marks it abandoned.
                    std::mem::forget(guard);
                })
                .join()
                .unwrap();
        });
        assert!(
            locks.acquire(BusKind::Smbus).is_ok(),
            "(abandoned mutex): expected WAIT_ABANDONED to count as acquired"
        );
    }

    #[test]
    fn two_instances_with_one_name_share_the_mutex() {
        let first = private_locks(Duration::from_millis(40));
        let second = SystemBusLocks::with_names(first.names.clone(), Duration::from_millis(40));
        let held = first.acquire(BusKind::Pci).unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| sender.send(second.acquire(BusKind::Pci).map(|_| ())).unwrap());
        });
        assert_eq!(receiver.recv().unwrap(), Err(BusError::Contended(BusKind::Pci)));
        drop(held);
    }

    #[test]
    fn the_guard_is_not_send() {
        // A `Send` guard would let a mutex be released on a thread that
        // does not own it. The two blanket impls below overlap exactly when
        // the type is `Send`, so the call compiles only for a `!Send` type.
        trait AmbiguousIfSend<Marker> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
        <BusGuard<'static> as AmbiguousIfSend<_>>::check();
    }

    #[test]
    fn fake_locks_count_live_guards_and_report_contention() {
        let locks = FakeBusLocks::held_by_another_program(&[BusKind::Smbus]);
        assert_eq!(
            locks.acquire(BusKind::Smbus).map(|_| ()),
            Err(BusError::Contended(BusKind::Smbus))
        );
        let guard = locks.acquire(BusKind::Isa).unwrap();
        assert_eq!(locks.live_guards(), 1);
        drop(guard);
        assert_eq!(locks.live_guards(), 0);
        assert_eq!(locks.acquired(), [BusKind::Isa]);
    }
}
