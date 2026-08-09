#![no_std]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use rustos_user_abi::performance::{WAITSET_MAX_EPOLL_OBJECTS, WAITSET_MAX_GLOBAL_INTERESTS};
use rustos_user_abi::syscall::{
    DvmBlockInfoWire, ServiceCheckpointRecordWire, EARLY_SYSTEM_BROKER_MAX_IO_BYTES,
    SERVICE_CHECKPOINT_ABI_VERSION, SERVICE_CHECKPOINT_FLAG_TOMBSTONE, VFS_IPC_OP_FTRUNCATE,
    VFS_IPC_OP_WRITE, WAITSET_MAX_INTERESTS,
};
use storage_core::{IoResult, StorageError};

pub const ENOENT: i32 = 2;
pub const ENOTDIR: i32 = 20;
pub const EROFS: i32 = 30;

const SNAPSHOT_ADMISSION_IDLE: u8 = 0;
const SNAPSHOT_ADMISSION_WRITING: u8 = 1;
const SNAPSHOT_ADMISSION_READY: u8 = 2;
const SNAPSHOT_ADMISSION_BUSY: u8 = 3;

/// One bounded executable-snapshot handoff from the endpoint receiver to the
/// bulk worker. The atomic state is the slot's publication authority: the
/// receiver reserves before writing and publishes with Release; the sole
/// worker claims with Acquire before reading and returns the slot only after
/// its terminal reply attempt.
pub struct SnapshotWorkerAdmission(AtomicU8);

impl SnapshotWorkerAdmission {
    pub const fn new() -> Self {
        Self(AtomicU8::new(SNAPSHOT_ADMISSION_IDLE))
    }

