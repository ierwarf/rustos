//! Linux file-metadata and bulk-copy syscalls on VFS-backed descriptors:
//! `fallocate`, `chmod`/`fchmod`/`fchmodat`/`fchmodat2`, `utimensat`,
//! `sendfile` and `copy_file_range`.
//!
//! - **Owner:** vfsd owns every inode, mode and timestamp; this module only
//!   validates the Linux argument shape, resolves descriptors with the rights
//!   the call requires, and issues one VFS request (`FALLOCATE`, `FCHMOD`,
//!   `CHMOD`, `SET_ATTRS`) per call. Permission policy belongs to vfsd.
//! - **Boundary:** validate user offsets and use only admitted descriptor rights.
//! - **Lifecycle:** a copy pins both open descriptions until it settles.
//! - **Concurrency:** cursor-based copies serialize through description gates.
//! - **Forbidden:** bypass seals or treat a pathname as an open capability.
//! - **Evidence:** copy argument/offset tests and the Chrome guest probe.
//! - **Bulk copy:** `sendfile` and `copy_file_range` bounce through one
//!   kernel buffer of at most [`COPY_CHUNK`] bytes and loop on positional
//!   read/write requests. Explicit offsets never touch the descriptor cursor;
//!   a remote-file cursor end reads and advances `RemoteVfsOpenDescription`
//!   directly under the description's cursor gate for the whole call.
//! - **Failure:** progress already made is returned instead of a later error.
use super::*;
use alloc::vec;
use rustos_user_abi::syscall::{
    VFS_CHOWN_FLAG_NOFOLLOW, VFS_IPC_OP_CHMOD, VFS_IPC_OP_CHOWN, VFS_IPC_OP_FALLOCATE,
    VFS_IPC_OP_FCHMOD, VFS_IPC_OP_FCHOWN, VFS_IPC_OP_LINKAT, VFS_IPC_OP_MKNOD,
    VFS_IPC_OP_PWRITE64, VFS_IPC_OP_SET_ATTRS, VFS_LINKAT_FLAG_SOURCE_DESCRIPTION,
    encode_two_path_payload,
};

const LINUX_EISDIR: i64 = 21;
const LINUX_EFBIG: i64 = 27;
const LINUX_ENAMETOOLONG: i64 = 36;

/// Largest bounce buffer of one copy iteration (a page multiple).
const COPY_CHUNK: usize = 16 * 4096;
/// Linux caps one `sendfile`/`copy_file_range` call at `MAX_RW_COUNT`.
const MAX_RW_COUNT: u64 = 0x7fff_f000;
const PERMISSION_BITS: u64 = 0o7777;
const O_APPEND_FLAG: u64 = 0o2000;
/// `VFS_IPC_OP_SET_ATTRS` mask bits (see vfsd `set_attrs_path`).
const ATTR_ATIME: u64 = rustos_user_abi::syscall::VFS_ATTR_ATIME;
const ATTR_MTIME: u64 = rustos_user_abi::syscall::VFS_ATTR_MTIME;
const ATTR_ATIME_NOW: u64 = rustos_user_abi::syscall::VFS_ATTR_ATIME_NOW;
const ATTR_MTIME_NOW: u64 = rustos_user_abi::syscall::VFS_ATTR_MTIME_NOW;

// ---------------------------------------------------------------------------
// fallocate
// ---------------------------------------------------------------------------

/// Validates `fallocate` arguments; returns the exclusive end offset.
fn fallocate_range(offset: u64, len: u64) -> Result<u64, i64> {
    let (offset, len) = (offset as i64, len as i64);
    if offset < 0 || len <= 0 {
        return Err(LINUX_EINVAL);
    }
    offset.checked_add(len).map(|end| end as u64).ok_or(LINUX_EFBIG)
}

pub fn syscall_linux_vfs_fallocate(fd: u64, mode: u64, offset: u64, len: u64) -> u64 {
    linux_unit_result((|| {
        let end = fallocate_range(offset, len)?;
        if let Some((memfd, rights)) = current_memfd_handle_with_rights(fd) {
            if !rights.allows_write() {
                return Err(LINUX_EBADF);
            }
            // Mode zero can eagerly allocate by extending memfd. KEEP_SIZE
            // needs backing reservation without publishing a new logical length.
            if mode != 0 {
                return Err(LINUX_EOPNOTSUPP);
            }
            if end > memfd.len() as u64 {
                let end = usize::try_from(end).map_err(|_| LINUX_EFBIG)?;
                memfd.truncate(end).map_err(memfd_error_to_linux_errno)?;
            }
            return Ok(());
        }
        let (remote, rights) = require_remote_vfs_with_rights(fd, LINUX_ENODEV)?;
        if !rights.allows_write() {
            return Err(LINUX_EBADF);
        }
        if remote.kind() != multitask::RemoteVfsHandleKind::File {
            return Err(LINUX_ENODEV);
        }
        call_vfs_fd_status(VFS_IPC_OP_FALLOCATE, fd, remote.remote_id(), |request| {
            request.arg0 = mode;
            request.arg1 = offset;
            request.arg2 = len;
        })
    })())
}

// ---------------------------------------------------------------------------
// chmod family
// ---------------------------------------------------------------------------

pub fn syscall_linux_vfs_fchmod(fd: u64, mode: u64) -> u64 {
    linux_unit_result((|| {
        let (remote, rights) = require_remote_vfs_with_rights(fd, LINUX_EOPNOTSUPP)?;
        // `O_PATH` descriptors grant neither read nor write and cannot `fchmod`.
        if !rights.allows_read() && !rights.allows_write() {
            return Err(LINUX_EBADF);
        }
        call_vfs_fd_status(VFS_IPC_OP_FCHMOD, fd, remote.remote_id(), |request| {
            request.arg0 = mode & PERMISSION_BITS;
        })
    })())
}

