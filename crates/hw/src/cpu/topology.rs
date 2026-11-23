//! Physical cores and their first logical processor, from the OS.
//!
//! Core temperature registers are per core, and a core with SMT answers on
//! either of its threads, so one read per physical core suffices. The OS
//! numbers logical processors in processor groups; this module returns each
//! core's first thread in that addressing.

use std::fmt;
use std::mem::{offset_of, size_of};

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError};
use windows_sys::Win32::System::SystemInformation::{
    GROUP_AFFINITY, GetLogicalProcessorInformationEx, PROCESSOR_RELATIONSHIP, RelationAll,
    RelationProcessorCore, RelationProcessorPackage, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

use crate::bus::LogicalCpu;

/// One physical processor package (socket) and the first thread of each of
/// its cores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// First logical processor of each physical core, in OS order.
    pub cores: Vec<LogicalCpu>,
}

/// The machine's packages.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CpuTopology {
    /// Packages in OS order. Empty when the OS reported no cores.
    pub packages: Vec<Package>,
}

/// Why the topology could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyError {
    /// `GetLogicalProcessorInformationEx` failed.
    Os {
        /// The Win32 error.
        win32: u32,
    },
    /// The returned buffer is malformed.
    Malformed {
        /// Byte offset of the bad record.
        offset: usize,
        /// What is wrong with it.
        reason: &'static str,
    },
}

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Os { win32 } => write!(
                f,
                "(topology): expected GetLogicalProcessorInformationEx to succeed | received: \
                 Win32 error {win32}"
            ),
            Self::Malformed { offset, reason } => write!(
                f,
                "(topology): expected well-formed processor records | received: {reason} at byte \
                 {offset}"
            ),
        }
    }
}

impl std::error::Error for TopologyError {}

impl CpuTopology {
    /// Asks the OS.
    pub fn read() -> Result<Self, TopologyError> {
        let mut bytes = 0u32;
        // SAFETY: a null buffer with a zero length is the documented way to
        // ask for the required size.
        unsafe { GetLogicalProcessorInformationEx(RelationAll, std::ptr::null_mut(), &mut bytes) };
        // SAFETY: reads the calling thread's last error.
        let win32 = unsafe { GetLastError() };
        if win32 != ERROR_INSUFFICIENT_BUFFER || bytes == 0 {
            return Err(TopologyError::Os { win32 });
        }
        // `u64` elements give the 8-byte alignment the records need.
        let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)];
        // SAFETY: `buffer` holds at least `bytes` bytes and is suitably
        // aligned for `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX`.
        let ok = unsafe {
            GetLogicalProcessorInformationEx(
                RelationAll,
                buffer.as_mut_ptr().cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
                &mut bytes,
            )
        };
        if ok == 0 {
            // SAFETY: reads the calling thread's last error.
            return Err(TopologyError::Os { win32: unsafe { GetLastError() } });
        }
        // SAFETY: the OS wrote `bytes` bytes into the buffer, which is large enough.
        let raw =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), bytes as usize) };
        Self::parse(raw)
    }

    /// Decodes the records `GetLogicalProcessorInformationEx(RelationAll)`
    /// returns: core and package records are used, others skipped.
    pub fn parse(bytes: &[u8]) -> Result<Self, TopologyError> {
        let mut cores = Vec::new();
        let mut packages: Vec<Vec<GroupMask>> = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let record = &bytes[offset..];
            let malformed = |reason| TopologyError::Malformed { offset, reason };
            let relationship =
                read_i32(record, offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Relationship))
                    .ok_or_else(|| malformed("record shorter than its header"))?;
            let size = read_u32(record, offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Size))
                .ok_or_else(|| malformed("record shorter than its header"))?
                as usize;
            if size == 0 || size > record.len() {
                return Err(malformed("record size zero or past the end of the buffer"));
            }
            if relationship == RelationProcessorCore || relationship == RelationProcessorPackage {
                let masks = group_masks(&record[..size])
                    .ok_or_else(|| malformed("group masks past the record"))?;
                if relationship == RelationProcessorCore {
                    let first = masks.iter().find_map(GroupMask::first_processor);
                    cores.push(first.ok_or_else(|| malformed("core without a processor"))?);
                } else {
                    packages.push(masks);
                }
            }
            offset += size;
        }

        // A system with no package records (virtual machines may omit them)
        // has one implicit package.
        let mut result: Vec<Package> = if packages.is_empty() {
            vec![Package { cores: Vec::new() }]
        } else {
            packages.iter().map(|_| Package { cores: Vec::new() }).collect()
        };
        for core in cores {
            let owner = packages
                .iter()
                .position(|masks| masks.iter().any(|mask| mask.contains(core)))
                .unwrap_or(0);
            result[owner].cores.push(core);
        }
        result.retain(|package| !package.cores.is_empty());
        Ok(Self { packages: result })
    }
}

