//! Sensor metadata and the shared-memory section that carries live values.
//!
//! Metadata (names, kinds, grouping) travels over the pipe once. Values are
//! published by the service into a named section that clients map
//! read-only, so the UI can refresh at its own rate without waking the
//! service.
//!
//! # Section layout
//!
//! The section is an array of 64-bit words, [`SECTION_BYTES`] long:
//!
//! | Word | Content |
//! |---|---|
//! | 0 | magic (low 32 bits), layout version (high 32 bits) |
//! | 1 | total size in bytes (low), entry capacity (high) |
//! | 2 | sequence counter (seqlock: odd while a write is in progress) |
//! | 3 | sample time, milliseconds since the Unix epoch |
//! | 4 | entry count (low), metadata generation (high) |
//! | 5–7 | reserved, zero |
//! | 8… | entries, two words each: sensor ID (low) and flags (high); value as `f64` bits |
//!
//! Every word is accessed atomically, so a reader racing the writer never
//! performs an unsynchronised read; the sequence counter tells it whether
//! the words it copied belong to one sample.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// `"SYSP"` in little-endian byte order.
pub const SECTION_MAGIC: u32 = u32::from_le_bytes(*b"SYSP");
/// Version of the section layout.
pub const SECTION_LAYOUT_VERSION: u32 = 1;
/// Size of the section in bytes.
pub const SECTION_BYTES: usize = 16 * 1024;
/// Size of the section in 64-bit words.
pub const SECTION_WORDS: usize = SECTION_BYTES / 8;
/// Words occupied by the header.
pub const HEADER_WORDS: usize = 8;
/// Most sensors the section can carry.
pub const SECTION_CAPACITY: usize = (SECTION_WORDS - HEADER_WORDS) / 2;

/// Entry flag: the value is a valid reading.
const FLAG_VALID: u64 = 1 << 32;
/// Attempts a reader makes before reporting the section as busy.
const READ_ATTEMPTS: usize = 64;

/// Physical quantity a sensor reports. The unit is fixed per kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SensorKind {
    /// Degrees Celsius.
    Temperature,
    /// Utilisation in percent.
    Load,
    /// Megahertz.
    Frequency,
    /// Revolutions per minute.
    Fan,
    /// Fan duty in percent.
    Duty,
    /// Volts.
    Voltage,
    /// Watts.
    Power,
    /// Bytes.
    Data,
    /// Bytes per second.
    Throughput,
    /// A fill level or remaining share, in percent.
    Level,
}

impl SensorKind {
    /// Unit suffix for display.
    pub fn unit(self) -> &'static str {
        match self {
            Self::Temperature => "°C",
            Self::Load | Self::Duty | Self::Level => "%",
            Self::Frequency => "MHz",
            Self::Fan => "RPM",
            Self::Voltage => "V",
            Self::Power => "W",
            Self::Data => "B",
            Self::Throughput => "B/s",
        }
    }
}

/// Kind of hardware a group of sensors belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceClass {
    /// Processor.
    Cpu,
    /// Graphics adapter.
    Gpu,
    /// System memory.
    Memory,
    /// Disk or SSD.
    Storage,
    /// Motherboard (Super I/O).
    Board,
    /// Battery.
    Battery,
    /// Network adapter.
    Network,
}

/// A piece of hardware that owns sensors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Display name, e.g. the CPU brand string.
    pub name: String,
    /// What kind of hardware it is.
    pub class: DeviceClass,
}

/// Static description of one sensor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SensorInfo {
    /// ID used in the shared-memory section. Stable for one metadata
    /// generation only.
    pub id: u32,
    /// Index into [`SensorList::devices`].
    pub device: u32,
    /// Stable key, e.g. `cpu0.package.temp`. Used in profiles and logs.
    pub key: String,
    /// Display label, e.g. `Package`.
    pub label: String,
    /// Quantity and unit.
    pub kind: SensorKind,
    /// Whether the reading needs the PawnIO driver.
    pub driver: bool,
}

/// All sensor metadata, as sent over the pipe.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SensorList {
    /// Incremented whenever the set of sensors changes. Matches the
    /// generation in the section header.
    pub generation: u32,
    /// Hardware the sensors belong to.
    pub devices: Vec<DeviceInfo>,
    /// The sensors.
    pub sensors: Vec<SensorInfo>,
}

/// One reading copied out of the section.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Sensor ID, see [`SensorInfo::id`].
    pub id: u32,
    /// The reading, or `None` when the sensor could not be read.
    pub value: Option<f64>,
}

/// Header of a consistent sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleHeader {
    /// When the sample was taken, milliseconds since the Unix epoch. Zero
    /// before the first sample.
    pub timestamp_ms: u64,
    /// Metadata generation the IDs refer to.
    pub generation: u32,
}

