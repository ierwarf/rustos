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
use core::sync::atomic::{AtomicU64, Ordering};
use sha2::{Digest, Sha256};
use storage_core::file_io::{FileIdentity, ReadAt, TransferControl, TransferError, TransferFailure};
use vfsd::snapshot_pipeline::{PipelineControl, WindowDigest};

static NEXT_EXECUTABLE_SNAPSHOT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Mint one nonzero, never-reused-in-process snapshot transaction identity.
/// Exhaustion fails closed rather than wrapping and making an old evidence
/// record ambiguous with a new seal.
pub(crate) fn next_executable_snapshot_request_id() -> Result<u64, i32> {
    loop {
        let current = NEXT_EXECUTABLE_SNAPSHOT_REQUEST_ID.load(Ordering::Acquire);
        if current == 0 || current == u64::MAX {
            return Err(EOVERFLOW);
        }
        if NEXT_EXECUTABLE_SNAPSHOT_REQUEST_ID
            .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(current);
        }
    }
}

/// Mount-generation storage: the FAT volume and every cache derived from it.
///
/// This is deliberately separate from `VfsState`. The endpoint receive owner
/// owns namespace and descriptor state, while bulk executable-snapshot reads
/// run on a separate worker. Giving each of them its own `VfsState` gave each
/// its own `FatVolume` over the same device and its own caches, so the worker
/// could never hit a warm entry and two independent mutable views of one
/// device existed at once. Storage is therefore a single shared owner, and a
/// namespace-only request never has to wait behind a bulk read.
pub(crate) use vfsd::lru::LruIndex;

pub(crate) struct VfsStorage {
    pub(crate) mounts: MountTable,
    /// Positive + negative metadata cache. `Ok(_)` is a resolved entry;
    /// `Err(errno)` is a negative cache (e.g. ENOENT) so back-to-back stat()s
    /// of common missing libc paths return without touching the volume.
    pub(crate) metadata_cache: BTreeMap<CacheKey, Result<Metadata, i32>>,
    pub(crate) metadata_cache_lru: LruIndex<CacheKey>,
    /// Cached directory listings, keyed by `(mount_id, canonical_rel_path)`.
    pub(crate) dir_entries_cache: BTreeMap<CacheKey, Vec<DirEntry>>,
    pub(crate) dir_entries_cache_lru: LruIndex<CacheKey>,
    /// Complete bytes for bounded files on the current mount generation,
    /// invalidated on any mutation or remount.
    pub(crate) file_bytes_cache: BTreeMap<CacheKey, Vec<u8>>,
    pub(crate) file_bytes_cache_lru: LruIndex<CacheKey>,
    pub(crate) file_bytes_cache_bytes: usize,
    pub(crate) read_maps: BTreeMap<CacheKey, storage_core::file_io::FileReadMap>,
    pub(crate) read_maps_lru: LruIndex<CacheKey>,
    pub(crate) read_map_bytes: usize,
    /// Terminally sealed executable images owned by the current mount
    /// generation. Handle transfer duplicates the descriptor into loaderd;
    /// keeping one vfsd reference lets common interpreters/DLLs be reused
    /// without re-reading the storage DVM on every process launch.
    pub(crate) executable_snapshot_cache: BTreeMap<CacheKey, ExecutableSnapshot>,
    pub(crate) executable_snapshot_cache_bytes: usize,
    pub(crate) executable_snapshot_cache_seq: u64,
    /// Per-canonical-node open reference count, live length, and deferred
    /// unlink state for backends where `!supports_unlink_while_open`.
    pub(crate) open_nodes: BTreeMap<CacheKey, OpenNodeState>,
    pub(crate) mount_generation: u64,
    /// Exact storaged DVM block-provider generation that backs `mounts`.
    pub(crate) dvm_storage_epoch: u64,
    pub(crate) cache_generation: u64,
    /// Whether the boot-time memory filesystems are mounted.
    pub(crate) tmpfs_ready: bool,
}

