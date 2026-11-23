//! Where the app's data comes from.
//!
//! The views never talk to the pipe. They see a [`Backend`], which is the
//! service in normal use and a scripted fixture in demo mode and in tests.

use std::fmt;
use std::sync::Arc;

use syspilot_proto::message::{Event, Request};
use syspilot_proto::sensor::{Sample, SectionReader};
use syspilot_proto::win::MappedSection;
use syspilot_proto::{Client, ClientError, EventStream, Instance};

use crate::model::Snapshot;

/// Enforcement events shown in the history page.
const HISTORY_LIMIT: u32 = 200;

/// Why the backend could not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The service is not installed or not started.
    NotRunning,
    /// Anything else, as a sentence for the user.
    Other(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning => f.write_str("The SysPilot service is not running."),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl From<ClientError> for Failure {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::NotRunning => Self::NotRunning,
            other => Self::Other(other.to_string()),
        }
    }
}

/// Something the service reported on its event stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// States, profile, status or the sensor set changed: load again.
    Data,
    /// A new sensor sample.
    Sample {
        /// Sensor set the IDs belong to.
        generation: u32,
        /// One reading per sensor.
        samples: Vec<Sample>,
    },
}

/// The app's source of data and its way to change things.
pub trait Backend: Send + Sync {
    /// Reads everything the views show.
    fn load(&self) -> Result<Snapshot, Failure>;
    /// Asks for a change. Refused unless the app is elevated.
    fn send(&self, request: &Request) -> Result<(), Failure>;
    /// Starts listening for changes. While a listener exists, sensors are
    /// being sampled.
    fn listen(&self) -> Result<Box<dyn Listener>, Failure>;
}

/// An open stream of changes.
pub trait Listener: Send {
    /// Blocks until the next change. An error ends the stream.
    fn next_change(&mut self) -> Result<Change, Failure>;
}

/// A backend handle that can be part of a component's input.
#[derive(Clone)]
pub struct BackendHandle(pub Arc<dyn Backend>);

impl PartialEq for BackendHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The real service, reached over its pipe.
#[derive(Debug)]
pub struct ServiceBackend {
    instance: Instance,
}

impl ServiceBackend {
    /// A backend for the service running as `instance`.
    pub fn new(instance: Instance) -> Self {
        Self { instance }
    }
}

impl Backend for ServiceBackend {
    fn load(&self) -> Result<Snapshot, Failure> {
        let mut client = Client::connect(&self.instance)?;
        let elevated = client.welcome().elevated;
        // The profile and the history are for administrators only; asking
        // without elevation would just be refused.
        let (profile, history) = if elevated {
            (Some(client.profile()?), client.history(HISTORY_LIMIT)?)
        } else {
            (None, Vec::new())
        };
        let (catalog, states) = (client.catalog()?, client.states()?);
        // The sensors come before the status: listing them is what makes
        // the service look at the hardware, and the status reports what
        // that look found.
        let (sensors, system) = (client.sensors()?, client.system_info()?);
        let status = Some(client.status()?);
        Ok(Snapshot { elevated, status, catalog, states, profile, history, sensors, system })
    }

    fn send(&self, request: &Request) -> Result<(), Failure> {
        Ok(Client::connect(&self.instance)?.command(request)?)
    }

    fn listen(&self) -> Result<Box<dyn Listener>, Failure> {
        let stream = Client::connect(&self.instance)?.subscribe(true)?;
        Ok(Box::new(ServiceListener { stream, instance: self.instance.clone(), section: None }))
    }
}

/// The service's event stream plus its shared sensor section.
struct ServiceListener {
    stream: EventStream,
    instance: Instance,
    /// Mapped on the first sample: the service creates it when sampling
    /// starts.
    section: Option<MappedSection>,
}

impl ServiceListener {
    /// Copies the current sample out of the section. `None` when the
    /// section is not there (yet) or was caught mid-write every time; the
    /// next sample is a second away.
    fn read_sample(&mut self) -> Option<Change> {
        if self.section.is_none() {
            self.section = MappedSection::open(&self.instance.sensor_section_name()).ok();
        }
        let reader = SectionReader::new(self.section.as_ref()?.words()).ok()?;
        let mut samples = Vec::new();
        let header = reader.read(&mut samples).ok()?;
        Some(Change::Sample { generation: header.generation, samples })
    }
}

impl Listener for ServiceListener {
    fn next_change(&mut self) -> Result<Change, Failure> {
        loop {
            match self.stream.next_event()? {
                Event::Stopping => return Err(Failure::NotRunning),
                Event::Sampled => {
                    if let Some(sample) = self.read_sample() {
                        return Ok(sample);
                    }
                }
                Event::StatesChanged
                | Event::ProfileChanged
                | Event::SensorsChanged
                | Event::StatusChanged => return Ok(Change::Data),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_proto::message::{ErrorCode, ServiceError};

    #[test]
    fn a_missing_pipe_is_reported_as_the_service_not_running() {
        assert_eq!(Failure::from(ClientError::NotRunning), Failure::NotRunning);
        let instance = Instance::named("SysPilot-Test.ui-absent").unwrap();
        assert_eq!(ServiceBackend::new(instance).load(), Err(Failure::NotRunning));
    }

    #[test]
    fn other_failures_keep_the_service_message() {
        let refused = ServiceError { code: ErrorCode::AccessDenied, message: "not allowed".into() };
        let failure = Failure::from(ClientError::Service(refused));
        assert_eq!(failure, Failure::Other("not allowed".into()));
        assert_eq!(failure.to_string(), "not allowed");
    }
}
