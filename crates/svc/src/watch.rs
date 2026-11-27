//! Registry change notifications.
//!
//! Each watched key has an event that Windows signals when the key changes
//! and a thread-pool wait that turns the signal into an
//! [`Event::RegistryChanged`]. The thread pool multiplexes any number of
//! waits, so there is no 64-handle limit and no thread per key. Nothing
//! runs while nothing changes.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use syspilot_engine::WatchSpec;
use syspilot_engine::os::Hive;
use syspilot_proto::win::{OwnedHandle, wide};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_NOTIFY, KEY_WOW64_64KEY, REG_NOTIFY_CHANGE_LAST_SET,
    REG_NOTIFY_CHANGE_NAME, REG_NOTIFY_THREAD_AGNOSTIC, RegCloseKey, RegNotifyChangeKeyValue,
    RegOpenKeyExW,
};
use windows_sys::Win32::System::Threading::{
    CloseThreadpoolWait, CreateEventW, CreateThreadpoolWait, PTP_CALLBACK_INSTANCE, PTP_WAIT,
    SetThreadpoolWait, WaitForThreadpoolWaitCallbacks,
};

use crate::events::{Event, EventQueue};

/// Notifications from one key within [`BURST_WINDOW`] before the watch is
/// paused. A key that something rewrites constantly must not turn into a
/// wake-up per write.
const BURST_LIMIT: u32 = 20;
const BURST_WINDOW: Duration = Duration::from_secs(1);
/// How long a bursting watch stays paused.
const BURST_PAUSE: Duration = Duration::from_secs(5);

/// What the thread-pool callback needs. Heap-allocated so its address stays
/// valid while the wait exists.
struct CallbackContext {
    queue: Arc<EventQueue>,
    id: u64,
}

/// Runs on a thread-pool thread when a watched key's event is signalled.
unsafe extern "system" fn on_key_changed(
    _instance: PTP_CALLBACK_INSTANCE,
    context: *mut c_void,
    _wait: PTP_WAIT,
    _result: u32,
) {
    // SAFETY: `context` is the `CallbackContext` registered with this wait.
    // `ActiveWatch::drop` waits for running callbacks before freeing it.
    let context = unsafe { &*context.cast::<CallbackContext>() };
    context.queue.push(Event::RegistryChanged(context.id));
}

/// One armed registry watch.
struct ActiveWatch {
    id: u64,
    spec: WatchSpec,
    key: HKEY,
    event: OwnedHandle,
    wait: PTP_WAIT,
    context: *mut CallbackContext,
    burst_started: Instant,
    burst_count: u32,
    /// Set while the watch is paused after a burst.
    resume_at: Option<Instant>,
}

impl ActiveWatch {
    fn open(spec: WatchSpec, id: u64, queue: &Arc<EventQueue>) -> Option<Self> {
        let (root, path) = match &spec.hive {
            Hive::Machine => (HKEY_LOCAL_MACHINE, wide(&spec.key)),
            Hive::User(sid) => (HKEY_USERS, wide(&format!(r"{sid}\{}", spec.key))),
        };
        let mut key: HKEY = ptr::null_mut();
        // SAFETY: `path` is NUL-terminated; `key` is a valid out-pointer.
        let status = unsafe {
            RegOpenKeyExW(root, path.as_ptr(), 0, KEY_NOTIFY | KEY_WOW64_64KEY, &mut key)
        };
        if status != 0 {
            return None;
        }
        // SAFETY: null attributes and name create an anonymous auto-reset
        // event, initially not signalled.
        let event = match OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) })
        {
            Ok(event) => event,
            Err(_) => {
                // SAFETY: `key` was opened above and is closed exactly once.
                unsafe { RegCloseKey(key) };
                return None;
            }
        };
        let context = Box::into_raw(Box::new(CallbackContext { queue: queue.clone(), id }));
        // SAFETY: the callback matches the required signature; `context`
        // stays valid until `drop` has waited for outstanding callbacks.
        let wait =
            unsafe { CreateThreadpoolWait(Some(on_key_changed), context.cast(), ptr::null()) };
        let mut watch = Self {
            id,
            spec,
            key,
            event,
            wait,
            context,
            burst_started: Instant::now(),
            burst_count: 0,
            resume_at: None,
        };
        // On failure `watch` is dropped here, which releases everything.
        (wait != 0 && watch.arm()).then_some(watch)
    }

    /// Asks for one notification of the next change. Returns `false` when
    /// the key can no longer be watched (it was deleted).
    fn arm(&mut self) -> bool {
        // SAFETY: `key` and `event` are valid for the lifetime of `self`.
        // The thread-agnostic flag keeps the registration alive regardless
        // of which thread made it.
        let status = unsafe {
            RegNotifyChangeKeyValue(
                self.key,
                i32::from(self.spec.subtree),
                REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_THREAD_AGNOSTIC,
                self.event.raw(),
                1,
            )
        };
        if status != 0 {
            return false;
        }
        // SAFETY: `wait` and `event` are valid; a null timeout waits
        // indefinitely.
        unsafe { SetThreadpoolWait(self.wait, self.event.raw(), ptr::null()) };
        true
    }
}