impl VfsStorage {
    pub(crate) const fn new() -> Self {
        Self {
            mounts: MountTable::new(),
            metadata_cache: BTreeMap::new(),
            metadata_cache_lru: LruIndex::new(),
            dir_entries_cache: BTreeMap::new(),
            dir_entries_cache_lru: LruIndex::new(),
            file_bytes_cache: BTreeMap::new(),
            file_bytes_cache_lru: LruIndex::new(),
            file_bytes_cache_bytes: 0,
            read_maps: BTreeMap::new(),
            read_maps_lru: LruIndex::new(),
            read_map_bytes: 0,
            executable_snapshot_cache: BTreeMap::new(),
            executable_snapshot_cache_bytes: 0,
            executable_snapshot_cache_seq: 0,
            open_nodes: BTreeMap::new(),
            mount_generation: 1,
            dvm_storage_epoch: 0,
            cache_generation: 1,
            tmpfs_ready: false,
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
    pub(crate) mount_id: u64,
    pub(crate) cache_key: CacheKey,
    pub(crate) file_len: usize,
    pub(crate) read_map: Option<alloc::sync::Arc<storage_core::file_io::FileReadMap>>,
    pub(crate) metadata_len: u64,
    pub(crate) verbose: bool,
    /// The immutable bootstrap image and the DVM-mounted volume are distinct
    /// evidence sources. A sealed memfd proves bytes, not which source
    /// supplied them, so retain that source through commit and publication.
    pub(crate) backing: ExecutableSnapshotBacking,
    /// Storaged's nonzero block-provider generation, or zero for bootstrap.
    pub(crate) storage_epoch: u64,
    /// Vfsd-local transaction identity for one fresh source read and seal.
    pub(crate) request_id: u64,
    /// The mount generation the plan was resolved against.
    ///
    /// The bulk read runs without storage held, so a remount can land between
    /// the plan and the commit. Sealing then would admit an image read from the
    /// previous volume into the new generation's cache, where nothing would
    /// ever invalidate it. The completion re-checks this before it seals.
    pub(crate) mount_generation: u64,
}

pub(crate) enum ExecutableSnapshotAdmission {
    Cached(ExecutableSnapshotOpen),
    Read(ExecutableSnapshotPlan),
}

/// A source lease has no live FAT borrow. Every bounded read revalidates the
/// exact mount/provider/map generations, then releases storage before user
/// copy, hashing, memfd writes, sealing, or reply publication.
struct SnapshotReader<'a> {
    plan: &'a ExecutableSnapshotPlan,
    source: Option<alloc::boxed::Box<dyn ReadAt<Error = storage_core::file_io::RangeError> + Send>>,
    read_ns: u64,
}
impl ReadAt for SnapshotReader<'_> {
    type Error = i32;
    fn identity(&self) -> FileIdentity {
        self.plan.read_map.as_ref().map(|map| map.identity()).unwrap_or(FileIdentity {
            volume: u64::MAX, object: self.plan.request_id, generation: self.plan.mount_generation,
        })
    }
    fn len(&self) -> u64 { self.plan.metadata_len }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, i32> {
        // Check namespace/provider epochs briefly, then release that owner
        // before device IPC. The data lease contains no FAT namespace borrow.
        let started = monotonic_nanos();
        let result = (|| {
            let (valid, mount_gen, storage_epoch) = {
                let mut storage = lock_vfs_storage();
                let res = storage.validate_executable_snapshot_plan(self.plan);
                (res, storage.mount_generation, storage.dvm_storage_epoch)
            };
            if valid.is_err() {
                snapshot_cache::invalidate_if_stale(mount_gen, storage_epoch);
            }
            valid?;
            match self.source.as_mut() {
                Some(source) => source.read_at(offset, output).map_err(map_range_error),
                None => early_system::read(self.plan.path.as_str(), offset, output)?.ok_or(EIO),
            }
        })();
        self.read_ns = self.read_ns.saturating_add(monotonic_nanos().saturating_sub(started));
        result
    }
}

