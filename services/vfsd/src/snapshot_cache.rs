//! Bounded 1-entry cache of the most recent sealed immutable-volume executable
//! snapshot for zero-storage-I/O `VFS_IPC_OP_BULK_READ` sharing.
//!
//! - **Owner:** `vfsd` owns the cached read-only sealed memfd descriptor and
//!   all eviction/invalidation closes; callers receive an independent `dup`ed
//!   descriptor wrapped in [`CachedSnapshotFd`].
//! - **Boundary:** Cache lookups are keyed by immutable volume identity
//!   (`mount_id`, `FileIdentity`, `path`, `file_len`, `mount_generation`, and
//!   `dvm_storage_epoch`). Only sealed memfds from immutable DVM volumes are
//!   admitted. Reads use kernel-direct `SYS_PREAD64` and never call `syscalld`.
//! - **Lifecycle:** Registered after an immutable-volume executable snapshot is
//!   sealed and committed; invalidated and closed on mount-generation change,
//!   DVM storage-epoch change, or replacement by a newer snapshot.
//! - **Concurrency:** Protected by a leaf `spin::Mutex` that never acquires
//!   `VFS_STORAGE`. `F_DUPFD_CLOEXEC` runs under the leaf lock so eviction
//!   cannot close the fd mid-dup; all `SYS_CLOSE` and `SYS_PREAD64` syscalls
//!   run outside the lock.
//! - **Failure:** Descriptor-duplication pressure or invalid keys skip caching
//!   without failing snapshot completion; stale lookups evict the stale entry
//!   and return a miss so callers fall back cleanly.
//! - **Forbidden:** No mutable-volume entries, no unsealed memfds, no
//!   `syscalld` IPC, no holding `VFS_STORAGE` from inside this module, and no
//!   closing descriptors while another thread can `dup` them.
//! - **Evidence:** Unit tests in this module covering hit, eviction, mount
//!   generation invalidation, DVM storage-epoch invalidation, and stale lookup.

use alloc::string::String;
use rustos_user_abi::linux as linux_abi;
use storage_core::file_io::FileIdentity;

use super::{close_fd, duplicate_snapshot_fd, EIO, EOVERFLOW};

/// Exact cache key for a sealed immutable-volume executable snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotCacheKey {
    pub(crate) path: String,
    pub(crate) mount_id: u64,
    pub(crate) file_identity: Option<FileIdentity>,
    pub(crate) file_len: u64,
    pub(crate) mount_generation: u64,
    pub(crate) dvm_storage_epoch: u64,
}

impl SnapshotCacheKey {
    fn is_valid(&self) -> bool {
        !self.path.is_empty()
            && self.file_len > 0
            && self.mount_generation != 0
            && self.dvm_storage_epoch != 0
    }

    fn matches_mapped(
        &self,
        mount_id: u64,
        file_identity: FileIdentity,
        file_len: u64,
        mount_generation: u64,
        dvm_storage_epoch: u64,
    ) -> bool {
        mount_generation != 0
            && dvm_storage_epoch != 0
            && file_len > 0
            && self.mount_id == mount_id
            && self.file_identity == Some(file_identity)
            && self.file_len == file_len
            && self.mount_generation == mount_generation
            && self.dvm_storage_epoch == dvm_storage_epoch
    }

    fn matches_path(
        &self,
        path: &str,
        file_len: u64,
        mount_generation: u64,
        dvm_storage_epoch: u64,
    ) -> bool {
        mount_generation != 0
            && dvm_storage_epoch != 0
            && file_len > 0
            && self.path == path
            && self.file_len == file_len
            && self.mount_generation == mount_generation
            && self.dvm_storage_epoch == dvm_storage_epoch
    }
}

struct SnapshotCacheEntry {
    key: SnapshotCacheKey,
    fd: i32,
}

/// Pure 1-entry state machine returning evicted descriptors so callers always
/// perform `SYS_CLOSE` outside the mutex.
pub(crate) struct SnapshotCacheSlot {
    entry: Option<SnapshotCacheEntry>,
}