impl Drop for ActiveWatch {
    fn drop(&mut self) {
        if self.wait != 0 {
            // SAFETY: `wait` is valid. A null event clears the handle, which
            // stops new callbacks from being queued.
            unsafe { SetThreadpoolWait(self.wait, ptr::null_mut(), ptr::null()) };
            // SAFETY: `wait` is still valid. Waiting (and cancelling queued
            // callbacks) guarantees none is running when the context is freed
            // below.
            unsafe { WaitForThreadpoolWaitCallbacks(self.wait, 1) };
            // SAFETY: `wait` is valid, no callback is pending or running any
            // more, and it is closed exactly once.
            unsafe { CloseThreadpoolWait(self.wait) };
        }
        // SAFETY: `context` came from `Box::into_raw` in `open` and no
        // callback can use it any more (see above).
        drop(unsafe { Box::from_raw(self.context) });
        // SAFETY: `key` was opened in `open` and is closed exactly once.
        unsafe { RegCloseKey(self.key) };
    }
}

/// The set of registry keys currently watched.
pub struct WatchSet {
    queue: Arc<EventQueue>,
    active: Vec<ActiveWatch>,
    next_id: u64,
}

impl WatchSet {
    /// An empty set that reports changes to `queue`.
    pub fn new(queue: Arc<EventQueue>) -> Self {
        Self { queue, active: Vec::new(), next_id: 1 }
    }

    /// Makes the watched keys equal to `wanted`: existing watches are kept,
    /// missing ones added, the rest removed. A key that cannot be opened is
    /// skipped; it is retried on the next call.
    pub fn reconcile(&mut self, wanted: &[WatchSpec]) {
        self.active.retain(|watch| wanted.contains(&watch.spec));
        for spec in wanted {
            if self.active.iter().any(|watch| watch.spec == *spec) {
                continue;
            }
            if let Some(watch) = ActiveWatch::open(spec.clone(), self.next_id, &self.queue) {
                self.active.push(watch);
            }
            self.next_id += 1;
        }
    }

    /// Handles [`Event::RegistryChanged`]: re-arms the watch and returns
    /// what it watches, or `None` when the watch is gone or was just paused
    /// because the key changes too often.
    pub fn fired(&mut self, id: u64, now: Instant) -> Option<WatchSpec> {
        let position = self.active.iter().position(|watch| watch.id == id)?;
        let watch = &mut self.active[position];
        if now.duration_since(watch.burst_started) > BURST_WINDOW {
            watch.burst_started = now;
            watch.burst_count = 0;
        }
        watch.burst_count += 1;
        if watch.burst_count > BURST_LIMIT {
            // Stay disarmed for a while. `resume_due` re-arms and reports
            // the key as changed, so nothing that happened meanwhile is
            // missed.
            watch.resume_at = Some(now + BURST_PAUSE);
            return None;
        }
        // Arm before the caller reads the key, so a change that lands in
        // between still produces a notification.
        if watch.arm() {
            Some(watch.spec.clone())
        } else {
            // The key was deleted. Report the change; the next reconcile
            // picks a key that still exists.
            Some(self.active.remove(position).spec.clone())
        }
    }

    /// When the earliest paused watch should resume.
    pub fn next_resume(&self) -> Option<Instant> {
        self.active.iter().filter_map(|watch| watch.resume_at).min()
    }

    /// Re-arms the watches whose pause is over and returns them: their
    /// keys must be treated as changed.
    pub fn resume_due(&mut self, now: Instant) -> Vec<WatchSpec> {
        let mut resumed = Vec::new();
        self.active.retain_mut(|watch| {
            if watch.resume_at.is_none_or(|at| at > now) {
                return true;
            }
            watch.resume_at = None;
            watch.burst_count = 0;
            watch.burst_started = now;
            resumed.push(watch.spec.clone());
            watch.arm()
        });
        resumed
    }

    /// Number of keys being watched.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.active.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_engine::os::{RegValue, Registry, reg_type};
    use syspilot_engine::win::{WinRegistry, process_user_sid};
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    /// A scratch key in the current user's hive, removed on drop.
    struct ScratchKey {
        hive: Hive,
        key: String,
    }

    impl ScratchKey {
        fn new(tag: &str) -> Self {
            let hive = Hive::User(process_user_sid().unwrap());
            let key = format!(r"Software\SysPilot-Test\watch-{tag}-{}", std::process::id());
            let scratch = Self { hive, key };
            scratch.write("Value", 0);
            scratch
        }

