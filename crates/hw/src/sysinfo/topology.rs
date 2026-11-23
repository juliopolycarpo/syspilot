//! Processor cores, threads and caches from
//! `GetLogicalProcessorInformationEx(RelationAll)`.
//!
//! The reply is a packed list of variable-size records. [`parse_topology`]
//! reads it from bytes so it can be tested with hand-built records.

use std::mem::offset_of;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError};
use windows_sys::Win32::System::SystemInformation::{
    CACHE_RELATIONSHIP, CacheData, CacheInstruction, CacheTrace, CacheUnified, GROUP_AFFINITY,
    GetLogicalProcessorInformationEx, PROCESSOR_RELATIONSHIP, RelationAll, RelationCache,
    RelationProcessorCore, RelationProcessorPackage, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

use crate::os::bytes::{read_u8, read_u16, read_u32, read_u64};

/// Offset of the relationship-specific part of a record.
const BODY: usize = offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
/// Offsets of the fields read, from the start of a record.
const PROCESSOR_GROUP_COUNT: usize = BODY + offset_of!(PROCESSOR_RELATIONSHIP, GroupCount);
const PROCESSOR_GROUP_MASKS: usize = BODY + offset_of!(PROCESSOR_RELATIONSHIP, GroupMask);
const CACHE_LEVEL: usize = BODY + offset_of!(CACHE_RELATIONSHIP, Level);
const CACHE_SIZE: usize = BODY + offset_of!(CACHE_RELATIONSHIP, CacheSize);
const CACHE_TYPE: usize = BODY + offset_of!(CACHE_RELATIONSHIP, Type);
/// Size of one `GROUP_AFFINITY`; its `Mask` is the first field.
const GROUP_AFFINITY_BYTES: usize = size_of::<GROUP_AFFINITY>();

// The parser relies on these layouts; a binding change fails the build here
// instead of misreading a buffer.
const _: () = {
    assert!(BODY == 8);
    assert!(PROCESSOR_GROUP_COUNT == 30);
    assert!(PROCESSOR_GROUP_MASKS == 32);
    assert!(CACHE_LEVEL == 8);
    assert!(CACHE_SIZE == 12);
    assert!(CACHE_TYPE == 16);
    assert!(GROUP_AFFINITY_BYTES == 16);
    assert!(offset_of!(GROUP_AFFINITY, Mask) == 0);
};

/// Initial buffer for the query; a machine with many cores asks for more.
const INITIAL_BUFFER_BYTES: usize = 4096;
/// How many times the query is retried with the size the OS asked for.
const QUERY_ATTEMPTS: usize = 3;

/// What a cache stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum CacheKind {
    Unified,
    Instruction,
    Data,
    Trace,
}

/// Caches of one kind: `count` instances of `size_bytes` each, such as the
/// fourteen 32 KiB level-1 data caches of fourteen cores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CacheGroup {
    pub level: u8,
    pub kind: CacheKind,
    pub size_bytes: u32,
    pub count: u32,
}

/// Processor packages, cores, hardware threads and caches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Topology {
    /// Physical processors (sockets).
    pub packages: u32,
    pub cores: u32,
    /// Hardware threads across all groups.
    pub logical_processors: u32,
    /// Ordered by level, then kind, then size.
    pub caches: Vec<CacheGroup>,
}

fn cache_kind(raw: u32) -> Option<CacheKind> {
    // The bindings declare these as `i32` constants, which cannot be patterns.
    [
        (CacheUnified, CacheKind::Unified),
        (CacheInstruction, CacheKind::Instruction),
        (CacheData, CacheKind::Data),
        (CacheTrace, CacheKind::Trace),
    ]
    .into_iter()
    .find(|&(value, _)| u32::try_from(value) == Ok(raw))
    .map(|(_, kind)| kind)
}

/// Hardware threads of a core record: the set bits of its group masks.
fn core_threads(record: &[u8]) -> Option<u32> {
    let groups = usize::from(read_u16(record, PROCESSOR_GROUP_COUNT)?);
    let mut threads = 0;
    for group in 0..groups {
        let mask = read_u64(record, PROCESSOR_GROUP_MASKS + group * GROUP_AFFINITY_BYTES)?;
        threads += mask.count_ones();
    }
    Some(threads)
}