/// Adapts the snapshot session and deadline to the pipeline's control hooks.
struct SnapshotPipelineControl<'a> {
    control: &'a vfsd::snapshot_sessions::SnapshotControl,
    deadline: Option<AbsoluteDeadline>,
}
impl PipelineControl for SnapshotPipelineControl<'_> {
    fn check(&self) -> Result<(), TransferControl> {
        if self.control.cancelled() { Err(TransferControl::Cancelled) }
        else if budget_expired(self.deadline, monotonic_nanos()) { Err(TransferControl::Expired) }
        else { Ok(()) }
    }
    fn progress(&self, bytes: u64) { self.control.progress(bytes); }
}

/// SHA-256 of the image, with the time the hasher stage was busy.
struct SnapshotDigest {
    sha: Sha256,
    ns: u64,
}
impl SnapshotDigest {
    fn new() -> Self { Self { sha: Sha256::new(), ns: 0 } }
    fn finalize(self) -> [u8; 32] {
        let mut bytes = [0; 32];
        bytes.copy_from_slice(&self.sha.finalize());
        bytes
    }
}
impl WindowDigest for SnapshotDigest {
    fn update(&mut self, bytes: &[u8]) {
        let started = monotonic_nanos();
        self.sha.update(bytes);
        self.ns = self.ns.saturating_add(monotonic_nanos().saturating_sub(started));
    }
}

