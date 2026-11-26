// SPDX-License-Identifier: MIT

use rustos_user_abi::syscall::{
    ProductExecutableSnapshotEvidence, VFS_EXECUTABLE_SNAPSHOT_DIRFD_ROOT,
};

static SHARED_PATH_FILES: spin::Mutex<vfsd::tmpfs::SharedPathStore<CacheKey>> =
    spin::Mutex::new(vfsd::tmpfs::SharedPathStore::new());
fn map_range_error(error: storage_core::file_io::RangeError) -> i32 {
    use storage_core::file_io::RangeError;
    match error {
        RangeError::Stale => EAGAIN,
        RangeError::Budget | RangeError::NoMemory => ENOMEM,
        RangeError::Invalid => EIO,
        RangeError::Io(error) => map_fat_error(storage_fat::FatError::Io(error)),
    }
}

/// Whether an `open` metadata errno is an answer rather than a fault.
///
/// `open` is how a caller asks whether a path exists, so `ENOENT` is the
/// defined reply to a probe and not something the filesystem got wrong -
/// Linux logs nothing for it either. Storage that has not finished coming up
/// is likewise a retry, not a fault. Both arrive at a steady cadence from
/// pollers, and every line they write competes for the one serialized debug
/// sink that carries the acceptance evidence, so logging them turns a normal
/// probe into evidence loss. Genuine faults still get a line.
fn open_metadata_errno_is_routine(errno: i32) -> bool {
    errno == ENOENT || block::is_transient_storage_not_ready(errno)
}

fn credentials(request: &VfsIpcRequest, real: bool) -> vfsd::permissions::Credentials {
    if real { vfsd::permissions::Credentials { uid: request.uid, gid: request.gid } }
    else { vfsd::permissions::Credentials { uid: request.euid, gid: request.egid } }
}
fn check_metadata_permission(metadata: Metadata, request: &VfsIpcRequest, bits: u32, real: bool) -> Result<(), i32> {
    if metadata.fs.is_some_and(|fs| !vfsd::permissions::permits(fs, credentials(request, real), bits)) { Err(EACCES) } else { Ok(()) }
}
fn check_owner(metadata: Metadata, request: &VfsIpcRequest) -> Result<(), i32> {
    if metadata.fs.is_some_and(|fs| !vfsd::permissions::owner(fs, credentials(request, false))) { Err(EPERM) } else { Ok(()) }
}
fn check_parent(storage: &mut VfsStorage, path: &str, request: &VfsIpcRequest, victim: bool) -> Result<(), i32> {
    let end = path.rfind('/').unwrap_or(0);
    let parent = if end == 0 { "/" } else { &path[..end] };
    let dir = storage.metadata(parent)?;
    if dir.kind != RemoteKind::Directory { return Err(ENOTDIR); }
    check_metadata_permission(dir, request, 3, false)?;
    if victim {
        if let (Some(dir), Some(child)) = (dir.fs, storage.metadata(path)?.fs) {
            if !vfsd::permissions::sticky_allows(dir, child, credentials(request, false)) { return Err(EPERM); }
        }
    }
    Ok(())
}

impl VfsState {
    /// Converts an executable request into one canonical absolute path while
    /// the namespace receive owner has exclusive access to cwd state. The
    /// worker receives only this resolved request and therefore cannot invent
    /// a second cwd authority while performing bulk I/O.
    fn resolve_executable_snapshot_request(
        &mut self,
        mut request: VfsExecutableSnapshotRequest,
    ) -> Result<VfsExecutableSnapshotRequest, i32> {
        if request.version != VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION
            || request.op != VFS_EXECUTABLE_SNAPSHOT_OP_OPEN
            || request.flags != 0
            || request.reserved0 != 0
            || request.job_id != 0
            || request.deadline_ns == 0
            || request.path_len == 0
            || request.path_len as usize > request.path.len()
        {
            return Err(EINVAL);
        }
        let raw_path = core::str::from_utf8(&request.path[..request.path_len as usize])
            .map_err(|_| EINVAL)?;
        if raw_path.as_bytes().contains(&0) {
            return Err(EINVAL);
        }
        let base = executable_snapshot_resolution_base(
            raw_path,
            request.target_pid,
            request.dirfd == VFS_EXECUTABLE_SNAPSHOT_DIRFD_ROOT,
            is_at_fdcwd(request.dirfd),
            self.cwd.get(&request.target_pid).map(String::as_str),
        )?;
        let resolved = normalize_absolute_path(base, raw_path)?;
        request.path.fill(0);
        request.path_len = resolved.len() as u32;
        request.path[..resolved.len()].copy_from_slice(resolved.as_bytes());
        Ok(request)
    }

    fn cwd_for_pid(&mut self, pid: u64) -> String {
        self.cwd
            .entry(pid)
            .or_insert_with(|| String::from("/"))
            .clone()
    }

    fn chdir(&mut self, pid: u64, path: &str) -> i32 {
        match lock_vfs_storage().metadata(path) {
            Ok(metadata) if metadata.kind == RemoteKind::Directory => {
                self.cwd.insert(pid, path.to_string());
                0
            }
            Ok(_) => ENOTDIR,
            Err(errno) => errno,
        }
    }

    /// Preserve the raw component order until after symlink expansion.
    fn resolve_path_follow(
        &mut self,
        request: &VfsIpcRequest,
        dirfd: u64,
        path: &str,
        follow_last: bool,
    ) -> Result<String, i32> {
        vfsd::path::check_path_capacity(path.len())?;
        if path.is_empty() || path.bytes().any(|b| b == 0 || b == b'\\')
        {
            return Err(EINVAL);
        }
        use alloc::borrow::Cow;
        let raw = if path.starts_with('/') {
            Cow::Borrowed(path)
        } else {
            // Only the authenticated netd socket-namespace shape may delegate
            // cwd selection; normal requests always use their stamped PID.
            let pid = if request.flags & rustos_user_abi::syscall::VFS_REQUEST_FLAG_NETD_SOCKET_NODE != 0 {
                if request.arg3 == 0 { return Err(EACCES); }
                request.arg3
            } else { request.pid };
            let base = self.resolve_path_with_base(pid, dirfd, request.remote_id, ".")?;
            Cow::Owned(if base == "/" { format!("/{path}") } else { format!("{base}/{path}") })
        };
        // Check search at every step, including before `..` and on each
        // symlink target, rather than only on the normalized final path.
        let mut pending = vfsd::path::SymlinkComponents::new(&raw);
        let mut current = String::with_capacity(raw.len().max(1).min(VFS_IPC_PATH_CAPACITY));
        current.push('/');
        let mut storage = lock_vfs_storage();
        let mut expansions = 0;
        while let Some(component) = pending.next_component() {
            if storage.tmpfs_owns(&current) {
                let dir = storage.metadata(&current)?;
                if dir.kind != RemoteKind::Directory { return Err(ENOTDIR); }
                check_metadata_permission(dir, request, 1, false)?;
            }
            if component == "." { continue; }
            if component == ".." {
                let end = current.rfind('/').unwrap_or(0); current.truncate(end.max(1)); continue;
            }
            if current.len() > 1 { current.push('/'); }
            current.push_str(component);
            vfsd::path::check_path_capacity(current.len())?;
            let Some((entry, rel)) = storage.mounts.resolve(&current) else { continue; };
            if !entry.volume.capabilities().supports_symlinks { continue; }
            match entry.volume.metadata(rel) {
                Ok(meta) if meta.kind == FsNodeKind::Symlink && (follow_last || !pending.is_empty()) => {
                    expansions += 1;
                    if expansions > vfsd::SYMLINK_FOLLOW_MAX { return Err(ELOOP); }
                    let target = entry.volume.readlink(rel).map_err(vfsd::map_fs_error)?;
                    if target.is_empty() || target.bytes().any(|b| b == 0 || b == b'\\') { return Err(EINVAL); }
                    let end = current.rfind('/').unwrap_or(0); current.truncate(end.max(1));
                    if target.starts_with('/') { current.truncate(1); }
                    pending.prepend_target(target);
                }
                Ok(meta) if !pending.is_empty() && meta.kind != FsNodeKind::Directory => return Err(ENOTDIR),
                Ok(_) => {},
                Err(storage_core::FsError::NotFound) if pending.is_empty() => {},
                Err(storage_core::FsError::NotFound) => return Err(ENOENT),
                Err(e) => return Err(vfsd::map_fs_error(e)),
            }
        }
        Ok(current)
    }

    fn resolve_path_with_base(
        &mut self,
        pid: u64,
        dirfd: u64,
        base_remote_id: u64,
        path: &str,
    ) -> Result<String, i32> {
        vfsd::path::check_path_capacity(path.len())?;
        if path.is_empty() {
            return Err(EINVAL);
        }
        let base: &str = if path.starts_with('/') {
            "/"
        } else if is_at_fdcwd(dirfd) {
            if let Some(cwd) = self.cwd.get(&pid) {
                cwd.as_str()
            } else {
                let mut clean_relative = !path.starts_with('/');
                for component in path.split('/') {
                    if component.is_empty() || component == "." || component == ".." {
                        clean_relative = false;
                        break;
                    }
                }
                if clean_relative {
                    // The fast path must honour the same bound and NUL
                    // rejection as `normalize_absolute_path`; it skips the
                    // normalizer only because the path has no `.`/`..`.
                    vfsd::path::check_path_capacity(path.len() + 1)?;
                    if path.as_bytes().iter().any(|&b| b == 0 || b == b'\\')
                    {
                        return Err(EINVAL);
                    }
                    let mut resolved = String::with_capacity(path.len() + 1);
                    resolved.push('/');
                    resolved.push_str(path);
                    return Ok(resolved);
                }
                return normalize_absolute_path("/", path);
            }
        } else {
            let base_handle_id = if base_remote_id != 0 {
                base_remote_id
            } else {
                dirfd
            };
            let handle = self.handles.get(&base_handle_id).ok_or(EBADF)?;
            if handle.stale {
                return Err(ESTALE);
            }
            if handle.kind != RemoteKind::Directory {
                return Err(ENOTDIR);
            }
            if handle.node != 0 {
                let storage = lock_vfs_storage();
                let mount = storage.mounts.by_id(handle.mount_id).ok_or(ESTALE)?;
                let relative = mount.volume.directory_path_node(handle.node).map_err(vfsd::map_fs_error)?;
                let base = if relative.is_empty() { mount.mount_point.clone() }
                    else if mount.mount_point == "/" { format!("/{relative}") }
                    else { format!("{}/{relative}", mount.mount_point) };
                return normalize_absolute_path(&base, path);
            }
            handle.path.as_str()
        };
        normalize_absolute_path(base, path)
    }