impl SnapshotCacheSlot {
    pub(crate) const fn new() -> Self {
        Self { entry: None }
    }

    /// Replaces the single cached entry. Returns `Ok(Some(old_fd))` if a prior
    /// entry was evicted, or `Err(fd)` if the key or descriptor is invalid.
    pub(crate) fn insert(&mut self, key: SnapshotCacheKey, fd: i32) -> Result<Option<i32>, i32> {
        if !key.is_valid() || fd < 0 {
            return Err(fd);
        }
        let evicted = self
            .entry
            .replace(SnapshotCacheEntry { key, fd })
            .map(|old| old.fd);
        Ok(evicted)
    }

    /// Evicts the cached entry if its mount generation or DVM storage epoch no
    /// longer matches the current live generations.
    pub(crate) fn invalidate_if_stale(
        &mut self,
        mount_generation: u64,
        dvm_storage_epoch: u64,
    ) -> Option<i32> {
        let stale = self.entry.as_ref().is_some_and(|entry| {
            entry.key.mount_generation != mount_generation
                || entry.key.dvm_storage_epoch != dvm_storage_epoch
        });
        if stale {
            self.entry.take().map(|old| old.fd)
        } else {
            None
        }
    }

    /// Looks up a `BulkReadSource::Mapped` file by its immutable `FileIdentity`
    /// and epochs, calling `duplicate` while the slot is locked. Returns
    /// `(dup_fd, evicted_stale_fd)`.
    pub(crate) fn lookup_mapped_with<F>(
        &mut self,
        mount_id: u64,
        file_identity: FileIdentity,
        file_len: u64,
        mount_generation: u64,
        dvm_storage_epoch: u64,
        duplicate: F,
    ) -> (Option<i32>, Option<i32>)
    where
        F: FnOnce(i32) -> Result<i32, i32>,
    {
        if let Some(evicted) = self.invalidate_if_stale(mount_generation, dvm_storage_epoch) {
            return (None, Some(evicted));
        }
        let Some(entry) = self.entry.as_ref() else {
            return (None, None);
        };
        if !entry.key.matches_mapped(
            mount_id,
            file_identity,
            file_len,
            mount_generation,
            dvm_storage_epoch,
        ) {
            return (None, None);
        }
        (duplicate(entry.fd).ok(), None)
    }

    /// Looks up a path-backed read by canonical path, length, and epochs,
    /// calling `duplicate` while the slot is locked. Returns
    /// `(dup_fd, evicted_stale_fd)`.
    pub(crate) fn lookup_path_with<F>(
        &mut self,
        path: &str,
        file_len: u64,
        mount_generation: u64,
        dvm_storage_epoch: u64,
        duplicate: F,
    ) -> (Option<i32>, Option<i32>)
    where
        F: FnOnce(i32) -> Result<i32, i32>,
    {
        if let Some(evicted) = self.invalidate_if_stale(mount_generation, dvm_storage_epoch) {
            return (None, Some(evicted));
        }
        let Some(entry) = self.entry.as_ref() else {
            return (None, None);
        };
        if !entry
            .key
            .matches_path(path, file_len, mount_generation, dvm_storage_epoch)
        {
            return (None, None);
        }
        (duplicate(entry.fd).ok(), None)
    }
}

/// Owned `dup`ed handle to a cached sealed immutable-volume memfd.
pub(crate) struct CachedSnapshotFd(i32);

impl CachedSnapshotFd {
    pub(crate) fn raw(&self) -> i32 {
        self.0
    }