fn transfer_errno(error: &TransferFailure<i32, i32>) -> i32 {
    match &error.error {
        TransferError::Read(errno) | TransferError::Write(errno) => *errno,
        TransferError::Control(TransferControl::Expired) => ETIMEDOUT,
        TransferError::Control(TransferControl::Cancelled) => 125,
        TransferError::SourceChanged => EAGAIN,
        _ => EIO,
    }
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
/// ABI v3 admission requires a nonzero, bounded absolute job lease.
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
    control: &vfsd::snapshot_sessions::SnapshotControl,
) -> Result<ExecutableSnapshotOpen, i32> {
    let deadline = request_deadline(request);
    let started_ns = monotonic_nanos();
    if control.cancelled() { return Err(125); }
    if budget_expired(deadline, started_ns) {
        return Err(ETIMEDOUT);
    }
    let (admission, current_mount_gen, current_storage_epoch) = {
        let mut storage = lock_vfs_storage();
        let res = storage.plan_executable_snapshot(request);
        (res, storage.mount_generation, storage.dvm_storage_epoch)
    };
    snapshot_cache::invalidate_if_stale(current_mount_gen, current_storage_epoch);
    let plan = match admission? {
        ExecutableSnapshotAdmission::Cached(open) => {
            control.set_total(open.file_bytes());
            control.progress(open.file_bytes());
            return Ok(open);
        },
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
    control.set_total(plan.metadata_len);
    let path = plan.path.clone();
    let file_len = plan.file_len;
    let mut writer = SnapshotWriter::new(plan.metadata_len)?;
    let source = match plan.read_map.as_ref() {
        Some(map) => {
            let storage = lock_vfs_storage();
            let mount = storage.mounts.by_id(plan.mount_id).ok_or(ENODEV)?;
            let read_map_source = mount.read_map_source.as_ref().ok_or(EOPNOTSUPP)?;
            Some(read_map_source.read_source(map.clone()).map_err(map_range_error)?)
        }
        None => None,
    };
    let mut reader = SnapshotReader { plan: &plan, source, read_ns: 0 };
    // The ring is allocated once for this snapshot (billed by the writer's
    // reservation, `snapshot_reservation_bytes`) and freed as soon as the
    // transfer has stopped, before sealing and commit.
    let geometry = vfsd::snapshot_budget::snapshot_ring_geometry(reader.len()).ok_or(EIO)?;
    let mut ring = Vec::new();
    ring.try_reserve_exact(geometry.buffer_bytes().ok_or(EOVERFLOW)?).map_err(|_| ENOMEM)?;
    ring.resize(geometry.buffer_bytes().ok_or(EOVERFLOW)?, 0);
    // The digest is product evidence only for a verbose plan
    // (`commit_executable_snapshot`), so other plans do not pay for it.
    let mut digest = plan.verbose.then(SnapshotDigest::new);
    let hooks = SnapshotPipelineControl { control, deadline };
    let transfer_started = monotonic_nanos();
    let bulk_before = crate::block::bulk_read_counters();
    let _ = crate::block::take_bulk_read_max_nanos();
    let transfer = vfsd::snapshot_pipeline::run(
        &mut reader, &mut writer, digest.as_mut(), &mut ring, geometry, &hooks,
        &crate::bulk_lanes::StageThreads,
    );
    drop(ring);
    if let Err(error) = &transfer { control.progress(error.completed_bytes); }
    if file_len > 128 * 1024 * 1024 {
        let bulk = crate::block::bulk_read_counters().since(bulk_before);
        let stats = transfer.as_ref().copied().unwrap_or_default();
        // `total_us` is wall time. `read_us`/`write_us`/`hash_us` are the busy
        // time of each stage (they overlap, so they no longer sum to the
        // wall); `*_stall_us` is how long a stage waited on another one.
        ipc::debug_line(&format!("vfsd: snapshot transfer path={path} completed={} total_us={} read_us={} write_us={} hash_us={} storaged_calls={} storaged_us={} storaged_max_us={} windows={} helper_stages={} read_stall_us={} write_stall_us={} hash_stall_us={} status={}",
            control.completed_bytes(),
            monotonic_nanos().saturating_sub(transfer_started) / 1000, reader.read_ns / 1000, writer.write_ns() / 1000,
            digest.as_ref().map_or(0, |digest| digest.ns) / 1000, bulk.calls, bulk.nanos / 1000,
            crate::block::take_bulk_read_max_nanos() / 1000,
            stats.windows, stats.helper_stages, stats.reader_stall_ns / 1000,
            stats.writer_stall_ns / 1000, stats.hasher_stall_ns / 1000,
            transfer.as_ref().err().map_or(0, transfer_errno)));
    }
    transfer.map_err(|error| transfer_errno(&error))?;
    let snapshot = writer.seal(plan.mount_generation)?;
    if budget_expired(deadline, monotonic_nanos()) { return Err(ETIMEDOUT); }
    let digest_bytes = digest.map(SnapshotDigest::finalize);
    let read_ns = monotonic_nanos();
    let (publication, cache_key, current_mount_gen, current_storage_epoch) = {
        let mut storage = lock_vfs_storage();
        let pub_res = storage.commit_executable_snapshot(&plan, &snapshot, digest_bytes);
        // Retain in the 1-entry sealed snapshot cache only when the image is
        // small enough not to pin multi-hundred-MiB physical RAM across the boot
        // (<= 32 MiB; the 300 MiB Chrome binary is already backed by the kernel
        // demand-paged shared library object on first `mmap`).
        let is_immutable_dvm = plan.backing == ExecutableSnapshotBacking::DvmVolume
            && plan.storage_epoch != 0
            && plan.metadata_len <= 32 * 1024 * 1024
            && storage
                .mounts
                .by_id(plan.mount_id)
                .is_some_and(|mount| mount.volume.is_immutable());
        let key = (pub_res.is_ok() && is_immutable_dvm).then(|| snapshot_cache::SnapshotCacheKey {
            path: plan.path.clone(),
            mount_id: plan.mount_id,
            file_identity: plan.read_map.as_ref().map(|map| map.identity()),
            file_len: plan.metadata_len,
            mount_generation: plan.mount_generation,
            dvm_storage_epoch: plan.storage_epoch,
        });
        (pub_res, key, storage.mount_generation, storage.dvm_storage_epoch)
    };
    snapshot_cache::invalidate_if_stale(current_mount_gen, current_storage_epoch);
    // The old object (including hundreds of MiB of frames on error) must be
    // destroyed only after the storage owner has been released.
    let result = publication.map(|cached| cached.unwrap_or(snapshot));
    if let (Ok(ref open), Some(key)) = (&result, cache_key) {
        snapshot_cache::register(key, open.fd());
    }
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
