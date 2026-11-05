use super::VFS_IPC_PATH_CAPACITY;

/// Private loaderd-to-vfsd request for an immutable, terminally sealed file
/// snapshot. The returned memfd is transferred out-of-band with the reply;
/// this operation is never exposed as a Linux filesystem syscall.
pub const VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION: u16 = 3;
/// Begin a bounded job; returns EINPROGRESS and a one-shot, owner-bound ID.
pub const VFS_EXECUTABLE_SNAPSHOT_OP_OPEN: u16 = 1;
/// Nonblocking progress query; consumes the result only on a terminal reply.
pub const VFS_EXECUTABLE_SNAPSHOT_OP_POLL: u16 = 2;
pub const VFS_EXECUTABLE_SNAPSHOT_OP_CANCEL: u16 = 3;
pub const VFS_EXECUTABLE_SNAPSHOT_PENDING: i32 = 115;
/// Large work has its own lease, not a longer global/control IPC timeout.
// 1-vCPU concurrent GUI evidence: 138,149,888 bytes confirmed in 24 s.
// 120 s admits the 512-MiB policy ceiling with bounded scheduling headroom.
// This is job lifetime, NOT a longer blocking IPC/control wait.
pub const VFS_EXECUTABLE_SNAPSHOT_MAX_LEASE_MS: u64 = 120_000;
/// Ring3 materialization policy, not a filesystem or Ring0 address limit.
/// vfsd separately reserves final frames, metadata and scratch before reading.
pub const VFS_EXECUTABLE_SNAPSHOT_MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Stage-root basis for trusted loaderd launches that do not replace a live
/// process and therefore have no target PID cwd namespace.
pub const VFS_EXECUTABLE_SNAPSHOT_DIRFD_ROOT: u64 = 0;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsExecutableSnapshotRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub requester_pid: u64,
    pub requester_tid: u64,
    /// Process whose VFS namespace owns relative-path resolution. A zero value
    /// selects the trusted stage root and requires `dirfd` to be
    /// `VFS_EXECUTABLE_SNAPSHOT_DIRFD_ROOT`.
    pub target_pid: u64,
    /// Directory basis for resolution. Executable snapshots admit `AT_FDCWD`
    /// for a live target PID or the explicit stage-root basis for PID zero;
    /// vfsd, as namespace owner, interprets the value.
    pub dirfd: u64,
    pub max_bytes: u64,
    pub path_len: u32,
    pub reserved0: u32,
    /// Job's absolute CLOCK_MONOTONIC lease end (required, never renewed by
    /// POLL). Each control RPC has its own shorter bounded transport wait.
    ///
    /// CLOCK_MONOTONIC is comparable between services. OPEN/POLL do not retain
    /// their reply capabilities across data I/O. Expiry revokes publication
    /// even without client traffic. Workers reclaim completed orphans and observe
    /// active cancellation at a bounded backend window; physical cleanup can
    /// therefore follow expiry by that operation's bounded timeout grace.
    /// A late backend completion is discarded, never returned as executable.
    pub deadline_ns: u64,
    /// Zero on OPEN; exact owner/lease-bound token on POLL and CANCEL.
    pub job_id: u64,
    pub path: [u8; VFS_IPC_PATH_CAPACITY],
}

// Endpoint dispatch is size-tagged; an ABI change must not alias a different
// private wire record, even though the caller's role is authenticated too.
const _: () = assert!(core::mem::size_of::<VfsExecutableSnapshotRequest>() != core::mem::size_of::<super::VfsIpcRequest>());
const _: () = assert!(core::mem::size_of::<VfsExecutableSnapshotRequest>() != core::mem::size_of::<super::CommercialMaxProtocolRequest>());

impl Default for VfsExecutableSnapshotRequest {
    fn default() -> Self {
        Self {
            version: VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION,
            op: VFS_EXECUTABLE_SNAPSHOT_OP_OPEN,
            flags: 0,
            requester_pid: 0,
            requester_tid: 0,
            target_pid: 0,
            dirfd: 0,
            max_bytes: 0,
            path_len: 0,
            reserved0: 0,
            deadline_ns: 0,
            job_id: 0,
            path: [0; VFS_IPC_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsExecutableSnapshotResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub reserved0: u32,
    pub resolved_path_len: u32,
    pub file_bytes: u64,
    pub mount_generation: u64,
    pub job_id: u64,
    /// Bytes acknowledged by the private sink; not a published executable.
    pub completed_bytes: u64,
    /// Vfsd-owned canonical identity of the returned sealed executable.
    pub resolved_path: [u8; VFS_IPC_PATH_CAPACITY],
}

impl Default for VfsExecutableSnapshotResponse {
    fn default() -> Self {
        Self {
            version: 0,
            op: 0,
            status: 0,
            reserved0: 0,
            resolved_path_len: 0,
            file_bytes: 0,
            mount_generation: 0,
            job_id: 0,
            completed_bytes: 0,
            resolved_path: [0; VFS_IPC_PATH_CAPACITY],
        }
    }
}