    /// Reads bytes from the sealed memfd at `offset` via kernel-direct
    /// `SYS_PREAD64` (`local_memfd_io`; never touches `syscalld` or `storaged`).
    pub(crate) fn pread_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, i32> {
        if out.is_empty() {
            return Ok(0);
        }
        // SAFETY: `out` is a live writable slice and `self.0` is an owned
        // private duplicate of a sealed memfd descriptor; `SYS_PREAD64` writes
        // at most `out.len()` bytes without mutating any shared cursor.
        let status = unsafe {
            rustos_svc_runtime::syscall::syscall4(
                linux_abi::SYS_PREAD64,
                self.0 as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
                offset,
            )
        };
        if status < 0 {
            return Err((-status) as i32);
        }
        let read = usize::try_from(status).map_err(|_| EOVERFLOW)?;
        if read > out.len() {
            return Err(EIO);
        }
        Ok(read)
    }
}

impl Drop for CachedSnapshotFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            close_fd(self.0);
        }
    }
}

static SEALED_SNAPSHOT_CACHE: spin::Mutex<SnapshotCacheSlot> =
    spin::Mutex::new(SnapshotCacheSlot::new());

/// Registers a newly completed sealed immutable-volume executable snapshot.
///
/// Duplicates `source_fd` into an independent read-only cache descriptor; if
/// descriptor pressure prevents duplication, leaves the cache unchanged.
pub(crate) fn register(key: SnapshotCacheKey, source_fd: i32) {
    if !key.is_valid() || source_fd < 0 {
        return;
    }
    let Ok(cache_fd) = duplicate_snapshot_fd(source_fd) else {
        return;
    };
    let evicted = {
        let mut slot = SEALED_SNAPSHOT_CACHE.lock();
        slot.insert(key, cache_fd)
    };
    match evicted {
        Ok(Some(old_fd)) => close_fd(old_fd),
        Ok(None) => {}
        Err(rejected_fd) => close_fd(rejected_fd),
    }
}

/// Invalidates and closes the cached snapshot if either `mount_generation` or
/// `dvm_storage_epoch` has advanced.
pub(crate) fn invalidate_if_stale(mount_generation: u64, dvm_storage_epoch: u64) {
    let evicted = {
        let mut slot = SEALED_SNAPSHOT_CACHE.lock();
        slot.invalidate_if_stale(mount_generation, dvm_storage_epoch)
    };
    if let Some(old_fd) = evicted {
        close_fd(old_fd);
    }
}

/// Looks up a `BulkReadSource::Mapped` file in the 1-entry sealed snapshot
/// cache, returning an owned duplicate fd on hit.
pub(crate) fn lookup_mapped(
    mount_id: u64,
    file_identity: FileIdentity,
    file_len: u64,
    mount_generation: u64,
    dvm_storage_epoch: u64,
) -> Option<CachedSnapshotFd> {
    let (dup_fd, evicted) = {
        let mut slot = SEALED_SNAPSHOT_CACHE.lock();
        slot.lookup_mapped_with(
            mount_id,
            file_identity,
            file_len,
            mount_generation,
            dvm_storage_epoch,
            duplicate_snapshot_fd,
        )
    };
    if let Some(old_fd) = evicted {
        close_fd(old_fd);
    }
    dup_fd.map(CachedSnapshotFd)
}

