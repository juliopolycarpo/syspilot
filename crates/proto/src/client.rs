//! Pipe client used by the UI, the CLI and the integration tests.

use std::fmt;
use std::io::{self, Read, Write};

use crate::frame::{FrameError, read_frame, write_frame};
use crate::message::{ErrorCode, Event, Request, Response, ServiceError, Welcome};
use crate::product::{Instance, PROTOCOL_VERSION};
use crate::win::{PipeCanceller, PipeStream, is_owned_by_system_or_administrators};

/// Milliseconds to wait for a free pipe instance when the service is busy.
const CONNECT_TIMEOUT_MS: u32 = 3000;

/// Why talking to the service failed.
#[derive(Debug)]
pub enum ClientError {
    /// The pipe does not exist: the service is not running.
    NotRunning,
    /// The pipe exists but was not created by LocalSystem or an
    /// administrator. Something else is impersonating the service.
    UntrustedServer,
    /// The service speaks another protocol version.
    ProtocolMismatch {
        /// What the service said about it.
        message: String,
    },
    /// The service answered with an error.
    Service(ServiceError),
    /// The service sent a response that does not fit the request.
    UnexpectedResponse {
        /// What the client was waiting for.
        expected: &'static str,
    },
    /// The connection broke.
    Frame(FrameError),
    /// Connecting failed for another reason.
    Io(io::Error),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning => f.write_str("the service is not running"),
            Self::UntrustedServer => {
                f.write_str("the pipe is not owned by the system; refusing to talk to it")
            }
            Self::ProtocolMismatch { message } => write!(f, "protocol mismatch: {message}"),
            Self::Service(error) => write!(f, "{}", error.message),
            Self::UnexpectedResponse { expected } => {
                write!(f, "(response): expected {expected} | received: another response type")
            }
            Self::Frame(error) => error.fmt(f),
            Self::Io(error) => write!(f, "cannot connect to the service: {error}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<FrameError> for ClientError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

impl ClientError {
    /// Whether the failure is the service refusing a mutating request from
    /// a client that is not elevated.
    pub fn is_access_denied(&self) -> bool {
        matches!(self, Self::Service(error) if error.code == ErrorCode::AccessDenied)
    }
}

/// A request/response connection to the service.
///
/// Generic over the transport so the protocol logic is testable without a
/// pipe; real clients use [`Client::connect`].
#[derive(Debug)]
pub struct Client<S = PipeStream> {
    stream: S,
    welcome: Welcome,
}

impl Client<PipeStream> {
    /// Connects to the service of `instance` and performs the handshake.
    pub fn connect(instance: &Instance) -> Result<Self, ClientError> {
        let stream =
            PipeStream::connect(&instance.pipe_name(), CONNECT_TIMEOUT_MS).map_err(|error| {
                match error.kind() {
                    io::ErrorKind::NotFound => ClientError::NotRunning,
                    _ => ClientError::Io(error),
                }
            })?;
        // Requests can carry commands that will run as SYSTEM, and responses
        // drive what an elevated UI shows: only talk to the real service.
        if !is_owned_by_system_or_administrators(stream.handle()).map_err(ClientError::Io)? {
            return Err(ClientError::UntrustedServer);
        }
        Self::handshake(stream)
    }

    /// A token that aborts a blocked call on this connection from another
    /// thread.
    pub fn canceller(&self) -> PipeCanceller {
        self.stream.canceller()
    }
}

impl<S: Read + Write> Client<S> {
    /// Performs the handshake over an established transport.
    pub fn handshake(mut stream: S) -> Result<Self, ClientError> {
        write_frame(&mut stream, &Request::Hello { protocol: PROTOCOL_VERSION })?;
        match read_frame(&mut stream)? {
            Response::Hello(welcome) => Ok(Self { stream, welcome }),
            Response::Error(error) if error.code == ErrorCode::ProtocolMismatch => {
                Err(ClientError::ProtocolMismatch { message: error.message })
            }
            Response::Error(error) => Err(ClientError::Service(error)),
            _ => Err(ClientError::UnexpectedResponse { expected: "Hello" }),
        }
    }

    /// What the service said in the handshake.
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Sends one request and returns its response. A [`Response::Error`]
    /// becomes [`ClientError::Service`].
    pub fn request(&mut self, request: &Request) -> Result<Response, ClientError> {
        write_frame(&mut self.stream, request)?;
        match read_frame(&mut self.stream)? {
            Response::Error(error) => Err(ClientError::Service(error)),
            response => Ok(response),
        }
    }

    /// Turns the connection into an event stream.
    pub fn subscribe(mut self, sensors: bool) -> Result<EventStream<S>, ClientError> {
        match self.request(&Request::Subscribe { sensors })? {
            Response::Subscribed => Ok(EventStream { stream: self.stream }),
            _ => Err(ClientError::UnexpectedResponse { expected: "Subscribed" }),
        }
    }
}

/// A subscribed connection. Dropping it ends the subscription.
#[derive(Debug)]
pub struct EventStream<S = PipeStream> {
    stream: S,
}

impl EventStream<PipeStream> {
    /// A token that aborts a blocked [`EventStream::next_event`] from
    /// another thread.
    pub fn canceller(&self) -> PipeCanceller {
        self.stream.canceller()
    }
}

impl<S: Read> EventStream<S> {
    /// Blocks until the service pushes the next event.
    pub fn next_event(&mut self) -> Result<Event, ClientError> {
        match read_frame(&mut self.stream)? {
            Response::Event(event) => Ok(event),
            _ => Err(ClientError::UnexpectedResponse { expected: "Event" }),
        }
    }
}

/// Generates typed request helpers that unwrap the matching response.
macro_rules! typed_requests {
    ($($(#[$doc:meta])* $name:ident($($arg:ident: $ty:ty),*) => $request:expr, $variant:ident -> $out:ty;)*) => {
        impl<S: Read + Write> Client<S> {
            $(
                $(#[$doc])*
                pub fn $name(&mut self $(, $arg: $ty)*) -> Result<$out, ClientError> {
                    match self.request(&$request)? {
                        Response::$variant(value) => Ok(value),
                        _ => Err(ClientError::UnexpectedResponse { expected: stringify!($variant) }),
                    }
                }
            )*
        }
    };
}

typed_requests! {
    /// The tweak catalog.
    catalog() => Request::Catalog, Catalog -> crate::catalog::Catalog;
    /// State of every tweak and custom action.
    states() => Request::States, States -> Vec<crate::message::EntryStatus>;
    /// The stored profile.
    profile() => Request::Profile, Profile -> crate::profile::Profile;
    /// Recent enforcement events, newest first.
    history(limit: u32) => Request::History { limit }, History -> Vec<crate::message::HistoryEvent>;
    /// Sensor metadata.
    sensors() => Request::Sensors, Sensors -> crate::sensor::SensorList;
    /// Static system information.
    system_info() => Request::SystemInfo, SystemInfo -> crate::message::SystemInfo;
}

impl<S: Read + Write> Client<S> {
    /// Service health and capabilities.
    pub fn status(&mut self) -> Result<crate::message::ServiceStatus, ClientError> {
        // Not one of the generated helpers: this response comes boxed.
        match self.request(&Request::Status)? {
            Response::Status(status) => Ok(*status),
            _ => Err(ClientError::UnexpectedResponse { expected: "Status" }),
        }
    }

    /// Sends a request whose only successful answer is [`Response::Ok`].
    pub fn command(&mut self, request: &Request) -> Result<(), ClientError> {
        match self.request(request)? {
            Response::Ok => Ok(()),
            _ => Err(ClientError::UnexpectedResponse { expected: "Ok" }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// An in-memory transport: reads come from a script of pre-encoded
    /// responses, writes are recorded.
    #[derive(Debug)]
    struct ScriptedTransport {
        responses: Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl ScriptedTransport {
        fn replying(responses: &[Response]) -> Self {
            let mut wire = Vec::new();
            for response in responses {
                write_frame(&mut wire, response).unwrap();
            }
            Self { responses: Cursor::new(wire), sent: Vec::new() }
        }

        fn sent_requests(&self) -> Vec<Request> {
            let mut cursor = Cursor::new(self.sent.clone());
            std::iter::from_fn(|| read_frame(&mut cursor).ok()).collect()
        }
    }

    impl Read for ScriptedTransport {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.responses.read(buffer)
        }
    }

    impl Write for ScriptedTransport {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.sent.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn welcome(elevated: bool) -> Response {
        Response::Hello(Welcome { protocol: PROTOCOL_VERSION, version: "0.1.0".into(), elevated })
    }

    fn service_error(code: ErrorCode) -> Response {
        Response::Error(ServiceError { code, message: "refused".into() })
    }

    #[test]
    fn handshake_sends_the_protocol_version_first() {
        let client = Client::handshake(ScriptedTransport::replying(&[welcome(true)])).unwrap();
        assert!(client.welcome().elevated);
        assert_eq!(client.stream.sent_requests(), [Request::Hello { protocol: PROTOCOL_VERSION }]);
    }

    #[test]
    fn protocol_mismatch_is_a_distinct_error() {
        let transport = ScriptedTransport::replying(&[service_error(ErrorCode::ProtocolMismatch)]);
        let error = Client::handshake(transport).unwrap_err();
        assert!(matches!(error, ClientError::ProtocolMismatch { .. }), "{error:?}");
    }

    #[test]
    fn service_errors_surface_with_their_code() {
        let transport =
            ScriptedTransport::replying(&[welcome(false), service_error(ErrorCode::AccessDenied)]);
        let mut client = Client::handshake(transport).unwrap();
        let error = client.command(&Request::ClearHistory).unwrap_err();
        assert!(error.is_access_denied(), "{error:?}");
    }

    #[test]
    fn typed_helper_rejects_a_mismatched_response() {
        let transport = ScriptedTransport::replying(&[welcome(false), Response::Ok]);
        let mut client = Client::handshake(transport).unwrap();
        let error = client.states().unwrap_err();
        assert!(
            matches!(error, ClientError::UnexpectedResponse { expected: "States" }),
            "{error:?}"
        );
    }

    #[test]
    fn subscription_yields_pushed_events_then_reports_the_close() {
        let transport = ScriptedTransport::replying(&[
            welcome(false),
            Response::Subscribed,
            Response::Event(Event::StatesChanged),
            Response::Event(Event::Stopping),
        ]);
        let mut events = Client::handshake(transport).unwrap().subscribe(true).unwrap();
        assert_eq!(events.next_event().unwrap(), Event::StatesChanged);
        assert_eq!(events.next_event().unwrap(), Event::Stopping);
        let error = events.next_event().unwrap_err();
        assert!(matches!(error, ClientError::Frame(FrameError::Closed)), "{error:?}");
    }

    #[test]
    fn connecting_without_a_service_reports_not_running() {
        let instance = Instance::named("SysPilot-Test.absent").unwrap();
        let error = Client::connect(&instance).unwrap_err();
        assert!(matches!(error, ClientError::NotRunning), "{error:?}");
    }
}
