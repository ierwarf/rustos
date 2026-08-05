//! The single mount-generation storage owner.
//!
//! - **Owner:** this module owns the FAT volume and every cache derived from
//!   it. `VfsState` owns namespace, descriptor, and checkpoint state and never
//!   holds storage.
//! - **Boundary:** the endpoint receive owner and the snapshot worker are
//!   threads of one address space; both reach storage only through
//!   [`lock_vfs_storage`].
//! - **Lifecycle:** a snapshot is planned while storage is held, read in
//!   bounded chunks while it is not, and committed under a fresh acquisition.
//! - **Concurrency:** no holder ever retains storage across a bulk read, so
//!   the longest wait is one chunk. That hold discipline, not the waiter, is
//!   what bounds contention; this service must not use yield as a progress
//!   mechanism.
//! - **Failure:** a short or over-long chunk read fails the snapshot rather
//!   than sealing a partial image.
//! - **Forbidden:** no second `FatVolume` over the same device, no storage
//!   held across a bulk read, and no re-entrant acquisition.
//! - **Evidence:** `vfs-open-description` and the vfsd recovery scenarios.

use super::*;

/// Mount-generation storage: the FAT volume and every cache derived from it.
///
/// This is deliberately separate from `VfsState`. The endpoint receive owner
/// owns namespace and descriptor state, while bulk executable-snapshot reads
/// run on a separate worker. Giving each of them its own `VfsState` gave each
/// its own `FatVolume` over the same device and its own caches, so the worker
/// could never hit a warm entry and two independent mutable views of one
/// device existed at once. Storage is therefore a single shared owner, and a
/// namespace-only request never has to wait behind a bulk read.
pub(crate) struct VfsStorage {
    pub(crate) volume: Option<FatVolume<BootBlockDevice>>,
    /// Positive + negative metadata cache. `Ok(_)` is a resolved entry;
    /// `Err(errno)` is a negative cache (e.g. ENOENT) so back-to-back stat()s
    /// of common missing libc paths return without touching FAT. The whole map
    /// is dropped whenever `mount_generation` changes.
    pub(crate) metadata_cache: BTreeMap<String, Result<Metadata, i32>>,
    /// Cached directory listings, keyed by absolute path. Linux startup
    /// re-reads `/`, `/dev`, library directories, etc.; FAT traversal per call
    /// is expensive enough to dominate boot time when libc walks PATH.
    pub(crate) dir_entries_cache: BTreeMap<String, Vec<DirEntry>>,
    /// Complete bytes for bounded read-only files on the current mount
    /// generation. Persistent mutation is not implemented by vfsd, and every
    /// remount invalidates this cache before the replacement volume is used.
    pub(crate) file_bytes_cache: BTreeMap<String, Vec<u8>>,
    pub(crate) file_bytes_cache_bytes: usize,
    /// Terminally sealed executable images owned by the current mount
    /// generation. Handle transfer duplicates the descriptor into loaderd;
    /// keeping one vfsd reference lets common interpreters/DLLs be reused
    /// without re-reading the storage DVM on every process launch.
    pub(crate) executable_snapshot_cache: BTreeMap<String, ExecutableSnapshot>,
    pub(crate) executable_snapshot_cache_bytes: usize,
    pub(crate) mount_generation: u64,
    pub(crate) cache_generation: u64,
}

impl VfsStorage {
    pub(crate) const fn new() -> Self {
        Self {
            volume: None,
            metadata_cache: BTreeMap::new(),
            dir_entries_cache: BTreeMap::new(),
            file_bytes_cache: BTreeMap::new(),
            file_bytes_cache_bytes: 0,
            executable_snapshot_cache: BTreeMap::new(),
            executable_snapshot_cache_bytes: 0,
            mount_generation: 1,
            cache_generation: 1,
        }
    }
}

/// The single storage owner, shared by the receive owner and the snapshot
/// worker, which run as threads of one address space.
pub(crate) struct SharedVfsStorage {
    held: AtomicBool,
    storage: UnsafeCell<VfsStorage>,
}