/// Why a section could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionError {
    /// The mapping is smaller than the layout requires.
    TooSmall {
        /// Words available.
        words: usize,
    },
    /// The magic number does not match: not a sensor section.
    BadMagic {
        /// The value found.
        received: u32,
    },
    /// The section uses a different layout version.
    LayoutVersion {
        /// The version found.
        received: u32,
    },
    /// The writer was active on every attempt.
    Busy,
}

impl fmt::Display for SectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall { words } => {
                write!(f, "(sensor section): expected {SECTION_WORDS} words | received: {words}")
            }
            Self::BadMagic { received } => write!(
                f,
                "(sensor section): expected magic {SECTION_MAGIC:#010x} | received: {received:#010x}"
            ),
            Self::LayoutVersion { received } => write!(
                f,
                "(sensor section): expected layout version {SECTION_LAYOUT_VERSION} | received: {received}"
            ),
            Self::Busy => f.write_str("(sensor section): no consistent sample after 64 attempts"),
        }
    }
}

impl std::error::Error for SectionError {}

fn pack(low: u32, high: u32) -> u64 {
    u64::from(low) | (u64::from(high) << 32)
}

fn low(word: u64) -> u32 {
    word as u32
}

fn high(word: u64) -> u32 {
    (word >> 32) as u32
}

/// The single writer of a sensor section.
pub struct SectionWriter<'a> {
    words: &'a [AtomicU64],
}

impl<'a> SectionWriter<'a> {
    /// Takes ownership of zero-initialised (or previously initialised)
    /// section memory and writes the header.
    pub fn new(words: &'a [AtomicU64]) -> Result<Self, SectionError> {
        if words.len() < SECTION_WORDS {
            return Err(SectionError::TooSmall { words: words.len() });
        }
        words[1].store(pack(SECTION_BYTES as u32, SECTION_CAPACITY as u32), Ordering::Relaxed);
        // The magic goes last: a reader that sees it also sees a full header.
        words[0].store(pack(SECTION_MAGIC, SECTION_LAYOUT_VERSION), Ordering::Release);
        Ok(Self { words })
    }

    /// Publishes one sample. Readings beyond [`SECTION_CAPACITY`] are
    /// dropped. Returns the number of entries written.
    pub fn publish(
        &mut self,
        timestamp_ms: u64,
        generation: u32,
        samples: impl IntoIterator<Item = Sample>,
    ) -> usize {
        let sequence = &self.words[2];
        // Odd sequence: readers back off until the matching even store.
        // `&mut self` guarantees a single writer, so a plain add suffices.
        let start = sequence.load(Ordering::Relaxed).wrapping_add(1);
        sequence.store(start, Ordering::SeqCst);

        let mut count = 0;
        for sample in samples.into_iter().take(SECTION_CAPACITY) {
            let base = HEADER_WORDS + count * 2;
            let flags = if sample.value.is_some() { FLAG_VALID } else { 0 };
            self.words[base].store(u64::from(sample.id) | flags, Ordering::Relaxed);
            self.words[base + 1]
                .store(sample.value.unwrap_or_default().to_bits(), Ordering::Relaxed);
            count += 1;
        }
        self.words[3].store(timestamp_ms, Ordering::Relaxed);
        self.words[4].store(pack(count as u32, generation), Ordering::Relaxed);

        sequence.store(start.wrapping_add(1), Ordering::SeqCst);
        count
    }
}

/// A reader of a sensor section. Any number may exist.
pub struct SectionReader<'a> {
    words: &'a [AtomicU64],
}

impl<'a> SectionReader<'a> {
    /// Wraps mapped section memory after checking the header.
    pub fn new(words: &'a [AtomicU64]) -> Result<Self, SectionError> {
        if words.len() < SECTION_WORDS {
            return Err(SectionError::TooSmall { words: words.len() });
        }
        let identity = words[0].load(Ordering::Acquire);
        if low(identity) != SECTION_MAGIC {
            return Err(SectionError::BadMagic { received: low(identity) });
        }
        if high(identity) != SECTION_LAYOUT_VERSION {
            return Err(SectionError::LayoutVersion { received: high(identity) });
        }
        Ok(Self { words })
    }