/// `fchmodat`/`fchmodat2` flag check: `AT_SYMLINK_NOFOLLOW` is refused because
/// vfsd always follows a trailing symlink (Linux itself answers
/// `EOPNOTSUPP` when the target is a link); unknown bits are `EINVAL`.
fn fchmodat_flags(flags: u64) -> Result<bool, i64> {
    let known = linux_abi::AT_SYMLINK_NOFOLLOW | linux_abi::AT_EMPTY_PATH;
    if flags & !known != 0 {
        return Err(LINUX_EINVAL);
    }
    if flags & linux_abi::AT_SYMLINK_NOFOLLOW != 0 {
        return Err(LINUX_EOPNOTSUPP);
    }
    Ok(flags & linux_abi::AT_EMPTY_PATH != 0)
}

pub fn syscall_linux_vfs_fchmodat(dirfd: u64, path_ptr: u64, mode: u64, flags: u64) -> u64 {
    linux_unit_result((|| {
        let empty_path_allowed = fchmodat_flags(flags)?;
        let path = copy_current_user_path_allow_empty(path_ptr, VFS_IPC_PATH_CAPACITY)?;
        if path.is_empty() {
            return if empty_path_allowed {
                let (remote, rights) = require_remote_vfs_with_rights(dirfd, LINUX_EOPNOTSUPP)?;
                if !rights.allows_read() && !rights.allows_write() {
                    return Err(LINUX_EBADF);
                }
                call_vfs_fd_status(VFS_IPC_OP_FCHMOD, dirfd, remote.remote_id(), |request| {
                    request.arg0 = mode & PERMISSION_BITS;
                })
            } else {
                Err(LINUX_ENOENT)
            };
        }
        let mut request = new_vfs_request(VFS_IPC_OP_CHMOD);
        request.arg0 = mode & PERMISSION_BITS;
        populate_vfs_path_base(&mut request, dirfd, &path)?;
        call_vfs_status(&request)
    })())
}

// ---------------------------------------------------------------------------
// chown / mknod / link families
// ---------------------------------------------------------------------------

pub fn syscall_linux_vfs_fchown(fd: u64, owner: u64, group: u64) -> u64 {
    linux_unit_result((|| {
        let (remote, rights) = require_remote_vfs_with_rights(fd, LINUX_EOPNOTSUPP)?;
        if !rights.allows_read() && !rights.allows_write() {
            return Err(LINUX_EBADF);
        }
        call_vfs_fd_status(VFS_IPC_OP_FCHOWN, fd, remote.remote_id(), |request| {
            request.arg0 = owner;
            request.arg1 = group;
        })
    })())
}

fn fchownat_flags(flags: u64) -> Result<(bool, u32), i64> {
    let known = linux_abi::AT_SYMLINK_NOFOLLOW | linux_abi::AT_EMPTY_PATH;
    if flags & !known != 0 {
        return Err(LINUX_EINVAL);
    }
    let vfs_flags = if flags & linux_abi::AT_SYMLINK_NOFOLLOW != 0 {
        VFS_CHOWN_FLAG_NOFOLLOW
    } else {
        0
    };
    Ok((flags & linux_abi::AT_EMPTY_PATH != 0, vfs_flags))
}

pub fn syscall_linux_vfs_fchownat(
    dirfd: u64,
    path_ptr: u64,
    owner: u64,
    group: u64,
    flags: u64,
) -> u64 {
    linux_unit_result((|| {
        let (empty_path_allowed, vfs_flags) = fchownat_flags(flags)?;
        let path = copy_current_user_path_allow_empty(path_ptr, VFS_IPC_PATH_CAPACITY)?;
        if path.is_empty() {
            return if empty_path_allowed {
                let (remote, rights) = require_remote_vfs_with_rights(dirfd, LINUX_EOPNOTSUPP)?;
                if !rights.allows_read() && !rights.allows_write() {
                    return Err(LINUX_EBADF);
                }
                call_vfs_fd_status(VFS_IPC_OP_FCHOWN, dirfd, remote.remote_id(), |request| {
                    request.arg0 = owner;
                    request.arg1 = group;
                })
            } else {
                Err(LINUX_ENOENT)
            };
        }
        let mut request = new_vfs_request(VFS_IPC_OP_CHOWN);
        request.arg0 = owner;
        request.arg1 = group;
        request.flags = vfs_flags;
        populate_vfs_path_base(&mut request, dirfd, &path)?;
        call_vfs_status(&request)
    })())
}

pub fn syscall_linux_vfs_mknodat(dirfd: u64, path_ptr: u64, mode: u64, dev: u64) -> u64 {
    linux_unit_result(call_vfs_path_status(VFS_IPC_OP_MKNOD, dirfd, path_ptr, |request| {
        request.arg0 = mode;
        request.arg1 = dev;
    }))
}

fn linkat_flags(flags: u64) -> Result<(bool, u64), i64> {
    let known = linux_abi::AT_SYMLINK_FOLLOW | linux_abi::AT_EMPTY_PATH;
    if flags & !known != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok((
        flags & linux_abi::AT_EMPTY_PATH != 0,
        flags & linux_abi::AT_SYMLINK_FOLLOW,
    ))
}

fn name_to_handle_at_flags(flags: u64) -> Result<bool, i64> {
    let known = linux_abi::AT_SYMLINK_FOLLOW | linux_abi::AT_EMPTY_PATH;
    if flags & !known != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(flags & linux_abi::AT_EMPTY_PATH != 0)
}

/// Validates `name_to_handle_at` arguments and reports `EOPNOTSUPP` because
/// RustOS VFS descriptions do not expose exportable NFS-style file handles.
pub fn syscall_linux_name_to_handle_at(
    _dirfd: u64,
    pathname_ptr: u64,
    handle_ptr: u64,
    mount_id_ptr: u64,
    flags: u64,
) -> u64 {
    linux_unit_result((|| {
        let empty_path_allowed = name_to_handle_at_flags(flags)?;
        let path = copy_current_user_path_allow_empty(pathname_ptr, VFS_IPC_PATH_CAPACITY)?;
        if path.is_empty() && !empty_path_allowed {
            return Err(LINUX_ENOENT);
        }
        if handle_ptr == 0 || mount_id_ptr == 0 {
            return Err(LINUX_EFAULT);
        }
        let _handle_bytes = read_user_struct::<u32>(handle_ptr)?;
        validate_user_write(mount_id_ptr, core::mem::size_of::<i32>())?;
        Err(LINUX_EOPNOTSUPP)
    })())
}