/// Reads the records of a `RelationAll` reply. A record that is cut short or
/// declares a size below its header ends the walk; what was read before it is
/// kept.
pub(crate) fn parse_topology(bytes: &[u8]) -> Topology {
    let mut topology = Topology::default();
    let mut caches: Vec<CacheGroup> = Vec::new();
    let mut offset = 0;
    while let (Some(relationship), Some(size)) =
        (read_u32(bytes, offset), read_u32(bytes, offset + 4))
    {
        let size = size as usize;
        // A size below the header would never advance the walk.
        if size < BODY {
            break;
        }
        let Some(record) = bytes.get(offset..offset.saturating_add(size)) else { break };
        // The relationship constants are CamelCase `i32`s, which cannot be
        // match patterns without tripping the naming lint.
        let relationship = i32::try_from(relationship).ok();
        if relationship == Some(RelationProcessorCore) {
            topology.cores += 1;
            topology.logical_processors += core_threads(record).unwrap_or(0);
        } else if relationship == Some(RelationProcessorPackage) {
            topology.packages += 1;
        } else if relationship == Some(RelationCache) {
            let fields = (
                read_u8(record, CACHE_LEVEL),
                read_u32(record, CACHE_SIZE),
                read_u32(record, CACHE_TYPE).and_then(cache_kind),
            );
            if let (Some(level), Some(size_bytes), Some(kind)) = fields {
                let same = |g: &&mut CacheGroup| {
                    (g.level, g.kind, g.size_bytes) == (level, kind, size_bytes)
                };
                match caches.iter_mut().find(same) {
                    Some(group) => group.count += 1,
                    None => caches.push(CacheGroup { level, kind, size_bytes, count: 1 }),
                }
            }
        }
        offset += size;
    }
    caches.sort();
    topology.caches = caches;
    topology
}