        fn write(&self, name: &str, value: u32) {
            let value = RegValue { kind: reg_type::DWORD, data: value.to_le_bytes().to_vec() };
            WinRegistry.write(&self.hive, &self.key, name, &value).unwrap();
        }

        fn spec(&self) -> WatchSpec {
            WatchSpec { hive: self.hive.clone(), key: self.key.clone(), subtree: false }
        }
    }

    impl Drop for ScratchKey {
        fn drop(&mut self) {
            let registry = WinRegistry;
            for name in ["Value", "Other"] {
                let _ = registry.delete_value(&self.hive, &self.key, name);
            }
            let _ = registry.delete_key_if_empty(&self.hive, &self.key);
            let _ = registry.delete_key_if_empty(&self.hive, r"Software\SysPilot-Test");
        }
    }

    /// Waits for the next `RegistryChanged` event, up to ten seconds.
    fn next_change(queue: &EventQueue) -> Option<u64> {
        loop {
            match queue.pop() {
                Some(Event::RegistryChanged(id)) => return Some(id),
                Some(_) => continue,
                None => {}
            }
            // SAFETY: the handle is valid for the lifetime of `queue`.
            if unsafe { WaitForSingleObject(queue.signal(), 10_000) } != WAIT_OBJECT_0 {
                return None;
            }
        }
    }

    #[test]
    fn change_to_a_watched_key_is_reported_and_the_watch_re_arms() {
        let scratch = ScratchKey::new("change");
        let queue = EventQueue::new().unwrap();
        let mut watches = WatchSet::new(queue.clone());
        watches.reconcile(&[scratch.spec()]);
        assert_eq!(watches.len(), 1);

        for round in 1..=3 {
            scratch.write("Value", round);
            let id = next_change(&queue).expect("a change notification within ten seconds");
            assert_eq!(watches.fired(id, Instant::now()), Some(scratch.spec()), "round {round}");
        }
    }

    #[test]
    fn reconcile_keeps_existing_watches_and_drops_unwanted_ones() {
        let first = ScratchKey::new("keep");
        let second = ScratchKey::new("drop");
        let queue = EventQueue::new().unwrap();
        let mut watches = WatchSet::new(queue.clone());
        watches.reconcile(&[first.spec(), second.spec()]);
        assert_eq!(watches.len(), 2);

        watches.reconcile(&[first.spec()]);
        assert_eq!(watches.len(), 1);
        second.write("Value", 9);
        first.write("Value", 9);
        let id = next_change(&queue).unwrap();
        assert_eq!(
            watches.fired(id, Instant::now()),
            Some(first.spec()),
            "only the kept key reports"
        );
    }

    #[test]
    fn missing_key_is_skipped() {
        let queue = EventQueue::new().unwrap();
        let mut watches = WatchSet::new(queue);
        let missing = WatchSpec {
            hive: Hive::Machine,
            key: r"SOFTWARE\SysPilot-Test\no-such-key".into(),
            subtree: false,
        };
        watches.reconcile(&[missing]);
        assert_eq!(watches.len(), 0);
    }

    #[test]
    fn unknown_watch_id_is_ignored() {
        let mut watches = WatchSet::new(EventQueue::new().unwrap());
        assert_eq!(watches.fired(42, Instant::now()), None);
    }

    #[test]
    fn bursting_key_is_paused_then_resumed_as_changed() {
        let scratch = ScratchKey::new("burst");
        let queue = EventQueue::new().unwrap();
        let mut watches = WatchSet::new(queue.clone());
        watches.reconcile(&[scratch.spec()]);

        // One instant for the whole burst: the outcome must not depend on
        // how fast this machine delivers notifications.
        let burst_time = Instant::now();
        let mut paused = false;
        for round in 0..=BURST_LIMIT {
            // Always a new value: Windows does not notify for a write that
            // leaves the data unchanged.
            scratch.write("Value", round + 1);
            let id = next_change(&queue).unwrap();
            paused = watches.fired(id, burst_time).is_none();
        }
        assert!(paused, "the watch kept re-arming after {BURST_LIMIT} notifications in a second");
        let resume_at = watches.next_resume().expect("a paused watch has a resume time");

        assert!(watches.resume_due(burst_time).is_empty(), "the pause is not over yet");
        assert_eq!(watches.resume_due(resume_at), [scratch.spec()]);
        assert_eq!(watches.next_resume(), None);

        // Armed again: the next change is reported.
        scratch.write("Other", 1);
        let id = next_change(&queue).unwrap();
        assert_eq!(watches.fired(id, resume_at), Some(scratch.spec()));
    }
}