pub fn syscall_linux_vfs_linkat(
    olddirfd: u64,
    oldpath_ptr: u64,
    newdirfd: u64,
    newpath_ptr: u64,
    flags: u64,
) -> u64 {
    linux_unit_result((|| {
        let (empty_path_allowed, follow_flag) = linkat_flags(flags)?;
        let old_path = copy_current_user_path_allow_empty(oldpath_ptr, VFS_IPC_PATH_CAPACITY)?;
        let new_path = copy_current_user_path_allow_empty(newpath_ptr, VFS_IPC_PATH_CAPACITY)?;
        if new_path.is_empty() {
            return Err(LINUX_ENOENT);
        }
        let mut request = new_vfs_request(VFS_IPC_OP_LINKAT);
        if old_path.is_empty() {
            if !empty_path_allowed {
                return Err(LINUX_ENOENT);
            }
            let (remote, _rights) = require_remote_vfs_with_rights(olddirfd, LINUX_EOPNOTSUPP)?;
            request.flags = VFS_LINKAT_FLAG_SOURCE_DESCRIPTION;
            request.fd = olddirfd;
            request.remote_id = remote.remote_id();
        } else {
            request.dirfd = olddirfd;
            request.remote_id = resolve_vfs_dirfd_remote_id(olddirfd, &old_path)?;
        }
        request.arg0 = newdirfd;
        request.arg1 = follow_flag;
        request.arg2 = resolve_vfs_dirfd_remote_id(newdirfd, &new_path)?;
        let encoded_len =
            encode_two_path_payload(old_path.as_bytes(), new_path.as_bytes(), &mut request.payload)
                .ok_or(LINUX_ENAMETOOLONG)?;
        request.payload_len = encoded_len as u32;
        call_vfs_status(&request)
    })())
}

// ---------------------------------------------------------------------------
// utimensat / utime / utimes / futimesat
// ---------------------------------------------------------------------------

/// One decoded `timespec` of `utimensat`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimeSpec {
    Set(i64, u32),
    Now,
    Omit,
}

fn decode_utime_spec(tv_sec: i64, tv_nsec: i64) -> Result<TimeSpec, i64> {
    if tv_nsec == linux_abi::UTIME_NOW as i64 {
        Ok(TimeSpec::Now)
    } else if tv_nsec == linux_abi::UTIME_OMIT as i64 {
        Ok(TimeSpec::Omit)
    } else if (0..1_000_000_000).contains(&tv_nsec) {
        Ok(TimeSpec::Set(tv_sec, tv_nsec as u32))
    } else {
        Err(LINUX_EINVAL)
    }
}

/// Encode seconds and packed nanoseconds; vfsd resolves NOW once per request.
fn utime_attrs(atime: TimeSpec, mtime: TimeSpec) -> (u64, i64, i64, u64) {
    let resolve = |spec, bit, now_bit| match spec {
        TimeSpec::Set(secs, nanos) => (bit, secs, nanos),
        TimeSpec::Now => (bit | now_bit, 0, 0),
        TimeSpec::Omit => (0, 0, 0),
    };
    let (amask, asecs, ananos) = resolve(atime, ATTR_ATIME, ATTR_ATIME_NOW);
    let (mmask, msecs, mnanos) = resolve(mtime, ATTR_MTIME, ATTR_MTIME_NOW);
    (amask | mmask, asecs, msecs, u64::from(ananos) | (u64::from(mnanos) << 32))
}

fn decode_utime_timeval(tv_sec: i64, tv_usec: i64) -> Result<TimeSpec, i64> {
    if (0..1_000_000).contains(&tv_usec) {
        Ok(TimeSpec::Set(tv_sec, (tv_usec as u32) * 1000))
    } else {
        Err(LINUX_EINVAL)
    }
}

fn set_vfs_times(
    dirfd: u64,
    path: Option<&str>,
    flags: u64,
    atime: TimeSpec,
    mtime: TimeSpec,
) -> LinuxResult<()> {
    // Linux specifies that two OMITs succeed without checking the target.
    if atime == TimeSpec::Omit && mtime == TimeSpec::Omit {
        return Ok(());
    }
    let (mask, atime_secs, mtime_secs, nanos) = utime_attrs(atime, mtime);
    let mut request = new_vfs_request(VFS_IPC_OP_SET_ATTRS);
    request.arg0 = mask;
    request.arg1 = atime_secs as u64;
    request.arg2 = mtime_secs as u64;
    request.arg3 = nanos;
    request.flags = (flags & linux_abi::AT_SYMLINK_NOFOLLOW) as u32;
    let by_descriptor = match path {
        None => true,
        Some(path) => path.is_empty() && flags & linux_abi::AT_EMPTY_PATH != 0,
    };
    if by_descriptor {
        // `futimens`: vfsd addresses the open description's node, which still
        // exists after the name was unlinked.
        let (remote, rights) = require_remote_vfs_with_rights(dirfd, LINUX_EOPNOTSUPP)?;
        if !rights.allows_read() && !rights.allows_write() {
            return Err(LINUX_EBADF);
        }
        request.fd = dirfd;
        request.remote_id = remote.remote_id();
    } else if let Some(path) = path {
        if path.is_empty() {
            return Err(LINUX_ENOENT);
        }
        populate_vfs_path_base(&mut request, dirfd, path)?;
    }
    call_vfs_status(&request)
}

pub fn syscall_linux_vfs_utimensat(dirfd: u64, path_ptr: u64, times_ptr: u64, flags: u64) -> u64 {
    linux_unit_result((|| {
        let known = linux_abi::AT_SYMLINK_NOFOLLOW | linux_abi::AT_EMPTY_PATH;
        if flags & !known != 0 {
            return Err(LINUX_EINVAL);
        }
        let (atime, mtime) = if times_ptr == 0 {
            (TimeSpec::Now, TimeSpec::Now)
        } else {
            let times = read_user_struct_as::<[linux_abi::LinuxTimespec; 2]>(times_ptr)?;
            (
                decode_utime_spec(times[0].tv_sec, times[0].tv_nsec)?,
                decode_utime_spec(times[1].tv_sec, times[1].tv_nsec)?,
            )
        };
        let path = if path_ptr == 0 {
            None
        } else {
            Some(copy_current_user_path_allow_empty(path_ptr, VFS_IPC_PATH_CAPACITY)?)
        };
        set_vfs_times(dirfd, path.as_deref(), flags, atime, mtime)
    })())
}