/// A processor-group affinity mask from a record.
#[derive(Debug, Clone, Copy)]
struct GroupMask {
    group: u16,
    mask: usize,
}

impl GroupMask {
    fn first_processor(&self) -> Option<LogicalCpu> {
        (self.mask != 0)
            .then(|| LogicalCpu { group: self.group, index: self.mask.trailing_zeros() as u8 })
    }

    fn contains(&self, cpu: LogicalCpu) -> bool {
        self.group == cpu.group && self.mask & cpu.mask() != 0
    }
}

/// The group masks of a core or package record (both use
/// `PROCESSOR_RELATIONSHIP`).
fn group_masks(record: &[u8]) -> Option<Vec<GroupMask>> {
    let union_at = offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
    let count = read_u16(record, union_at + offset_of!(PROCESSOR_RELATIONSHIP, GroupCount))?;
    let masks_at = union_at + offset_of!(PROCESSOR_RELATIONSHIP, GroupMask);
    (0..usize::from(count))
        .map(|index| {
            let at = masks_at + index * size_of::<GROUP_AFFINITY>();
            Some(GroupMask {
                mask: usize::try_from(read_u64(record, at + offset_of!(GROUP_AFFINITY, Mask))?)
                    .ok()?,
                group: read_u16(record, at + offset_of!(GROUP_AFFINITY, Group))?,
            })
        })
        .collect()
}

fn read_array<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at + N)?.try_into().ok()
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    read_array(bytes, at).map(u16::from_le_bytes)
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    read_array(bytes, at).map(u32::from_le_bytes)
}

