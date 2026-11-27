//! The request pipe.
//!
//! One listener thread accepts connections; each connection gets a thread
//! that does the blocking pipe I/O and forwards requests to the event loop.
//! The loop itself never touches a pipe, so a client that stops reading can
//! only stall its own thread.
//!
//! Security properties, all enforced here:
//!
//! - The first pipe instance is created exclusively, so another process
//!   cannot own the name first and impersonate the service.
//! - Remote clients are rejected.
//! - Local authenticated users may connect, but cannot create further
//!   instances of the pipe.
//! - Each connection's privilege is decided once, from the client's token,
//!   after the handshake.

use std::io;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use syspilot_proto::frame::{read_frame, write_frame};
use syspilot_proto::message::{
    ErrorCode, Event as ClientEvent, Request, Response, ServiceError, Welcome,
};
use syspilot_proto::win::{OwnedHandle, PipeStream, SecurityDescriptor, wide};
use syspilot_proto::{Instance, PROTOCOL_VERSION};
use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PeekNamedPipe,
};

use crate::events::{Event, EventQueue};
use crate::privilege::{is_process_elevated, pipe_client_is_elevated};

/// LocalSystem and Administrators: full control. Authenticated users: read
/// and write data, but not `FILE_CREATE_PIPE_INSTANCE` (which generic write
/// would include), so they cannot add instances of their own.
const PIPE_ACL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019b;;;AU)";
/// How often an idle subscriber connection checks that its client is still
/// there. Only connections that carry no sensor ticks ever wait this long.
const LIVENESS_CHECK: Duration = Duration::from_secs(20);

static NEXT_SUBSCRIBER: AtomicU64 = AtomicU64::new(1);

/// The pipe's security descriptor.
///
/// Clients only trust a pipe owned by LocalSystem or Administrators. A
/// service gets such an owner by default, an elevated console run does not
/// (the owner would be the user), so the owner is set explicitly. A process
/// that is not elevated may not assign that owner; it keeps its own, and
/// clients refuse it, which is the right outcome for an unprivileged server.
fn pipe_sddl() -> String {
    let owner = if is_process_elevated() { "O:BA" } else { "" };
    format!("{owner}{PIPE_ACL}")
}
/// Connections served at once. Further clients wait for a free instance.
const MAX_CONNECTIONS: usize = 32;
/// Events buffered per subscriber before it is considered gone.
const SUBSCRIBER_BACKLOG: usize = 64;
const BUFFER_BYTES: u32 = 64 * 1024;

/// Counts live connections so the listener can stop at the limit.
#[derive(Default)]
struct ConnectionLimit {
    live: Mutex<usize>,
    freed: Condvar,
}

impl ConnectionLimit {
    /// Blocks until a connection slot is free, then takes it.
    fn acquire(&self) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        while *live >= MAX_CONNECTIONS {
            live = self.freed.wait(live).unwrap_or_else(PoisonError::into_inner);
        }
        *live += 1;
    }

    fn release(&self) {
        *self.live.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        self.freed.notify_one();
    }
}

/// What the loop has handed to connections but they have not written yet:
/// answers to requests, and the events of live subscriptions. The loop
/// hands them to each connection's thread, which writes them; a process
/// that ends right after its last answer and its `Stopping` event, as a
/// console run does on `Shutdown`, waits here so that they are not lost.
#[derive(Debug, Default)]
pub struct PendingWrites {
    unwritten: Mutex<usize>,
    written: Condvar,
}

/// One answer, or one subscription's events, owed; dropping it, once they
/// are written or never will be, settles it.
struct OwedWrite<'a>(&'a PendingWrites);

impl Drop for OwedWrite<'_> {
    fn drop(&mut self) {
        *self.0.unwritten.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        self.0.written.notify_all();
    }
}

impl PendingWrites {
    fn owe(&self) -> OwedWrite<'_> {
        *self.unwritten.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        OwedWrite(self)
    }

    /// Waits until everything owed has been written, or `bound` has passed.
    /// Returns whether it all was. Once the loop has shut down, something is
    /// only still owed while its write is under way: parked and queued
    /// requests were failed, and each subscription's channel closed after
    /// its `Stopping` event. A client that stops reading holds this up to
    /// `bound`.
    pub fn wait_until_written(&self, bound: Duration) -> bool {
        let unwritten = self.unwritten.lock().unwrap_or_else(PoisonError::into_inner);
        let (unwritten, _) = self
            .written
            .wait_timeout_while(unwritten, bound, |unwritten| *unwritten > 0)
            .unwrap_or_else(PoisonError::into_inner);
        *unwritten == 0
    }
}