pub fn syscall_linux_vfs_utime(path_ptr: u64, times_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let path = copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY)?;
        let (atime, mtime) = if times_ptr == 0 {
            (TimeSpec::Now, TimeSpec::Now)
        } else {
            let times = read_user_struct_as::<[i64; 2]>(times_ptr)?;
            (TimeSpec::Set(times[0], 0), TimeSpec::Set(times[1], 0))
        };
        set_vfs_times(linux_abi::AT_FDCWD as u64, Some(&path), 0, atime, mtime)
    })())
}

pub fn syscall_linux_vfs_futimesat(dirfd: u64, path_ptr: u64, times_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let path = if path_ptr == 0 {
            None
        } else {
            Some(copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY)?)
        };
        let (atime, mtime) = if times_ptr == 0 {
            (TimeSpec::Now, TimeSpec::Now)
        } else {
            let times = read_user_struct_as::<[linux_abi::LinuxTimeval; 2]>(times_ptr)?;
            (
                decode_utime_timeval(times[0].tv_sec, times[0].tv_usec)?,
                decode_utime_timeval(times[1].tv_sec, times[1].tv_usec)?,
            )
        };
        set_vfs_times(dirfd, path.as_deref(), 0, atime, mtime)
    })())
}

// ---------------------------------------------------------------------------
// sendfile / copy_file_range
// ---------------------------------------------------------------------------

/// One end of a bulk copy: a regular VFS file, memfd, or (for `sendfile`
/// output) a pinned local pipe/socket or netd socket.
enum CopyEnd {
    Remote { fd: u64, remote: multitask::RemoteVfsHandle },
    Memfd(multitask::MemfdHandle),
    Local(super::local_fd::LocalFd),
    Socket { fd: u64, snapshot: SocketSnapshot },
}

impl CopyEnd {
    /// Identity of the underlying file, for the `copy_file_range` overlap rule.
    /// Two descriptors of one memfd or one open description compare equal; two
    /// separate opens of the same path do not (vfsd offers no cheaper identity).
    fn file_identity(&self) -> (u8, u64) {
        match self {
            CopyEnd::Remote { remote, .. } => (0, remote.remote_id()),
            CopyEnd::Memfd(memfd) => (1, memfd.object_id()),
            CopyEnd::Local(local) => (2, local.token_id()),
            CopyEnd::Socket { snapshot, .. } => (3, snapshot.token()),
        }
    }

    /// Identity of the open description (cursor owner).
    fn description_identity(&self) -> (u8, u64) {
        match self {
            CopyEnd::Remote { remote, .. } => (0, remote.remote_id()),
            CopyEnd::Memfd(memfd) => (1, memfd.token_id()),
            CopyEnd::Local(local) => (2, local.token_id()),
            CopyEnd::Socket { snapshot, .. } => (3, snapshot.token()),
        }
    }

    fn pread(&self, offset: u64, dest: &mut [u8]) -> Result<usize, i64> {
        match self {
            CopyEnd::Remote { fd, remote } => {
                kernel_vfs_pread_bytes_pinned(*fd, remote.remote_id(), offset, dest)
            }
            CopyEnd::Memfd(memfd) => {
                let offset = usize::try_from(offset).map_err(|_| LINUX_EINVAL)?;
                Ok(memfd.read_at(offset, dest))
            }
            CopyEnd::Local(_) | CopyEnd::Socket { .. } => Err(LINUX_EINVAL),
        }
    }

    /// Positional write of up to `data.len()` bytes; a short count is progress.
    fn pwrite(&self, offset: u64, data: &[u8]) -> Result<usize, i64> {
        match self {
            CopyEnd::Remote { fd, remote } => {
                let mut written = 0usize;
                while written < data.len() {
                    let chunk = &data[written..];
                    let chunk = &chunk[..chunk.len().min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY)];
                    let at = offset.checked_add(written as u64).ok_or(LINUX_EOVERFLOW)?;
                    if at > i64::MAX as u64 {
                        return if written > 0 { Ok(written) } else { Err(LINUX_EOVERFLOW) };
                    }
                    let mut request = new_vfs_request(VFS_IPC_OP_PWRITE64);
                    request.fd = *fd;
                    request.remote_id = remote.remote_id();
                    request.arg0 = at;
                    request.arg1 = chunk.len() as u64;
                    request.payload_len = chunk.len() as u32;
                    request.payload[..chunk.len()].copy_from_slice(chunk);
                    let accepted = match call_vfs_ipc_request(&request).and_then(|response| {
                        ensure_vfs_status(&response)?;
                        if response.value > chunk.len() as u64 {
                            return Err(LINUX_EINVAL);
                        }
                        Ok(response.value as usize)
                    }) {
                        Ok(accepted) => accepted,
                        Err(errno) => return if written > 0 { Ok(written) } else { Err(errno) },
                    };
                    written += accepted;
                    if accepted < chunk.len() {
                        break;
                    }
                }
                Ok(written)
            }
            CopyEnd::Memfd(memfd) => {
                let offset = usize::try_from(offset).map_err(|_| LINUX_EINVAL)?;
                memfd.write_at(offset, data).map_err(memfd_error_to_linux_errno)
            }
            CopyEnd::Local(_) | CopyEnd::Socket { .. } => Err(LINUX_EINVAL),
        }
    }

    fn cursor(&mut self) -> Result<u64, i64> {
        match self {
            CopyEnd::Remote { remote, .. } => Ok(remote.cursor()),
            CopyEnd::Memfd(memfd) => memfd
                .seek(0, multitask::FileHandleSeekWhence::Current)
                .map_err(|_| LINUX_EINVAL),
            CopyEnd::Local(_) | CopyEnd::Socket { .. } => Err(LINUX_ESPIPE),
        }
    }

    fn advance_cursor(&mut self, start: u64, copied: u64) -> Result<(), i64> {
        match self {
            CopyEnd::Remote { remote, .. } => {
                let delta = usize::try_from(copied).map_err(|_| LINUX_EOVERFLOW)?;
                remote.advance_cursor(delta);
                Ok(())
            }
            CopyEnd::Memfd(memfd) => {
                let end = start
                    .checked_add(copied)
                    .and_then(|value| i64::try_from(value).ok())
                    .ok_or(LINUX_EOVERFLOW)?;
                memfd
                    .seek(end, multitask::FileHandleSeekWhence::Start)
                    .map(|_| ())
                    .map_err(|_| LINUX_EINVAL)
            }
            CopyEnd::Local(_) | CopyEnd::Socket { .. } => Err(LINUX_ESPIPE),
        }
    }

    /// Write at the descriptor cursor (honours `O_APPEND` in vfsd) and advance it.
    fn write_cursor(&mut self, data: &[u8]) -> Result<usize, i64> {
        match self {
            CopyEnd::Remote { fd, remote } => {
                let chunk = &data[..data.len().min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY)];
                let observed_epoch =
                    ipc_ops::service_endpoint_epoch(IPC_SERVICE_VFSD).unwrap_or(0);
                let mut request = new_vfs_request(VFS_IPC_OP_WRITE);
                request.fd = *fd;
                request.remote_id = remote.remote_id();
                request.flags |= VFS_WRITE_FLAG_CURSOR_SYNC;
                request.arg0 = remote.cursor();
                request.payload_len = chunk.len() as u32;
                request.payload[..chunk.len()].copy_from_slice(chunk);
                let response = call_vfs_ipc_request(&request)?;
                ensure_vfs_status(&response)?;
                if response.value > chunk.len() as u64 {
                    return Err(LINUX_EINVAL);
                }
                remote.set_synced_cursor(response.aux, observed_epoch);
                Ok(response.value as usize)
            }
            CopyEnd::Memfd(memfd) => memfd.write_from(data).map_err(memfd_error_to_linux_errno),
            CopyEnd::Local(local) => super::local_fd::write_pinned_local(local, data),
            CopyEnd::Socket { fd, snapshot } => netd_socket_write_bytes(snapshot, *fd, data),
        }
    }
}