// SAFETY: `held` is the sole admission gate. A caller enters the cell only
// after winning the Acquire compare-exchange and leaves before the Release
// store, so exactly one thread ever holds a reference to the storage.
unsafe impl Sync for SharedVfsStorage {}

static VFS_STORAGE: SharedVfsStorage = SharedVfsStorage {
    held: AtomicBool::new(false),
    storage: UnsafeCell::new(VfsStorage::new()),
};

/// Spin bound for one storage acquisition.
///
/// No holder retains storage across a bulk read, so a legitimate wait is one
/// short phase or one chunk. Exceeding this bound means the owner will never be
/// released, which is a lock-discipline bug rather than contention. Failing
/// loudly is mandatory: a silent spin here strands every filesystem request in
/// the system with no diagnostic.
const STORAGE_ACQUIRE_SPIN_LIMIT: u64 = 1 << 24;
/// Spins before falling back to sleeping. Short holds resolve inside this.
const STORAGE_ACQUIRE_SPIN_BEFORE_SLEEP: u64 = 4_096;

pub(crate) struct VfsStorageGuard;

/// Acquires the single storage owner.
///
/// Spinning is admissible here only because no holder ever retains storage
/// across a bulk read: a snapshot takes it for its plan, releases it for the
/// chunked read, and re-takes it to commit. The longest possible wait is one
/// chunk. This service must not use yield as a progress mechanism, so the
/// bound has to come from the hold discipline rather than from the waiter.
pub(crate) fn lock_vfs_storage() -> VfsStorageGuard {
    let mut spins: u64 = 0;
    loop {
        // ORDERING: Acquire observes the previous holder's complete storage
        // mutation before this caller may reach it.
        if VFS_STORAGE
            .held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return VfsStorageGuard;
        }
        spins = spins.saturating_add(1);
        assert!(
            spins < STORAGE_ACQUIRE_SPIN_LIMIT,
            "vfsd storage owner was not released within its bounded hold; the \
             usual cause is two live guards in one expression or a re-entrant \
             acquisition, which this owner does not support"
        );
        if spins < STORAGE_ACQUIRE_SPIN_BEFORE_SLEEP {
            core::hint::spin_loop();
        } else {
            // The holder may own storage for one device read. Spinning through
            // that would burn a scheduling turn that the holder needs.
            rustos_svc_runtime::syscall::sleep_millis(1);
        }
    }
}

impl core::ops::Deref for VfsStorageGuard {
    type Target = VfsStorage;

    fn deref(&self) -> &VfsStorage {
        // SAFETY: this guard exists only for the exclusive admitted holder.
        unsafe { &*VFS_STORAGE.storage.get() }
    }
}

impl core::ops::DerefMut for VfsStorageGuard {
    fn deref_mut(&mut self) -> &mut VfsStorage {
        // SAFETY: see `Deref`; the guard is the exclusive admission token.
        unsafe { &mut *VFS_STORAGE.storage.get() }
    }
}

impl Drop for VfsStorageGuard {
    fn drop(&mut self) {
        // ORDERING: Release publishes this holder's storage mutation to the
        // next acquirer.
        VFS_STORAGE.held.store(false, Ordering::Release);
    }
}

/// Immutable description of one executable-snapshot read.
///
/// Produced while storage is held, consumed without it. Nothing in here is a
/// live borrow of storage, which is what lets the bulk read run in bounded
/// chunks that other requests can interleave with.
pub(crate) struct ExecutableSnapshotPlan {
    pub(crate) path: String,
    pub(crate) file_len: usize,
    pub(crate) metadata_len: u64,
    pub(crate) verbose: bool,
}

pub(crate) enum ExecutableSnapshotAdmission {
    Cached(ExecutableSnapshotOpen),
    Read(ExecutableSnapshotPlan),
}