fn create_instance(
    name: &[u16],
    security: &SecurityDescriptor,
    first: bool,
) -> io::Result<OwnedHandle> {
    let attributes = security.attributes();
    let open_mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    // SAFETY: `name` is NUL-terminated; `attributes` borrows `security`,
    // which outlives the call.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_BYTES,
            BUFFER_BYTES,
            0,
            &attributes,
        )
    };
    OwnedHandle::new(handle)
}

/// Creates the pipe and starts serving it. Fails if the name is already
/// taken, which means another copy (or an impostor) is running. Returns what
/// its connections have yet to write.
pub fn start(instance: &Instance, queue: Arc<EventQueue>) -> io::Result<Arc<PendingWrites>> {
    let name = wide(&instance.pipe_name());
    let security = SecurityDescriptor::from_sddl(&pipe_sddl())?;
    let first = create_instance(&name, &security, true)?;
    let pending = Arc::new(PendingWrites::default());
    let for_listener = pending.clone();
    std::thread::Builder::new()
        .name("pipe-listener".into())
        .spawn(move || listen(name, security, first, queue, for_listener))?;
    Ok(pending)
}

fn listen(
    name: Vec<u16>,
    security: SecurityDescriptor,
    first: OwnedHandle,
    queue: Arc<EventQueue>,
    pending: Arc<PendingWrites>,
) {
    let limit = Arc::new(ConnectionLimit::default());
    let mut next = Some(first);
    loop {
        limit.acquire();
        let instance =
            match next.take().map_or_else(|| create_instance(&name, &security, false), Ok) {
                Ok(instance) => instance,
                // Out of resources: the service keeps enforcing without a pipe.
                Err(_) => return,
            };
        // SAFETY: `instance` is a pipe handle opened for synchronous I/O, so
        // no overlapped structure is needed.
        let connected = unsafe { ConnectNamedPipe(instance.raw(), ptr::null_mut()) } != 0
            // A client that connected between creation and this call.
            // SAFETY: trivially safe; reads thread-local state.
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        if !connected {
            limit.release();
            continue;
        }
        let stream = PipeStream::from_handle(instance);
        let (queue, limit_for_thread, pending) = (queue.clone(), limit.clone(), pending.clone());
        let spawned = std::thread::Builder::new().name("pipe-connection".into()).spawn(move || {
            serve(stream, &queue, &pending);
            limit_for_thread.release();
        });
        if spawned.is_err() {
            limit.release();
        }
    }
}

fn refuse(stream: &mut PipeStream, code: ErrorCode, message: impl Into<String>) {
    let _ = write_frame(stream, &Response::Error(ServiceError { code, message: message.into() }));
}