/// Resolves `fd` to a copy end after the access-mode check (`EBADF`) and the
/// file-type check (`EISDIR` for directories, `EINVAL` for anything that is
/// not a regular file: pipes, sockets, devices).
fn resolve_copy_end(fd: u64, need_read: bool) -> Result<(CopyEnd, u64), i64> {
    let status = current_fd_status_flags(fd).unwrap_or(0);
    let allowed = |rights: kernel_object::api::HandleRights| {
        if need_read {
            rights.allows_read()
        } else {
            rights.allows_write()
        }
    };
    if let Some((memfd, rights)) = current_memfd_handle_with_rights(fd) {
        return if allowed(rights) {
            Ok((CopyEnd::Memfd(memfd), status))
        } else {
            Err(LINUX_EBADF)
        };
    }
    if let Some((remote, rights)) = current_remote_vfs_handle_with_rights(fd) {
        if !allowed(rights) {
            return Err(LINUX_EBADF);
        }
        return match remote.kind() {
            multitask::RemoteVfsHandleKind::File => Ok((
                CopyEnd::Remote {
                    fd,
                    remote,
                },
                status,
            )),
            multitask::RemoteVfsHandleKind::Directory => Err(LINUX_EISDIR),
            multitask::RemoteVfsHandleKind::Device | multitask::RemoteVfsHandleKind::Inotify => {
                Err(LINUX_EINVAL)
            }
        };
    }
    Err(if current_kernel_handle(fd).is_some() { LINUX_EINVAL } else { LINUX_EBADF })
}

/// `copy_file_range` argument rules independent of any descriptor state.
/// `Ok(len)` is the clamped length; `len == 0` copies nothing.
fn copy_file_range_args(
    flags: u64,
    len: u64,
    off_in: Option<u64>,
    off_out: Option<u64>,
) -> Result<u64, i64> {
    if flags != 0 {
        return Err(LINUX_EINVAL);
    }
    let len = len.min(MAX_RW_COUNT);
    for offset in [off_in, off_out].into_iter().flatten() {
        if (offset as i64) < 0 {
            return Err(LINUX_EINVAL);
        }
        if (offset as i64).checked_add(len as i64).is_none() {
            return Err(LINUX_EOVERFLOW);
        }
    }
    Ok(len)
}

/// `true` when `[a, a+len)` and `[b, b+len)` intersect.
fn ranges_overlap(a: u64, b: u64, len: u64) -> bool {
    if len == 0 {
        return false;
    }
    let a_end = u128::from(a) + u128::from(len);
    let b_end = u128::from(b) + u128::from(len);
    u128::from(a) < b_end && u128::from(b) < a_end
}

/// Where a copy end reads/writes: an explicit offset or its cursor.
enum CopyPos {
    At(u64),
    Cursor,
}