    fn open_remote_checkpointed(
        &mut self,
        request: &VfsIpcRequest,
        path: &str,
        flags: u64,
    ) -> Result<(u64, RemoteHandle), i32> {
        let id = request.arg3;
        if id == 0 {
            return Err(EINVAL);
        }
        if let Some(handle) = self.handles.get(&id).cloned() {
            if handle.stale {
                return Err(ESTALE);
            }
            let chunk_count = handle
                .path
                .len()
                .div_ceil(SERVICE_CHECKPOINT_VALUE_CAPACITY);
            let (operation_hi, operation_lo) =
                checkpoint_suboperation(request, chunk_count as u64 + 1);
            let key = checkpoint_handle_key(id);
            let Some(current) = self.checkpoint_records.get(&key).copied() else {
                return Err(EIO);
            };
            let mut expected =
                checkpoint_handle_record(id, &handle, VFSD_OPEN_MUTATION_OPEN, flags, 0)?;
            expected.revision = current.revision;
            expected.operation_hi = operation_hi;
            expected.operation_lo = operation_lo;
            return if current == expected && handle.path == path {
                // An idempotent open replay may arrive after a lost receipt
                // or a restarted vfsd. Fund only once per exact description.
                self.bill_open_description(id, request.pid)?;
                Ok((id, handle))
            } else {
                Err(EINVAL)
            };
        }

        let accmode = flags & linux_abi::O_ACCMODE;
        if !matches!(
            accmode,
            linux_abi::O_RDONLY | linux_abi::O_WRONLY | linux_abi::O_RDWR
        ) {
            return Err(EINVAL);
        }
        let writable = accmode != linux_abi::O_RDONLY;
        let append = (flags & linux_abi::O_APPEND) != 0;
        let create = (flags & linux_abi::O_CREAT) != 0;
        let create_new = create && (flags & linux_abi::O_EXCL) != 0;
        let truncate = (flags & linux_abi::O_TRUNC) != 0;

        let mut mount_id = 0_u64;
        let mut reset_live_len = false;
        let tmpfs_owned = lock_vfs_storage().tmpfs_owns(path);
        // `O_TMPFILE` = `O_DIRECTORY | __O_TMPFILE`: `path` names a directory
        // and the result is an unnamed regular file inside it.
        const O_TMPFILE_MASK: u64 = 0o20_200_000;
        let is_tmpfile = flags & O_TMPFILE_MASK == O_TMPFILE_MASK;
        let create_attrs = FsCreateAttrs::new(request.arg1 as u32, request.euid, request.egid);
        if tmpfs_owned {
            let mut storage = lock_vfs_storage();
            if is_tmpfile { check_metadata_permission(storage.metadata(path)?, request, 3, false)?; }
            else {
                match storage.metadata(path) {
                    Ok(meta) => {
                        if create_new { return Err(EEXIST); }
                        let needed = (if accmode != linux_abi::O_WRONLY { 4 } else { 0 }) | (if writable || truncate { 2 } else { 0 });
                        check_metadata_permission(meta, request, needed, false)?;
                    }
                    Err(ENOENT) if create => check_parent(&mut storage, path, request, false)?,
                    Err(e) => return Err(e),
                }
            }
        }
        let mut pending_proc_snapshot: Option<Vec<u8>> = None;
        let metadata = match vfsd::proc_fs::lookup(path, request.pid) {
            Some(Ok(node)) => {
                const LINUX_O_NOFOLLOW: u64 = 0o400_000;
                const ELOOP: i32 = 40;
                match node {
                    vfsd::proc_fs::ProcNode::SelfLink => {
                        if flags & LINUX_O_NOFOLLOW != 0 {
                            return Err(ELOOP);
                        }
                        Metadata::plain(RemoteKind::Directory, 0, path_inode(path.as_bytes()))
                    }
                    vfsd::proc_fs::ProcNode::PidDir
                    | vfsd::proc_fs::ProcNode::NsDir
                    | vfsd::proc_fs::ProcNode::SysDir
                    | vfsd::proc_fs::ProcNode::SysFsDir
                    | vfsd::proc_fs::ProcNode::SysKernelDir
                    | vfsd::proc_fs::ProcNode::SysFsInotifyDir => {
                        if writable || create || truncate {
                            return Err(EACCES);
                        }
                        Metadata::plain(RemoteKind::Directory, 0, path_inode(path.as_bytes()))
                    }
                    vfsd::proc_fs::ProcNode::ExeLink => {
                        return Err(if flags & LINUX_O_NOFOLLOW != 0 {
                            ELOOP
                        } else {
                            ENOENT
                        });
                    }
                    // Namespace links name a namespace object RustOS does not
                    // expose as a file; `readlink` is the supported access.
                    vfsd::proc_fs::ProcNode::NsLink(_) => {
                        return Err(if flags & LINUX_O_NOFOLLOW != 0 {
                            ELOOP
                        } else {
                            EOPNOTSUPP
                        });
                    }
                    // Kernel-fact nodes are delegated by `vfs_openat` before
                    // an open description exists; reaching here means a
                    // caller bypassed that, which must not mint a handle.
                    vfsd::proc_fs::ProcNode::Kernel(kernel, number) => {
                        if !vfsd::proc_fs::is_ring3_formatted(kernel) || writable {
                            return Err(EOPNOTSUPP);
                        }
                        if flags & O_DIRECTORY != 0 {
                            return Err(ENOTDIR);
                        }
                        if create || truncate {
                            return Err(EACCES);
                        }
                        let bytes = format_kernel_proc_snapshot(
                            request.pid,
                            request.tid,
                            kernel,
                            number,
                        )?;
                        let len = bytes.len() as u64;
                        pending_proc_snapshot = Some(bytes);
                        Metadata::plain(
                            RemoteKind::File,
                            len,
                            rustos_user_abi::procfs::node_inode(request.pid, kernel, number),
                        )
                    }
                    file => {
                        if flags & O_DIRECTORY != 0 {
                            return Err(ENOTDIR);
                        }
                        if create || truncate {
                            return Err(EROFS);
                        }
                        if writable {
                            return Err(EACCES);
                        }
                        let bytes = lock_vfs_storage().proc_virtual_file_bytes(file)?;
                        let len = bytes.len() as u64;
                        pending_proc_snapshot = Some(bytes);
                        Metadata::plain(RemoteKind::File, len, path_inode(path.as_bytes()))
                    }
                }
            }
            Some(Err(errno)) => return Err(errno),
            None => {
                if let Some(sys_file) = vfsd::proc_fs::sys_virtual_file(path) {
                    if flags & O_DIRECTORY != 0 {
                        return Err(ENOTDIR);
                    }
                    if writable {
                        return Err(EACCES);
                    }
                    if create || truncate {
                        return Err(EROFS);
                    }
                    let bytes = lock_vfs_storage().sys_virtual_file_bytes(sys_file)?;
                    let len = bytes.len() as u64;
                    pending_proc_snapshot = Some(bytes);
                    Metadata::plain(RemoteKind::File, len, path_inode(path.as_bytes()))
                } else if path == "/sys" || path.starts_with("/sys/") {
                    if writable {
                        return Err(EACCES);
                    }
                    if create || truncate {
                        return Err(EROFS);
                    }
                    lock_vfs_storage().metadata(path)?
                } else if !tmpfs_owned
                    && (path == "/" || path == "/proc" || path == "/dev" || path.starts_with("/dev/"))
                {
                    let meta = lock_vfs_storage().metadata(path).inspect_err(|&errno| {
                        if !open_metadata_errno_is_routine(errno) {
                            debug_line(&format!(
                                "vfsd: open failed stage=metadata errno={errno} path={path}"
                            ));
                        }
                    })?;
                    if (create || truncate) && meta.kind != RemoteKind::Device {
                        return Err(EROFS);
                    }
                    meta
                } else if let Some(image_len) = if tmpfs_owned {
                    None
                } else {
                    early_system::file_len(path)?
                } {
                    if writable || create || truncate {
                        return Err(EROFS);
                    }
                    Metadata::plain(RemoteKind::File, image_len, path_inode(path.as_bytes()))
                } else {
                    let mut storage = lock_vfs_storage();
                    let (mid, meta, should_reset) = if is_tmpfile {
                        storage.open_tmpfile_on_mount(path, writable, create_attrs)
                    } else {
                        storage.open_on_mount(
                            path,
                            FsOpenOptions {
                                create,
                                create_new,
                                truncate,
                                write: writable,
                                append,
                            },
                            flags & O_DIRECTORY != 0,
                            create_attrs,
                        )
                    }.inspect_err(|&errno| {
                        if !open_metadata_errno_is_routine(errno) {
                            debug_line(&format!(
                                "vfsd: open failed stage=metadata errno={errno} path={path}"
                            ));
                        }
                    })?;
                    mount_id = mid;
                    reset_live_len = should_reset;
                    meta
                }
            }
        };
        // A memory-filesystem open is already pinned by `open_on_mount`; every
        // failure from here on must drop that pin again.
        let node = metadata.fs.map_or(0, |fs| fs.node_id);
        let result = self.publish_open_handle(
            request,
            id,
            path,
            flags,
            metadata,
            (mount_id, node),
            reset_live_len,
            is_tmpfile,
        );
        if result.is_err() && node != 0 {
            lock_vfs_storage().unpin_node(mount_id, node);
        }
        if let (Ok(_), Some(bytes)) = (&result, pending_proc_snapshot) {
            insert_proc_snapshot(id, bytes);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_open_handle(
        &mut self,
        request: &VfsIpcRequest,
        id: u64,
        path: &str,
        flags: u64,
        metadata: Metadata,
        (mount_id, node): (u64, u64),
        reset_live_len: bool,
        is_tmpfile: bool,
    ) -> Result<(u64, RemoteHandle), i32> {
        let accmode = flags & linux_abi::O_ACCMODE;
        let writable = accmode != linux_abi::O_RDONLY;
        let append = (flags & linux_abi::O_APPEND) != 0;
        if !is_tmpfile && flags & O_DIRECTORY != 0 && metadata.kind != RemoteKind::Directory {
            return Err(ENOTDIR);
        }
        if metadata.kind == RemoteKind::Directory && writable {
            return Err(EISDIR);
        }
        let handle = RemoteHandle {
            checkpoint_version: VFSD_OPEN_CHECKPOINT_VERSION,
            stale: false,
            kind: metadata.kind,
            mount_id,
            writable: writable && metadata.kind != RemoteKind::Directory,
            append,
            path: path.to_string(),
            node,
            cursor: 0,
            len: metadata.len,
            refs: 1,
            status_flags: flags & !linux_abi::O_CLOEXEC,
            last_mutation: VFSD_OPEN_MUTATION_OPEN,
            last_start: flags,
            last_result: 0,
        };
        // Never publish a new open description without charging its exact
        // client Job. The storage metadata lock has been dropped above and
        // the rootd control IPC runs in the service receiver, not ring0.
        self.bill_open_description(id, request.pid)?;
        if let Err(errno)=self.checkpoint_open_description(request, id, &handle) {
            // A checkpoint partial write can be replayed, but its unexposed
            // session charge must not leak when an OPEN did not complete.
            // The exact ticket survives if the refund ACK is ambiguous.
            let refund=self.cancel_unpublished_open_description(id);
            if refund.is_err() {
                return Err(EBUSY);
            }
            return Err(errno);
        }
        // This source point remains before the live handle table insert.
        // No later fallible transition may forget this exact client ticket.
        if self.handles.insert(id, handle.clone()).is_some() {
            debug_line(&format!(
                "vfsd: open failed stage=install errno={EIO} path={path}"
            ));
            return Err(EIO);
        }
        if mount_id != 0 && node == 0 {
            lock_vfs_storage().note_open_node(path, metadata.len, reset_live_len);
        }
        Ok((id, handle))
    }

    fn read_remote_into(
        &self,
        request: &VfsIpcRequest,
        offset: Option<u64>,
        len: usize,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        let id = request.remote_id;
        let (path, start, file_len, node) = {
            let handle = self.handles.get(&id).ok_or(EBADF)?;
            if handle.kind == RemoteKind::Device && is_input_device_node(handle.path.as_str()) {
                return Err(EAGAIN);
            }
            if handle.kind == RemoteKind::Device && handle.path == "/dev/null" {
                return Ok(0);
            }
            if handle.kind == RemoteKind::Device
                && matches!(handle.path.as_str(), "/dev/zero" | "/dev/full")
            {
                let count = len.min(dest.len());
                dest[..count].fill(0);
                return Ok(count);
            }
            if handle.kind != RemoteKind::File {
                return Err(EISDIR);
            }
            let start = vfsd::cursor_sync::admit_regular_file_read_offset(offset)?;
            let mut storage = lock_vfs_storage();
            let file_len = if handle.node != 0 {
                storage
                    .node_len(handle.mount_id, handle.node)
                    .ok_or(ESTALE)?
            } else {
                storage.live_len_for_path(handle.path.as_str(), handle.len)
            };
            (
                handle.path.as_str(),
                start,
                file_len,
                (handle.mount_id, handle.node),
            )
        };
        let available = file_len.saturating_sub(start);
        let len = len.min(usize::try_from(available).unwrap_or(usize::MAX)).min(dest.len());
        let read = if len == 0 {
            0
        } else if let Some(read) = read_proc_snapshot_range(id, start, &mut dest[..len]) {
            read
        } else {
            Self::read_slice_for_handle(
                node,
                path,
                request.pid,
                file_len,
                start,
                &mut dest[..len],
            )?
        };
        Ok(read)
    }

    /// Reads through the pinned node of a memory-filesystem open, else by path.
    fn read_slice_for_handle(
        (mount_id, node): (u64, u64),
        path: &str,
        pid: u64,
        file_len: u64,
        start: u64,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        if node != 0 {
            return lock_vfs_storage().read_node(mount_id, node, start, dest);
        }
        Self::read_slice_by_path(path, pid, file_len, start, dest)
    }

    fn read_slice_by_path(
        path: &str,
        pid: u64,
        file_len: u64,
        start: u64,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        let mut storage = lock_vfs_storage();
        if let Some(Ok(node)) = vfsd::proc_fs::lookup(path, pid) {
            if vfsd::proc_fs::is_virtual_file(node) {
                return storage.read_proc_file_into(node, start, dest);
            }
        }
        if let Some(file) = vfsd::proc_fs::sys_virtual_file(path) {
            return storage.read_sys_file_into(file, start, dest);
        }
        storage.read_file_slice_into(path, file_len, start, dest)
    }
}

impl VfsStorage {
    fn proc_virtual_file_bytes(&self, node: vfsd::proc_fs::ProcNode) -> Result<Vec<u8>, i32> {
        use vfsd::proc_fs::{self, MemInfoSnapshot, ProcNode};
        match node {
            ProcNode::MeminfoFile => {
                let mut info = linux_abi::LinuxSysinfo::default();
                let status = unsafe {
                    rustos_svc_runtime::syscall::syscall1(
                        linux_abi::SYS_SYSINFO,
                        (&mut info as *mut linux_abi::LinuxSysinfo) as u64,
                    )
                };
                if status != 0 {
                    return Err(EIO);
                }
                let unit = u64::from(info.mem_unit.max(1));
                Ok(proc_fs::format_meminfo(&MemInfoSnapshot {
                    mem_total_bytes: info.totalram.saturating_mul(unit),
                    mem_free_bytes: info.freeram.saturating_mul(unit),
                    buffers_bytes: info.bufferram.saturating_mul(unit),
                    swap_total_bytes: info.totalswap.saturating_mul(unit),
                    swap_free_bytes: info.freeswap.saturating_mul(unit),
                }))
            }
            ProcNode::CpuinfoFile => {
                let mask = online_cpu_mask().ok_or(EIO)?;
                Ok(proc_fs::format_cpuinfo(mask))
            }
            ProcNode::InotifySysctl(kind) => Ok(proc_fs::format_inotify_sysctl(kind)),
            ProcNode::FilesystemsFile => Ok(proc_fs::format_filesystems().to_vec()),
            ProcNode::MountsFile => Ok(proc_fs::format_mounts(&self.mounts.mount_infos())),
            ProcNode::MountInfoFile => Ok(proc_fs::format_mountinfo(&self.mounts.mount_infos())),
            ProcNode::CgroupFile => Ok(proc_fs::format_cgroup()),
            ProcNode::SysKernelFile(kind) => Ok(proc_fs::format_sys_kernel(kind)),
            ProcNode::UptimeFile => {
                Ok(proc_fs::format_uptime(rustos_svc_runtime::syscall::monotonic_nanos()))
            }
            ProcNode::SelfLink
            | ProcNode::PidDir
            | ProcNode::ExeLink
            | ProcNode::SysDir
            | ProcNode::SysFsDir
            | ProcNode::SysFsInotifyDir
            | ProcNode::SysKernelDir
            | ProcNode::NsDir
            | ProcNode::NsLink(_)
            | ProcNode::Kernel(..) => Err(EINVAL),
        }
    }

    fn sys_virtual_file_bytes(&self, _file: vfsd::proc_fs::SysFile) -> Result<Vec<u8>, i32> {
        // online, possible and present are all the admitted hardware CPUs: the
        // fixed topology has no hotplug-capable-but-offline slots.
        let mask = online_cpu_mask().ok_or(EIO)?;
        Ok(vfsd::proc_fs::format_cpulist(mask))
    }

    fn read_proc_file_into(
        &self,
        node: vfsd::proc_fs::ProcNode,
        start: u64,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        let bytes = self.proc_virtual_file_bytes(node)?;
        Ok(copy_file_cache_range(&bytes, start, dest))
    }

    fn read_sys_file_into(
        &self,
        file: vfsd::proc_fs::SysFile,
        start: u64,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        let bytes = self.sys_virtual_file_bytes(file)?;
        Ok(copy_file_cache_range(&bytes, start, dest))
    }

    fn read_file_slice_into(
        &mut self,
        path: &str,
        file_len: u64,
        start: u64,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        match early_system::read(path, start, dest)? {
            Some(read) => Ok(read),
            None => {
                self.invalidate_caches_if_remounted();
                let (_mount_id, rel_path, key, _parent_key, caps, _is_immutable, volume, read_map_source) =
                    self.resolve_mount(path)?;
                if caps.posix_nodes() {
                    // Mutable memory filesystem: no path-keyed content cache.
                    return volume
                        .read_at(&rel_path, start, dest)
                        .map_err(vfsd::map_fs_error);
                }
                {
                    let mut shared = SHARED_PATH_FILES.lock();
                    shared.sync_generation(self.mount_generation);
                    if let Some(res) = shared.read_at(&key, start, dest) {
                        return res.map_err(vfsd::map_fs_error);
                    }
                }
                let cached = self
                    .file_bytes_cache
                    .get(&key)
                    .map(|bytes| copy_file_cache_range(bytes, start, dest));
                if let Some(read) = cached {
                    self.file_bytes_cache_lru.touch(&key);
                    return Ok(read);
                }

                let cacheable_len = should_materialize_file_cache(file_len, start, dest.len());
                if let Some(expected_len) = cacheable_len {
                    let bytes = read_volume_file_to_vec(&*volume, &rel_path, expected_len)?;
                    let read = copy_file_cache_range(&bytes, start, dest);
                    while self
                        .file_bytes_cache_bytes
                        .checked_add(bytes.len())
                        .is_none_or(|total| total > FILE_BYTES_CACHE_BUDGET_BYTES)
                    {
                        let Some(evict_key) = self.file_bytes_cache_lru.pop_lru() else {
                            self.file_bytes_cache.clear();
                            self.file_bytes_cache_bytes = 0;
                            break;
                        };
                        if let Some(old) = self.file_bytes_cache.remove(&evict_key) {
                            self.file_bytes_cache_bytes =
                                self.file_bytes_cache_bytes.saturating_sub(old.len());
                        }
                    }
                    if bytes.len() <= FILE_BYTES_CACHE_BUDGET_BYTES {
                        self.file_bytes_cache_bytes += bytes.len();
                        self.file_bytes_cache_lru.touch(&key);
                        self.file_bytes_cache.insert(key, bytes);
                    }
                    return Ok(read);
                }

                if let Some(rms) = read_map_source {
                    return self.read_mapped_file_on(&rel_path, &key, &*rms, file_len, start, dest);
                }
                volume
                    .read_at(&rel_path, start, dest)
                    .map_err(vfsd::map_fs_error)
            }
        }
    }

    /// Plans and caches the on-volume read map of `path` (bounded; evicts the
    /// whole set when full). The map is the storage layer's identity of the
    /// file: its volume/object/generation never alias a different file.
    fn ensure_read_map(
        &mut self,
        rel_path: &str,
        key: &CacheKey,
        read_map_source: &dyn vfsd::mount_table::ReadMapSupport,
        len: u64,
    ) -> Result<(), i32> {
        // A revoked map (its file, FAT chain or directory entry was written)
        // is dropped here and replanned, instead of failing every later read.
        if self
            .read_maps
            .get(key)
            .is_some_and(|map| !read_map_source.read_map_is_current(map))
        {
            self.read_maps_lru.remove(key);
            if let Some(old) = self.read_maps.remove(key) {
                self.read_map_bytes = self.read_map_bytes.saturating_sub(old.metadata_bytes());
            }
        }
        if !self.read_maps.contains_key(key) {
            let map = read_map_source
                .plan_read(rel_path, vfsd::snapshot_budget::SNAPSHOT_MAP_BYTES)
                .map_err(map_range_error)?;
            if map.len() != len {
                return Err(EIO);
            }
            let cost = map.metadata_bytes();
            while self.read_maps.len() >= 128
                || self
                    .read_map_bytes
                    .checked_add(cost)
                    .is_none_or(|n| n > 8 * 1024 * 1024)
            {
                let Some(evict_key) = self.read_maps_lru.pop_lru() else {
                    self.read_maps.clear();
                    self.read_map_bytes = 0;
                    break;
                };
                if let Some(old) = self.read_maps.remove(&evict_key) {
                    self.read_map_bytes = self.read_map_bytes.saturating_sub(old.metadata_bytes());
                }
            }
            self.read_map_bytes += cost;
            self.read_maps_lru.touch(key);
            self.read_maps.insert(key.clone(), map);
        } else {
            self.read_maps_lru.touch(key);
        }
        Ok(())
    }

    /// Attests the backing file of an open description for the shared library
    /// cache.
    fn file_identity(&mut self, path: &str, len: u64) -> Result<VfsFileIdentityWire, i32> {
        self.invalidate_caches_if_remounted();
        if let Some(image_len) = early_system::file_len(path)? {
            if image_len != len {
                return Err(EIO);
            }
            return Ok(vfsd::early_system_identity(path, len));
        }
        let (mount_id, rel_path, key, _parent_key, caps, is_immutable, volume, read_map_source) =
            self.resolve_mount(path)?;
        if caps.posix_nodes() {
            // A memory filesystem has no storage epoch; its identity is the
            // per-mount node id and is always mutable, hence never cached.
            let node_id = volume.metadata(&rel_path).map_err(vfsd::map_fs_error)?.node_id;
            return Ok(VfsFileIdentityWire {
                volume: mount_id,
                object: node_id,
                generation: self.mount_generation,
                storage_epoch: 0,
                mount_generation: self.mount_generation,
                len,
                flags: VFS_FILE_IDENTITY_FLAG_MUTABLE_VOLUME,
            });
        }
        if self.dvm_storage_epoch == 0 {
            return Err(EIO);
        }
        if !is_immutable {
            let node_id = volume
                .metadata(&rel_path)
                .map(|m| m.node_id)
                .unwrap_or_else(|_| path_inode(path.as_bytes()));
            return Ok(VfsFileIdentityWire {
                volume: mount_id,
                object: node_id,
                generation: self.mount_generation,
                storage_epoch: self.dvm_storage_epoch,
                mount_generation: self.mount_generation,
                len,
                flags: VFS_FILE_IDENTITY_FLAG_MUTABLE_VOLUME,
            });
        }
        let rms = read_map_source.ok_or(EOPNOTSUPP)?;
        self.ensure_read_map(&rel_path, &key, &*rms, len)?;
        let identity = self.read_maps.get(&key).ok_or(EIO)?.identity();
        Ok(VfsFileIdentityWire {
            volume: identity.volume,
            object: identity.object,
            generation: identity.generation,
            storage_epoch: self.dvm_storage_epoch,
            mount_generation: self.mount_generation,
            len,
            flags: VFS_FILE_IDENTITY_FLAG_IMMUTABLE_VOLUME,
        })
    }

    fn read_mapped_file_on(
        &mut self,
        rel_path: &str,
        key: &CacheKey,
        read_map_source: &dyn vfsd::mount_table::ReadMapSupport,
        len: u64,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, i32> {
        self.ensure_read_map(rel_path, key, read_map_source, len)?;
        let map = self.read_maps.get(key).ok_or(EIO)?;
        if map.len() != len {
            return Err(EIO);
        }
        read_map_source
            .read_mapped_at(map, offset, output)
            .map_err(map_range_error)
    }

    /// Resolves a snapshot request into either a warm cache hit or an
    /// immutable read plan, holding storage only for that resolution.
    ///
    /// The bulk read deliberately happens outside this call. Holding the single
    /// storage owner across a multi-megabyte volume read would block every
    /// namespace request behind it, which is the head-of-line stall this
    /// service is required not to have.
    fn plan_executable_snapshot(
        &mut self,
        request: &VfsExecutableSnapshotRequest,
    ) -> Result<ExecutableSnapshotAdmission, i32> {
        if request.version != VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION
            || request.op != VFS_EXECUTABLE_SNAPSHOT_OP_OPEN
            || request.flags != 0
            || request.reserved0 != 0
            || request.requester_pid == 0
            || request.requester_tid == 0
            || request.max_bytes == 0
            || request.max_bytes > VFS_EXECUTABLE_SNAPSHOT_MAX_FILE_BYTES
            || request.path_len == 0
            || request.path_len as usize > request.path.len()
        {
            return Err(EINVAL);
        }
        let raw_path =
            core::str::from_utf8(&request.path[..request.path_len as usize]).map_err(|_| EINVAL)?;
        if raw_path.as_bytes().contains(&0) {
            return Err(EINVAL);
        }
        if !raw_path.starts_with('/') {
            return Err(EINVAL);
        }
        let path = normalize_absolute_path("/", raw_path)?;
        let early_system_owned = early_system::file_len(path.as_str())?.is_some();

        self.invalidate_caches_if_remounted();
        let (mount_id, rel_path, cache_key, read_map_source) = if early_system_owned {
            (0_u64, String::new(), (0_u64, path.clone()), None)
        } else {
            let (mid, rel, key, _pk, caps, _imm, _vol, rms) = self.resolve_mount(path.as_str())?;
            if caps.posix_nodes() {
                // A mutable memory file must never be sealed into the
                // mount-generation snapshot cache.
                return Err(EOPNOTSUPP);
            }
            (mid, rel, key, rms)
        };
        if let Some(snapshot) = self.executable_snapshot_cache.get_mut(&cache_key) {
            self.executable_snapshot_cache_seq =
                self.executable_snapshot_cache_seq.wrapping_add(1);
            snapshot.last_used_seq = self.executable_snapshot_cache_seq;
            let cached_fd = snapshot.fd;
            let cached_bytes = snapshot.file_bytes;
            return ExecutableSnapshotOpen::duplicate_cached(
                cached_fd,
                cached_bytes,
                self.mount_generation,
                request.max_bytes,
                duplicate_snapshot_fd,
                close_fd as fn(i32),
            ).map(ExecutableSnapshotAdmission::Cached);
        }

        let metadata = self.metadata(path.as_str())?;
        if metadata.kind != RemoteKind::File {
            return Err(EISDIR);
        }
        if metadata.len == 0 || metadata.len > request.max_bytes {
            return Err(if metadata.len == 0 { ENOEXEC } else { EOVERFLOW });
        }
        let file_len = usize::try_from(metadata.len).map_err(|_| EOVERFLOW)?;
        let product_interactive_snapshot = path == "/apps/wayclick/wayclick.elf";
        let backing = if early_system_owned {
            ExecutableSnapshotBacking::EarlySystem
        } else {
            ExecutableSnapshotBacking::DvmVolume
        };
        let storage_epoch = match backing {
            ExecutableSnapshotBacking::EarlySystem => 0,
            ExecutableSnapshotBacking::DvmVolume => {
                if self.dvm_storage_epoch == 0 {
                    return Err(EIO);
                }
                self.dvm_storage_epoch
            }
        };
        let request_id = next_executable_snapshot_request_id()?;
        let read_map = if backing == ExecutableSnapshotBacking::DvmVolume {
            let rms = read_map_source.ok_or(EOPNOTSUPP)?;
            let map = rms
                .plan_read(rel_path.as_str(), vfsd::snapshot_budget::SNAPSHOT_MAP_BYTES)
                .map_err(map_range_error)?;
            if map.len() != metadata.len { return Err(EIO); }
            Some(alloc::sync::Arc::new(map))
        } else { None };
        Ok(ExecutableSnapshotAdmission::Read(ExecutableSnapshotPlan {
            path,
            mount_id,
            cache_key,
            file_len,
            read_map,
            metadata_len: metadata.len,
            verbose: !early_system_owned || product_interactive_snapshot,
            backing,
            storage_epoch,
            request_id,
            mount_generation: self.mount_generation,
        }))
    }

    /// Only the epoch check requires namespace ownership; detached device I/O
    /// occurs after the caller releases it.
    fn validate_executable_snapshot_plan(
        &mut self,
        plan: &ExecutableSnapshotPlan,
    ) -> Result<(), i32> {
        self.invalidate_caches_if_remounted();
        if plan.mount_generation != self.mount_generation
            || (plan.backing == ExecutableSnapshotBacking::DvmVolume && plan.storage_epoch != self.dvm_storage_epoch)
        { return Err(EAGAIN); }
        Ok(())
    }

    /// Seals the completed image and admits it to the mount-generation cache.
    ///
    /// The plan was resolved against a mount generation that the bulk read ran
    /// outside of. A remount in between makes the bytes stale, and sealing them
    /// would place an image from the previous volume into the current
    /// generation's cache where no invalidation would ever reach it. The caller
    /// re-plans instead; that is a transient condition, not a failed open.
    fn commit_executable_snapshot(
        &mut self,
        plan: &ExecutableSnapshotPlan,
        snapshot: &ExecutableSnapshotOpen,
        digest_bytes: Option<[u8; 32]>,
    ) -> Result<Option<ExecutableSnapshotOpen>, i32> {
        self.invalidate_caches_if_remounted();
        if !vfsd::snapshot_plan_is_current(plan.mount_generation, self.mount_generation)
            || (plan.backing == ExecutableSnapshotBacking::DvmVolume
                && plan.storage_epoch != self.dvm_storage_epoch)
        {
            return Err(EAGAIN);
        }
        if let Some(map) = plan.read_map.as_ref() {
            let is_current = self
                .mounts
                .by_id(plan.mount_id)
                .and_then(|m| m.read_map_source.as_ref())
                .is_some_and(|rms| rms.read_map_is_current(map));
            if !is_current {
                return Err(EAGAIN);
            }
        }
        let path = plan.path.clone();
        let file_len = plan.file_len;
        // Another worker may have published the same path while this plan
        // read without storage ownership. Reuse that entry instead of leaking
        // its descriptor and counting the same image twice.
        if let Some(snapshot) = self
            .executable_snapshot_cache
            .get(&plan.cache_key)
            .copied()
        {
            return ExecutableSnapshotOpen::duplicate_cached(
                snapshot.fd,
                snapshot.file_bytes,
                plan.mount_generation,
                plan.metadata_len,
                duplicate_snapshot_fd,
                close_fd as fn(i32),
            ).map(Some);
        }
        if snapshot.file_bytes() != plan.metadata_len || snapshot.mount_generation() != plan.mount_generation {
            return Err(EIO);
        }
        let fd = snapshot.fd();
        if plan.verbose {
            ipc::debug_line(
                executable_snapshot_marker(
                    path.as_str(),
                    file_len,
                    plan.backing,
                    plan.mount_generation,
                )
                .as_str(),
            );
            match plan.backing {
                ExecutableSnapshotBacking::EarlySystem => {
                    let _ = ipc::product_milestone(
                        PRODUCT_MILESTONE_BOOTSTRAP_EXECUTABLE_SNAPSHOT_SEALED,
                        file_len as u64,
                        plan.mount_generation,
                    );
                }
                ExecutableSnapshotBacking::DvmVolume => {
                    // Product evidence must carry a real digest; a plan that
                    // skipped hashing cannot produce it.
                    let Some(digest) = digest_bytes else {
                        return Err(EIO);
                    };
                    let evidence = ProductExecutableSnapshotEvidence {
                        storage_epoch: plan.storage_epoch,
                        mount_generation: plan.mount_generation,
                        request_id: plan.request_id,
                        file_bytes: file_len as u64,
                        digest,
                        ..ProductExecutableSnapshotEvidence::default()
                    };
                    if ipc::product_executable_snapshot_evidence(&evidence) != 0 {
                        return Err(EIO);
                    }
                }
            }
        }

        if file_len > EXECUTABLE_SNAPSHOT_CACHE_BUDGET_BYTES {
            return Ok(None);
        }
        // Cache admission is optional. The request still owns the original
        // sealed fd if descriptor pressure refuses the separate cache handle.
        let cache_fd = match duplicate_snapshot_fd(fd) {
            Ok(fd) => fd,
            Err(_) => return Ok(None),
        };
        while self
            .executable_snapshot_cache_bytes
            .checked_add(file_len)
            .is_none_or(|total| total > EXECUTABLE_SNAPSHOT_CACHE_BUDGET_BYTES)
        {
            let Some(oldest_key) = self
                .executable_snapshot_cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_used_seq)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(evicted) = self.executable_snapshot_cache.remove(&oldest_key) {
                close_fd(evicted.fd);
                self.executable_snapshot_cache_bytes = self
                    .executable_snapshot_cache_bytes
                    .saturating_sub(evicted.file_bytes as usize);
            }
        }
        self.executable_snapshot_cache_seq = self.executable_snapshot_cache_seq.wrapping_add(1);
        self.executable_snapshot_cache.insert(
            plan.cache_key.clone(),
            ExecutableSnapshot {
                fd: cache_fd,
                file_bytes: plan.metadata_len,
                last_used_seq: self.executable_snapshot_cache_seq,
            },
        );
        self.executable_snapshot_cache_bytes += file_len;
        Ok(None)
    }
}

impl VfsState {
    fn render_getdents_payload(
        &mut self,
        id: u64,
        caller_pid: u64,
        cursor: u64,
        user_len: usize,
        payload: &mut [u8],
    ) -> Result<(usize, u64), i32> {
        if user_len < 24 {
            return Err(EINVAL);
        }
        let handle = self.handles.get(&id).ok_or(EBADF)?;
        if handle.kind != RemoteKind::Directory {
            return Err(ENOTDIR);
        }
        // Discover virtual inventory before entering the storage owner: the
        // /dev inventory can make a synchronous call to devmgrd.
        let virtual_entries = if handle.node == 0 {
            virtual_dir_entries(&handle.path, caller_pid)?
        } else {
            None
        };
        let limit = user_len.min(payload.len());
        let mut written = 0;
        let mut next = cursor;
        lock_vfs_storage().visit_directory(handle, virtual_entries, cursor, |cookie, entry| {
            let Some(len) = encode_dirent_into(entry, cookie, &mut payload[written..limit]) else {
                return if written == 0 { Err(EINVAL) } else { Ok(false) };
            };
            written += len;
            next = cookie;
            Ok(true)
        })?;
        Ok((written, next))
    }
}

impl VfsStorage {
    /// Mounts the boot-time memory filesystems (`/tmp`, `/dev/shm`, `/run`).
    /// They need no storage device, so they exist before the boot disk does.
    fn ensure_tmpfs_mounts(&mut self) {
        if self.tmpfs_ready {
            return;
        }
        match self.mounts.mount_tmpfs_defaults(tmpfs_clock, Some(shared_frames::create_memfd_frames)) {
            Ok(()) => self.tmpfs_ready = true,
            Err(err) => debug_line(&format!("vfsd: tmpfs mount failed err={err}")),
        }
    }

    /// Whether `abs_path` lies on a memory-filesystem mount (and therefore is
    /// never one of the virtual `/dev`, `/proc`, `/sys` trees or boot FAT).
    fn tmpfs_owns(&mut self, abs_path: &str) -> bool {
        self.ensure_tmpfs_mounts();
        matches!(
            self.mounts.resolve(abs_path),
            Some((entry, _))
                if entry.mount_point != "/" && entry.volume.capabilities().posix_nodes()
        )
    }

    fn posix_metadata(mount_id: u64, meta: storage_core::fs::FsMetadata) -> Metadata {
        Metadata {
            kind: RemoteKind::from_fs(meta.kind),
            len: meta.len,
            // Unique across mounts: node ids are per mount.
            inode: (mount_id << 40) | (meta.node_id & ((1 << 40) - 1)),
            fs: Some(meta),
            dev: mount_id,
            rdev: None,
        }
    }

    fn node_volume(
        &self,
        mount_id: u64,
    ) -> Result<alloc::sync::Arc<dyn storage_core::fs::FsVolume>, i32> {
        self.mounts
            .by_id(mount_id)
            .map(|entry| entry.volume.clone())
            .ok_or(ESTALE)
    }

    /// Synchronous node operations finish under this storage hold. Borrow the
    /// mount's existing volume instead of incrementing/decrementing its Arc on
    /// every metadata lookup and data chunk. node_volume serves owned leases.
    fn node_volume_ref(&self, mount_id: u64) -> Result<&dyn storage_core::fs::FsVolume, i32> {
        self.mounts.by_id(mount_id).map(|entry| entry.volume.as_ref()).ok_or(ESTALE)
    }

    fn node_metadata(&self, mount_id: u64, node: u64) -> Result<Metadata, i32> {
        let meta = self
            .node_volume_ref(mount_id)?
            .metadata_node(node)
            .map_err(vfsd::map_fs_error)?;
        Ok(Self::posix_metadata(mount_id, meta))
    }

    fn node_len(&self, mount_id: u64, node: u64) -> Option<u64> {
        self.node_metadata(mount_id, node).ok().map(|m| m.len)
    }

    /// Stable `(mount_id, node_id)` identity of the file behind a path-based
    /// (non-pinned) open description, for the advisory lock table. It is the
    /// same identity `file_identity` reports for a mutable volume: the volume's
    /// node id, or the path hash when the volume has none. The read-only
    /// early-system image has no mount and uses mount `0` with the path hash.
    fn lock_identity(&mut self, path: &str) -> Result<(u64, u64), i32> {
        if early_system::file_len(path)?.is_some() {
            return Ok((0, path_inode(path.as_bytes())));
        }
        let (mount_id, rel_path, _key, _parent, _caps, _immutable, volume, _map) =
            self.resolve_mount(path)?;
        let node = volume
            .metadata(&rel_path)
            .map(|m| m.node_id)
            .unwrap_or_else(|_| path_inode(path.as_bytes()));
        Ok((mount_id, node))
    }

    fn read_node(&self, mount_id: u64, node: u64, start: u64, dest: &mut [u8]) -> Result<usize, i32> {
        self.node_volume_ref(mount_id)?
            .read_node_at(node, start, dest)
            .map_err(vfsd::map_fs_error)
    }

    fn write_node(
        &self,
        mount_id: u64,
        node: u64,
        offset: Option<u64>,
        data: &[u8],
    ) -> Result<(u64, usize, u64), i32> {
        let volume = self.node_volume_ref(mount_id)?;
        // `None` appends: length and write are one storage hold, so two
        // O_APPEND writers can never pick the same offset.
        let start = match offset {
            Some(start) => start,
            None => volume.metadata_node(node).map_err(vfsd::map_fs_error)?.len,
        };
        let (written, new_len) = volume
            .write_node_at(node, start, data)
            .map_err(vfsd::map_fs_error)?;
        Ok((start, written, new_len))
    }

    fn unpin_node(&self, mount_id: u64, node: u64) {
        if let Ok(volume) = self.node_volume_ref(mount_id) {
            volume.release_node(node);
        }
    }

    fn ensure_mounted(&mut self) -> Result<(), i32> {
        if !self.mounts.has_root() {
            let device = BootBlockDevice::open().inspect_err(|&errno| {
                if !block::is_transient_storage_not_ready(errno) {
                    debug_line(&format!(
                        "vfsd: volume unavailable stage=block-info errno={errno}"
                    ));
                }
            })?;
            let storage_epoch = device.generation;
            let device_read_only = device.read_only;
            let shared_dev = SharedBlockDevice::new(device);
            let data = self.mounts
                .mount_fat_disk_into(
                    shared_dev,
                    device_read_only,
                    storage_epoch,
                    || rustos_svc_runtime::syscall::sleep_millis(1),
                )
                .inspect_err(|&errno| {
                    debug_line(&format!(
                        "vfsd: volume unavailable stage=fat-admission errno={errno}"
                    ));
                })?;
            if let vfsd::mount_table::DataMountStatus::Unavailable(errno) = data {
                debug_line(&format!("vfsd: /data volume unavailable stage=fat-admission errno={errno}"));
            }
            self.dvm_storage_epoch = storage_epoch;
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn resolve_mount(
        &mut self,
        abs_path: &str,
    ) -> Result<
        (
            u64,
            String,
            CacheKey,
            Option<CacheKey>,
            storage_core::fs::FsCapabilities,
            bool,
            alloc::sync::Arc<dyn storage_core::fs::FsVolume>,
            Option<alloc::sync::Arc<dyn vfsd::mount_table::ReadMapSupport>>,
        ),
        i32,
    > {
        self.ensure_tmpfs_mounts();
        // A memory-filesystem path never needs the boot disk.
        let on_tmpfs = matches!(
            self.mounts.resolve(abs_path),
            Some((entry, _))
                if entry.mount_point != "/" && entry.volume.capabilities().posix_nodes()
        );
        if !on_tmpfs {
            self.ensure_mounted()?;
        }
        if let Some(errno) = self.mounts.data_mount_error(abs_path) {
            return Err(errno);
        }
        let (entry, rel) = self.mounts.resolve(abs_path).ok_or(ENOENT)?;
        let caps = entry.volume.capabilities();
        let is_immutable = entry.volume.is_immutable();
        let key = cache_key_for_mount(entry.id, caps.case_policy, rel);
        let parent_key =
            parent_rel_path(rel).map(|p| cache_key_for_mount(entry.id, caps.case_policy, p));
        Ok((
            entry.id,
            rel.to_string(),
            key,
            parent_key,
            caps,
            is_immutable,
            entry.volume.clone(),
            entry.read_map_source.clone(),
        ))
    }

    fn invalidate_path_caches(
        &mut self,
        key: &CacheKey,
        parent_key: Option<&CacheKey>,
    ) {
        self.metadata_cache.remove(key);
        self.metadata_cache_lru.remove(key);
        self.dir_entries_cache.remove(key);
        self.dir_entries_cache_lru.remove(key);
        self.file_bytes_cache_lru.remove(key);
        if let Some(bytes) = self.file_bytes_cache.remove(key) {
            self.file_bytes_cache_bytes = self.file_bytes_cache_bytes.saturating_sub(bytes.len());
        }
        self.read_maps_lru.remove(key);
        if let Some(map) = self.read_maps.remove(key) {
            self.read_map_bytes = self.read_map_bytes.saturating_sub(map.metadata_bytes());
        }
        if let Some(snapshot) = self.executable_snapshot_cache.remove(key) {
            close_fd(snapshot.fd);
            self.executable_snapshot_cache_bytes = self
                .executable_snapshot_cache_bytes
                .saturating_sub(snapshot.file_bytes as usize);
        }
        // A node appearing or disappearing at `key` changes what a cached
        // answer *below* it means (`key/x` flips between `ENOENT` and
        // `ENOTDIR` when a file replaces a missing or removed directory), and
        // negative answers are cached.
        if !key.1.is_empty() {
            for k in vfsd::mount_table::descendant_keys(&self.metadata_cache, key) {
                self.metadata_cache.remove(&k);
                self.metadata_cache_lru.remove(&k);
            }
        }
        if let Some(pk) = parent_key {
            self.dir_entries_cache.remove(pk);
            self.dir_entries_cache_lru.remove(pk);
            self.metadata_cache.remove(pk);
            self.metadata_cache_lru.remove(pk);
        }
    }

    /// `key` itself plus everything below it.
    fn subtree_keys<V>(map: &BTreeMap<CacheKey, V>, key: &CacheKey) -> Vec<CacheKey> {
        let mut keys = vfsd::mount_table::descendant_keys(map, key);
        if map.contains_key(key) {
            keys.push(key.clone());
        }
        keys
    }

    fn invalidate_subtree_caches(&mut self, key: &CacheKey) {
        for k in Self::subtree_keys(&self.metadata_cache, key) {
            self.metadata_cache.remove(&k);
            self.metadata_cache_lru.remove(&k);
        }
        for k in Self::subtree_keys(&self.dir_entries_cache, key) {
            self.dir_entries_cache.remove(&k);
            self.dir_entries_cache_lru.remove(&k);
        }
        for k in Self::subtree_keys(&self.file_bytes_cache, key) {
            self.file_bytes_cache_lru.remove(&k);
            if let Some(bytes) = self.file_bytes_cache.remove(&k) {
                self.file_bytes_cache_bytes = self.file_bytes_cache_bytes.saturating_sub(bytes.len());
            }
        }
        for k in Self::subtree_keys(&self.read_maps, key) {
            self.read_maps_lru.remove(&k);
            if let Some(map) = self.read_maps.remove(&k) {
                self.read_map_bytes = self.read_map_bytes.saturating_sub(map.metadata_bytes());
            }
        }
        for k in Self::subtree_keys(&self.executable_snapshot_cache, key) {
            if let Some(snapshot) = self.executable_snapshot_cache.remove(&k) {
                close_fd(snapshot.fd);
                self.executable_snapshot_cache_bytes = self
                    .executable_snapshot_cache_bytes
                    .saturating_sub(snapshot.file_bytes as usize);
            }
        }
    }

    fn open_on_mount(
        &mut self,
        abs_path: &str,
        opts: FsOpenOptions,
        dir_only: bool,
        attrs: FsCreateAttrs,
    ) -> Result<(u64, Metadata, bool), i32> {
        let (mount_id, rel_path, key, parent_key, caps, _imm, volume, _rms) =
            self.resolve_mount(abs_path)?;
        if caps.posix_nodes() {
            return Self::open_posix_node(mount_id, &rel_path, &*volume, opts, dir_only, attrs);
        }
        if self.open_nodes.get(&key).is_some_and(|n| n.pending_unlink) {
            return Err(if opts.create { EBUSY } else { ENOENT });
        }
        if dir_only {
            if opts.write || opts.create || opts.truncate {
                return Err(EISDIR);
            }
            let meta = volume
                .metadata(&rel_path)
                .map_err(vfsd::map_fs_error)?;
            if meta.kind != FsNodeKind::Directory {
                return Err(ENOTDIR);
            }
            return Ok((
                mount_id,
                Metadata::plain(RemoteKind::Directory, 0, path_inode(abs_path.as_bytes())),
                false,
            ));
        }
        if (opts.create || opts.truncate || opts.write) && caps.read_only {
            if let Ok(meta) = volume.metadata(&rel_path) {
                if meta.kind == FsNodeKind::Directory && opts.write {
                    return Err(EISDIR);
                }
            }
            return Err(EROFS);
        }
        let fs_meta = volume
            .open_prepare(&rel_path, opts)
            .map_err(vfsd::map_fs_error)?;
        if opts.create || opts.truncate {
            self.invalidate_path_caches(&key, parent_key.as_ref());
        }
        let mut live_len = fs_meta.len;
        {
            let mut shared = SHARED_PATH_FILES.lock();
            shared.sync_generation(self.mount_generation);
            if opts.truncate {
                if let Some(res) = shared.truncate(&key, 0) {
                    live_len = res.map_err(vfsd::map_fs_error)?;
                }
            } else if let Some(len) = shared.len_of(&key) {
                live_len = len;
            }
        }
        let kind = match fs_meta.kind {
            FsNodeKind::File | FsNodeKind::Symlink | FsNodeKind::Socket | FsNodeKind::Fifo => {
                RemoteKind::File
            }
            FsNodeKind::Directory => RemoteKind::Directory,
        };
        Ok((
            mount_id,
            Metadata::plain(
                kind,
                live_len,
                vfsd::path::path_inode_for_policy(abs_path.as_bytes(), caps.case_policy),
            ),
            opts.truncate,
        ))
    }

    /// Opens a node of a memory filesystem and pins it. The returned
    /// `Metadata::fs` carries the pinned node id; the caller owns one pin.
    fn open_posix_node(
        mount_id: u64,
        rel_path: &str,
        volume: &dyn storage_core::fs::FsVolume,
        opts: FsOpenOptions,
        dir_only: bool,
        attrs: FsCreateAttrs,
    ) -> Result<(u64, Metadata, bool), i32> {
        if dir_only && (opts.write || opts.create || opts.truncate) {
            return Err(EISDIR);
        }
        let meta = volume
            .open_prepare_with(rel_path, opts, attrs)
            .map_err(vfsd::map_fs_error)?;
        match meta.kind {
            FsNodeKind::Directory => {}
            _ if dir_only => return Err(ENOTDIR),
            // Resolution already followed links; a symlink here means
            // `O_NOFOLLOW`, and a socket/fifo inode cannot be opened for I/O.
            FsNodeKind::Symlink => return Err(ELOOP),
            FsNodeKind::Socket | FsNodeKind::Fifo => return Err(ENXIO),
            FsNodeKind::File => {}
        }
        // Pin by path in the same storage hold that resolved it, so the node
        // named by `meta.node_id` cannot change in between.
        let node = volume.retain_node(rel_path).map_err(vfsd::map_fs_error)?;
        debug_assert_eq!(node, meta.node_id);
        Ok((mount_id, Self::posix_metadata(mount_id, meta), opts.truncate))
    }

    /// `O_TMPFILE`: an unnamed, already pinned regular file inside `abs_dir`.
    fn open_tmpfile_on_mount(
        &mut self,
        abs_dir: &str,
        writable: bool,
        attrs: FsCreateAttrs,
    ) -> Result<(u64, Metadata, bool), i32> {
        if !writable {
            return Err(EINVAL);
        }
        let (mount_id, rel_path, _key, _pk, caps, _imm, volume, _rms) =
            self.resolve_mount(abs_dir)?;
        if !caps.posix_nodes() {
            return Err(EOPNOTSUPP);
        }
        let node = volume
            .open_unnamed(&rel_path, attrs)
            .map_err(vfsd::map_fs_error)?;
        match volume.metadata_node(node) {
            Ok(meta) => Ok((mount_id, Self::posix_metadata(mount_id, meta), false)),
            Err(err) => {
                volume.release_node(node);
                Err(vfsd::map_fs_error(err))
            }
        }
    }

    fn note_open_node(&mut self, abs_path: &str, len: u64, reset_len: bool) {
        if let Ok((_mid, _rel, key, _pk, _caps, _imm, _vol, _rms)) = self.resolve_mount(abs_path) {
            let entry = self.open_nodes.entry(key).or_insert(OpenNodeState {
                refs: 0,
                live_len: len,
                pending_unlink: false,
            });
            entry.refs = entry.refs.saturating_add(1);
            if reset_len || entry.refs == 1 {
                entry.live_len = len;
            }
        }
    }

    fn note_close_node(&mut self, abs_path: &str) {
        if let Ok((_mid, rel_path, key, parent_key, _caps, _imm, volume, _rms)) =
            self.resolve_mount(abs_path)
        {
            let mut should_unlink = false;
            let mut last_ref_closed = false;
            if let Some(entry) = self.open_nodes.get_mut(&key) {
                entry.refs = entry.refs.saturating_sub(1);
                if entry.refs == 0 {
                    last_ref_closed = true;
                    should_unlink = entry.pending_unlink;
                    self.open_nodes.remove(&key);
                }
            }
            if last_ref_closed {
                let mut shared = SHARED_PATH_FILES.lock();
                shared.sync_generation(self.mount_generation);
                if should_unlink {
                    shared.remove(&key);
                } else {
                    let _ = shared.flush_entry(&key, &*volume);
                }
            }
            if should_unlink {
                let _ = volume.unlink(&rel_path);
                self.invalidate_path_caches(&key, parent_key.as_ref());
            }
        }
    }

    fn live_len_for_path(&mut self, abs_path: &str, fallback: u64) -> u64 {
        if let Ok((_mid, _rel, key, _pk, _caps, _imm, _vol, _rms)) = self.resolve_mount(abs_path) {
            {
                let mut shared = SHARED_PATH_FILES.lock();
                shared.sync_generation(self.mount_generation);
                if let Some(len) = shared.len_of(&key) {
                    return len;
                }
            }
            if let Some(node) = self.open_nodes.get(&key) {
                return node.live_len;
            }
        }
        fallback
    }

    fn live_stat_for_path(&mut self, abs_path: &str, fallback_len: u64) -> (u64, u64) {
        if let Ok((_mid, _rel, key, _pk, caps, _imm, _vol, _rms)) = self.resolve_mount(abs_path) {
            let len = {
                let mut shared = SHARED_PATH_FILES.lock();
                shared.sync_generation(self.mount_generation);
                shared.len_of(&key)
            }
            .unwrap_or_else(|| {
                self.open_nodes
                    .get(&key)
                    .map_or(fallback_len, |node| node.live_len)
            });
            let inode = vfsd::path::path_inode_for_policy(abs_path.as_bytes(), caps.case_policy);
            return (len, inode);
        }
        (fallback_len, path_inode(abs_path.as_bytes()))
    }

    /// Writes through the mount's volume (or the live shared mapping of the
    /// path). Admission is per mount: a read-only mount is `EROFS`, a writable
    /// FAT volume mutates in place. FAT has no journal, so durability is only
    /// what `flush_path`/`fsync` (volume flush plus device barrier) establishes.
    fn write_at_path(&mut self, abs_path: &str, offset: u64, data: &[u8]) -> Result<(usize, u64), i32> {
        let (_mid, rel_path, key, _parent_key, caps, _imm, volume, _rms) =
            self.resolve_mount(abs_path)?;
        if caps.read_only {
            return Err(EROFS);
        }
        let shared_res = {
            let mut shared = SHARED_PATH_FILES.lock();
            shared.sync_generation(self.mount_generation);
            shared.write_at(&key, offset, data)
        };
        let (written, new_len) = match shared_res {
            Some(res) => res.map_err(vfsd::map_fs_error)?,
            None => volume
                .write_at(&rel_path, offset, data)
                .map_err(vfsd::map_fs_error)?,
        };
        if let Some(node) = self.open_nodes.get_mut(&key) {
            node.live_len = new_len;
        }
        // A write changes this file's length and bytes, never the directory
        // listing, so the parent's cached entries stay valid.
        self.invalidate_path_caches(&key, None);
        Ok((written, new_len))
    }

    fn truncate_path(&mut self, abs_path: &str, new_len: u64) -> Result<u64, i32> {
        let (_mid, rel_path, key, _parent_key, caps, _imm, volume, _rms) =
            self.resolve_mount(abs_path)?;
        if caps.read_only {
            return Err(EROFS);
        }
        let shared_res = {
            let mut shared = SHARED_PATH_FILES.lock();
            shared.sync_generation(self.mount_generation);
            shared.truncate(&key, new_len)
        };
        let actual_len = match shared_res {
            Some(res) => res.map_err(vfsd::map_fs_error)?,
            None => volume
                .truncate(&rel_path, new_len)
                .map_err(vfsd::map_fs_error)?,
        };
        if let Some(node) = self.open_nodes.get_mut(&key) {
            node.live_len = actual_len;
        }
        self.invalidate_path_caches(&key, None);
        Ok(actual_len)
    }

    fn flush_path(&mut self, abs_path: &str) -> Result<(), i32> {
        let owned = self.tmpfs_owns(abs_path);
        if !owned
            && (abs_path == "/proc"
                || abs_path.starts_with("/proc/")
                || abs_path == "/sys"
                || abs_path.starts_with("/sys/")
                || abs_path == "/dev"
                || abs_path.starts_with("/dev/")
                || early_system::file_len(abs_path)?.is_some())
        {
            return Err(EINVAL);
        }
        let (_mid, _rel, key, _pk, _caps, _imm, volume, _rms) = self.resolve_mount(abs_path)?;
        {
            let mut shared = SHARED_PATH_FILES.lock();
            shared.sync_generation(self.mount_generation);
            shared
                .flush_entry(&key, &*volume)
                .map_err(vfsd::map_fs_error)?;
        }
        volume.flush().map_err(vfsd::map_fs_error)
    }

    /// Paths vfsd serves itself (never a writable volume): the synthetic root,
    /// `/dev`, `/proc`, `/sys`. A memory-filesystem mount that overlays part
    /// of that namespace (`/dev/shm`) is not virtual.
    fn is_virtual_path(&mut self, p: &str) -> bool {
        !self.tmpfs_owns(p)
            && (p == "/"
                || p == "/dev"
                || p.starts_with("/dev/")
                || p == "/proc"
                || p.starts_with("/proc/")
                || p == "/sys"
                || p.starts_with("/sys/"))
    }

    fn mkdir_path(&mut self, abs_path: &str, attrs: FsCreateAttrs, request: &VfsIpcRequest) -> i32 {
        if self.tmpfs_owns(abs_path) {
            let end = abs_path.rfind('/').unwrap_or(0);
            let parent = if end == 0 { "/" } else { &abs_path[..end] };
            let dir = match self.metadata(parent) {
                Ok(dir) if dir.kind == RemoteKind::Directory => dir,
                Ok(_) => return ENOTDIR,
                Err(errno) => return errno,
            };
            // Linux name lookup reports an existing target before testing
            // parent write permission, but still requires directory search.
            if let Err(errno) = check_metadata_permission(dir, request, 1, false) {
                return errno;
            }
            match self.metadata(abs_path) {
                Ok(_) => return EEXIST,
                Err(ENOENT) => {}
                Err(errno) => return errno,
            }
            if let Err(errno) = check_metadata_permission(dir, request, 2, false) {
                return errno;
            }
        } else if let Err(errno) = check_parent(self, abs_path, request, false) {
            return errno;
        }
        if self.is_virtual_path(abs_path) {
            return EROFS;
        }
        if !self.tmpfs_owns(abs_path) && matches!(early_system::file_len(abs_path), Ok(Some(_))) {
            return EEXIST;
        }
        let (_mid, rel_path, key, parent_key, caps, _imm, volume, _rms) =
            match self.resolve_mount(abs_path) {
                Ok(resolved) => resolved,
                Err(errno) => return errno,
            };
        if caps.read_only {
            return EROFS;
        }
        match volume.mkdir_with(&rel_path, attrs) {
            Ok(()) => {
                self.invalidate_path_caches(&key, parent_key.as_ref());
                0
            }
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    fn unlink_path(&mut self, abs_path: &str, remove_dir: bool) -> i32 {
        if self.is_virtual_path(abs_path) {
            return EROFS;
        }
        if !self.tmpfs_owns(abs_path) && matches!(early_system::file_len(abs_path), Ok(Some(_))) {
            return EROFS;
        }
        let (_mid, rel_path, key, parent_key, caps, _imm, volume, _rms) =
            match self.resolve_mount(abs_path) {
                Ok(resolved) => resolved,
                Err(errno) => return errno,
            };
        if caps.read_only {
            return EROFS;
        }
        if remove_dir {
            if rel_path.is_empty() {
                return EBUSY;
            }
            return match volume.rmdir(&rel_path) {
                Ok(()) => {
                    self.invalidate_path_caches(&key, parent_key.as_ref());
                    self.invalidate_subtree_caches(&key);
                    0
                }
                Err(err) => vfsd::map_fs_error(err),
            };
        }
        if !caps.supports_unlink_while_open {
            if let Some(node) = self.open_nodes.get_mut(&key) {
                if node.pending_unlink {
                    return ENOENT;
                }
                if node.refs > 0 {
                    node.pending_unlink = true;
                    self.invalidate_path_caches(&key, parent_key.as_ref());
                    return 0;
                }
            }
        }
        match volume.unlink(&rel_path) {
            Ok(()) => {
                {
                    let mut shared = SHARED_PATH_FILES.lock();
                    shared.sync_generation(self.mount_generation);
                    shared.remove(&key);
                }
                self.invalidate_path_caches(&key, parent_key.as_ref());
                0
            }
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    fn rename_paths(&mut self, old_abs: &str, new_abs: &str, no_replace: bool) -> i32 {
        if self.is_virtual_path(old_abs) || self.is_virtual_path(new_abs) {
            return EROFS;
        }
        let early_owned = |path: &str| matches!(early_system::file_len(path), Ok(Some(_)));
        if (!self.tmpfs_owns(old_abs) && early_owned(old_abs))
            || (!self.tmpfs_owns(new_abs) && early_owned(new_abs))
        {
            return EROFS;
        }
        let (old_mid, old_rel, old_key, old_pk, old_caps, _imm1, volume, _rms1) =
            match self.resolve_mount(old_abs) {
                Ok(r) => r,
                Err(e) => return e,
            };
        let (new_mid, new_rel, new_key, new_pk, _new_caps, _imm2, _vol2, _rms2) =
            match self.resolve_mount(new_abs) {
                Ok(r) => r,
                Err(e) => return e,
            };
        if old_mid != new_mid {
            return EXDEV;
        }
        if old_caps.read_only {
            return EROFS;
        }
        if self.open_nodes.get(&old_key).is_some_and(|n| n.pending_unlink) {
            return ENOENT;
        }
        if !old_caps.supports_unlink_while_open
            && (self.open_nodes.get(&old_key).is_some_and(|n| n.refs > 0)
                || self.open_nodes.get(&new_key).is_some_and(|n| n.refs > 0))
        {
            return EBUSY;
        }
        // A path-addressed volume cannot rename a directory out from under
        // descriptors that name their file by path.
        if !old_caps.supports_unlink_while_open
            && !old_key.1.is_empty()
            && vfsd::mount_table::descendant_keys(&self.open_nodes, &old_key)
                .iter()
                .any(|k| self.open_nodes.get(k).is_some_and(|node| node.refs > 0))
        {
            return EBUSY;
        }
        match volume.rename(&old_rel, &new_rel, !no_replace, no_replace) {
            Ok(()) => {
                {
                    let mut shared = SHARED_PATH_FILES.lock();
                    shared.sync_generation(self.mount_generation);
                    shared.rename(&old_key, new_key.clone(), new_rel);
                }
                self.invalidate_path_caches(&old_key, old_pk.as_ref());
                self.invalidate_subtree_caches(&old_key);
                self.invalidate_path_caches(&new_key, new_pk.as_ref());
                self.invalidate_subtree_caches(&new_key);
                0
            }
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    fn statfs_path(&mut self, abs_path: &str) -> Result<VfsStatFsWire, i32> {
        if self.is_virtual_path(abs_path) {
            return Ok(VfsStatFsWire {
                f_type: 0x0102_1994,
                f_bsize: 4096,
                f_namelen: 255,
                f_frsize: 4096,
                ..VfsStatFsWire::default()
            });
        }
        let (mid, _rel, _key, _pk, _caps, is_immutable, volume, _rms) =
            self.resolve_mount(abs_path)?;
        let st = volume.statfs().map_err(vfsd::map_fs_error)?;
        Ok(VfsStatFsWire {
            f_type: st.fs_type_magic as i64,
            f_bsize: st.block_size as i64,
            f_blocks: st.total_blocks,
            f_bfree: st.free_blocks,
            f_bavail: st.avail_blocks,
            f_files: st.total_inodes,
            f_ffree: st.free_inodes,
            f_fsid: [0, mid as i32],
            f_namelen: i64::from(st.max_name_len),
            f_frsize: st.block_size as i64,
            f_flags: if is_immutable { 1 } else { 0 },
            f_spare: [0; 4],
        })
    }

    fn symlink_path(&mut self, target: &str, link_abs: &str, attrs: FsCreateAttrs) -> i32 {
        if self.is_virtual_path(link_abs) {
            return EROFS;
        }
        let (_mid, rel_path, key, parent_key, caps, _imm, volume, _rms) =
            match self.resolve_mount(link_abs) {
                Ok(r) => r,
                Err(e) => return e,
            };
        if caps.read_only {
            return EROFS;
        }
        match volume.symlink_with(target, &rel_path, attrs) {
            Ok(()) => {
                self.invalidate_path_caches(&key, parent_key.as_ref());
                0
            }
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    fn readlink_path(&mut self, abs_path: &str) -> Result<String, i32> {
        let (_mid, rel_path, _key, _pk, caps, _imm, volume, _rms) = self.resolve_mount(abs_path)?;
        if !caps.supports_symlinks {
            // Existing non-link nodes are EINVAL, missing ones ENOENT.
            return Err(match volume.metadata(&rel_path) {
                Ok(_) => EINVAL,
                Err(err) => vfsd::map_fs_error(err),
            });
        }
        volume.readlink(&rel_path).map_err(vfsd::map_fs_error)
    }

    fn mknod_path(&mut self, abs_path: &str, mode: u64, attrs: FsCreateAttrs) -> i32 {
        use rustos_user_abi::syscall::{vfs_mknod_kind, VfsMknodKind};
        let Some(kind) = vfs_mknod_kind(mode) else {
            return EINVAL;
        };
        match kind {
            VfsMknodKind::Device => return EPERM,
            VfsMknodKind::Regular | VfsMknodKind::Socket | VfsMknodKind::Fifo => {}
        }
        if self.is_virtual_path(abs_path) {
            return EROFS;
        }
        let (_mid, rel_path, key, parent_key, caps, _imm, volume, _rms) =
            match self.resolve_mount(abs_path) {
                Ok(r) => r,
                Err(e) => return e,
            };
        if caps.read_only {
            return EROFS;
        }
        let result = match kind {
            VfsMknodKind::Socket => volume.mknod_socket(&rel_path, attrs),
            VfsMknodKind::Fifo => volume.mknod_fifo(&rel_path, attrs),
            VfsMknodKind::Regular => volume
                .open_prepare_with(
                    &rel_path,
                    FsOpenOptions {
                        create: true,
                        create_new: true,
                        ..FsOpenOptions::default()
                    },
                    attrs,
                )
                .map(|_| ()),
            VfsMknodKind::Device => unreachable!(),
        };
        match result {
            Ok(()) => {
                self.invalidate_path_caches(&key, parent_key.as_ref());
                0
            }
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    fn chmod_path(&mut self, abs_path: &str, mode: u32) -> i32 {
        if self.is_virtual_path(abs_path) {
            return EROFS;
        }
        let (_mid, rel_path, key, parent_key, caps, _imm, volume, _rms) =
            match self.resolve_mount(abs_path) {
                Ok(r) => r,
                Err(e) => return e,
            };
        if caps.read_only {
            return EROFS;
        }
        match volume.chmod(&rel_path, mode) {
            Ok(()) => {
                self.invalidate_path_caches(&key, parent_key.as_ref());
                0
            }
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    fn chmod_node(&mut self, mount_id: u64, node: u64, mode: u32) -> i32 {
        match self
            .node_volume(mount_id)
            .and_then(|volume| volume.chmod_node(node, mode).map_err(vfsd::map_fs_error))
        {
            Ok(()) => 0,
            Err(errno) => errno,
        }
    }

    /// `chown(2)` of a resolved path or a pinned node. One storage hold judges
    /// the credentials and applies the change.
    ///
    /// A volume that stores POSIX owners records the ids. One that cannot (FAT)
    /// reports owner 0 / group 0 for every node, so the call succeeds as a
    /// no-op when it asks for exactly that and is `EPERM` otherwise, which is
    /// what Linux vfat does for a request that differs from the mount's own
    /// owner. Read-only mounts and virtual trees are `EROFS`.
    fn chown_target(
        &mut self,
        target: AttrTarget<'_>,
        cred: vfsd::permissions::OwnerCredentials,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> i32 {
        let (volume, posix, owner, key) = match target {
            AttrTarget::Node(mount_id, node) => {
                let volume = match self.node_volume(mount_id) {
                    Ok(volume) => volume,
                    Err(errno) => return errno,
                };
                let meta = match volume.metadata_node(node) {
                    Ok(meta) => meta,
                    Err(err) => return vfsd::map_fs_error(err),
                };
                (volume, true, (meta.uid, meta.gid), None)
            }
            AttrTarget::Path(abs_path) => {
                if self.is_virtual_path(abs_path) {
                    return EROFS;
                }
                let (_mid, rel_path, key, parent_key, caps, _imm, volume, _rms) =
                    match self.resolve_mount(abs_path) {
                        Ok(r) => r,
                        Err(e) => return e,
                    };
                if caps.read_only {
                    return EROFS;
                }
                let posix = caps.posix_nodes();
                let owner = if posix {
                    match volume.metadata(&rel_path) {
                        Ok(meta) => (meta.uid, meta.gid),
                        Err(err) => return vfsd::map_fs_error(err),
                    }
                } else {
                    match volume.metadata(&rel_path) {
                        Ok(_) => (0, 0),
                        Err(err) => return vfsd::map_fs_error(err),
                    }
                };
                (volume, posix, owner, Some((key, parent_key, rel_path)))
            }
        };
        if !vfsd::permissions::chown_allowed(owner.0, owner.1, cred, uid, gid) {
            return EPERM;
        }
        if !posix {
            let unchanged = |id: Option<u32>| id.is_none_or(|id| id == 0);
            return if unchanged(uid) && unchanged(gid) { 0 } else { EPERM };
        }
        let result = match (target, &key) {
            (AttrTarget::Node(_, node), _) => volume.chown_node(node, uid, gid),
            (AttrTarget::Path(_), Some((_, _, rel_path))) => volume.chown(rel_path, uid, gid),
            (AttrTarget::Path(_), None) => return EINVAL,
        };
        match result {
            Ok(()) => {
                if let Some((key, parent_key, _)) = key {
                    self.invalidate_path_caches(&key, parent_key.as_ref());
                }
                0
            }
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    /// `link(2)` of two resolved paths. Hard links exist on volumes with POSIX
    /// nodes only (`EPERM` on FAT, as Linux vfat); two mounts are `EXDEV`.
    fn link_paths(&mut self, old_abs: &str, new_abs: &str) -> i32 {
        if self.is_virtual_path(old_abs) || self.is_virtual_path(new_abs) {
            return EPERM;
        }
        let (old_mount, old_rel, _k, _pk, _caps, _imm, volume, _rms) =
            match self.resolve_mount(old_abs) {
                Ok(r) => r,
                Err(e) => return e,
            };
        let (new_mount, new_rel, key, parent_key, caps, _imm, _volume, _rms) =
            match self.resolve_mount(new_abs) {
                Ok(r) => r,
                Err(e) => return e,
            };
        if old_mount != new_mount {
            return EXDEV;
        }
        if caps.read_only {
            return EROFS;
        }
        if !caps.posix_nodes() {
            return EPERM;
        }
        let status = Self::link_status(volume.link(&old_rel, &new_rel));
        if status == 0 {
            self.invalidate_path_caches(&key, parent_key.as_ref());
        }
        status
    }

    /// Gives the pinned node of an open description a new name
    /// (`O_TMPFILE`, `linkat` through `/proc/self/fd`).
    fn link_node_path(&mut self, mount_id: u64, node: u64, new_abs: &str) -> i32 {
        if self.is_virtual_path(new_abs) {
            return EPERM;
        }
        let (new_mount, new_rel, key, parent_key, caps, _imm, volume, _rms) =
            match self.resolve_mount(new_abs) {
                Ok(r) => r,
                Err(e) => return e,
            };
        if new_mount != mount_id {
            return EXDEV;
        }
        if caps.read_only {
            return EROFS;
        }
        if !caps.posix_nodes() {
            return EPERM;
        }
        let status = Self::link_status(volume.link_node(node, &new_rel));
        if status == 0 {
            self.invalidate_path_caches(&key, parent_key.as_ref());
        }
        status
    }

    /// `link(2)` errno of a volume result: a directory source is `EPERM`
    /// (the volume's `PermissionDenied`), a full link count `EMLINK`.
    fn link_status(result: Result<(), storage_core::fs::FsError>) -> i32 {
        use storage_core::fs::FsError;
        const EMLINK: i32 = 31;
        match result {
            Ok(()) => 0,
            Err(FsError::PermissionDenied) => EPERM,
            Err(FsError::TooBig) => EMLINK,
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    fn fallocate_node(
        &mut self,
        mount_id: u64,
        node: u64,
        keep_size: bool,
        offset: u64,
        len: u64,
    ) -> Result<u64, i32> {
        self.node_volume(mount_id)?
            .fallocate_node(node, offset, len, keep_size)
            .map_err(vfsd::map_fs_error)
    }

    /// Moves a pinned TmpFs node's bytes into shared frames (idempotent) and
    /// names them for the mapping broker. A volume without shared frames
    /// (the early-system image) is `ENODEV`: the caller keeps its
    /// existing private-copy policy.
    fn shared_backing_node(
        &mut self,
        mount_id: u64,
        node: u64,
    ) -> Result<storage_core::fs::FsSharedBacking, i32> {
        self.node_volume(mount_id)?
            .shared_backing_node(node)
            .map_err(|err| match err {
                storage_core::fs::FsError::Unsupported => ENODEV,
                other => vfsd::map_fs_error(other),
            })
    }

    fn supports_shared_backing_path(&mut self, abs_path: &str) -> bool {
        if self.is_virtual_path(abs_path)
            || matches!(early_system::file_len(abs_path), Ok(Some(_)))
        {
            return false;
        }
        matches!(
            self.resolve_mount(abs_path),
            Ok((_, _, _, _, _, false, _, _))
        )
    }

    fn shared_backing_path(
        &mut self,
        abs_path: &str,
        writable: bool,
    ) -> Result<storage_core::fs::FsSharedBacking, i32> {
        if self.is_virtual_path(abs_path) || early_system::file_len(abs_path)?.is_some() {
            return Err(ENODEV);
        }
        let (_mid, rel_path, key, _pk, _caps, is_immutable, volume, _rms) =
            self.resolve_mount(abs_path)?;
        if is_immutable {
            return Err(ENODEV);
        }
        let mut shared = SHARED_PATH_FILES.lock();
        shared
            .ensure_shared(
                key,
                &rel_path,
                &*volume,
                shared_frames::create_memfd_frames,
                writable,
                self.mount_generation,
            )
            .map_err(vfsd::map_fs_error)
    }

    fn sync_shared_backing_by_object(&mut self, object_id: u64) -> Result<(), i32> {
        let mut shared = SHARED_PATH_FILES.lock();
        shared.sync_generation(self.mount_generation);
        let mounts = &self.mounts;
        shared
            .flush_by_object_id(object_id, |key| mounts.by_id(key.0).map(|entry| entry.volume.clone()))
            .map_err(vfsd::map_fs_error)
    }

    fn truncate_node(&mut self, mount_id: u64, node: u64, new_len: u64) -> Result<u64, i32> {
        self.node_volume(mount_id)?
            .truncate_node(node, new_len)
            .map_err(vfsd::map_fs_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn set_attrs_path(
        &mut self,
        target: AttrTarget<'_>,
        mask: u64,
        atime_secs: i64,
        mtime_secs: i64,
        packed: u64,
    ) -> i32 {
        use rustos_user_abi::syscall::{VFS_ATTR_ATIME_NOW, VFS_ATTR_MTIME_NOW};
        let now = (mask & (VFS_ATTR_ATIME_NOW | VFS_ATTR_MTIME_NOW) != 0).then(tmpfs_clock);
        let atime = (mask & 1 != 0).then(|| now.filter(|_| mask & VFS_ATTR_ATIME_NOW != 0).unwrap_or(FsTimestamp {
            secs: atime_secs,
            nanos: packed as u32,
        }));
        let mtime = (mask & 2 != 0).then(|| now.filter(|_| mask & VFS_ATTR_MTIME_NOW != 0).unwrap_or(FsTimestamp {
            secs: mtime_secs,
            nanos: (packed >> 32) as u32,
        }));
        let crtime = (mask & 4 != 0).then_some(FsTimestamp { secs: mtime_secs, nanos: 0 });
        let dos_attrs = packed as u32;
        match target {
            AttrTarget::Node(mount_id, node) => {
                let volume = match self.node_volume(mount_id) {
                    Ok(volume) => volume,
                    Err(errno) => return errno,
                };
                if mask & (4 | 8) != 0 {
                    return EOPNOTSUPP;
                }
                if mask & 7 != 0 {
                    if let Err(err) = volume.set_times_node(node, atime, mtime) {
                        return vfsd::map_fs_error(err);
                    }
                }
                0
            }
            AttrTarget::Path(abs_path) => {
                if self.is_virtual_path(abs_path) {
                    return EROFS;
                }
                let (_mid, rel_path, key, _parent_key, caps, _imm, volume, _rms) =
                    match self.resolve_mount(abs_path) {
                        Ok(r) => r,
                        Err(e) => return e,
                    };
                if caps.read_only {
                    return EROFS;
                }
                if mask & 7 != 0 {
                    if let Err(err) = volume.set_times(&rel_path, atime, mtime, crtime) {
                        return vfsd::map_fs_error(err);
                    }
                }
                if mask & 8 != 0 {
                    if let Err(err) = volume.set_dos_attrs(&rel_path, dos_attrs) {
                        return vfsd::map_fs_error(err);
                    }
                }
                self.invalidate_path_caches(&key, None);
                0
            }
        }
    }

    fn metadata(&mut self, path: &str) -> Result<Metadata, i32> {
        let owned = self.tmpfs_owns(path);
        if !owned && (path == "/" || path == "/proc") {
            return Ok(Metadata::plain(RemoteKind::Directory, 0, path_inode(path.as_bytes())));
        }
        if !owned && vfsd::proc_fs::proc_sys_dir_children(path).is_some() {
            return Ok(Metadata::plain(RemoteKind::Directory, 0, path_inode(path.as_bytes())));
        }
        if !owned {
            if let Some(Ok(node)) = vfsd::proc_fs::lookup(path, 0) {
                if vfsd::proc_fs::is_virtual_file(node) {
                    let len = self.proc_virtual_file_bytes(node)?.len() as u64;
                    return Ok(Metadata::plain(RemoteKind::File, len, path_inode(path.as_bytes())));
                }
            }
        }
        if owned {
            // Memory filesystem: authoritative and mutable, so never cached.
            // tmpfs_owns already admitted this mount. No path-keyed cache is
            // used here, so avoid constructing relative/parent/cache Strings
            // and cloning the volume for every search-permission check.
            if let Some(errno) = self.mounts.data_mount_error(path) {
                return Err(errno);
            }
            let (mount, rel_path) = self.mounts.resolve(path).ok_or(ENOENT)?;
            return mount.volume
                .metadata(rel_path)
                .map(|meta| Self::posix_metadata(mount.id, meta))
                .map_err(vfsd::map_fs_error);
        }
        if path == "/sys" || path.starts_with("/sys/") {
            if let Some(file) = vfsd::proc_fs::sys_virtual_file(path) {
                let len = self.sys_virtual_file_bytes(file)?.len() as u64;
                return Ok(Metadata::plain(RemoteKind::File, len, path_inode(path.as_bytes())));
            }
            if vfsd::proc_fs::sys_dir_children(path).is_some() {
                return Ok(Metadata::plain(RemoteKind::Directory, 0, path_inode(path.as_bytes())));
            }
            return Err(ENOENT);
        }
        if path == "/dev" || path.starts_with("/dev/") {
            match devmgrd_lookup(path) {
                Ok(kind) => {
                    let mut metadata = Metadata::plain(kind, 0, path_inode(path.as_bytes()));
                    if kind == RemoteKind::Device {
                        metadata.rdev = device_rdev_for_path(path);
                    }
                    return Ok(metadata);
                }
                Err(errno) => return Err(errno),
            }
        }
        self.invalidate_caches_if_remounted();
        if let Some(len) = early_system::file_len(path)? {
            // Never cached: this answer is a table lookup, and a `(0, path)`
            // entry has no reader, so inserting one only evicts useful entries
            // when the bounded cache fills.
            return Ok(Metadata::plain(RemoteKind::File, len, path_inode(path.as_bytes())));
        }
        let (_mid, rel_path, key, _pk, caps, _imm, volume, _rms) = self.resolve_mount(path)?;
        if self.open_nodes.get(&key).is_some_and(|n| n.pending_unlink) {
            return Err(ENOENT);
        }
        let shared_len = {
            let mut shared = SHARED_PATH_FILES.lock();
            shared.sync_generation(self.mount_generation);
            shared.len_of(&key)
        };
        if let Some(entry) = self.metadata_cache.get(&key).copied() {
            self.metadata_cache_lru.touch(&key);
            return match (entry, shared_len) {
                (Ok(mut meta), Some(len)) => {
                    meta.len = len;
                    Ok(meta)
                }
                (other, _) => other,
            };
        }
        let result = match volume.metadata(&rel_path) {
            Ok(meta) => Ok(Metadata::plain(
                match meta.kind {
                    FsNodeKind::File
                    | FsNodeKind::Symlink
                    | FsNodeKind::Socket
                    | FsNodeKind::Fifo => RemoteKind::File,
                    FsNodeKind::Directory => RemoteKind::Directory,
                },
                shared_len.unwrap_or(meta.len),
                vfsd::path::path_inode_for_policy(path.as_bytes(), caps.case_policy),
            )),
            Err(err) => Err(vfsd::map_fs_error(err)),
        };
        if result.is_ok()
            || result
                .as_ref()
                .is_err_and(|errno| cacheable_metadata_errno(*errno))
        {
            self.insert_metadata_cache(key, result);
        }
        result
    }

    /// Negative and positive answers are keyed by `(mount_id, canonical_rel_path)`,
    /// so the cache must have a hard entry bound or a path-probing client could
    /// exhaust the namespace service heap.
    fn insert_metadata_cache(&mut self, key: CacheKey, result: Result<Metadata, i32>) {
        // Replacing an existing answer does not grow the cache, so it must not
        // evict an unrelated entry.
        while !self.metadata_cache.contains_key(&key)
            && self.metadata_cache.len() >= vfsd::METADATA_CACHE_MAX_ENTRIES
        {
            let Some(evict_key) = self.metadata_cache_lru.pop_lru() else {
                self.metadata_cache.clear();
                break;
            };
            self.metadata_cache.remove(&evict_key);
        }
        self.metadata_cache_lru.touch(&key);
        self.metadata_cache.insert(key, result);
    }

    fn inotify_target_for_path(
        &mut self,
        path: &str,
        only_dir: bool,
    ) -> Result<vfsd::inotify::WatchTarget, i32> {
        let meta = self.metadata(path)?;
        let is_dir = meta.kind == RemoteKind::Directory;
        if only_dir && !is_dir {
            return Err(ENOTDIR);
        }
        let (mount_id, mount_generation, node) = if let Ok((mid, _, _, _, _, _, _, _)) = self.resolve_mount(path) {
            (mid, self.mount_generation, meta.fs.map_or(0, |f| f.node_id))
        } else {
            (0, 1, 0)
        };
        Ok(vfsd::inotify::WatchTarget {
            mount_id,
            mount_generation,
            node,
            canonical_path: path.to_string(),
            is_dir,
        })
    }

    /// Returns `(mount_id, mount_generation, node, parent_node, is_dir)` for `path`.
    fn inotify_path_context_locked(&mut self, path: &str) -> (u64, u64, u64, u64, bool) {
        let meta = self.metadata(path).ok();
        let is_dir = meta.is_some_and(|m| m.kind == RemoteKind::Directory);
        let node = meta.and_then(|m| m.fs.map(|f| f.node_id)).unwrap_or(0);
        let parent_path = match path.rfind('/') {
            Some(0) => "/",
            Some(pos) => &path[..pos],
            None => "/",
        };
        let parent_node = if path == "/" {
            0
        } else {
            self.metadata(parent_path)
                .ok()
                .and_then(|m| m.fs.map(|f| f.node_id))
                .unwrap_or(0)
        };
        let (mount_id, mount_generation) = if let Ok((mid, _, _, _, _, _, _, _)) = self.resolve_mount(path) {
            (mid, self.mount_generation)
        } else {
            (0, 1)
        };
        (mount_id, mount_generation, node, parent_node, is_dir)
    }

    fn visit_directory(
        &mut self,
        handle: &RemoteHandle,
        virtual_entries: Option<Vec<DirEntry>>,
        cursor: u64,
        visit: impl FnMut(u64, &DirEntry) -> Result<bool, i32>,
    ) -> Result<(), i32> {
        use storage_core::fs::CasePolicy;
        self.ensure_tmpfs_mounts();
        if let Some(entries) = virtual_entries {
            return visit_directory_entries(
                cursor, &self.directory_overlay(Some(&handle.path)), CasePolicy::Exact,
                entries.iter().enumerate().map(|(i, entry)| (i as u64 + 1, entry)), visit,
            );
        }
        if handle.node != 0 {
            let mount = self.mounts.by_id(handle.mount_id).ok_or(ESTALE)?;
            let path = match mount.volume.directory_path_node(handle.node) {
                Ok(rel) if rel.is_empty() => Some(mount.mount_point.clone()),
                Ok(rel) if mount.mount_point == "/" => Some(format!("/{rel}")),
                Ok(rel) => Some(format!("{}/{rel}", mount.mount_point)),
                // An open directory may be detached. Do not inject mounts
                // using its former name (which can now refer to another node).
                Err(storage_core::FsError::NotFound) => None,
                Err(err) => return Err(vfsd::map_fs_error(err)),
            };
            let entries: Vec<_> = mount.volume.read_dir_node(handle.node)
                .map_err(vfsd::map_fs_error)?.into_iter().map(DirEntry::from_fs).collect();
            return visit_directory_entries(
                cursor, &self.directory_overlay(path.as_deref()), mount.volume.capabilities().case_policy,
                entries.iter().map(|entry| (entry.cookie, entry)), visit,
            );
        }
        self.invalidate_caches_if_remounted();
        let (mount_id, rel_path, key, _pk, caps, _imm, volume, _rms) = self.resolve_mount(&handle.path)?;
        let overlay = self.directory_overlay(Some(&handle.path));
        if caps.posix_nodes() {
            let entries: Vec<_> = volume.read_dir(&rel_path).map_err(vfsd::map_fs_error)?
                .into_iter().map(DirEntry::from_fs).collect();
            return visit_directory_entries(
                cursor, &overlay, caps.case_policy, entries.iter().map(|entry| (entry.cookie, entry)), visit,
            );
        }
        if !self.dir_entries_cache.contains_key(&key) {
            let resolved = volume.read_dir(&rel_path).map_err(vfsd::map_fs_error)?
                .into_iter().map(DirEntry::from_fs).collect();
            while self.dir_entries_cache.len() >= vfsd::DIR_ENTRIES_CACHE_MAX_ENTRIES {
                let Some(evict_key) = self.dir_entries_cache_lru.pop_lru() else {
                    self.dir_entries_cache.clear();
                    break;
                };
                self.dir_entries_cache.remove(&evict_key);
            }
            self.dir_entries_cache_lru.touch(&key);
            self.dir_entries_cache.insert(key.clone(), resolved);
        } else {
            self.dir_entries_cache_lru.touch(&key);
        }
        let cached = self.dir_entries_cache.get(&key).ok_or(EIO)?;
        let hide_failed_data = handle.path == "/" && self.mounts.data_mount_error("/data").is_some();
        let visible = cached.iter().filter(|entry| {
            // A configured but failed data mount must not masquerade as a
            // same-name placeholder on the read-only boot volume.
            if hide_failed_data && entry.name.eq_ignore_ascii_case("data") {
                return false;
            }
            // Skip past cookies before constructing any path keys.
            if vfsd::directory::backing_cookie(entry.cookie).is_none_or(|cookie| cookie <= cursor) {
                return false;
            }
            let child_rel = if rel_path.is_empty() { entry.name.clone() }
                else { format!("{rel_path}/{}", entry.name) };
            let child = cache_key_for_mount(mount_id, caps.case_policy, &child_rel);
            !self.open_nodes.get(&child).is_some_and(|node| node.pending_unlink)
        });
        visit_directory_entries(cursor, &overlay, caps.case_policy,
            visible.map(|entry| (entry.cookie, entry)), visit)
    }

    fn directory_overlay(&self, path: Option<&str>) -> Vec<(u64, DirEntry)> {
        let Some(path) = path else { return Vec::new() };
        const ROOT_VIRTUAL: [&str; 3] = ["dev", "proc", "sys"];
        let mut entries = Vec::new();
        if path == "/" {
            entries.extend(ROOT_VIRTUAL.iter().enumerate().map(|(i, name)|
                (i as u64 + 1, DirEntry::new(name, RemoteKind::Directory))));
        }
        entries.extend(self.mounts.submounts_at(path).into_iter().filter_map(|(id, name)| {
            if path == "/" && ROOT_VIRTUAL.contains(&name) { return None; }
            let cookie = id.checked_add(ROOT_VIRTUAL.len() as u64)?;
            (cookie < vfsd::directory::FILE_COOKIE_BASE)
                .then(|| (cookie, DirEntry::new(name, RemoteKind::Directory)))
        }));
        entries
    }
}


fn visit_directory_entries<'a>(
    cursor: u64,
    overlay: &'a [(u64, DirEntry)],
    case_policy: storage_core::fs::CasePolicy,
    entries: impl IntoIterator<Item = (u64, &'a DirEntry)>,
    mut visit: impl FnMut(u64, &DirEntry) -> Result<bool, i32>,
) -> Result<(), i32> {
    let overlay: Vec<_> = overlay.iter().map(|(cookie, entry)| (*cookie, entry)).collect();
    vfsd::directory::visit_merged(cursor, &overlay, entries, |a, b| match case_policy {
        storage_core::fs::CasePolicy::Exact => a.name == b.name,
        storage_core::fs::CasePolicy::AsciiInsensitive => a.name.eq_ignore_ascii_case(&b.name),
        storage_core::fs::CasePolicy::UnicodePreservingInsensitive =>
            a.name.eq_ignore_ascii_case(&b.name) || a.name.to_lowercase() == b.name.to_lowercase(),
    }, |cookie, entry| visit(cookie, entry))
}

/// The virtual directories here and devmgrd's advertised device directory
/// are fixed tables. Their slots are stable within a running image.
fn virtual_dir_entries(path: &str, caller_pid: u64) -> Result<Option<Vec<DirEntry>>, i32> {
    if matches!(path, "/dev" | "/dev/input" | "/dev/dri") {
        return devmgrd_dir_entries(path).map(Some);
    }
    if path == "/proc" {
        return Ok(Some(["cpuinfo", "filesystems", "meminfo", "mounts", "self", "sys", "uptime"].into_iter()
            .map(|name| DirEntry::new(name, match name {
                "self" => RemoteKind::Symlink,
                "sys" => RemoteKind::Directory,
                _ => RemoteKind::File,
            }))
            .collect()));
    }
    if let Some(children) = vfsd::proc_fs::proc_sys_dir_children(path) {
        return Ok(Some(children.iter().map(|&(name, dt)| {
            DirEntry::new(name, if dt == rustos_user_abi::procfs::DT_DIR {
                RemoteKind::Directory
            } else {
                RemoteKind::File
            })
        }).collect()));
    }
    if let Some(Ok(node @ (vfsd::proc_fs::ProcNode::PidDir | vfsd::proc_fs::ProcNode::SelfLink
        | vfsd::proc_fs::ProcNode::NsDir))) = vfsd::proc_fs::lookup(path, caller_pid)
    {
        let entries = if node == vfsd::proc_fs::ProcNode::NsDir { vfsd::proc_fs::ns_dir_entries() }
            else { vfsd::proc_fs::pid_dir_entries() };
        return Ok(Some(entries.into_iter().map(|(name, kind)| DirEntry::new(name, match kind {
            rustos_user_abi::procfs::DT_DIR => RemoteKind::Directory,
            rustos_user_abi::procfs::DT_LNK => RemoteKind::Symlink,
            _ => RemoteKind::File,
        })).collect()));
    }
    if let Some(children) = vfsd::proc_fs::sys_dir_children(path) {
        return Ok(Some(children.iter().map(|name| {
            let child = format!("{path}/{name}");
            let kind = if vfsd::proc_fs::sys_virtual_file(&child).is_some() { RemoteKind::File }
                else { RemoteKind::Directory };
            DirEntry::new(name, kind)
        }).collect()));
    }
    if path.starts_with("/sys/") { return Err(ENOENT); }
    Ok(None)
}
