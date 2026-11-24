//! The queue that funnels everything into the service's single event loop.
//!
//! Pipe threads, thread-pool callbacks and the service control handler all
//! push here; only the event loop pops. That keeps one writer for all state.

use std::collections::VecDeque;
use std::io;
use std::ptr;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, PoisonError};

use syspilot_proto::message::{Event as ClientEvent, Request, Response};
use syspilot_proto::win::OwnedHandle;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};

/// Something the event loop must react to.
pub enum Event {
    /// Stop the service.
    Stop,
    /// A client request, with where to send the response.
    Request {
        /// The request.
        /// Boxed: a request can carry a whole profile, and every other
        /// event is a few words.
        request: Box<Request>,
        /// Whether the client is an elevated administrator.
        elevated: bool,
        /// Channel the connection thread waits on.
        reply: SyncSender<Response>,
    },
    /// A client turned its connection into an event stream.
    Subscribe {
        /// Identifies the subscription in [`Event::Unsubscribed`].
        id: u64,
        /// Whether it reads live sensor values.
        sensors: bool,
        /// Where its events go. Full or closed means the client is gone.
        sink: SyncSender<ClientEvent>,
    },
    /// The subscriber's connection ended.
    Unsubscribed(u64),
    /// A watched registry key changed.
    RegistryChanged(u64),
    /// A user logged on to the given session.
    SessionLogon(u32),
    /// The machine is about to sleep.
    Suspend,
    /// The machine woke up.
    Resume,
    /// The updater ended a job.
    UpdateFinished {
        /// What it was asked to do.
        job: crate::updates::Job,
        /// What it reported, or why it failed.
        outcome: crate::updates::Outcome,
    },
}

/// A multi-producer queue with a Win32 event that is signalled while the
/// queue is non-empty, so the loop can wait on it together with a timer.
pub struct EventQueue {
    /// `None` once the loop has ended.
    events: Mutex<Option<VecDeque<Event>>>,
    signal: OwnedHandle,
}

impl EventQueue {
    /// Creates an empty queue.
    pub fn new() -> io::Result<Arc<Self>> {
        // SAFETY: null attributes and name create an anonymous event;
        // auto-reset, initially not signalled.
        let signal = OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) })?;
        Ok(Arc::new(Self { events: Mutex::new(Some(VecDeque::new())), signal }))
    }

    /// Appends an event and wakes the loop. After [`EventQueue::close`] the
    /// event is dropped instead.
    pub fn push(&self, event: Event) {
        let mut events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(queued) = events.as_mut() else { return };
        queued.push_back(event);
        drop(events);
        // SAFETY: `signal` is a valid event handle owned by `self`.
        unsafe { SetEvent(self.signal.raw()) };
    }

    /// Takes the oldest event.
    pub fn pop(&self) -> Option<Event> {
        self.events.lock().unwrap_or_else(PoisonError::into_inner).as_mut()?.pop_front()
    }

    /// Ends the queue: pending and future events are dropped. Dropping a
    /// request drops its reply channel, so a client that asks a loop which
    /// has ended gets a closed connection rather than no answer.
    pub fn close(&self) {
        *self.events.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The handle to wait on. Auto-reset: after a wake-up the loop must
    /// drain the queue before waiting again.
    pub fn signal(&self) -> HANDLE {
        self.signal.raw()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    fn is_signalled(queue: &EventQueue) -> bool {
        // SAFETY: the handle is valid for the lifetime of `queue`.
        match unsafe { WaitForSingleObject(queue.signal(), 0) } {
            WAIT_OBJECT_0 => true,
            WAIT_TIMEOUT => false,
            other => panic!("expected signalled or timeout | received: {other}"),
        }
    }

    #[test]
    fn events_come_out_in_order() {
        let queue = EventQueue::new().unwrap();
        queue.push(Event::RegistryChanged(1));
        queue.push(Event::SessionLogon(2));
        assert!(matches!(queue.pop(), Some(Event::RegistryChanged(1))));
        assert!(matches!(queue.pop(), Some(Event::SessionLogon(2))));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn pushing_signals_the_wait_handle_once() {
        let queue = EventQueue::new().unwrap();
        assert!(!is_signalled(&queue));
        queue.push(Event::Resume);
        queue.push(Event::Suspend);
        assert!(is_signalled(&queue), "a push wakes the loop");
        assert!(!is_signalled(&queue), "the event resets after one wake-up");
    }

    #[test]
    fn closing_drops_pending_and_later_events_with_their_reply_channels() {
        let queue = EventQueue::new().unwrap();
        let request =
            |reply| Event::Request { request: Box::new(Request::Status), elevated: false, reply };
        let (pending_reply, pending) = std::sync::mpsc::sync_channel(1);
        queue.push(request(pending_reply));
        queue.close();
        let (late_reply, late) = std::sync::mpsc::sync_channel(1);
        queue.push(request(late_reply));

        assert!(queue.pop().is_none());
        assert!(pending.recv().is_err(), "a request pending at close was left waiting");
        assert!(late.recv().is_err(), "a request after close was left waiting");
    }

    #[test]
    fn pushes_from_other_threads_arrive() {
        let queue = EventQueue::new().unwrap();
        std::thread::scope(|scope| {
            for id in 0..4 {
                let queue = &queue;
                scope.spawn(move || queue.push(Event::RegistryChanged(id)));
            }
        });
        let mut received: Vec<u64> = std::iter::from_fn(|| queue.pop())
            .map(|event| match event {
                Event::RegistryChanged(id) => id,
                _ => panic!("expected RegistryChanged"),
            })
            .collect();
        received.sort_unstable();
        assert_eq!(received, [0, 1, 2, 3]);
    }
}
