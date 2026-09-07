use super::VFS_IPC_PATH_CAPACITY;

/// Private loaderd-to-vfsd request for an immutable, terminally sealed file
/// snapshot. The returned memfd is transferred out-of-band with the reply;
/// this operation is never exposed as a Linux filesystem syscall.
pub const VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION: u16 = 2;
pub const VFS_EXECUTABLE_SNAPSHOT_OP_OPEN: u16 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsExecutableSnapshotRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub requester_pid: u64,
    pub requester_tid: u64,
    /// Process whose VFS namespace owns relative-path resolution. A zero value
    /// is valid only when `path` is already absolute.
    pub target_pid: u64,
    /// Directory basis for resolution. Executable snapshots currently admit
    /// only `AT_FDCWD`; vfsd, as namespace owner, interprets the value.
    pub dirfd: u64,
    pub max_bytes: u64,
    pub path_len: u32,
    pub reserved0: u32,
    /// The caller's absolute `CLOCK_MONOTONIC` deadline in nanoseconds, or 0
    /// when the caller sets none.
    ///
    /// `CLOCK_MONOTONIC` is derived from the system tick counter, so this is
    /// comparable in the provider's own process. Carrying the end instant
    /// rather than a duration is what lets the provider decide *not* to reply:
    /// a reply produced after the caller abandoned its reply capability is
    /// rejected by the kernel and reported to the caller as a permission
    /// failure, which is how a latency problem gets misread as an authority
    /// problem. See `V5-DEADLINE-012`.
    pub deadline_ns: u64,
    pub path: [u8; VFS_IPC_PATH_CAPACITY],
}

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
            resolved_path: [0; VFS_IPC_PATH_CAPACITY],
        }
    }
}