    pub fn try_reserve(&self) -> bool {
        self.0
            .compare_exchange(
                SNAPSHOT_ADMISSION_IDLE,
                SNAPSHOT_ADMISSION_WRITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub fn publish_ready(&self) {
        self.0
            .compare_exchange(
                SNAPSHOT_ADMISSION_WRITING,
                SNAPSHOT_ADMISSION_READY,
                Ordering::Release,
                Ordering::Acquire,
            )
            .expect("snapshot admission published without reservation");
    }

    pub fn try_claim(&self) -> bool {
        self.0
            .compare_exchange(
                SNAPSHOT_ADMISSION_READY,
                SNAPSHOT_ADMISSION_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub fn release(&self) {
        self.0
            .compare_exchange(
                SNAPSHOT_ADMISSION_BUSY,
                SNAPSHOT_ADMISSION_IDLE,
                Ordering::Release,
                Ordering::Acquire,
            )
            .expect("snapshot admission released without worker ownership");
    }
}

impl Default for SnapshotWorkerAdmission {
    fn default() -> Self {
        Self::new()
    }
}

const EINTR: i32 = 4;
const EIO: i32 = 5;
const EAGAIN: i32 = 11;
const EINVAL: i32 = 22;
const ENODEV: i32 = 19;
const ENOSYS: i32 = 38;
const ETIMEDOUT: i32 = 110;

/// Whether a snapshot planned against `plan_generation` may still be sealed.
///
/// The bulk read runs with storage released, so a remount can land between the
/// plan and the commit. Sealing a stale image would admit bytes from the
/// previous volume into the current generation's cache, where no invalidation
/// would ever reach them: every cache drop is keyed on the generation change
/// that already happened. The caller re-plans instead, which is a transient
/// condition rather than a failed open.
///
/// This lives here rather than beside the commit because `services/vfsd`'s
/// binary target sets `test = false`, so a `#[test]` in `state_storage.rs`
/// would never execute and would read as coverage while providing none.
pub fn snapshot_plan_is_current(plan_generation: u64, mount_generation: u64) -> bool {
    plan_generation == mount_generation
}

/// Bounds synchronous-console amplification from cancelled reply bursts.
pub fn reply_failure_diagnostic_due(total: u64) -> bool {
    total == 1 || (total.is_power_of_two() && total.trailing_zeros() % 3 == 0)
}

/// Admits post-bootstrap demotion only after the exact terminal UI snapshot
/// reply succeeds.
pub fn ui_bootstrap_snapshot_reply_completed(
    request_path: &[u8],
    declared_len: usize,
    expected_path: &[u8],
    reply: i64,
) -> bool {
    reply >= 0
        && request_path
            .get(..declared_len)
            .is_some_and(|path| path == expected_path)
}

pub fn executable_snapshot_marker(path: &str, file_len: usize) -> alloc::string::String {
    format!("vfsd: executable snapshot sealed path={path} bytes={file_len}")
}

pub fn validate_dvm_block_range(
    block_size: usize,
    block_count: u64,
    lba: u64,
    len: usize,
) -> IoResult<()> {
    if block_size == 0 || len == 0 || !len.is_multiple_of(block_size) {
        return Err(StorageError::InvalidInput);
    }
    let requested = u64::try_from(len / block_size).map_err(|_| StorageError::InvalidInput)?;
    let end = lba
        .checked_add(requested)
        .ok_or(StorageError::InvalidInput)?;
    if end > block_count {
        return Err(StorageError::InvalidInput);
    }
    Ok(())
}

pub fn admit_dvm_block_geometry(
    info: DvmBlockInfoWire,
    response_generation: u64,
    response_capacity_sectors: u64,
    maximum_block_size: usize,
    expected_flags: u32,
) -> Result<(usize, u64), i32> {
    let block_size = usize::try_from(info.logical_block_size).map_err(|_| EINVAL)?;
    if info.generation != response_generation
        || info.capacity_sectors != response_capacity_sectors
        || info.generation == 0
        || !matches!(block_size, 512 | 1024 | 2048 | 4096)
        || block_size > maximum_block_size
        || info.physical_block_size < info.logical_block_size
        || !info
            .physical_block_size
            .is_multiple_of(info.logical_block_size)
        // Geometry admission also fixes the transport authority. Accepting a
        // writable signed epoch here would let the DVM volume silently exceed
        // the read-only policy requested by BootBlockDevice.
        || info.flags != expected_flags
        || info.reserved0 != 0
    {
        return Err(EIO);
    }
    let sectors_per_block = (block_size / 512) as u64;
    if !info.capacity_sectors.is_multiple_of(sectors_per_block) {
        return Err(EIO);
    }
    let block_count = info.capacity_sectors / sectors_per_block;
    if block_count == 0 || block_count.checked_mul(block_size as u64).is_none() {
        return Err(EINVAL);
    }
    Ok((block_size, block_count))
}

pub fn storage_error_from_linux_status(status: i64) -> StorageError {
    let errno = status
        .checked_neg()
        .and_then(|errno| i32::try_from(errno).ok());
    match errno {
        Some(EINTR) => StorageError::Interrupted,
        Some(EAGAIN) => StorageError::WouldBlock,
        Some(EINVAL) => StorageError::InvalidInput,
        Some(ETIMEDOUT) => StorageError::Timeout,
        Some(ENODEV) => StorageError::NotPresent,
        Some(ENOSYS) => StorageError::Unsupported,
        _ => StorageError::DeviceFault,
    }
}

/// Converts a service-directory lookup result into the VFS storage-backend
/// status. An unpublished storaged endpoint is a retryable lifecycle state,
/// never proof that the requested filesystem path is absent.
pub const fn storage_service_lookup_errno(endpoint: i64) -> i32 {
    if endpoint == 0 || endpoint == -(ENOENT as i64) {
        EAGAIN
    } else if endpoint < 0 && endpoint >= -(i32::MAX as i64) {
        (-endpoint) as i32
    } else {
        EIO
    }
}

/// Preserves retryable block-backend failures across the FAT adapter rather
/// than collapsing a readiness race into a permanent device fault.
pub const fn storage_io_error_to_linux_errno(error: StorageError) -> i32 {
    match error {
        StorageError::WouldBlock => EAGAIN,
        StorageError::NotPresent => ENODEV,
        StorageError::InvalidInput => EINVAL,
        _ => EIO,
    }
}

pub fn checked_next_generation(current: u64) -> Option<u64> {
    current.checked_add(1).filter(|next| *next != 0)
}

pub fn bounded_early_system_chunk(remaining: usize) -> usize {
    remaining.min(EARLY_SYSTEM_BROKER_MAX_IO_BYTES)
}

pub fn cooperative_bulk_yield_state(total_bytes: usize, byte_budget: usize) -> (usize, bool) {
    if byte_budget == 0 || total_bytes < byte_budget {
        (total_bytes, false)
    } else {
        (total_bytes % byte_budget, true)
    }
}

pub fn cacheable_metadata_errno(errno: i32) -> bool {
    matches!(errno, ENOENT | ENOTDIR)
}

/// Immutable DVM file bytes may be retained only within these service-owned
/// bounds. The values are policy, rather than a transport capability: a cache
/// miss must remain a bounded range read through the current storage epoch.
pub const FILE_BYTES_CACHE_MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
pub const FILE_BYTES_CACHE_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// Returns the exact file size only when the current request demonstrates a
/// complete-file reuse pattern. Header probes, partial sequential reads, and
/// nonzero-offset reads must remain bounded range reads: a `pread` is never
/// authority to transfer bytes the caller did not request across the DVM
/// storage boundary.
pub fn should_materialize_file_cache(
    file_len: u64,
    start: u64,
    request_len: usize,
) -> Option<usize> {
    let file_len = usize::try_from(file_len).ok()?;
    (file_len > 0
        && file_len <= FILE_BYTES_CACHE_MAX_ENTRY_BYTES
        && file_len <= FILE_BYTES_CACHE_BUDGET_BYTES
        && start == 0
        && request_len >= file_len)
        .then_some(file_len)
}

pub const VFSD_CHECKPOINT_HANDLE_TAG: u64 = 0x4844_4c45_0000_0001;
pub const VFSD_CHECKPOINT_PATH_TAG: u64 = 0x4850_4154_0000_0000;
pub const VFSD_OPEN_CHECKPOINT_VERSION: u16 = 1;
pub const VFSD_OPEN_MUTATION_STAGING: u16 = 0;
pub const VFSD_OPEN_MUTATION_OPEN: u16 = 1;
pub const VFSD_OPEN_MUTATION_READ: u16 = 2;
pub const VFSD_OPEN_MUTATION_LSEEK: u16 = 3;
pub const VFSD_OPEN_MUTATION_GETDENTS: u16 = 4;
pub const VFSD_OPEN_MUTATION_FCNTL: u16 = 5;
pub const VFSD_OPEN_MUTATION_STABLE: u16 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekPositionError {
    InvalidWhence,
    Negative,
    Overflow,
}

pub fn checked_seek_position(
    cursor: u64,
    len: u64,
    offset: i64,
    whence: u64,
) -> Result<u64, SeekPositionError> {
    let base = match whence {
        0 => 0_i128,
        1 => i128::from(cursor),
        2 => i128::from(len),
        _ => return Err(SeekPositionError::InvalidWhence),
    };
    let next = base
        .checked_add(i128::from(offset))
        .ok_or(SeekPositionError::Overflow)?;
    if next < 0 {
        return Err(SeekPositionError::Negative);
    }
    if next > i128::from(i64::MAX) {
        return Err(SeekPositionError::Overflow);
    }
    Ok(next as u64)
}

/// Service-private durable state for one ordinary VFS open description. The
/// wire is exactly one rootd checkpoint value; path bytes are child records.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenDescriptionCheckpointWire {
    pub version: u16,
    pub kind: u16,
    pub last_mutation: u16,
    pub reserved0: u16,
    pub path_len: u32,
    pub refs: u32,
    pub cursor: u64,
    pub len: u64,
    pub status_flags: u64,
    pub content_identity: u64,
    pub last_start: u64,
    pub last_result: u64,
}

const _: () = assert!(core::mem::size_of::<OpenDescriptionCheckpointWire>() == 64);

impl OpenDescriptionCheckpointWire {
    pub fn valid(&self, path_capacity: usize) -> bool {
        self.version == VFSD_OPEN_CHECKPOINT_VERSION
            && matches!(self.kind, 1..=3)
            && matches!(
                self.last_mutation,
                VFSD_OPEN_MUTATION_STAGING
                    | VFSD_OPEN_MUTATION_OPEN
                    | VFSD_OPEN_MUTATION_READ
                    | VFSD_OPEN_MUTATION_LSEEK
                    | VFSD_OPEN_MUTATION_GETDENTS
                    | VFSD_OPEN_MUTATION_FCNTL
                    | VFSD_OPEN_MUTATION_STABLE
            )
            && self.reserved0 == 0
            && self.path_len != 0
            && self.path_len as usize <= path_capacity
            && self.refs == 1
    }
}

pub fn checkpoint_path_key(remote_id: u64, chunk_index: usize) -> Option<(u64, u64)> {
    let chunk_index = u32::try_from(chunk_index).ok()?;
    Some((remote_id, VFSD_CHECKPOINT_PATH_TAG | u64::from(chunk_index)))
}

pub fn valid_checkpoint_record(record: &ServiceCheckpointRecordWire) -> bool {
    let value_len = record.value_len as usize;
    record.version == SERVICE_CHECKPOINT_ABI_VERSION
        && record.flags & !SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
        && (record.key_hi != 0 || record.key_lo != 0)
        && (record.operation_hi != 0 || record.operation_lo != 0)
        && record.revision != 0
        && value_len <= record.value.len()
        && record.value[value_len..].iter().all(|byte| *byte == 0)
        && (record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0 || value_len == 0)
        && (record.parent_hi != record.key_hi || record.parent_lo != record.key_lo)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WaitSetInterestKey {
    pub target_fd: u64,
    pub provider: u16,
    pub object_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitSetInterestRecord {
    pub key: WaitSetInterestKey,
    pub provider_epoch: u64,
    pub events: u32,
    pub data: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitSetRegistryError {
    Exists,
    NotFound,
    Capacity,
    Overflow,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WaitSetRegistry {
    epolls: BTreeMap<u64, WaitSetEpoll>,
    object_interests: BTreeMap<(u16, u64), BTreeSet<WaitSetObjectInterest>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WaitSetEpoll {
    interests: BTreeMap<WaitSetInterestKey, WaitSetInterestRecord>,
    cursor: usize,
}

/// One reverse-index membership. The index makes final-object retirement
/// proportional to exactly the affected registrations rather than every live
/// epoll object. Its cardinality is bounded by
/// `WAITSET_MAX_GLOBAL_INTERESTS`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct WaitSetObjectInterest {
    epoll_token: u64,
    key: WaitSetInterestKey,
}

impl WaitSetRegistry {
    pub fn create(&mut self, token: u64) -> Result<(), WaitSetRegistryError> {
        self.insert_epoll(token)
    }

    pub fn restore(&mut self, token: u64) -> Result<(), WaitSetRegistryError> {
        self.insert_epoll(token)
    }

    /// Restores a checkpoint batch as one registry transaction. A malformed
    /// or over-capacity batch leaves the pre-existing registry untouched.
    pub fn restore_all(&mut self, tokens: &[u64]) -> Result<(), WaitSetRegistryError> {
        let mut candidate = self.clone();
        for token in tokens {
            candidate.restore(*token)?;
        }
        *self = candidate;
        Ok(())
    }

    fn insert_epoll(&mut self, token: u64) -> Result<(), WaitSetRegistryError> {
        if token == 0 || self.epolls.contains_key(&token) {
            return Err(WaitSetRegistryError::Exists);
        }
        if self.epolls.len() >= WAITSET_MAX_EPOLL_OBJECTS {
            return Err(WaitSetRegistryError::Capacity);
        }
        self.epolls.insert(
            token,
            WaitSetEpoll {
                interests: BTreeMap::new(),
                cursor: 0,
            },
        );
        Ok(())
    }

    fn object_key(key: WaitSetInterestKey) -> (u16, u64) {
        (key.provider, key.object_id)
    }

    fn insert_object_interest(&mut self, token: u64, key: WaitSetInterestKey) {
        let members = self
            .object_interests
            .entry(Self::object_key(key))
            .or_default();
        let inserted = members.insert(WaitSetObjectInterest {
            epoll_token: token,
            key,
        });
        assert!(inserted, "waitset reverse index duplicated a live interest");
        assert!(
            members.len() <= WAITSET_MAX_GLOBAL_INTERESTS,
            "waitset reverse index exceeded global interest capacity"
        );
    }

    fn remove_object_interest(&mut self, token: u64, key: WaitSetInterestKey) {
        let object = Self::object_key(key);
        let remove_bucket = {
            let members = self
                .object_interests
                .get_mut(&object)
                .expect("waitset reverse index lacks a live object");
            assert!(
                members.remove(&WaitSetObjectInterest {
                    epoll_token: token,
                    key,
                }),
                "waitset reverse index lacks a live interest"
            );
            members.is_empty()
        };
        if remove_bucket {
            self.object_interests.remove(&object);
        }
    }

    pub fn retire(&mut self, token: u64) -> Result<(), WaitSetRegistryError> {
        let epoll = self
            .epolls
            .remove(&token)
            .ok_or(WaitSetRegistryError::NotFound)?;
        for interest in epoll.interests.into_values() {
            self.remove_object_interest(token, interest.key);
        }
        Ok(())
    }

    pub fn add(
        &mut self,
        token: u64,
        interest: WaitSetInterestRecord,
    ) -> Result<(), WaitSetRegistryError> {
        let epoll = self
            .epolls
            .get_mut(&token)
            .ok_or(WaitSetRegistryError::NotFound)?;
        if epoll.interests.contains_key(&interest.key) {
            return Err(WaitSetRegistryError::Exists);
        }
        if epoll.interests.len() >= WAITSET_MAX_INTERESTS {
            return Err(WaitSetRegistryError::Capacity);
        }
        epoll.interests.insert(interest.key, interest);
        self.insert_object_interest(token, interest.key);
        Ok(())
    }

    pub fn modify(
        &mut self,
        token: u64,
        interest: WaitSetInterestRecord,
    ) -> Result<(), WaitSetRegistryError> {
        let epoll = self
            .epolls
            .get_mut(&token)
            .ok_or(WaitSetRegistryError::NotFound)?;
        if !epoll.interests.contains_key(&interest.key) {
            return Err(WaitSetRegistryError::NotFound);
        }
        epoll.interests.insert(interest.key, interest);
        Ok(())
    }

    pub fn delete(
        &mut self,
        token: u64,
        key: WaitSetInterestKey,
    ) -> Result<(), WaitSetRegistryError> {
        let epoll = self
            .epolls
            .get_mut(&token)
            .ok_or(WaitSetRegistryError::NotFound)?;
        let interest = epoll
            .interests
            .remove(&key)
            .ok_or(WaitSetRegistryError::NotFound)?;
        self.remove_object_interest(token, interest.key);
        Ok(())
    }

    /// Removes exactly one provider object from every live epoll object.
    ///
    /// The reverse index owns all work for this turn, so no purge scans the
    /// whole registry or allocates a transient whole-registry work list.
    /// The return value is bounded by `WAITSET_MAX_GLOBAL_INTERESTS`.
    pub fn purge(&mut self, provider: u16, object_id: u64) -> usize {
        let Some(members) = self.object_interests.remove(&(provider, object_id)) else {
            return 0;
        };
        let purged = members.len();
        assert!(
            purged <= WAITSET_MAX_GLOBAL_INTERESTS,
            "waitset purge exceeded global interest capacity"
        );
        for member in members {
            let epoll = self
                .epolls
                .get_mut(&member.epoll_token)
                .expect("waitset reverse index references a retired epoll");
            assert!(
                epoll.interests.remove(&member.key).is_some(),
                "waitset reverse index references a missing interest"
            );
            if epoll.interests.is_empty() {
                epoll.cursor = 0;
            } else {
                epoll.cursor %= epoll.interests.len();
            }
        }
        purged
    }

    pub fn matching_interests(
        &self,
        provider: u16,
        object_id: u64,
    ) -> Vec<(u64, WaitSetInterestRecord)> {
        let Some(members) = self.object_interests.get(&(provider, object_id)) else {
            return Vec::new();
        };
        assert!(
            members.len() <= WAITSET_MAX_GLOBAL_INTERESTS,
            "waitset reverse index exceeded global interest capacity"
        );
        // `WaitSetObjectInterest` orders token before interest key. Callers
        // can therefore collapse this bounded list into one readiness update
        // per affected epoll without another allocation or global scan.
        let mut matches = Vec::with_capacity(members.len());
        for member in members {
            let epoll = self
                .epolls
                .get(&member.epoll_token)
                .expect("waitset reverse index references a retired epoll");
            let interest = epoll
                .interests
                .get(&member.key)
                .expect("waitset reverse index references a missing interest");
            matches.push((member.epoll_token, *interest));
        }
        matches
    }

    pub fn snapshot(
        &mut self,
        token: u64,
        max: usize,
    ) -> Result<Vec<WaitSetInterestRecord>, WaitSetRegistryError> {
        let epoll = self
            .epolls
            .get_mut(&token)
            .ok_or(WaitSetRegistryError::NotFound)?;
        let count = epoll.interests.len();
        let start = epoll.cursor.min(count.saturating_sub(1));
        let snapshot = epoll
            .interests
            .values()
            .skip(start)
            .chain(epoll.interests.values().take(start))
            .take(max)
            .copied()
            .collect::<Vec<_>>();
        if count != 0 {
            epoll.cursor = (start + 1) % count;
        }
        Ok(snapshot)
    }
}

/// Persistent mutation remains unavailable until a journal/recovery protocol
/// is implemented. Keeping this decision in the testable policy library makes
/// the service dispatch and the formal admission model share one source gate.
pub const fn persistent_mutation_status(op: u16) -> Option<i32> {
    match op {
        VFS_IPC_OP_WRITE | VFS_IPC_OP_FTRUNCATE => Some(EROFS),
        _ => None,
    }
}

pub fn mkdir_policy(path: &str, euid: u32) -> i32 {
    let run_user_path = format!("/run/user/{euid}");
    if path == "/run" || path == "/run/user" || path == run_user_path.as_str() {
        0
    } else {
        EROFS
    }
}

pub fn unlink_policy(path: &str) -> i32 {
    if path.starts_with("/run/") {
        ENOENT
    } else {
        EROFS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn snapshot_worker_admission_is_single_slot_and_exact_owner() {
        let admission = SnapshotWorkerAdmission::new();
        assert!(admission.try_reserve());
        assert!(!admission.try_reserve());
        assert!(!admission.try_claim());
        admission.publish_ready();
        assert!(admission.try_claim());
        assert!(!admission.try_claim());
        admission.release();
        assert!(admission.try_reserve());
    }

    #[test]
    fn a_remount_between_plan_and_commit_forbids_sealing_the_read_image() {
        assert!(snapshot_plan_is_current(7, 7));
        // A remount in either direction invalidates the plan. The generation is
        // a monotonic counter, but the check must not assume the direction:
        // "the mount I planned against" is the property, not "an older mount".
        assert!(!snapshot_plan_is_current(7, 8));
        assert!(!snapshot_plan_is_current(8, 7));
    }

    #[test]
    fn snapshot_worker_admissions_are_independent_slots() {
        // A pool is only a pool if one worker's job cannot occupy another's
        // slot. Reserving one admission must leave the other claimable.
        let pool = [
            SnapshotWorkerAdmission::new(),
            SnapshotWorkerAdmission::new(),
        ];
        assert!(pool[0].try_reserve());
        assert!(pool[1].try_reserve());
        pool[0].publish_ready();
        // Only the published slot may be claimed; the other is still WRITING.
        assert!(pool[0].try_claim());
        assert!(!pool[1].try_claim());
        pool[1].publish_ready();
        assert!(pool[1].try_claim());
        pool[0].release();
        assert!(pool[0].try_reserve());
        assert!(!pool[1].try_reserve());
    }

    #[test]
    fn reply_failure_diagnostics_are_first_then_exponentially_rate_limited() {
        let reported = (1..=70)
            .filter(|total| reply_failure_diagnostic_due(*total))
            .collect::<Vec<_>>();
        assert_eq!(reported, [1, 8, 64]);
    }

    #[test]
    fn ui_bootstrap_demotion_requires_successful_terminal_snapshot_reply() {
        let path = b"services/uiserver/uiserver.elf";
        assert!(!ui_bootstrap_snapshot_reply_completed(
            path,
            path.len(),
            path,
            -22
        ));
        assert!(ui_bootstrap_snapshot_reply_completed(
            path,
            path.len(),
            path,
            0
        ));
        assert!(!ui_bootstrap_snapshot_reply_completed(
            path,
            path.len() - 1,
            path,
            0
        ));
    }

    #[test]
    fn executable_snapshot_marker_binds_path_and_exact_length() {
        assert_eq!(
            executable_snapshot_marker("apps/wayclick/wayclick.elf", 916_224),
            "vfsd: executable snapshot sealed path=apps/wayclick/wayclick.elf bytes=916224"
        );
    }
    #[test]
    fn dvm_block_range_rejects_empty_overflow_and_end_overrun() {
        assert_eq!(
            validate_dvm_block_range(512, 8, 0, 0),
            Err(StorageError::InvalidInput)
        );
        assert_eq!(
            validate_dvm_block_range(512, u64::MAX, u64::MAX, 512),
            Err(StorageError::InvalidInput)
        );
        assert_eq!(
            validate_dvm_block_range(512, 8, 7, 1024),
            Err(StorageError::InvalidInput)
        );
        assert_eq!(validate_dvm_block_range(512, 8, 6, 1024), Ok(()));
    }

    #[test]
    fn storage_geometry_rejects_provider_overflow_unknown_flags_and_foreign_binding() {
        let valid = DvmBlockInfoWire {
            generation: 7,
            capacity_sectors: 8192,
            logical_block_size: 4096,
            physical_block_size: 4096,
            flags: 1,
            ..DvmBlockInfoWire::default()
        };
        assert_eq!(
            admit_dvm_block_geometry(valid, 7, 8192, 64 * 1024, 1),
            Ok((4096, 1024))
        );
        assert!(admit_dvm_block_geometry(valid, 8, 8192, 64 * 1024, 1).is_err());
        assert!(admit_dvm_block_geometry(valid, 7, 4096, 64 * 1024, 1).is_err());
        assert!(admit_dvm_block_geometry(
            DvmBlockInfoWire { flags: 0, ..valid },
            7,
            8192,
            64 * 1024,
            1
        )
        .is_err());
        assert!(admit_dvm_block_geometry(
            DvmBlockInfoWire { flags: 3, ..valid },
            7,
            8192,
            64 * 1024,
            1,
        )
        .is_err());

        let overflowing = DvmBlockInfoWire {
            capacity_sectors: u64::MAX - 7,
            ..valid
        };
        assert!(admit_dvm_block_geometry(
            overflowing,
            overflowing.generation,
            overflowing.capacity_sectors,
            64 * 1024,
            1,
        )
        .is_err());
    }

    #[test]
    fn broker_status_preserves_recoverable_storage_failures() {
        assert_eq!(
            storage_error_from_linux_status(-4),
            StorageError::Interrupted
        );
        assert_eq!(
            storage_error_from_linux_status(-11),
            StorageError::WouldBlock
        );
        assert_eq!(
            storage_error_from_linux_status(-19),
            StorageError::NotPresent
        );
        assert_eq!(
            storage_error_from_linux_status(-22),
            StorageError::InvalidInput
        );
        assert_eq!(
            storage_error_from_linux_status(-38),
            StorageError::Unsupported
        );
        assert_eq!(storage_error_from_linux_status(-110), StorageError::Timeout);
        assert_eq!(
            storage_error_from_linux_status(i64::MIN),
            StorageError::DeviceFault
        );
    }

    #[test]
    fn unpublished_storaged_is_retryable_not_a_missing_file() {
        assert_eq!(storage_service_lookup_errno(0), EAGAIN);
        assert_eq!(storage_service_lookup_errno(-(ENOENT as i64)), EAGAIN);
        assert_eq!(storage_service_lookup_errno(-13), 13);
        assert_eq!(storage_service_lookup_errno(i64::MIN), EIO);
    }

    #[test]
    fn bulk_readiness_race_remains_eagain_through_the_fat_adapter() {
        let storage_error = storage_error_from_linux_status(-(EAGAIN as i64));
        assert_eq!(storage_error, StorageError::WouldBlock);
        assert_eq!(storage_io_error_to_linux_errno(storage_error), EAGAIN);
        assert_eq!(
            storage_io_error_to_linux_errno(StorageError::NotPresent),
            ENODEV
        );
        assert_eq!(
            storage_io_error_to_linux_errno(StorageError::DeviceFault),
            EIO
        );
    }

    #[test]
    fn cache_generation_never_saturates_into_false_stability() {
        assert_eq!(checked_next_generation(1), Some(2));
        assert_eq!(checked_next_generation(u64::MAX), None);
    }

    #[test]
    fn early_system_reads_chunk_larger_vfs_buffers_to_the_broker_bound() {
        assert_eq!(bounded_early_system_chunk(1), 1);
        assert_eq!(
            bounded_early_system_chunk(EARLY_SYSTEM_BROKER_MAX_IO_BYTES),
            EARLY_SYSTEM_BROKER_MAX_IO_BYTES
        );
        assert_eq!(
            bounded_early_system_chunk(EARLY_SYSTEM_BROKER_MAX_IO_BYTES * 16),
            EARLY_SYSTEM_BROKER_MAX_IO_BYTES
        );
    }

    #[test]
    fn dvm_elf_header_probes_do_not_materialize_entire_files() {
        assert_eq!(should_materialize_file_cache(913_960, 0, 64), None);
        assert_eq!(should_materialize_file_cache(913_960, 0, 672), None);
        assert_eq!(
            should_materialize_file_cache(913_960, 4_096, 64 * 1024),
            None
        );
    }

    #[test]
    fn dvm_file_cache_requires_a_complete_file_read() {
        assert_eq!(
            should_materialize_file_cache(20 * 1024, 0, 20 * 1024),
            Some(20 * 1024)
        );
        assert_eq!(should_materialize_file_cache(913_960, 0, 64 * 1024), None);
        assert_eq!(
            should_materialize_file_cache(913_960, 0, 913_960),
            Some(913_960)
        );
        assert_eq!(
            should_materialize_file_cache(
                (FILE_BYTES_CACHE_MAX_ENTRY_BYTES + 1) as u64,
                0,
                64 * 1024
            ),
            None
        );
    }

    #[test]
    fn transient_metadata_failures_never_enter_the_negative_cache() {
        assert!(cacheable_metadata_errno(ENOENT));
        assert!(cacheable_metadata_errno(ENOTDIR));
        assert!(!cacheable_metadata_errno(5));
        assert!(!cacheable_metadata_errno(19));
        assert!(!cacheable_metadata_errno(110));
    }

    #[test]
    fn persistent_mutation_admission_remains_read_only() {
        assert_eq!(persistent_mutation_status(VFS_IPC_OP_WRITE), Some(EROFS));
        assert_eq!(
            persistent_mutation_status(VFS_IPC_OP_FTRUNCATE),
            Some(EROFS)
        );
        assert_eq!(persistent_mutation_status(0xffff), None);
        assert_eq!(mkdir_policy("/var/lib/rustos", 0), EROFS);
        assert_eq!(unlink_policy("/var/lib/rustos/state"), EROFS);
        assert_eq!(mkdir_policy("/run/user/1000", 1000), 0);
        assert_eq!(unlink_policy("/run/user/1000/socket"), ENOENT);
    }

    #[test]
    fn checkpoint_wire_rejects_unknown_or_noncanonical_state() {
        let mut record = ServiceCheckpointRecordWire {
            key_lo: 1,
            operation_lo: 2,
            revision: 1,
            value_len: 1,
            ..ServiceCheckpointRecordWire::default()
        };
        record.value[0] = 7;
        assert!(valid_checkpoint_record(&record));
        record.value[1] = 1;
        assert!(!valid_checkpoint_record(&record));
        record.value[1] = 0;
        record.flags = 2;
        assert!(!valid_checkpoint_record(&record));
    }

    #[test]
    fn open_description_wire_is_one_checkpoint_value_and_strictly_bounded() {
        assert_eq!(
            core::mem::size_of::<OpenDescriptionCheckpointWire>(),
            rustos_user_abi::syscall::SERVICE_CHECKPOINT_VALUE_CAPACITY
        );
        let wire = OpenDescriptionCheckpointWire {
            version: VFSD_OPEN_CHECKPOINT_VERSION,
            kind: 1,
            last_mutation: VFSD_OPEN_MUTATION_OPEN,
            reserved0: 0,
            path_len: 7,
            refs: 1,
            cursor: 0,
            len: 11,
            status_flags: 0,
            content_identity: 9,
            last_start: 0,
            last_result: 0,
        };
        assert!(wire.valid(256));
        assert!(!OpenDescriptionCheckpointWire { refs: 2, ..wire }.valid(256));
        assert!(!OpenDescriptionCheckpointWire {
            path_len: 257,
            ..wire
        }
        .valid(256));
        assert_ne!(checkpoint_path_key(7, 0), checkpoint_path_key(7, 1));
    }

    #[test]
    fn seek_position_never_wraps_signed_linux_off_t() {
        assert_eq!(checked_seek_position(9, 20, -4, 1), Ok(5));
        assert_eq!(
            checked_seek_position(9, 20, -21, 2),
            Err(SeekPositionError::Negative)
        );
        assert_eq!(
            checked_seek_position(i64::MAX as u64, 0, 1, 1),
            Err(SeekPositionError::Overflow)
        );
        assert_eq!(
            checked_seek_position(0, 0, 0, 99),
            Err(SeekPositionError::InvalidWhence)
        );
    }

    #[test]
    fn cache_hot_bulk_work_has_a_bounded_cooperative_burst() {
        let budget = 64 * 1024;
        assert_eq!(
            cooperative_bulk_yield_state(63 * 1024, budget),
            (63 * 1024, false)
        );
        assert_eq!(cooperative_bulk_yield_state(64 * 1024, budget), (0, true));
        assert_eq!(
            cooperative_bulk_yield_state(65 * 1024, budget),
            (1024, true)
        );
    }

    fn interest(fd: u64, object_id: u64) -> WaitSetInterestRecord {
        WaitSetInterestRecord {
            key: WaitSetInterestKey {
                target_fd: fd,
                provider: 2,
                object_id,
            },
            provider_epoch: 7,
            events: 1,
            data: object_id,
        }
    }

    #[test]
    fn epoll_membership_binds_open_description_and_purges_last_close() {
        let mut registry = WaitSetRegistry::default();
        registry.create(41).unwrap();
        registry.add(41, interest(5, 101)).unwrap();
        registry.add(41, interest(5, 102)).unwrap();
        assert_eq!(
            registry.snapshot(41, WAITSET_MAX_INTERESTS).unwrap().len(),
            2
        );
        assert_eq!(registry.purge(2, 101), 1);
        assert_eq!(
            registry.snapshot(41, WAITSET_MAX_INTERESTS).unwrap(),
            vec![interest(5, 102)]
        );
    }

    #[test]
    fn epoll_object_cap_admits_the_boundary_and_rejects_one_more_unchanged() {
        let mut registry = WaitSetRegistry::default();
        for token in 1..=WAITSET_MAX_EPOLL_OBJECTS as u64 {
            registry.create(token).unwrap();
        }
        let before = registry.clone();

        assert_eq!(
            registry.create(WAITSET_MAX_EPOLL_OBJECTS as u64 + 1),
            Err(WaitSetRegistryError::Capacity)
        );
        assert_eq!(registry, before);
    }

    #[test]
    fn checkpoint_restore_over_capacity_is_atomic() {
        let mut registry = WaitSetRegistry::default();
        registry.create(900_000).unwrap();
        let before = registry.clone();
        let tokens = (1..=WAITSET_MAX_EPOLL_OBJECTS as u64).collect::<Vec<_>>();

        assert_eq!(
            registry.restore_all(&tokens),
            Err(WaitSetRegistryError::Capacity)
        );
        assert_eq!(registry, before);
    }

    #[test]
    fn purge_exact_object_uses_only_the_bounded_reverse_membership() {
        let mut registry = WaitSetRegistry::default();
        registry.create(41).unwrap();
        registry.create(42).unwrap();
        registry.add(41, interest(5, 101)).unwrap();
        registry.add(41, interest(5, 102)).unwrap();
        registry.add(42, interest(6, 101)).unwrap();

        assert_eq!(registry.matching_interests(2, 101).len(), 2);
        let purged = registry.purge(2, 101);
        assert_eq!(purged, 2);
        assert!(purged <= WAITSET_MAX_GLOBAL_INTERESTS);
        assert!(registry.matching_interests(2, 101).is_empty());
        assert_eq!(
            registry.snapshot(41, WAITSET_MAX_INTERESTS).unwrap(),
            vec![interest(5, 102)]
        );
        assert!(registry
            .snapshot(42, WAITSET_MAX_INTERESTS)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn epoll_registry_has_one_service_lifetime_until_final_retire() {
        let mut registry = WaitSetRegistry::default();
        registry.create(73).unwrap();
        registry.add(73, interest(8, 301)).unwrap();
        assert!(registry.snapshot(73, 1).is_ok());
        registry.retire(73).unwrap();
        assert!(registry.matching_interests(2, 301).is_empty());
        assert_eq!(
            registry.snapshot(73, 1),
            Err(WaitSetRegistryError::NotFound)
        );
        assert_eq!(registry.retire(73), Err(WaitSetRegistryError::NotFound));
    }

    #[test]
    fn epoll_snapshot_rotates_a_persistently_ready_prefix() {
        let mut registry = WaitSetRegistry::default();
        registry.create(41).unwrap();
        for object in [101, 102, 103] {
            registry.add(41, interest(object, object)).unwrap();
        }
        let first = registry.snapshot(41, 1).unwrap()[0].key.object_id;
        let second = registry.snapshot(41, 1).unwrap()[0].key.object_id;
        assert_ne!(first, second);
    }

    #[test]
    fn provider_restart_updates_epoch_without_duplicating_registration_identity() {
        let mut registry = WaitSetRegistry::default();
        registry.create(41).unwrap();
        let original = interest(5, 101);
        registry.add(41, original).unwrap();

        let mut restarted = original;
        restarted.provider_epoch = 8;
        assert_eq!(
            registry.add(41, restarted),
            Err(WaitSetRegistryError::Exists)
        );
        registry.modify(41, restarted).unwrap();
        assert_eq!(registry.snapshot(41, 2).unwrap(), vec![restarted]);
        registry.delete(41, restarted.key).unwrap();
        assert!(registry.snapshot(41, 2).unwrap().is_empty());
        assert!(registry.matching_interests(2, 101).is_empty());
    }
}