/// Whether the client end of `stream` is still open.
fn client_is_connected(stream: &PipeStream) -> bool {
    let mut available = 0;
    // SAFETY: the handle is a valid pipe handle; no data is copied (null
    // buffer of size zero) and `available` is a valid out-pointer.
    let peeked = unsafe {
        PeekNamedPipe(
            stream.handle().raw(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            &mut available,
            ptr::null_mut(),
        )
    };
    peeked != 0
}

/// Writes events to a subscriber until it leaves or the loop drops it.
fn forward_events(stream: &mut PipeStream, events: &Receiver<ClientEvent>) {
    loop {
        match events.recv_timeout(LIVENESS_CHECK) {
            Ok(event) => {
                if write_frame(stream, &Response::Event(event)).is_err() {
                    return;
                }
            }
            // A quiet subscription gives no failed write to notice a closed
            // client by, so ask the pipe.
            Err(RecvTimeoutError::Timeout) => {
                if !client_is_connected(stream) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Serves one connection until the client leaves or the service stops.
fn serve(mut stream: PipeStream, queue: &EventQueue, pending: &PendingWrites) {
    match read_frame(&mut stream) {
        Ok(Request::Hello { protocol }) if protocol == PROTOCOL_VERSION => {}
        Ok(Request::Hello { protocol }) => {
            let message = format!(
                "The service speaks protocol version {PROTOCOL_VERSION} and the app version {protocol}. Update both to the same release."
            );
            return refuse(&mut stream, ErrorCode::ProtocolMismatch, message);
        }
        Ok(_) => {
            return refuse(&mut stream, ErrorCode::Invalid, "(connection): expected Hello first");
        }
        Err(_) => return,
    }
    // Decided once: a token cannot gain privileges during a connection.
    let elevated = pipe_client_is_elevated(&stream);
    let welcome =
        Welcome { protocol: PROTOCOL_VERSION, version: env!("CARGO_PKG_VERSION").into(), elevated };
    if write_frame(&mut stream, &Response::Hello(welcome)).is_err() {
        return;
    }

    loop {
        let request = match read_frame(&mut stream) {
            Ok(request) => request,
            Err(_) => return,
        };
        match request {
            Request::Hello { .. } => {
                refuse(
                    &mut stream,
                    ErrorCode::Invalid,
                    "(connection): Hello was already exchanged",
                );
            }
            Request::Subscribe { sensors } => {
                // Owed for the whole subscription: the loop's shutdown sends
                // `Stopping` and then closes the channel, so this settles
                // once that last event is written.
                let _owed = pending.owe();
                let id = NEXT_SUBSCRIBER.fetch_add(1, Ordering::Relaxed);
                let (sink, events) = sync_channel(SUBSCRIBER_BACKLOG);
                queue.push(Event::Subscribe { id, sensors, sink });
                // From here the connection only carries events.
                if write_frame(&mut stream, &Response::Subscribed).is_ok() {
                    forward_events(&mut stream, &events);
                }
                queue.push(Event::Unsubscribed(id));
                return;
            }
            request => {
                // Owed from before the loop can see the request until the
                // answer is written, or the connection gives up on it.
                let _owed = pending.owe();
                let (reply, response) = sync_channel(1);
                queue.push(Event::Request { request: Box::new(request), elevated, reply });
                // A closed channel means the loop is shutting down.
                let Ok(response) = response.recv() else { return };
                if write_frame(&mut stream, &response).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use syspilot_proto::Client;

    static NEXT_INSTANCE: AtomicUsize = AtomicUsize::new(0);

    /// A pipe server on a unique test instance. The test plays the event
    /// loop by taking events off the queue.
    struct TestServer {
        instance: Instance,
        queue: Arc<EventQueue>,
        pending: Arc<PendingWrites>,
    }

    impl TestServer {
        fn start() -> Self {
            let name = format!(
                "SysPilot-Test.pipe-{}-{}",
                std::process::id(),
                NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
            );
            let instance = Instance::named(&name).unwrap();
            let queue = EventQueue::new().unwrap();
            let pending = start(&instance, queue.clone()).unwrap();
            Self { instance, queue, pending }
        }

        /// Connects without the pipe-owner check of `Client::connect`: a
        /// test run by a standard user owns its own pipe, which that check
        /// rightly refuses. The check is covered by the service tests.
        fn connect(&self) -> Client {
            let stream = PipeStream::connect(&self.instance.pipe_name(), 3000).unwrap();
            Client::handshake(stream).unwrap()
        }

        /// Blocks until an event arrives and returns it.
        fn next_event(&self) -> Event {
            loop {
                if let Some(event) = self.queue.pop() {
                    return event;
                }
                // SAFETY: the handle is valid for the lifetime of the queue.
                unsafe {
                    windows_sys::Win32::System::Threading::WaitForSingleObject(
                        self.queue.signal(),
                        10_000,
                    )
                };
            }
        }
    }

    #[test]
    fn handshake_reports_the_clients_elevation() {
        let server = TestServer::start();
        let client = server.connect();
        assert_eq!(client.welcome().protocol, PROTOCOL_VERSION);
        assert_eq!(client.welcome().version, env!("CARGO_PKG_VERSION"));
        assert_eq!(client.welcome().elevated, crate::privilege::is_process_elevated());
    }

    #[test]
    fn request_is_forwarded_and_its_reply_returned() {
        let server = TestServer::start();
        let mut client = server.connect();
        std::thread::scope(|scope| {
            let request = scope.spawn(|| client.request(&Request::States));
            match server.next_event() {
                Event::Request { request, reply, .. } => {
                    assert_eq!(*request, Request::States);
                    reply.send(Response::States(vec![])).unwrap();
                }
                _ => panic!("expected a Request event"),
            }
            assert_eq!(request.join().unwrap().unwrap(), Response::States(vec![]));
        });
    }

    #[test]
    fn waiting_for_replies_returns_at_once_when_none_is_owed() {
        let pending = PendingWrites::default();
        assert!(pending.wait_until_written(Duration::ZERO), "(owed replies): expected none");
    }

    #[test]
    fn waiting_for_replies_gives_up_at_its_bound() {
        let pending = PendingWrites::default();
        let owed = pending.owe();
        assert!(
            !pending.wait_until_written(Duration::from_millis(20)),
            "(wait with a reply owed): expected false at the bound | received: true"
        );
        drop(owed);
        assert!(
            pending.wait_until_written(Duration::ZERO),
            "(owed replies after settling): expected none"
        );
    }

    /// The regression behind a console run's `Shutdown`: the loop hands its
    /// answer over and the process may end straight after, so the answer
    /// must count as owed until the connection has written it.
    #[test]
    fn an_answer_is_owed_from_the_request_until_it_is_written() {
        let server = TestServer::start();
        let mut client = server.connect();
        std::thread::scope(|scope| {
            let request = scope.spawn(|| client.request(&Request::States));
            let Event::Request { reply, .. } = server.next_event() else {
                panic!("expected a Request event");
            };
            assert!(
                !server.pending.wait_until_written(Duration::from_millis(50)),
                "(reply before the loop answered): expected owed | received: written"
            );
            reply.send(Response::States(vec![])).unwrap();
            assert!(
                server.pending.wait_until_written(Duration::from_secs(5)),
                "(reply after the loop answered): expected written within 5 s | received: still owed"
            );
            assert_eq!(request.join().unwrap().unwrap(), Response::States(vec![]));
        });
    }

    /// Pins "until written", not "until handed over": an answer larger
    /// than the pipe's buffers cannot be written while its client does not
    /// read, so it must still be owed then.
    #[test]
    fn an_answer_stays_owed_while_its_write_is_under_way() {
        let server = TestServer::start();
        let mut stream = PipeStream::connect(&server.instance.pipe_name(), 3000).unwrap();
        write_frame(&mut stream, &Request::Hello { protocol: PROTOCOL_VERSION }).unwrap();
        assert!(matches!(read_frame(&mut stream).unwrap(), Response::Hello(_)));
        write_frame(&mut stream, &Request::States).unwrap();
        let Event::Request { reply, .. } = server.next_event() else {
            panic!("expected a Request event");
        };
        let message = "x".repeat(4 * BUFFER_BYTES as usize);
        let large = Response::Error(ServiceError { code: ErrorCode::Invalid, message });
        reply.send(large.clone()).unwrap();
        assert!(
            !server.pending.wait_until_written(Duration::from_millis(200)),
            "(answer larger than the pipe buffers, client not reading): expected owed | received: written"
        );
        assert_eq!(read_frame::<_, Response>(&mut stream).unwrap(), large);
        assert!(
            server.pending.wait_until_written(Duration::from_secs(5)),
            "(answer once the client read it): expected written within 5 s | received: still owed"
        );
    }

    #[test]
    fn an_answer_the_loop_drops_is_no_longer_owed() {
        let server = TestServer::start();
        let mut client = server.connect();
        std::thread::scope(|scope| {
            let request = scope.spawn(|| client.request(&Request::States));
            let Event::Request { reply, .. } = server.next_event() else {
                panic!("expected a Request event");
            };
            drop(reply);
            assert!(
                server.pending.wait_until_written(Duration::from_secs(5)),
                "(reply the loop dropped): expected settled within 5 s | received: still owed"
            );
            assert!(
                request.join().unwrap().is_err(),
                "(client of a dropped reply): expected an error"
            );
        });
    }

    #[test]
    fn second_first_instance_is_refused() {
        let server = TestServer::start();
        let error = start(&server.instance, EventQueue::new().unwrap()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[test]
    fn wrong_protocol_version_is_refused_with_both_versions() {
        let server = TestServer::start();
        let mut stream = PipeStream::connect(&server.instance.pipe_name(), 3000).unwrap();
        write_frame(&mut stream, &Request::Hello { protocol: PROTOCOL_VERSION + 1 }).unwrap();
        match read_frame(&mut stream).unwrap() {
            Response::Error(error) => {
                assert_eq!(error.code, ErrorCode::ProtocolMismatch);
                assert!(
                    error.message.contains(&format!("version {}", PROTOCOL_VERSION + 1)),
                    "{}",
                    error.message
                );
            }
            other => panic!("expected ProtocolMismatch | received: {other:?}"),
        }
    }

    #[test]
    fn request_before_hello_is_refused() {
        let server = TestServer::start();
        let mut stream = PipeStream::connect(&server.instance.pipe_name(), 3000).unwrap();
        write_frame(&mut stream, &Request::Status).unwrap();
        match read_frame(&mut stream).unwrap() {
            Response::Error(error) => assert_eq!(error.code, ErrorCode::Invalid),
            other => panic!("expected Invalid | received: {other:?}"),
        }
    }

    #[test]
    fn subscriber_receives_events_and_its_departure_is_noticed() {
        let server = TestServer::start();
        let mut events = server.connect().subscribe(true).unwrap();
        let Event::Subscribe { id, sensors, sink } = server.next_event() else {
            panic!("expected a Subscribe event");
        };
        assert!(sensors);

        sink.send(ClientEvent::StatesChanged).unwrap();
        assert_eq!(events.next_event().unwrap(), ClientEvent::StatesChanged);

        drop(events);
        // The connection thread notices the closed pipe on its next write,
        // drops the receiving end and tells the loop.
        let departed = (0..1000).any(|_| {
            std::thread::yield_now();
            sink.send(ClientEvent::Sampled).is_err()
        });
        assert!(departed, "the subscriber was never detected as gone");
        match server.next_event() {
            Event::Unsubscribed(gone) => assert_eq!(gone, id),
            _ => panic!("expected an Unsubscribed event"),
        }
    }

    /// The regression behind #65: the loop's shutdown sends `Stopping` and
    /// closes the subscription, and the process may end straight after, so
    /// the subscription must stay owed until that event is written.
    #[test]
    fn a_subscription_is_owed_until_its_last_event_is_written() {
        let server = TestServer::start();
        let mut events = server.connect().subscribe(false).unwrap();
        let Event::Subscribe { sink, .. } = server.next_event() else {
            panic!("expected a Subscribe event");
        };
        assert!(
            !server.pending.wait_until_written(Duration::from_millis(50)),
            "(live subscription): expected owed | received: settled"
        );
        // What `Host::shut_down` does: the last event, then the channel closes.
        sink.send(ClientEvent::Stopping).unwrap();
        drop(sink);
        assert!(
            server.pending.wait_until_written(Duration::from_secs(5)),
            "(closed subscription): expected settled within 5 s | received: still owed"
        );
        assert_eq!(events.next_event().unwrap(), ClientEvent::Stopping);
    }

    #[test]
    fn closed_client_is_detected_without_writing() {
        let name = format!(r"\\.\pipe\SysPilot-Test.pipe-peek-{}", std::process::id());
        let security = SecurityDescriptor::from_sddl(&pipe_sddl()).unwrap();
        let server =
            PipeStream::from_handle(create_instance(&wide(&name), &security, true).unwrap());
        let client = PipeStream::connect(&name, 3000).unwrap();
        assert!(client_is_connected(&server));
        drop(client);
        assert!(!client_is_connected(&server));
    }

    #[test]
    fn clients_trust_the_pipe_only_when_the_server_is_elevated() {
        let server = TestServer::start();
        match Client::connect(&server.instance) {
            Ok(_) => assert!(is_process_elevated(), "a pipe owned by a standard user was trusted"),
            Err(error) => {
                assert!(!is_process_elevated(), "{error:?}");
                assert!(matches!(error, syspilot_proto::ClientError::UntrustedServer), "{error:?}");
            }
        }
    }
}