/// Core loop shared by both syscalls. `src_pos`/`dst_pos` select explicit
/// offsets or cursors. Returns bytes copied and the position each end reached
/// (for explicit offsets the caller writes it back to user space). Holds the
/// cursor gate of every cursor end, lower `remote_id` first.
fn copy_bytes(
    src: &mut CopyEnd,
    src_pos: &CopyPos,
    dst: &mut CopyEnd,
    dst_pos: &CopyPos,
    len: u64,
    reject_same_desc_overlap: bool,
) -> Result<(u64, u64, u64), i64> {
    let mut gates = [None, None];
    let mut want: [Option<u64>; 2] = [None, None];
    for (end, pos) in [(&*src, src_pos), (&*dst, dst_pos)] {
        if let (CopyEnd::Remote { remote, .. }, CopyPos::Cursor) = (end, pos) {
            let remote_id = remote.remote_id();
            if !want.contains(&Some(remote_id)) {
                let slot = usize::from(want[0].is_some());
                want[slot] = Some(remote_id);
            }
        }
    }
    if let [Some(a), Some(b)] = want
        && a > b
    {
        want.swap(0, 1);
    }
    for (slot, id) in want.iter().enumerate() {
        if let Some(id) = id {
            gates[slot] = Some(acquire_vfs_cursor_gate(*id)?);
        }
    }

    let mut src_at = match src_pos {
        CopyPos::At(offset) => *offset,
        CopyPos::Cursor => src.cursor()?,
    };
    let src_start = src_at;
    let mut dst_at = match dst_pos {
        CopyPos::At(offset) => *offset,
        CopyPos::Cursor => 0,
    };
    let dst_start = dst_at;
    if reject_same_desc_overlap && src.description_identity() == dst.description_identity() {
        match (src_pos, dst_pos) {
            (CopyPos::Cursor, CopyPos::At(b)) if ranges_overlap(src_at, *b, len) => {
                return Err(LINUX_EINVAL);
            }
            (CopyPos::At(a), CopyPos::Cursor) => {
                let dst_cursor = dst.cursor().map_err(|_| LINUX_EINVAL)?;
                if ranges_overlap(*a, dst_cursor, len) {
                    return Err(LINUX_EINVAL);
                }
            }
            (CopyPos::Cursor, CopyPos::Cursor) => return Err(LINUX_EINVAL),
            _ => {}
        }
    }
    let mut buffer = vec![0_u8; COPY_CHUNK.min(len.max(1) as usize)];
    let mut copied = 0_u64;
    let mut failure = None;
    'copy: while copied < len {
        let want_now = ((len - copied) as usize).min(buffer.len());
        let read = match src.pread(src_at, &mut buffer[..want_now]) {
            Ok(read) => read,
            Err(errno) => {
                failure = Some(errno);
                break;
            }
        };
        if read == 0 {
            break;
        }
        let mut flushed = 0usize;
        while flushed < read {
            let accepted = match dst_pos {
                CopyPos::At(_) => dst.pwrite(dst_at, &buffer[flushed..read]),
                CopyPos::Cursor => dst.write_cursor(&buffer[flushed..read]),
            };
            let accepted = match accepted {
                Ok(0) => {
                    failure = Some(LINUX_ENOSPC);
                    0
                }
                Ok(accepted) => accepted,
                Err(errno) => {
                    failure = Some(errno);
                    0
                }
            };
            if accepted == 0 {
                break;
            }
            flushed += accepted;
            copied += accepted as u64;
            let Some(next_src) = src_at.checked_add(accepted as u64) else {
                failure.get_or_insert(LINUX_EOVERFLOW);
                break 'copy;
            };
            let Some(next_dst) = dst_at.checked_add(accepted as u64) else {
                failure.get_or_insert(LINUX_EOVERFLOW);
                break 'copy;
            };
            src_at = next_src;
            dst_at = next_dst;
        }
        if flushed < read {
            break 'copy;
        }
        multitask::cond_resched();
    }
    // The source cursor advances by exactly the bytes delivered, never by
    // bytes that were read but not written.
    if matches!(src_pos, CopyPos::Cursor)
        && copied > 0
        && let Err(errno) = src.advance_cursor(src_start, copied)
    {
        failure.get_or_insert(errno);
    }
    drop(gates);
    match failure {
        Some(errno) if copied == 0 => Err(errno),
        _ => {
            let src_end = src_start.checked_add(copied).ok_or(LINUX_EOVERFLOW)?;
            let dst_end = dst_start.checked_add(copied).ok_or(LINUX_EOVERFLOW)?;
            Ok((copied, src_end, dst_end))
        }
    }
}

fn read_user_offset(ptr: u64) -> Result<u64, i64> {
    validate_user_write_as(ptr, size_of::<u64>())?;
    read_user_struct_as::<u64>(ptr)
}

pub fn syscall_linux_vfs_copy_file_range(
    fd_in: u64,
    off_in_ptr: u64,
    fd_out: u64,
    off_out_ptr: u64,
    len: u64,
    flags: u64,
) -> u64 {
    linux_result((|| {
        let (mut src, _) = resolve_copy_end(fd_in, true)?;
        let (mut dst, dst_status) = resolve_copy_end(fd_out, false)?;
        if dst_status & O_APPEND_FLAG != 0 {
            return Err(LINUX_EBADF);
        }
        let off_in = (off_in_ptr != 0).then(|| read_user_offset(off_in_ptr)).transpose()?;
        let off_out = (off_out_ptr != 0).then(|| read_user_offset(off_out_ptr)).transpose()?;
        let len = copy_file_range_args(flags, len, off_in, off_out)?;
        if len == 0 {
            return Ok(0);
        }
        // Same file: explicit/explicit and cursor/cursor overlap are fixed
        // upfront; cursor/explicit overlap is rechecked under the cursor gate in
        // `copy_bytes` once that gate is held.
        if src.file_identity() == dst.file_identity() {
            let same_description = src.description_identity() == dst.description_identity();
            match (off_in, off_out) {
                (Some(a), Some(b)) if ranges_overlap(a, b, len) => return Err(LINUX_EINVAL),
                (None, None) if same_description => return Err(LINUX_EINVAL),
                _ => {}
            }
        }
        let src_pos = off_in.map_or(CopyPos::Cursor, CopyPos::At);
        let dst_pos = off_out.map_or(CopyPos::Cursor, CopyPos::At);
        let _pins = pin_ends(&src, &dst)?;
        let (copied, src_end, dst_end) =
            copy_bytes(&mut src, &src_pos, &mut dst, &dst_pos, len, true)?;
        // Pointers were validated writable above; a late fault after progress
        // still reports the bytes copied.
        if off_in_ptr != 0 {
            let _ = write_user_bytes_as(off_in_ptr, &src_end.to_le_bytes());
        }
        if off_out_ptr != 0 {
            let _ = write_user_bytes_as(off_out_ptr, &dst_end.to_le_bytes());
        }
        Ok(copied)
    })())
}