/// Asks the OS for the topology of this machine.
pub(crate) fn read_topology() -> Option<Topology> {
    // `u64` words keep the buffer 8-byte aligned, as the API's structures need.
    let mut words = vec![0u64; INITIAL_BUFFER_BYTES / 8];
    for _ in 0..QUERY_ATTEMPTS {
        let mut length = (words.len() * 8) as u32;
        // SAFETY: the pointer addresses `words`, which is aligned for the
        // record type and valid for writes of `length` bytes; `length` is a
        // valid in/out pointer.
        let ok = unsafe {
            GetLogicalProcessorInformationEx(
                RelationAll,
                words.as_mut_ptr().cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
                &mut length,
            )
        };
        if ok != 0 {
            let bytes: Vec<u8> =
                words.iter().flat_map(|word| word.to_ne_bytes()).take(length as usize).collect();
            return Some(parse_topology(&bytes));
        }
        // SAFETY: reads the calling thread's last error; no arguments.
        if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
        // On this error `length` holds the size needed.
        words = vec![0u64; (length as usize).div_ceil(8)];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record: relationship, size, then `body` from offset 8 (zero-padded to
    /// `size`).
    fn record(relationship: i32, size: usize, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; size];
        bytes[..4].copy_from_slice(&relationship.to_le_bytes());
        bytes[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        bytes[BODY..BODY + body.len()].copy_from_slice(body);
        bytes
    }

    /// A core with the hardware threads in `mask` of processor group 0.
    fn core(mask: u64) -> Vec<u8> {
        let mut bytes = record(RelationProcessorCore, 48, &[]);
        bytes[PROCESSOR_GROUP_COUNT..PROCESSOR_GROUP_COUNT + 2]
            .copy_from_slice(&1u16.to_le_bytes());
        bytes[PROCESSOR_GROUP_MASKS..PROCESSOR_GROUP_MASKS + 8]
            .copy_from_slice(&mask.to_le_bytes());
        bytes
    }

    fn cache(level: u8, kind: i32, size: u32) -> Vec<u8> {
        let mut bytes = record(RelationCache, 56, &[]);
        bytes[CACHE_LEVEL] = level;
        bytes[CACHE_SIZE..CACHE_SIZE + 4].copy_from_slice(&size.to_le_bytes());
        bytes[CACHE_TYPE..CACHE_TYPE + 4].copy_from_slice(&kind.to_le_bytes());
        bytes
    }

    fn package() -> Vec<u8> {
        record(RelationProcessorPackage, 80, &[])
    }

    fn reply(records: &[Vec<u8>]) -> Vec<u8> {
        records.concat()
    }

    #[test]
    fn cores_threads_and_packages_are_counted() {
        let bytes = reply(&[core(0b11), core(0b1100), package(), core(0b1_0000)]);
        let topology = parse_topology(&bytes);
        assert_eq!((topology.packages, topology.cores, topology.logical_processors), (1, 3, 5));
    }

    #[test]
    fn identical_caches_are_grouped_and_ordered() {
        let bytes = reply(&[
            cache(3, CacheUnified, 35 << 20),
            cache(2, CacheUnified, 256 << 10),
            cache(1, CacheData, 32 << 10),
            cache(1, CacheInstruction, 32 << 10),
            cache(2, CacheUnified, 256 << 10),
            cache(1, CacheData, 32 << 10),
        ]);
        let groups: Vec<_> = parse_topology(&bytes)
            .caches
            .iter()
            .map(|g| (g.level, g.kind, g.size_bytes, g.count))
            .collect();
        assert_eq!(
            groups,
            [
                (1, CacheKind::Instruction, 32 << 10, 1),
                (1, CacheKind::Data, 32 << 10, 2),
                (2, CacheKind::Unified, 256 << 10, 2),
                (3, CacheKind::Unified, 35 << 20, 1),
            ]
        );
    }

    #[test]
    fn caches_of_the_same_level_with_different_sizes_stay_apart() {
        // A hybrid processor: 2 MiB per performance cluster, 4 MiB per efficiency cluster.
        let bytes = reply(&[cache(2, CacheUnified, 2 << 20), cache(2, CacheUnified, 4 << 20)]);
        assert_eq!(parse_topology(&bytes).caches.len(), 2);
    }

    #[test]
    fn core_spanning_two_groups_counts_the_threads_of_both() {
        let mut bytes = record(RelationProcessorCore, 64, &[]);
        bytes[PROCESSOR_GROUP_COUNT..PROCESSOR_GROUP_COUNT + 2]
            .copy_from_slice(&2u16.to_le_bytes());
        bytes[PROCESSOR_GROUP_MASKS..PROCESSOR_GROUP_MASKS + 8]
            .copy_from_slice(&0b11u64.to_le_bytes());
        let second = PROCESSOR_GROUP_MASKS + GROUP_AFFINITY_BYTES;
        bytes[second..second + 8].copy_from_slice(&0b1u64.to_le_bytes());
        assert_eq!(parse_topology(&bytes).logical_processors, 3);
    }

    #[test]
    fn unknown_relationships_are_skipped_by_their_size() {
        // NUMA node (1) and processor group (4) records sit between the ones read.
        let bytes = reply(&[core(0b1), record(1, 72, &[]), record(4, 96, &[]), core(0b1)]);
        let topology = parse_topology(&bytes);
        assert_eq!((topology.cores, topology.logical_processors), (2, 2));
    }

    #[test]
    fn cache_of_an_unknown_kind_is_ignored() {
        assert!(parse_topology(&cache(1, 9, 1024)).caches.is_empty());
    }

    #[test]
    fn empty_reply_is_an_empty_topology() {
        assert_eq!(parse_topology(&[]), Topology::default());
    }

    #[test]
    fn record_declaring_a_size_below_its_header_ends_the_walk() {
        let mut bytes = core(0b1);
        bytes.extend(record(RelationProcessorCore, 8, &[]));
        // Patch the second record's size to 4, below the 8-byte header.
        let second = bytes.len() - 8;
        bytes[second + 4..second + 8].copy_from_slice(&4u32.to_le_bytes());
        bytes.extend(core(0b1));
        assert_eq!(parse_topology(&bytes).cores, 1, "the walk stops at the bad record");
    }

    #[test]
    fn record_cut_short_ends_the_walk_with_what_came_before() {
        let mut bytes = reply(&[core(0b1), core(0b1)]);
        bytes.truncate(bytes.len() - 1);
        assert_eq!(parse_topology(&bytes).cores, 1);
    }

    #[test]
    fn record_declaring_a_size_beyond_u32_arithmetic_does_not_overflow() {
        let mut bytes = core(0b1);
        bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_topology(&bytes), Topology::default());
    }

    #[test]
    fn core_record_too_short_for_its_masks_counts_the_core_but_no_threads() {
        let bytes = record(RelationProcessorCore, 16, &[]);
        let topology = parse_topology(&bytes);
        assert_eq!((topology.cores, topology.logical_processors), (1, 0));
    }

    #[test]
    fn this_machine_reports_at_least_one_core_and_thread() {
        let topology = read_topology().expect("GetLogicalProcessorInformationEx failed");
        assert!(topology.cores >= 1, "cores: expected >= 1 | received: {}", topology.cores);
        assert!(
            topology.logical_processors >= topology.cores,
            "threads: expected >= cores ({}) | received: {}",
            topology.cores,
            topology.logical_processors
        );
        assert!(
            topology.packages >= 1,
            "packages: expected >= 1 | received: {}",
            topology.packages
        );
    }
}
