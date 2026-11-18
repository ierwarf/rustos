//! `VFS_IPC_OP_SHARED_BACKING`: coherent writable `MAP_SHARED` of TmpFs and FAT files.
//!
//! A TmpFs inode normally keeps its bytes as vfsd-private 4 KiB chunks, and a
//! FAT file lives on the block device. A writable shared mapping needs the
//! *same physical frames* that `read`, `write`, `pread`, `pwrite` and
//! `ftruncate` use, so the MM broker asks vfsd to back the file with a memfd
//! that vfsd owns and to name that memfd. Ring0 then maps the memfd's frames
//! exactly as for a `MAP_SHARED` memfd mapping.
//!
//! Request: ordinary VFS header. `remote_id` names the client's open
//! description, `pid` the client that owns it (the mapping broker acts for the
//! client, as for `FILE_IDENTITY`/`PREAD64`), `arg0` carries
//! `VFS_SHARED_BACKING_FLAG_*`. When `VFS_SHARED_BACKING_FLAG_SYNC` is set,
//! `remote_id` is `0` and `arg1` names the memfd object id to flush back to
//! disk (or `0` to flush all shared-backed path files).
//!
//! Response (`status == 0`): `value` is vfsd's own descriptor number of the
//! backing memfd and `aux` is that memfd's never-recycled object id. The
//! kernel re-checks both against vfsd's descriptor table before it trusts the
//! pair, so a descriptor that vfsd closed and reused between reply and lookup
//! is refused rather than mapped.
//!
//! Refusals: `EACCES` (write intent on a description that cannot write),
//! `ENODEV` (early-system/virtual files keep their existing private-copy
//! mapping policy), `ENOSPC` (the dense copy does not fit the mount quota),
//! `ENOMEM`/`EMFILE` (vfsd could not allocate the memfd).

/// Additive VFS IPC op; `VFS_IPC_ABI_VERSION` is unchanged.
pub const VFS_IPC_OP_SHARED_BACKING: u16 = 39;

/// The caller will map the backing writable.
pub const VFS_SHARED_BACKING_FLAG_WRITE: u64 = 1 << 0;

/// Flush dirty shared-backed file frames (identified by memfd object id in
/// `arg1`, or `0` for all) back to their backing filesystem.
pub const VFS_SHARED_BACKING_FLAG_SYNC: u64 = 1 << 1;

/// Mapping-broker operation: map a VFS file `MAP_SHARED` through its
/// vfsd-owned shared backing. Same argument shape as `MAP_MEMFD_SHARED` with
/// `fd` naming the client's remote-VFS file descriptor.
pub const MM_BROKER_OP_MAP_FILE_SHARED: u16 = 20;