fn resolve_sendfile_out_end(fd: u64) -> Result<CopyEnd, i64> {
    match resolve_copy_end(fd, false) {
        Ok((end, _)) => Ok(end),
        Err(LINUX_EINVAL) => {
            if let Some(local) = super::local_fd::current_local_fd(fd) {
                match &local {
                    super::local_fd::LocalFd::Pipe(pipe) => {
                        if pipe.role() == kernel_ps::api::PipeRole::Write {
                            Ok(CopyEnd::Local(local))
                        } else {
                            Err(LINUX_EBADF)
                        }
                    }
                    super::local_fd::LocalFd::UnixSocket(_) => {
                        if current_fd_allows_write(fd) {
                            Ok(CopyEnd::Local(local))
                        } else {
                            Err(LINUX_EBADF)
                        }
                    }
                    super::local_fd::LocalFd::EventFd(_)
                    | super::local_fd::LocalFd::TimerFd(_) => Err(LINUX_EINVAL),
                }
            } else if let Some(snapshot) = snapshot_socket(fd) {
                if current_fd_allows_write(fd) {
                    Ok(CopyEnd::Socket { fd, snapshot })
                } else {
                    Err(LINUX_EBADF)
                }
            } else {
                Err(LINUX_EINVAL)
            }
        }
        Err(errno) => Err(errno),
    }
}

pub fn syscall_linux_vfs_sendfile(out_fd: u64, in_fd: u64, offset_ptr: u64, count: u64) -> u64 {
    linux_result((|| {
        // The source must be a seekable regular file; the destination may be a
        // regular file, memfd, pipe write end, or socket.
        let (mut src, _) = resolve_copy_end(in_fd, true)?;
        let mut dst = resolve_sendfile_out_end(out_fd)?;
        let offset = (offset_ptr != 0)
            .then(|| {
                let off = read_user_offset(offset_ptr)?;
                if (off as i64) < 0 {
                    return Err(LINUX_EINVAL);
                }
                Ok(off)
            })
            .transpose()?;
        let count = count.min(MAX_RW_COUNT);
        if let Some(offset) = offset
            && (offset as i64).checked_add(count as i64).is_none()
        {
            return Err(LINUX_EOVERFLOW);
        }
        if count == 0 {
            return Ok(0);
        }
        if offset.is_none() && src.description_identity() == dst.description_identity() {
            // One cursor cannot be both the read and the write position.
            return Err(LINUX_EINVAL);
        }
        let src_pos = offset.map_or(CopyPos::Cursor, CopyPos::At);
        let _pins = pin_ends(&src, &dst)?;
        let (copied, src_end, _) =
            copy_bytes(&mut src, &src_pos, &mut dst, &CopyPos::Cursor, count, false)?;
        if offset_ptr != 0 {
            let _ = write_user_bytes_as(offset_ptr, &src_end.to_le_bytes());
        }
        Ok(copied)
    })())
}