/// Looks up a canonical path in the 1-entry sealed snapshot cache, returning
/// an owned duplicate fd on hit.
pub(crate) fn lookup_path(
    path: &str,
    file_len: u64,
    mount_generation: u64,
    dvm_storage_epoch: u64,
) -> Option<CachedSnapshotFd> {
    let (dup_fd, evicted) = {
        let mut slot = SEALED_SNAPSHOT_CACHE.lock();
        slot.lookup_path_with(
            path,
            file_len,
            mount_generation,
            dvm_storage_epoch,
            duplicate_snapshot_fd,
        )
    };
    if let Some(old_fd) = evicted {
        close_fd(old_fd);
    }
    dup_fd.map(CachedSnapshotFd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    const CHROME_ID: FileIdentity = FileIdentity {
        volume: 7,
        object: 42,
        generation: 1,
    };

    fn chrome_key(mount_generation: u64, dvm_storage_epoch: u64) -> SnapshotCacheKey {
        SnapshotCacheKey {
            path: "/apps/chrome/chrome".to_string(),
            mount_id: 2,
            file_identity: Some(CHROME_ID),
            file_len: 306 * 1024 * 1024,
            mount_generation,
            dvm_storage_epoch,
        }
    }

    #[test]
    fn insert_and_lookup_by_mapped_identity_and_path() {
        let mut slot = SnapshotCacheSlot::new();
        assert_eq!(slot.insert(chrome_key(3, 9), 100), Ok(None));

        let (hit, evicted) =
            slot.lookup_mapped_with(2, CHROME_ID, 306 * 1024 * 1024, 3, 9, |fd| Ok(fd + 1000));
        assert_eq!(hit, Some(1100));
        assert_eq!(evicted, None);

        let (hit, evicted) = slot.lookup_path_with(
            "/apps/chrome/chrome",
            306 * 1024 * 1024,
            3,
            9,
            |fd| Ok(fd + 2000),
        );
        assert_eq!(hit, Some(2100));
        assert_eq!(evicted, None);
    }

    #[test]
    fn second_insert_evicts_previous_entry() {
        let mut slot = SnapshotCacheSlot::new();
        assert_eq!(slot.insert(chrome_key(3, 9), 100), Ok(None));

        let other_key = SnapshotCacheKey {
            path: "/apps/other/bin".to_string(),
            mount_id: 2,
            file_identity: Some(FileIdentity {
                volume: 7,
                object: 99,
                generation: 1,
            }),
            file_len: 4096,
            mount_generation: 3,
            dvm_storage_epoch: 9,
        };
        assert_eq!(slot.insert(other_key, 101), Ok(Some(100)));

        let (miss, evicted) =
            slot.lookup_mapped_with(2, CHROME_ID, 306 * 1024 * 1024, 3, 9, |fd| Ok(fd));
        assert_eq!(miss, None);
        assert_eq!(evicted, None);
    }

    #[test]
    fn invalidate_on_mount_generation_or_epoch_change() {
        let mut slot = SnapshotCacheSlot::new();
        assert_eq!(slot.insert(chrome_key(3, 9), 100), Ok(None));
        assert_eq!(slot.invalidate_if_stale(3, 9), None);
        assert_eq!(slot.invalidate_if_stale(4, 9), Some(100));
        assert_eq!(slot.invalidate_if_stale(4, 9), None);

        assert_eq!(slot.insert(chrome_key(4, 9), 102), Ok(None));
        assert_eq!(slot.invalidate_if_stale(4, 10), Some(102));
    }

    #[test]
    fn stale_lookup_lazily_evicts_and_returns_old_fd() {
        let mut slot = SnapshotCacheSlot::new();
        assert_eq!(slot.insert(chrome_key(3, 9), 100), Ok(None));

        let (hit, evicted) =
            slot.lookup_mapped_with(2, CHROME_ID, 306 * 1024 * 1024, 4, 9, |fd| Ok(fd));
        assert_eq!(hit, None);
        assert_eq!(evicted, Some(100));
    }

    #[test]
    fn rejects_invalid_keys_and_mismatched_lookups() {
        let mut slot = SnapshotCacheSlot::new();
        let mut bad = chrome_key(3, 9);
        bad.dvm_storage_epoch = 0;
        assert_eq!(slot.insert(bad, 50), Err(50));
        assert_eq!(slot.insert(chrome_key(0, 9), 51), Err(51));
        assert_eq!(slot.insert(chrome_key(3, 9), -1), Err(-1));

        assert_eq!(slot.insert(chrome_key(3, 9), 100), Ok(None));
        // Wrong length does not hit and does not evict.
        let (miss, evicted) = slot.lookup_mapped_with(2, CHROME_ID, 1234, 3, 9, |fd| Ok(fd));
        assert_eq!((miss, evicted), (None, None));
        // Still cached for the real length.
        let (hit, evicted) =
            slot.lookup_mapped_with(2, CHROME_ID, 306 * 1024 * 1024, 3, 9, |fd| Ok(fd));
        assert_eq!((hit, evicted), (Some(100), None));
    }
}