/// Reads a planned snapshot in one acquisition.
///
/// This is deliberately a single read rather than a chunked one. FAT resolves a
/// ranged read by walking the cluster chain from the start of the file, so
/// splitting the read into chunks turns one linear read into a quadratic one,
/// and the cache-materialization rule only fires when the request covers the
/// whole file. One acquisition for one device read is therefore the minimum a
/// single-volume design can hold, and it is the same hold the pre-split code
/// took. What the split actually removes is the second `FatVolume`, not this
/// hold.
fn read_planned_executable_snapshot(plan: &ExecutableSnapshotPlan) -> Result<Vec<u8>, i32> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(plan.file_len).map_err(|_| ENOMEM)?;
    bytes.resize(plan.file_len, 0);
    let read = lock_vfs_storage().read_executable_snapshot_chunk(plan, 0, bytes.as_mut_slice())?;
    if read != plan.file_len {
        return Err(EIO);
    }
    Ok(bytes)
}

/// Plans, reads, and commits one executable snapshot.
///
/// Storage is acquired for the plan, released for the chunked bulk read, and
/// re-acquired to seal and cache the image. The image is never sealed from a
/// stale plan: `commit_executable_snapshot` runs against the storage state at
/// commit time and a remount between the phases invalidates the cache it
/// writes into.
/// The caller's absolute deadline, or `None` when it set none.
///
/// A zero `deadline_ns` is "unbounded", which is what an older caller sends. It
/// is not treated as "already expired": that would fail every request from a
/// caller that has not been updated yet.
fn request_deadline(request: &VfsExecutableSnapshotRequest) -> Option<AbsoluteDeadline> {
    (request.deadline_ns != 0).then(|| AbsoluteDeadline::after(request.deadline_ns, 0))
}

/// Whether the caller's budget is already gone.
///
/// Declining here is not a shortcut. A reply produced after the caller
/// abandoned its reply capability is rejected by the kernel, and the caller
/// surfaces that as a permission failure rather than as the timeout it is —
/// exactly the misdiagnosis `V5-DEADLINE-012` exists to prevent. Refusing to
/// start work the caller can no longer accept is the correct terminal.
fn budget_expired(deadline: Option<AbsoluteDeadline>, now_ns: u64) -> bool {
    deadline.is_some_and(|deadline| deadline.remaining_ns(now_ns).is_none())
}

pub(crate) fn open_executable_snapshot(
    request: &VfsExecutableSnapshotRequest,
) -> Result<ExecutableSnapshotOpen, i32> {
    let deadline = request_deadline(request);
    let started_ns = monotonic_nanos();
    if budget_expired(deadline, started_ns) {
        return Err(ETIMEDOUT);
    }
    let plan = match lock_vfs_storage().plan_executable_snapshot(request)? {
        ExecutableSnapshotAdmission::Cached(open) => return Ok(open),
        ExecutableSnapshotAdmission::Read(plan) => plan,
    };
    let planned_ns = monotonic_nanos();
    // The whole bulk read is ahead; starting it with no budget left only
    // produces a reply nobody is waiting for.
    if budget_expired(deadline, planned_ns) {
        return Err(ETIMEDOUT);
    }
    if plan.verbose {
        ipc::debug_line(
            format!(
                "vfsd: executable snapshot volume-read begin path={} bytes={}",
                plan.path.as_str(),
                plan.file_len
            )
            .as_str(),
        );
    }
    let path = plan.path.clone();
    let file_len = plan.file_len;
    let bytes = read_planned_executable_snapshot(&plan)?;
    let read_ns = monotonic_nanos();
    let result = lock_vfs_storage().commit_executable_snapshot(&plan, bytes.as_slice());
    let committed_ns = monotonic_nanos();
    // Emitted for every snapshot, not only slow ones. The caller can only ever
    // report "the reply did not arrive"; without a provider-side split of plan,
    // read, and commit there is no way to tell a slow device from a contended
    // lock from a descheduled worker, and that ambiguity is what kept
    // `V5-VFSD-HOL-007` open on inference.
    ipc::debug_line(
        format!(
            "vfsd: executable snapshot phases path={path} bytes={file_len} plan_us={} read_us={} commit_us={} total_us={} status={}",
            (planned_ns.saturating_sub(started_ns)) / 1_000,
            (read_ns.saturating_sub(planned_ns)) / 1_000,
            (committed_ns.saturating_sub(read_ns)) / 1_000,
            (committed_ns.saturating_sub(started_ns)) / 1_000,
            match &result {
                Ok(_) => 0,
                Err(errno) => *errno,
            }
        )
        .as_str(),
    );
    result
}