/// Pins every remote end for the whole copy so a concurrent `close` cannot
/// retire the open description between two bounce chunks. On failure nothing
/// stays pinned.
fn pin_ends(src: &CopyEnd, dst: &CopyEnd) -> LinuxResult<[Option<super::vfs_socket::RemoteVfsRefGuard>; 2]> {
    let pin_one = |end: &CopyEnd| -> LinuxResult<Option<super::vfs_socket::RemoteVfsRefGuard>> {
        match end {
            CopyEnd::Remote { fd, remote } => {
                super::vfs_socket::RemoteVfsRefGuard::acquire(*fd, remote.remote_id()).map(Some)
            }
            _ => Ok(None),
        }
    };
    Ok([pin_one(src)?, pin_one(dst)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallocate_range_rejects_negative_zero_and_overflow() {
        assert_eq!(fallocate_range(0, 4096), Ok(4096));
        assert_eq!(fallocate_range(10, 0), Err(LINUX_EINVAL));
        assert_eq!(fallocate_range((-1_i64) as u64, 8), Err(LINUX_EINVAL));
        assert_eq!(fallocate_range(0, (-5_i64) as u64), Err(LINUX_EINVAL));
        assert_eq!(fallocate_range(i64::MAX as u64, 1), Err(LINUX_EFBIG));
    }

    #[test]
    fn fchmodat_flags_gate_nofollow_and_unknown_bits() {
        assert_eq!(fchmodat_flags(0), Ok(false));
        assert_eq!(fchmodat_flags(linux_abi::AT_EMPTY_PATH), Ok(true));
        assert_eq!(fchmodat_flags(linux_abi::AT_SYMLINK_NOFOLLOW), Err(LINUX_EOPNOTSUPP));
        assert_eq!(fchmodat_flags(0x4000_0000), Err(LINUX_EINVAL));
    }

    #[test]
    fn utime_specs_decode_now_omit_and_reject_bad_nanoseconds() {
        assert_eq!(decode_utime_spec(7, 0), Ok(TimeSpec::Set(7, 0)));
        assert_eq!(decode_utime_spec(-7, 999_999_999), Ok(TimeSpec::Set(-7, 999_999_999)));
        assert_eq!(decode_utime_spec(0, linux_abi::UTIME_NOW as i64), Ok(TimeSpec::Now));
        assert_eq!(decode_utime_spec(0, linux_abi::UTIME_OMIT as i64), Ok(TimeSpec::Omit));
        assert_eq!(decode_utime_spec(0, 1_000_000_000), Err(LINUX_EINVAL));
        assert_eq!(decode_utime_spec(0, -1), Err(LINUX_EINVAL));
    }

    #[test]
    fn utime_attrs_preserves_nanoseconds_and_now_markers() {
        assert_eq!(utime_attrs(TimeSpec::Set(-1, 7), TimeSpec::Set(2, 9)), (3, -1, 2, 7 | (9_u64 << 32)));
        assert_eq!(utime_attrs(TimeSpec::Omit, TimeSpec::Now), (ATTR_MTIME | ATTR_MTIME_NOW, 0, 0, 0));
        assert_eq!(utime_attrs(TimeSpec::Now, TimeSpec::Omit), (ATTR_ATIME | ATTR_ATIME_NOW, 0, 0, 0));
        assert_eq!(utime_attrs(TimeSpec::Omit, TimeSpec::Omit), (0, 0, 0, 0));
    }

    #[test]
    fn copy_file_range_args_enforce_flags_offsets_and_overflow() {
        assert_eq!(copy_file_range_args(0, 100, None, None), Ok(100));
        assert_eq!(copy_file_range_args(0, u64::MAX, None, None), Ok(MAX_RW_COUNT));
        assert_eq!(copy_file_range_args(1, 100, None, None), Err(LINUX_EINVAL));
        assert_eq!(copy_file_range_args(0, 100, Some((-1_i64) as u64), None), Err(LINUX_EINVAL));
        assert_eq!(
            copy_file_range_args(0, 100, None, Some(i64::MAX as u64 - 10)),
            Err(LINUX_EOVERFLOW)
        );
        assert_eq!(copy_file_range_args(0, 0, Some(5), Some(9)), Ok(0));
    }

    fn remote_rights(kind: multitask::RemoteVfsHandleKind, status: u64) -> kernel_object::api::HandleRights {
        let remote = multitask::RemoteVfsHandle::new(9, kind, alloc::string::String::from("/x"), 0, 0)
            .expect("token");
        multitask::KernelHandle::RemoteVfs(remote).default_rights(status)
    }

    /// `read`/`pread`/`readv` gate on `allows_read()`: a write-only or `O_PATH`
    /// description must be refused, every readable one must pass.
    #[test]
    fn remote_read_gate_matches_the_open_access_mode() {
        use multitask::RemoteVfsHandleKind::{Device, Directory, File};
        assert!(remote_rights(File, linux_abi::O_RDONLY).allows_read());
        assert!(remote_rights(File, linux_abi::O_RDWR).allows_read());
        assert!(!remote_rights(File, linux_abi::O_WRONLY).allows_read());
        assert!(remote_rights(File, linux_abi::O_WRONLY).allows_write());
        assert!(!remote_rights(File, linux_abi::O_PATH).allows_read());
        assert!(!remote_rights(File, linux_abi::O_PATH).allows_write());
        assert!(!remote_rights(Directory, linux_abi::O_PATH | linux_abi::O_DIRECTORY).allows_read());
        assert!(remote_rights(Directory, linux_abi::O_RDONLY | linux_abi::O_DIRECTORY).allows_read());
        assert!(remote_rights(Device, linux_abi::O_WRONLY).allows_read());
    }

    #[test]
    fn fchownat_and_linkat_flags_gate_known_bits() {
        assert_eq!(fchownat_flags(0), Ok((false, 0)));
        assert_eq!(
            fchownat_flags(linux_abi::AT_SYMLINK_NOFOLLOW | linux_abi::AT_EMPTY_PATH),
            Ok((true, VFS_CHOWN_FLAG_NOFOLLOW))
        );
        assert_eq!(fchownat_flags(0x4000_0000), Err(LINUX_EINVAL));

        assert_eq!(linkat_flags(0), Ok((false, 0)));
        assert_eq!(
            linkat_flags(linux_abi::AT_SYMLINK_FOLLOW | linux_abi::AT_EMPTY_PATH),
            Ok((true, linux_abi::AT_SYMLINK_FOLLOW))
        );
        assert_eq!(linkat_flags(linux_abi::AT_SYMLINK_NOFOLLOW), Err(LINUX_EINVAL));
        assert_eq!(linkat_flags(0x4000_0000), Err(LINUX_EINVAL));

        assert_eq!(name_to_handle_at_flags(0), Ok(false));
        assert_eq!(
            name_to_handle_at_flags(linux_abi::AT_SYMLINK_FOLLOW | linux_abi::AT_EMPTY_PATH),
            Ok(true)
        );
        assert_eq!(
            name_to_handle_at_flags(linux_abi::AT_SYMLINK_NOFOLLOW),
            Err(LINUX_EINVAL)
        );
    }

    #[test]
    fn utime_timeval_converts_micros_and_rejects_out_of_range() {
        assert_eq!(decode_utime_timeval(10, 0), Ok(TimeSpec::Set(10, 0)));
        assert_eq!(
            decode_utime_timeval(-3, 999_999),
            Ok(TimeSpec::Set(-3, 999_999_000))
        );
        assert_eq!(decode_utime_timeval(0, -1), Err(LINUX_EINVAL));
        assert_eq!(decode_utime_timeval(0, 1_000_000), Err(LINUX_EINVAL));
    }

    #[test]
    fn range_overlap_is_symmetric_and_half_open() {
        assert!(ranges_overlap(0, 5, 10));
        assert!(ranges_overlap(5, 0, 10));
        assert!(!ranges_overlap(0, 10, 10));
        assert!(!ranges_overlap(10, 0, 10));
        assert!(ranges_overlap(3, 3, 1));
        assert!(!ranges_overlap(3, 3, 0));
    }

    #[test]
    fn remote_copy_end_reads_and_advances_kernel_cursor_without_lseek() {
        let remote = multitask::RemoteVfsHandle::new_with_epoch(
            42,
            multitask::RemoteVfsHandleKind::File,
            alloc::string::String::from("/data/copy.bin"),
            8192,
            0,
            7,
        )
        .expect("remote handle");
        remote.set_synced_cursor(100, 7);
        let mut end = CopyEnd::Remote {
            fd: 3,
            remote: remote.clone(),
        };
        assert_eq!(end.cursor(), Ok(100));
        assert_eq!(end.advance_cursor(100, 512), Ok(()));
        assert_eq!(end.cursor(), Ok(612));
        assert_eq!(remote.cursor(), 612);
        assert_eq!(remote.durable_cursor(), 100);
        assert!(remote.is_dirty(7));
    }

    #[test]
    fn ranges_overlap_handles_extreme_u64_boundaries() {
        assert!(ranges_overlap(u64::MAX, u64::MAX, 1));
        assert!(!ranges_overlap(u64::MAX - 1, u64::MAX, 1));
        assert!(ranges_overlap(u64::MAX - 2, u64::MAX - 1, 4));
    }

    #[test]
    fn memfd_advance_cursor_rejects_overflow() {
        let memfd = multitask::MemfdHandle::new(
            alloc::string::String::from("copy-overflow"),
            false,
            None,
        );
        let mut end = CopyEnd::Memfd(memfd);
        assert_eq!(end.advance_cursor(i64::MAX as u64, 1), Err(LINUX_EOVERFLOW));
        assert_eq!(end.advance_cursor(u64::MAX, 1), Err(LINUX_EOVERFLOW));
        assert_eq!(end.advance_cursor(10, 5), Ok(()));
        assert_eq!(end.cursor(), Ok(15));
    }
}
