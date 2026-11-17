// SPDX-License-Identifier: MIT

use rustos_user_abi::syscall::{
    ProductExecutableSnapshotEvidence, VFS_EXECUTABLE_SNAPSHOT_DIRFD_ROOT,
};
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

    fn resolve_path(
        &mut self,
        request: &VfsIpcRequest,
        pid: u64,
        dirfd: u64,
        path: &str,
    ) -> Result<String, i32> {
        self.resolve_path_with_base(pid, dirfd, request.remote_id, path)
    }

    fn resolve_path_with_base(
        &mut self,
        pid: u64,
        dirfd: u64,
        base_remote_id: u64,
        path: &str,
    ) -> Result<String, i32> {
        if path.is_empty() || path.len() > VFS_IPC_PATH_CAPACITY {
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
                    if path.len() + 1 > VFS_IPC_PATH_CAPACITY || path.as_bytes().iter().any(|&b| b == 0 || b == b'\\')
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
            if handle.kind != RemoteKind::Directory {
                return Err(ENOTDIR);
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
        let metadata = match vfsd::proc_fs::lookup(path, request.pid) {
            Some(Ok(node)) => {
                const LINUX_O_NOFOLLOW: u64 = 0o400_000;
                const ELOOP: i32 = 40;
                match node {
                    vfsd::proc_fs::ProcNode::SelfLink => {
                        if flags & LINUX_O_NOFOLLOW != 0 {
                            return Err(ELOOP);
                        }
                        Metadata {
                            kind: RemoteKind::Directory,
                            len: 0,
                            inode: path_inode(path.as_bytes()),
                        }
                    }
                    vfsd::proc_fs::ProcNode::PidDir => Metadata {
                        kind: RemoteKind::Directory,
                        len: 0,
                        inode: path_inode(path.as_bytes()),
                    },
                    vfsd::proc_fs::ProcNode::ExeLink => {
                        return Err(if flags & LINUX_O_NOFOLLOW != 0 {
                            ELOOP
                        } else {
                            ENOENT
                        });
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
                        let len = lock_vfs_storage().proc_virtual_file_bytes(file)?.len() as u64;
                        Metadata {
                            kind: RemoteKind::File,
                            len,
                            inode: path_inode(path.as_bytes()),
                        }
                    }
                }
            }
            Some(Err(errno)) => return Err(errno),
            None => {
                if vfsd::proc_fs::sys_virtual_file(path).is_some()
                    || path == "/sys"
                    || path.starts_with("/sys/")
                {
                    if writable {
                        return Err(EACCES);
                    }
                    if create || truncate {
                        return Err(EROFS);
                    }
                    lock_vfs_storage().metadata(path)?
                } else if path == "/" || path == "/proc" || path == "/run" || path.starts_with("/run/")
                    || path == "/dev" || path.starts_with("/dev/")
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
                } else if let Some(image_len) = early_system::file_len(path)? {
                    if writable || create || truncate {
                        return Err(EROFS);
                    }
                    Metadata {
                        kind: RemoteKind::File,
                        len: image_len,
                        inode: path_inode(path.as_bytes()),
                    }
                } else {
                    let mut storage = lock_vfs_storage();
                    let (mid, meta, should_reset) = storage.open_on_mount(
                        path,
                        FsOpenOptions {
                            create,
                            create_new,
                            truncate,
                            write: writable,
                            append,
                        },
                        flags & O_DIRECTORY != 0,
                    ).inspect_err(|&errno| {
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
        if flags & O_DIRECTORY != 0 && metadata.kind != RemoteKind::Directory {
            return Err(ENOTDIR);
        }
        if metadata.kind == RemoteKind::Directory && writable {
            return Err(EISDIR);
        }
        let handle = RemoteHandle {
            checkpoint_version: VFSD_OPEN_CHECKPOINT_VERSION,
            kind: metadata.kind,
            mount_id,
            writable: writable && metadata.kind != RemoteKind::Directory,
            append,
            path: path.to_string(),
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
        if mount_id != 0 {
            lock_vfs_storage().note_open_node(path, metadata.len, reset_live_len);
        }
        Ok((id, handle))
    }

    fn read_remote_into(
        &mut self,
        request: &VfsIpcRequest,
        offset: Option<u64>,
        len: usize,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        let id = request.remote_id;
        let (path, start, file_len) = {
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
            (
                handle.path.clone(),
                offset.unwrap_or(handle.cursor),
                lock_vfs_storage().live_len_for_path(handle.path.as_str(), handle.len),
            )
        };
        if offset.is_none()
            && self.checkpoint_operation_replayed(request, checkpoint_handle_key(id))
        {
            let handle = self.handles.get(&id).ok_or(EBADF)?;
            if handle.last_mutation != VFSD_OPEN_MUTATION_READ {
                return Err(EIO);
            }
            let (requested, replay_len) = unpack_u32_pair(handle.last_result);
            if !vfsd::read_replay_is_valid(
                handle.checkpoint_version, len, requested as usize, replay_len as usize,
                dest.len(), handle.last_start, file_len,
            ) {
                return Err(EIO);
            }
            if replay_len == 0 {
                return Ok(0);
            }
            let replay_len = replay_len as usize;
            let read = Self::read_slice_by_path(
                path.as_str(),
                request.pid,
                file_len,
                handle.last_start,
                &mut dest[..replay_len],
            )?;
            return (read == replay_len).then_some(read).ok_or(EIO);
        }
        if offset.is_none() && self.handles.get(&id).is_some_and(cursor_mutation_prepared) {
            return Err(EBUSY);
        }
        let requested_len = len;
        let available = file_len.saturating_sub(start);
        let len = len.min(usize::try_from(available).unwrap_or(usize::MAX)).min(dest.len());
        let read = if len == 0 {
            0
        } else {
            Self::read_slice_by_path(path.as_str(), request.pid, file_len, start, &mut dest[..len])?
        };
        if offset.is_none() {
            let mut candidate = self.handles.get(&id).cloned().ok_or(EBADF)?;
            candidate.checkpoint_version = VFSD_OPEN_CHECKPOINT_VERSION;
            candidate.cursor = candidate.cursor.checked_add(read as u64).ok_or(EOVERFLOW)?;
            candidate.last_mutation = VFSD_OPEN_MUTATION_READ;
            candidate.last_start = start;
            candidate.last_result = pack_u32_pair(requested_len, read)?;
            self.checkpoint_handle_state(
                request,
                id,
                &candidate,
                VFSD_OPEN_MUTATION_READ,
                start,
                candidate.last_result,
            )?;
            self.handles.insert(id, candidate);
        }
        Ok(read)
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
            ProcNode::FilesystemsFile => Ok(proc_fs::format_filesystems().to_vec()),
            ProcNode::MountsFile => Ok(proc_fs::format_mounts(!self.mounts.is_empty())),
            ProcNode::UptimeFile => {
                Ok(proc_fs::format_uptime(rustos_svc_runtime::syscall::monotonic_nanos()))
            }
            ProcNode::SelfLink | ProcNode::PidDir | ProcNode::ExeLink => Err(EINVAL),
        }
    }

    fn sys_virtual_file_bytes(&self, _file: vfsd::proc_fs::SysFile) -> Result<Vec<u8>, i32> {
        let mut mask = 0_u64;
        let status = unsafe {
            rustos_svc_runtime::syscall::syscall3(
                linux_abi::SYS_SCHED_GETAFFINITY,
                0,
                core::mem::size_of::<u64>() as u64,
                (&mut mask as *mut u64) as u64,
            )
        };
        if status < 0 || mask == 0 {
            return Err(EIO);
        }
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
                let (mount_id, rel_path, key, _parent_key, _caps, _is_immutable, volume, read_map_source) =
                    self.resolve_mount(path)?;
                if let Some(bytes) = self.file_bytes_cache.get(&key) {
                    return Ok(copy_file_cache_range(bytes, start, dest));
                }

                let cacheable_len = should_materialize_file_cache(file_len, start, dest.len());
                if let Some(expected_len) = cacheable_len {
                    let bytes = read_volume_file_to_vec(&*volume, &rel_path, expected_len)?;
                    let read = copy_file_cache_range(&bytes, start, dest);
                    if self
                        .file_bytes_cache_bytes
                        .checked_add(bytes.len())
                        .is_none_or(|total| total > FILE_BYTES_CACHE_BUDGET_BYTES)
                    {
                        self.file_bytes_cache.clear();
                        self.file_bytes_cache_bytes = 0;
                    }
                    self.file_bytes_cache_bytes += bytes.len();
                    self.file_bytes_cache.insert(key, bytes);
                    return Ok(read);
                }

                if let Some(rms) = read_map_source {
                    return self.read_mapped_file_on(mount_id, &rel_path, &key, &*rms, file_len, start, dest);
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
        if !self.read_maps.contains_key(key) {
            let map = read_map_source
                .plan_read(rel_path, vfsd::snapshot_budget::SNAPSHOT_MAP_BYTES)
                .map_err(map_range_error)?;
            if map.len() != len {
                return Err(EIO);
            }
            let cost = map.metadata_bytes();
            if self.read_maps.len() >= 128
                || self
                    .read_map_bytes
                    .checked_add(cost)
                    .is_none_or(|n| n > 8 * 1024 * 1024)
            {
                self.read_maps.clear();
                self.read_map_bytes = 0;
            }
            self.read_map_bytes += cost;
            self.read_maps.insert(key.clone(), map);
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
        let (mount_id, rel_path, key, _parent_key, _caps, is_immutable, volume, read_map_source) =
            self.resolve_mount(path)?;
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
        _mount_id: u64,
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
            let (mid, rel, key, _pk, _caps, _imm, _vol, rms) = self.resolve_mount(path.as_str())?;
            (mid, rel, key, rms)
        };
        if let Some(snapshot) = self
            .executable_snapshot_cache
            .get(&cache_key)
            .copied()
        {
            return ExecutableSnapshotOpen::duplicate_cached(
                snapshot.fd,
                snapshot.file_bytes,
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
        digest_bytes: [u8; 32],
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
                    let evidence = ProductExecutableSnapshotEvidence {
                        storage_epoch: plan.storage_epoch,
                        mount_generation: plan.mount_generation,
                        request_id: plan.request_id,
                        file_bytes: file_len as u64,
                        digest: digest_bytes,
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
        if self
            .executable_snapshot_cache_bytes
            .checked_add(file_len)
            .is_none_or(|total| total > EXECUTABLE_SNAPSHOT_CACHE_BUDGET_BYTES)
        {
            for snapshot in self.executable_snapshot_cache.values() {
                close_fd(snapshot.fd);
            }
            self.executable_snapshot_cache.clear();
            self.executable_snapshot_cache_bytes = 0;
        }
        self.executable_snapshot_cache.insert(
            plan.cache_key.clone(),
            ExecutableSnapshot {
                fd: cache_fd,
                file_bytes: plan.metadata_len,
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
        cursor: usize,
        user_len: usize,
        payload: &mut [u8],
    ) -> Result<(usize, usize), i32> {
        if user_len < 24 {
            return Err(EINVAL);
        }
        let path = {
            let Some(handle) = self.handles.get(&id) else {
                return Err(EBADF);
            };
            if handle.kind != RemoteKind::Directory {
                return Err(ENOTDIR);
            }
            handle.path.clone()
        };
        let limit = user_len.min(payload.len());
        let mut written = 0usize;
        let mut consumed = 0usize;
        let mut encode = |index: usize, entry: &DirEntry| -> Result<bool, i32> {
            let Some(record_len) = encode_dirent_into(entry, index + 1, &mut payload[written..limit])
            else {
                if written == 0 {
                    return Err(EINVAL);
                }
                return Ok(false);
            };
            written += record_len;
            consumed += 1;
            Ok(true)
        };
        if let Some(Ok(
            vfsd::proc_fs::ProcNode::PidDir | vfsd::proc_fs::ProcNode::SelfLink,
        )) = vfsd::proc_fs::lookup(path.as_str(), caller_pid)
        {
            if cursor == 0 {
                let entry = DirEntry::new("mounts", RemoteKind::File);
                let _ = encode(0, &entry)?;
            }
        } else {
            lock_vfs_storage().for_each_dir_entry_from(path.as_str(), cursor, encode)?;
        }
        Ok((written, consumed))
    }
}

impl VfsStorage {
    fn ensure_mounted(&mut self) -> Result<(), i32> {
        if self.mounts.is_empty() {
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
            let table = MountTable::mount_fat_disk(
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
            self.dvm_storage_epoch = storage_epoch;
            self.mounts = table;
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
        self.ensure_mounted()?;
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
        self.dir_entries_cache.remove(key);
        if let Some(bytes) = self.file_bytes_cache.remove(key) {
            self.file_bytes_cache_bytes = self.file_bytes_cache_bytes.saturating_sub(bytes.len());
        }
        if let Some(map) = self.read_maps.remove(key) {
            self.read_map_bytes = self.read_map_bytes.saturating_sub(map.metadata_bytes());
        }
        if let Some(snapshot) = self.executable_snapshot_cache.remove(key) {
            close_fd(snapshot.fd);
            self.executable_snapshot_cache_bytes = self
                .executable_snapshot_cache_bytes
                .saturating_sub(snapshot.file_bytes as usize);
        }
        if let Some(pk) = parent_key {
            self.dir_entries_cache.remove(pk);
            self.metadata_cache.remove(pk);
        }
    }

    fn invalidate_subtree_caches(&mut self, key: &CacheKey) {
        let prefix = if key.1.is_empty() {
            String::new()
        } else {
            format!("{}/", key.1)
        };
        let matches = |k: &CacheKey| -> bool {
            k.0 == key.0 && (k.1 == key.1 || prefix.is_empty() || k.1.starts_with(prefix.as_str()))
        };
        self.metadata_cache.retain(|k, _| !matches(k));
        self.dir_entries_cache.retain(|k, _| !matches(k));
        let mut removed_file_bytes = 0usize;
        self.file_bytes_cache.retain(|k, v| {
            if matches(k) {
                removed_file_bytes += v.len();
                false
            } else {
                true
            }
        });
        self.file_bytes_cache_bytes = self.file_bytes_cache_bytes.saturating_sub(removed_file_bytes);
        let mut removed_map_bytes = 0usize;
        self.read_maps.retain(|k, m| {
            if matches(k) {
                removed_map_bytes += m.metadata_bytes();
                false
            } else {
                true
            }
        });
        self.read_map_bytes = self.read_map_bytes.saturating_sub(removed_map_bytes);
        let mut removed_snap_bytes = 0usize;
        self.executable_snapshot_cache.retain(|k, snap| {
            if matches(k) {
                close_fd(snap.fd);
                removed_snap_bytes += snap.file_bytes as usize;
                false
            } else {
                true
            }
        });
        self.executable_snapshot_cache_bytes = self
            .executable_snapshot_cache_bytes
            .saturating_sub(removed_snap_bytes);
    }

    fn open_on_mount(
        &mut self,
        abs_path: &str,
        opts: FsOpenOptions,
        dir_only: bool,
    ) -> Result<(u64, Metadata, bool), i32> {
        let (mount_id, rel_path, key, parent_key, caps, _imm, volume, _rms) =
            self.resolve_mount(abs_path)?;
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
                Metadata {
                    kind: RemoteKind::Directory,
                    len: 0,
                    inode: path_inode(abs_path.as_bytes()),
                },
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
        let kind = match fs_meta.kind {
            FsNodeKind::File | FsNodeKind::Symlink => RemoteKind::File,
            FsNodeKind::Directory => RemoteKind::Directory,
        };
        Ok((
            mount_id,
            Metadata {
                kind,
                len: fs_meta.len,
                inode: vfsd::path::path_inode_for_policy(abs_path.as_bytes(), caps.case_policy),
            },
            opts.truncate,
        ))
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
            if let Some(entry) = self.open_nodes.get_mut(&key) {
                entry.refs = entry.refs.saturating_sub(1);
                if entry.refs == 0 {
                    should_unlink = entry.pending_unlink;
                    self.open_nodes.remove(&key);
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
            if let Some(node) = self.open_nodes.get(&key) {
                return node.live_len;
            }
        }
        fallback
    }

    fn live_stat_for_path(&mut self, abs_path: &str, fallback_len: u64) -> (u64, u64) {
        if let Ok((_mid, _rel, key, _pk, caps, _imm, _vol, _rms)) = self.resolve_mount(abs_path) {
            let len = self
                .open_nodes
                .get(&key)
                .map_or(fallback_len, |node| node.live_len);
            let inode = vfsd::path::path_inode_for_policy(abs_path.as_bytes(), caps.case_policy);
            return (len, inode);
        }
        (fallback_len, path_inode(abs_path.as_bytes()))
    }

    fn write_at_path(&mut self, abs_path: &str, offset: u64, data: &[u8]) -> Result<(usize, u64), i32> {
        let (_mid, rel_path, key, parent_key, caps, _imm, volume, _rms) =
            self.resolve_mount(abs_path)?;
        if caps.read_only {
            return Err(EROFS);
        }
        let (written, new_len) = volume
            .write_at(&rel_path, offset, data)
            .map_err(vfsd::map_fs_error)?;
        if let Some(node) = self.open_nodes.get_mut(&key) {
            node.live_len = new_len;
        }
        self.invalidate_path_caches(&key, parent_key.as_ref());
        Ok((written, new_len))
    }

    fn truncate_path(&mut self, abs_path: &str, new_len: u64) -> Result<u64, i32> {
        let (_mid, rel_path, key, parent_key, caps, _imm, volume, _rms) =
            self.resolve_mount(abs_path)?;
        if caps.read_only {
            return Err(EROFS);
        }
        let actual_len = volume
            .truncate(&rel_path, new_len)
            .map_err(vfsd::map_fs_error)?;
        if let Some(node) = self.open_nodes.get_mut(&key) {
            node.live_len = actual_len;
        }
        self.invalidate_path_caches(&key, parent_key.as_ref());
        Ok(actual_len)
    }

    fn flush_path(&mut self, abs_path: &str) -> Result<(), i32> {
        if abs_path == "/proc"
            || abs_path.starts_with("/proc/")
            || abs_path == "/sys"
            || abs_path.starts_with("/sys/")
            || abs_path == "/dev"
            || abs_path.starts_with("/dev/")
            || abs_path == "/run"
            || abs_path.starts_with("/run/")
            || early_system::file_len(abs_path)?.is_some()
        {
            return Err(EINVAL);
        }
        let (_mid, _rel, _key, _pk, _caps, _imm, volume, _rms) = self.resolve_mount(abs_path)?;
        volume.flush().map_err(vfsd::map_fs_error)
    }

    fn mkdir_path(&mut self, abs_path: &str, euid: u32) -> i32 {
        if abs_path == "/run" || abs_path.starts_with("/run/") {
            return self.run_dirs.mkdir(abs_path, euid);
        }
        if abs_path == "/"
            || abs_path == "/dev"
            || abs_path.starts_with("/dev/")
            || abs_path == "/proc"
            || abs_path.starts_with("/proc/")
            || abs_path == "/sys"
            || abs_path.starts_with("/sys/")
        {
            return EROFS;
        }
        if matches!(early_system::file_len(abs_path), Ok(Some(_))) {
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
        match volume.mkdir(&rel_path) {
            Ok(()) => {
                self.invalidate_path_caches(&key, parent_key.as_ref());
                0
            }
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    fn unlink_path(&mut self, abs_path: &str, remove_dir: bool) -> i32 {
        if abs_path.starts_with("/run/") {
            return ENOENT;
        }
        if abs_path == "/"
            || abs_path == "/run"
            || abs_path == "/dev"
            || abs_path.starts_with("/dev/")
            || abs_path == "/proc"
            || abs_path.starts_with("/proc/")
            || abs_path == "/sys"
            || abs_path.starts_with("/sys/")
        {
            return EROFS;
        }
        if matches!(early_system::file_len(abs_path), Ok(Some(_))) {
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
                self.invalidate_path_caches(&key, parent_key.as_ref());
                0
            }
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    fn is_virtual_path(p: &str) -> bool {
        p == "/"
            || p == "/dev"
            || p.starts_with("/dev/")
            || p == "/proc"
            || p.starts_with("/proc/")
            || p == "/sys"
            || p.starts_with("/sys/")
            || p == "/run"
            || p.starts_with("/run/")
    }

    fn rename_paths(&mut self, old_abs: &str, new_abs: &str, no_replace: bool) -> i32 {
        if Self::is_virtual_path(old_abs) || Self::is_virtual_path(new_abs) {
            return EROFS;
        }
        if matches!(early_system::file_len(old_abs), Ok(Some(_)))
            || matches!(early_system::file_len(new_abs), Ok(Some(_)))
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
        match volume.rename(&old_rel, &new_rel, !no_replace, no_replace) {
            Ok(()) => {
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
        if abs_path == "/proc"
            || abs_path.starts_with("/proc/")
            || abs_path == "/sys"
            || abs_path.starts_with("/sys/")
            || abs_path == "/dev"
            || abs_path.starts_with("/dev/")
            || abs_path == "/run"
            || abs_path.starts_with("/run/")
        {
            return Ok(VfsStatFsWire {
                f_type: 0x0102_1994,
                f_bsize: 4096,
                f_namelen: 255,
                f_frsize: 4096,
                ..VfsStatFsWire::default()
            });
        }
        let (_mid, _rel, _key, _pk, _caps, is_immutable, volume, _rms) =
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
            f_fsid: [0, 0],
            f_namelen: i64::from(st.max_name_len),
            f_frsize: st.block_size as i64,
            f_flags: if is_immutable { 1 } else { 0 },
            f_spare: [0; 4],
        })
    }

    fn symlink_path(&mut self, target: &str, link_abs: &str) -> i32 {
        if Self::is_virtual_path(link_abs) {
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
        match volume.symlink(target, &rel_path) {
            Ok(()) => {
                self.invalidate_path_caches(&key, parent_key.as_ref());
                0
            }
            Err(err) => vfsd::map_fs_error(err),
        }
    }

    fn set_attrs_path(
        &mut self,
        abs_path: &str,
        mask: u64,
        atime_secs: i64,
        mtime_secs: i64,
        dos_attrs: u32,
    ) -> i32 {
        if Self::is_virtual_path(abs_path) {
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
        if mask & 7 != 0 {
            let atime = (mask & 1 != 0).then_some(FsTimestamp {
                secs: atime_secs,
                nanos: 0,
            });
            let mtime = (mask & 2 != 0).then_some(FsTimestamp {
                secs: mtime_secs,
                nanos: 0,
            });
            let crtime = (mask & 4 != 0).then_some(FsTimestamp {
                secs: mtime_secs,
                nanos: 0,
            });
            if let Err(err) = volume.set_times(&rel_path, atime, mtime, crtime) {
                return vfsd::map_fs_error(err);
            }
        }
        if mask & 8 != 0 {
            if let Err(err) = volume.set_dos_attrs(&rel_path, dos_attrs) {
                return vfsd::map_fs_error(err);
            }
        }
        self.invalidate_path_caches(&key, parent_key.as_ref());
        0
    }

    fn metadata(&mut self, path: &str) -> Result<Metadata, i32> {
        if path == "/" || path == "/proc" || self.run_dirs.contains(path) {
            return Ok(Metadata {
                kind: RemoteKind::Directory,
                len: 0,
                inode: path_inode(path.as_bytes()),
            });
        }
        if path == "/sys" || path.starts_with("/sys/") {
            if let Some(file) = vfsd::proc_fs::sys_virtual_file(path) {
                let len = self.sys_virtual_file_bytes(file)?.len() as u64;
                return Ok(Metadata {
                    kind: RemoteKind::File,
                    len,
                    inode: path_inode(path.as_bytes()),
                });
            }
            if vfsd::proc_fs::sys_dir_children(path).is_some() {
                return Ok(Metadata {
                    kind: RemoteKind::Directory,
                    len: 0,
                    inode: path_inode(path.as_bytes()),
                });
            }
            return Err(ENOENT);
        }
        if path.starts_with("/run/") {
            // `/run` is vfsd's volatile namespace, not a FAT directory: a path
            // that `mkdir` has not created does not exist on the boot volume.
            return Err(ENOENT);
        }
        if path == "/dev" || path.starts_with("/dev/") {
            match devmgrd_lookup(path) {
                Ok(kind) => {
                    return Ok(Metadata {
                        kind,
                        len: 0,
                        inode: path_inode(path.as_bytes()),
                    });
                }
                Err(errno) => return Err(errno),
            }
        }
        self.invalidate_caches_if_remounted();
        if let Some(len) = early_system::file_len(path)? {
            let es_key = (0_u64, path.to_string());
            let metadata = Metadata {
                kind: RemoteKind::File,
                len,
                inode: path_inode(path.as_bytes()),
            };
            self.insert_metadata_cache(es_key, Ok(metadata));
            return Ok(metadata);
        }
        let (_mid, rel_path, key, _pk, caps, _imm, volume, _rms) = self.resolve_mount(path)?;
        if self.open_nodes.get(&key).is_some_and(|n| n.pending_unlink) {
            return Err(ENOENT);
        }
        if let Some(entry) = self.metadata_cache.get(&key) {
            return *entry;
        }
        let result = match volume.metadata(&rel_path) {
            Ok(meta) => Ok(Metadata {
                kind: match meta.kind {
                    FsNodeKind::File | FsNodeKind::Symlink => RemoteKind::File,
                    FsNodeKind::Directory => RemoteKind::Directory,
                },
                len: meta.len,
                inode: vfsd::path::path_inode_for_policy(path.as_bytes(), caps.case_policy),
            }),
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
        if self.metadata_cache.len() >= vfsd::METADATA_CACHE_MAX_ENTRIES {
            self.metadata_cache.clear();
        }
        self.metadata_cache.insert(key, result);
    }

    fn for_each_dir_entry_from(
        &mut self,
        path: &str,
        cursor: usize,
        mut visit: impl FnMut(usize, &DirEntry) -> Result<bool, i32>,
    ) -> Result<(), i32> {
        if path == "/dev" || path == "/dev/input" || path == "/dev/dri" {
            let entries = devmgrd_dir_entries(path)?;
            for (index, entry) in entries.iter().enumerate().skip(cursor) {
                if !visit(index, entry)? {
                    break;
                }
            }
            return Ok(());
        }
        if path == "/proc" {
            let proc_entries = [
                ("filesystems", RemoteKind::File),
                ("meminfo", RemoteKind::File),
                ("mounts", RemoteKind::File),
                ("self", RemoteKind::Directory),
                ("uptime", RemoteKind::File),
            ];
            for (index, (name, kind)) in proc_entries.iter().enumerate().skip(cursor) {
                let entry = DirEntry::new(name, *kind);
                if !visit(index, &entry)? {
                    break;
                }
            }
            return Ok(());
        }
        if let Some(children) = vfsd::proc_fs::sys_dir_children(path) {
            for (index, name) in children.iter().enumerate().skip(cursor) {
                let child_path = format!("{path}/{name}");
                let kind = if vfsd::proc_fs::sys_virtual_file(child_path.as_str()).is_some() {
                    RemoteKind::File
                } else {
                    RemoteKind::Directory
                };
                let entry = DirEntry::new(name, kind);
                if !visit(index, &entry)? {
                    break;
                }
            }
            return Ok(());
        }
        if path.starts_with("/sys/") {
            return Err(ENOENT);
        }
        if self.run_dirs.contains(path) {
            for (index, name) in self.run_dirs.children(path).into_iter().enumerate().skip(cursor) {
                let entry = DirEntry::new(name, RemoteKind::Directory);
                if !visit(index, &entry)? {
                    break;
                }
            }
            return Ok(());
        }
        if path.starts_with("/run/") {
            return Err(ENOENT);
        }
        self.invalidate_caches_if_remounted();
        let (mount_id, rel_path, key, _pk, caps, _imm, volume, _rms) = self.resolve_mount(path)?;
        let mut base_index = 0usize;
        const ROOT_VIRTUAL: [(&str, RemoteKind); 4] = [
            ("dev", RemoteKind::Directory),
            ("proc", RemoteKind::Directory),
            ("run", RemoteKind::Directory),
            ("sys", RemoteKind::Directory),
        ];
        if path == "/" {
            for (index, (name, kind)) in ROOT_VIRTUAL.iter().enumerate().skip(cursor) {
                let entry = DirEntry::new(name, *kind);
                if !visit(index, &entry)? {
                    return Ok(());
                }
            }
            base_index = ROOT_VIRTUAL.len();
        }
        let submounts: Vec<String> = self
            .mounts
            .submount_names_at(path)
            .into_iter()
            .map(String::from)
            .collect();
        for (offset, name) in submounts.iter().enumerate() {
            let idx = base_index + offset;
            if idx < cursor {
                continue;
            }
            let entry = DirEntry::new(name.as_str(), RemoteKind::Directory);
            if !visit(idx, &entry)? {
                return Ok(());
            }
        }
        base_index += submounts.len();

        if !self.dir_entries_cache.contains_key(&key) {
            let fs_entries = volume.read_dir(&rel_path).map_err(vfsd::map_fs_error)?;
            let resolved: Vec<DirEntry> = fs_entries.into_iter().map(DirEntry::from_fs).collect();
            if self.dir_entries_cache.len() >= vfsd::DIR_ENTRIES_CACHE_MAX_ENTRIES {
                self.dir_entries_cache.clear();
            }
            self.dir_entries_cache.insert(key.clone(), resolved);
        }
        let cached = self.dir_entries_cache.get(&key).ok_or(EIO)?;
        let name_matches = |a: &str, b: &str| match caps.case_policy {
            storage_core::fs::CasePolicy::Exact => a == b,
            storage_core::fs::CasePolicy::AsciiInsensitive
            | storage_core::fs::CasePolicy::UnicodePreservingInsensitive => {
                a.eq_ignore_ascii_case(b)
            }
        };
        let visible: Vec<&DirEntry> = cached
            .iter()
            .filter(|entry| {
                if path == "/"
                    && (ROOT_VIRTUAL.iter().any(|(v, _)| name_matches(&entry.name, v))
                        || submounts.iter().any(|s| name_matches(&entry.name, s)))
                {
                    return false;
                }
                let child_rel = if rel_path.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{rel_path}/{}", entry.name)
                };
                let child_key = cache_key_for_mount(mount_id, caps.case_policy, &child_rel);
                !self.open_nodes.get(&child_key).is_some_and(|n| n.pending_unlink)
            })
            .collect();
        let skip_vol = cursor.saturating_sub(base_index);
        for (offset, entry) in visible.into_iter().enumerate().skip(skip_vol) {
            if !visit(base_index + offset, entry)? {
                break;
            }
        }
        Ok(())
    }
}