    /// Copies the latest consistent sample into `out`, replacing its
    /// content.
    pub fn read(&self, out: &mut Vec<Sample>) -> Result<SampleHeader, SectionError> {
        let sequence = &self.words[2];
        for _ in 0..READ_ATTEMPTS {
            let before = sequence.load(Ordering::SeqCst);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let timestamp_ms = self.words[3].load(Ordering::Relaxed);
            let counts = self.words[4].load(Ordering::Relaxed);
            // The count may be torn garbage if a write is racing us; clamp
            // it so the copy below stays in bounds either way.
            let count = (low(counts) as usize).min(SECTION_CAPACITY);
            out.clear();
            for index in 0..count {
                let base = HEADER_WORDS + index * 2;
                let head = self.words[base].load(Ordering::Relaxed);
                let bits = self.words[base + 1].load(Ordering::Relaxed);
                let value = (head & FLAG_VALID != 0).then(|| f64::from_bits(bits));
                out.push(Sample { id: low(head), value });
            }
            if sequence.load(Ordering::SeqCst) == before {
                return Ok(SampleHeader { timestamp_ms, generation: high(counts) });
            }
        }
        out.clear();
        Err(SectionError::Busy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn section() -> Vec<AtomicU64> {
        (0..SECTION_WORDS).map(|_| AtomicU64::new(0)).collect()
    }

    #[test]
    fn layout_constants_are_consistent() {
        assert_eq!(SECTION_WORDS, 2048);
        assert_eq!(SECTION_CAPACITY, 1020);
        assert_eq!(HEADER_WORDS + SECTION_CAPACITY * 2, SECTION_WORDS);
    }

    #[test]
    fn reader_rejects_uninitialised_memory() {
        let words = section();
        let error = SectionReader::new(&words).err().unwrap();
        assert_eq!(error, SectionError::BadMagic { received: 0 });
    }

    #[test]
    fn reader_rejects_other_layout_versions() {
        let words = section();
        words[0].store(pack(SECTION_MAGIC, 7), Ordering::Relaxed);
        let error = SectionReader::new(&words).err().unwrap();
        assert_eq!(error, SectionError::LayoutVersion { received: 7 });
        assert_eq!(error.to_string(), "(sensor section): expected layout version 1 | received: 7");
    }

    #[test]
    fn short_mapping_is_rejected() {
        let words: Vec<AtomicU64> = (0..16).map(|_| AtomicU64::new(0)).collect();
        assert_eq!(SectionWriter::new(&words).err(), Some(SectionError::TooSmall { words: 16 }));
        assert_eq!(SectionReader::new(&words).err(), Some(SectionError::TooSmall { words: 16 }));
    }

    #[test]
    fn published_sample_is_read_back() {
        let words = section();
        let mut writer = SectionWriter::new(&words).unwrap();
        let reader = SectionReader::new(&words).unwrap();
        let mut out = Vec::new();

        assert_eq!(
            reader.read(&mut out).unwrap(),
            SampleHeader { timestamp_ms: 0, generation: 0 },
            "before the first publish the section is empty"
        );
        assert!(out.is_empty());

        let samples = [Sample { id: 7, value: Some(41.5) }, Sample { id: 9, value: None }];
        assert_eq!(writer.publish(1_700_000_000_123, 3, samples), 2);
        let header = reader.read(&mut out).unwrap();
        assert_eq!(header, SampleHeader { timestamp_ms: 1_700_000_000_123, generation: 3 });
        assert_eq!(out, samples);
    }

    #[test]
    fn readings_beyond_capacity_are_dropped() {
        let words = section();
        let mut writer = SectionWriter::new(&words).unwrap();
        let too_many = (0..SECTION_CAPACITY as u32 + 10).map(|id| Sample { id, value: Some(1.0) });
        assert_eq!(writer.publish(1, 1, too_many), SECTION_CAPACITY);
    }

    #[test]
    fn reader_gives_up_while_a_write_is_in_progress() {
        let words = section();
        let _writer = SectionWriter::new(&words).unwrap();
        words[2].store(1, Ordering::SeqCst);
        let mut out = vec![Sample { id: 1, value: None }];
        assert_eq!(SectionReader::new(&words).unwrap().read(&mut out), Err(SectionError::Busy));
        assert!(out.is_empty(), "a failed read must not leave partial data");
    }

    /// Each published sample carries one number in every field. A torn read
    /// would show up as fields that disagree.
    #[test]
    fn concurrent_reader_never_observes_a_torn_sample() {
        const ENTRIES: u32 = 200;
        const ROUNDS: u64 = 20_000;
        let words = section();
        let mut writer = SectionWriter::new(&words).unwrap();
        let done = AtomicBool::new(false);

        std::thread::scope(|scope| {
            let reader_thread = scope.spawn(|| {
                let reader = SectionReader::new(&words).unwrap();
                let mut out = Vec::new();
                let mut consistent_reads = 0u64;
                while !done.load(Ordering::Relaxed) {
                    let Ok(header) = reader.read(&mut out) else { continue };
                    let round = header.timestamp_ms;
                    assert_eq!(u64::from(header.generation), round & 0xFFFF, "header is torn");
                    if round == 0 {
                        continue;
                    }
                    assert_eq!(out.len(), ENTRIES as usize, "entry count is torn");
                    for sample in &out {
                        assert_eq!(
                            sample.value,
                            Some(round as f64),
                            "sensor {} belongs to another sample than round {round}",
                            sample.id
                        );
                    }
                    consistent_reads += 1;
                }
                consistent_reads
            });

            for round in 1..=ROUNDS {
                let samples = (0..ENTRIES).map(|id| Sample { id, value: Some(round as f64) });
                writer.publish(round, (round & 0xFFFF) as u32, samples);
            }
            done.store(true, Ordering::Relaxed);
            let consistent_reads = reader_thread.join().unwrap();
            assert!(consistent_reads > 0, "the reader never obtained a sample");
        });
    }
}