fn read_i32(bytes: &[u8], at: usize) -> Option<i32> {
    read_array(bytes, at).map(i32::from_le_bytes)
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    read_array(bytes, at).map(u64::from_le_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::SystemInformation::RelationCache;

    /// One record as the OS lays it out: header, `GroupCount`, then masks.
    fn record(relationship: i32, groups: &[(u16, usize)]) -> Vec<u8> {
        let union_at = offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
        let masks_at = union_at + offset_of!(PROCESSOR_RELATIONSHIP, GroupMask);
        let size = masks_at + groups.len().max(1) * size_of::<GROUP_AFFINITY>();
        let mut bytes = vec![0u8; size];
        bytes[offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Relationship)..][..4]
            .copy_from_slice(&relationship.to_le_bytes());
        bytes[offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Size)..][..4]
            .copy_from_slice(&(size as u32).to_le_bytes());
        let count_at = union_at + offset_of!(PROCESSOR_RELATIONSHIP, GroupCount);
        bytes[count_at..][..2].copy_from_slice(&(groups.len() as u16).to_le_bytes());
        for (index, (group, mask)) in groups.iter().enumerate() {
            let at = masks_at + index * size_of::<GROUP_AFFINITY>();
            bytes[at + offset_of!(GROUP_AFFINITY, Mask)..][..8]
                .copy_from_slice(&(*mask as u64).to_le_bytes());
            bytes[at + offset_of!(GROUP_AFFINITY, Group)..][..2]
                .copy_from_slice(&group.to_le_bytes());
        }
        bytes
    }

    fn buffer(records: &[Vec<u8>]) -> Vec<u8> {
        records.concat()
    }

    fn cpu(group: u16, index: u8) -> LogicalCpu {
        LogicalCpu { group, index }
    }

    #[test]
    fn smt_cores_report_their_first_thread() {
        let bytes = buffer(&[
            record(RelationProcessorCore, &[(0, 0b0011)]),
            record(RelationProcessorCore, &[(0, 0b1100)]),
            record(RelationProcessorCore, &[(0, 0b0011_0000)]),
            record(RelationProcessorPackage, &[(0, 0b0011_1111)]),
        ]);
        let topology = CpuTopology::parse(&bytes).unwrap();
        assert_eq!(topology.packages, [Package { cores: vec![cpu(0, 0), cpu(0, 2), cpu(0, 4)] }]);
    }

    #[test]
    fn cores_without_smt_use_their_only_thread() {
        let bytes = buffer(&[
            record(RelationProcessorCore, &[(0, 0b01)]),
            record(RelationProcessorCore, &[(0, 0b10)]),
            record(RelationProcessorPackage, &[(0, 0b11)]),
        ]);
        let topology = CpuTopology::parse(&bytes).unwrap();
        assert_eq!(topology.packages[0].cores, [cpu(0, 0), cpu(0, 1)]);
    }

    #[test]
    fn cores_are_split_between_packages_and_groups() {
        let bytes = buffer(&[
            record(RelationProcessorCore, &[(0, 0b11)]),
            record(RelationProcessorCore, &[(1, 0b101)]),
            record(RelationProcessorPackage, &[(0, 0b11)]),
            record(RelationProcessorPackage, &[(1, 0b111)]),
        ]);
        let topology = CpuTopology::parse(&bytes).unwrap();
        assert_eq!(topology.packages.len(), 2);
        assert_eq!(topology.packages[0].cores, [cpu(0, 0)]);
        assert_eq!(topology.packages[1].cores, [cpu(1, 0)]);
    }

    #[test]
    fn unrelated_records_are_skipped_by_their_size() {
        let bytes = buffer(&[
            record(RelationCache, &[(0, 0b1)]),
            record(RelationProcessorCore, &[(0, 0b1)]),
            record(RelationProcessorPackage, &[(0, 0b1)]),
        ]);
        let topology = CpuTopology::parse(&bytes).unwrap();
        assert_eq!(topology.packages[0].cores, [cpu(0, 0)]);
    }

    #[test]
    fn a_machine_without_package_records_has_one_implicit_package() {
        let bytes = buffer(&[record(RelationProcessorCore, &[(0, 0b1)])]);
        assert_eq!(CpuTopology::parse(&bytes).unwrap().packages[0].cores, [cpu(0, 0)]);
    }

    #[test]
    fn a_record_past_the_end_of_the_buffer_is_malformed() {
        let mut bytes = record(RelationProcessorCore, &[(0, 0b1)]);
        bytes.truncate(bytes.len() - 4);
        let error = CpuTopology::parse(&bytes).unwrap_err();
        assert!(matches!(error, TopologyError::Malformed { offset: 0, .. }), "{error}");
    }

    #[test]
    fn a_zero_size_record_cannot_loop_forever() {
        let mut bytes = record(RelationProcessorCore, &[(0, 0b1)]);
        bytes[offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Size)..][..4]
            .copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(CpuTopology::parse(&bytes), Err(TopologyError::Malformed { .. })));
    }

    #[test]
    fn a_core_with_an_empty_mask_is_malformed() {
        let bytes = record(RelationProcessorCore, &[(0, 0)]);
        assert!(matches!(CpuTopology::parse(&bytes), Err(TopologyError::Malformed { .. })));
    }

    #[test]
    fn this_machine_has_at_least_one_core() {
        let topology = CpuTopology::read().unwrap();
        let cores: usize = topology.packages.iter().map(|package| package.cores.len()).sum();
        assert!(cores >= 1, "{topology:?}");
        let logical = std::thread::available_parallelism().unwrap().get();
        assert!(cores <= logical, "{cores} cores cannot exceed {logical} logical processors");
    }
}
