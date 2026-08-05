// SPDX-License-Identifier: MIT

impl VfsState {
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
        if path.is_empty() || path.len() > VFS_IPC_PATH_CAPACITY {
            return Err(EINVAL);
        }
        let base = if path.starts_with('/') {
            "/".to_string()
        } else if is_at_fdcwd(dirfd) {
            if let Some(cwd) = self.cwd.get(&pid) {
                cwd.clone()
            } else {
                let mut clean_relative = !path.starts_with('/');
                for component in path.split('/') {
                    if component.is_empty() || component == "." || component == ".." {
                        clean_relative = false;
                        break;
                    }
                }
                if clean_relative {
                    let mut resolved = String::with_capacity(path.len() + 1);
                    unsafe {
                        let bytes = resolved.as_mut_vec();
                        bytes.push(b'/');
                        for byte in path.as_bytes() {
                            bytes.push(*byte);
                        }
                    }
                    return Ok(resolved);
                }
                return normalize_absolute_path("/", path);
            }
        } else {
            let base_handle_id = if request.remote_id != 0 {
                request.remote_id
            } else {
                dirfd
            };
            let handle = self.handles.get(&base_handle_id).ok_or(EBADF)?;
            if handle.kind != RemoteKind::Directory {
                return Err(ENOTDIR);
            }
            handle.path.clone()
        };
        normalize_absolute_path(base.as_str(), path)
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
                Ok((id, handle))
            } else {
                Err(EINVAL)
            };
        }

        let metadata = lock_vfs_storage().metadata(path).inspect_err(|&errno| {
            if !block::is_transient_storage_not_ready(errno) {
                debug_line(&format!(
                    "vfsd: open failed stage=metadata errno={errno} path={path}"
                ));
            }
        })?;
        if flags & O_DIRECTORY != 0 && metadata.kind != RemoteKind::Directory {
            return Err(ENOTDIR);
        }
        if flags & (O_CREAT | O_TRUNC) != 0 {
            return Err(EROFS);
        }
        let handle = RemoteHandle {
            kind: metadata.kind,
            path: path.to_string(),
            cursor: 0,
            len: metadata.len,
            refs: 1,
            status_flags: flags & !linux_abi::O_CLOEXEC,
            last_mutation: VFSD_OPEN_MUTATION_OPEN,
            last_start: flags,
            last_result: 0,
        };
        self.checkpoint_open_description(request, id, &handle)
            .inspect_err(|&errno| {
                debug_line(&format!(
                    "vfsd: open failed stage=checkpoint errno={errno} path={path}"
                ));
            })?;
        if self.handles.insert(id, handle.clone()).is_some() {
            debug_line(&format!(
                "vfsd: open failed stage=install errno={EIO} path={path}"
            ));
            return Err(EIO);
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
            if handle.kind != RemoteKind::File {
                return Err(EISDIR);
            }
            (
                handle.path.clone(),
                offset.unwrap_or(handle.cursor),
                handle.len,
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
            if requested as usize != len
                || replay_len as usize > len
                || handle.last_start > file_len
            {
                return Err(EIO);
            }
            if replay_len == 0 {
                return Ok(0);
            }
            let replay_len = replay_len as usize;
            let read = lock_vfs_storage().read_file_slice_into(
                path.as_str(),
                file_len,
                handle.last_start,
                &mut dest[..replay_len],
            )?;
            return (read == replay_len).then_some(read).ok_or(EIO);
        }
        if offset.is_none() && self.handles.get(&id).is_some_and(cursor_mutation_prepared) {
            return Err(EBUSY);
        }
        let available = file_len.saturating_sub(start);
        let len = len.min(available as usize).min(dest.len());
        let read = if len == 0 {
            0
        } else {
            lock_vfs_storage().read_file_slice_into(path.as_str(), file_len, start, &mut dest[..len])?
        };
        if offset.is_none() {
            let mut candidate = self.handles.get(&id).cloned().ok_or(EBADF)?;
            candidate.cursor = candidate.cursor.checked_add(read as u64).ok_or(EOVERFLOW)?;
            candidate.last_mutation = VFSD_OPEN_MUTATION_READ;
            candidate.last_start = start;
            candidate.last_result = pack_u32_pair(len, read)?;
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

}

impl VfsStorage {
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
                if let Some(bytes) = self.file_bytes_cache.get(path) {
                    return Ok(copy_file_cache_range(bytes, start, dest));
                }

                let cacheable_len = should_materialize_file_cache(file_len, start, dest.len());
                if let Some(expected_len) = cacheable_len {
                    let bytes = self
                        .volume()?
                        .read_file_to_vec(path)
                        .map_err(map_fat_error)?;
                    if bytes.len() != expected_len {
                        return Err(EIO);
                    }
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
                    self.file_bytes_cache.insert(path.to_string(), bytes);
                    return Ok(read);
                }

                self.volume()?
                    .read_file_range_into(path, start, dest)
                    .map_err(map_fat_error)
            }
        }
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
            || request.max_bytes > EXECUTABLE_SNAPSHOT_MAX_BYTES
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
        let path = normalize_absolute_path("/", raw_path)?;
        let early_system_owned = early_system::file_len(path.as_str())?.is_some();

        self.invalidate_caches_if_remounted();
        if let Some(snapshot) = self.executable_snapshot_cache.get(path.as_str()).copied() {
            return Ok(ExecutableSnapshotAdmission::Cached(ExecutableSnapshotOpen {
                fd: snapshot.fd,
                file_bytes: snapshot.file_bytes,
                close_after_reply: false,
            }));
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
        Ok(ExecutableSnapshotAdmission::Read(ExecutableSnapshotPlan {
            path,
            file_len,
            metadata_len: metadata.len,
            verbose: !early_system_owned || product_interactive_snapshot,
            mount_generation: self.mount_generation,
        }))
    }

    /// Reads one bounded chunk of a planned snapshot.
    ///
    /// The caller re-acquires storage per chunk so a namespace request waits at
    /// most one chunk, never a whole file.
    fn read_executable_snapshot_chunk(
        &mut self,
        plan: &ExecutableSnapshotPlan,
        offset: u64,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        self.read_file_slice_into(plan.path.as_str(), plan.metadata_len, offset, dest)
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
        bytes: &[u8],
    ) -> Result<ExecutableSnapshotOpen, i32> {
        self.invalidate_caches_if_remounted();
        if !vfsd::snapshot_plan_is_current(plan.mount_generation, self.mount_generation) {
            return Err(EAGAIN);
        }
        let path = plan.path.clone();
        let file_len = plan.file_len;
        let fd = create_terminally_sealed_snapshot(path.as_str(), bytes)?;
        if plan.verbose {
            ipc::debug_line(executable_snapshot_marker(path.as_str(), file_len).as_str());
            let _ = ipc::product_milestone(
                PRODUCT_MILESTONE_EXECUTABLE_SNAPSHOT_SEALED,
                file_len as u64,
                0,
            );
        }

        if file_len > EXECUTABLE_SNAPSHOT_CACHE_BUDGET_BYTES {
            return Ok(ExecutableSnapshotOpen {
                fd,
                file_bytes: plan.metadata_len,
                close_after_reply: true,
            });
        }
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
            path,
            ExecutableSnapshot {
                fd,
                file_bytes: plan.metadata_len,
            },
        );
        self.executable_snapshot_cache_bytes += file_len;
        Ok(ExecutableSnapshotOpen {
            fd,
            file_bytes: plan.metadata_len,
            close_after_reply: false,
        })
    }

}

impl VfsState {
    fn render_getdents_payload(
        &mut self,
        id: u64,
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
        let entries = lock_vfs_storage().dir_entries(path.as_str())?;
        let mut written = 0usize;
        let mut consumed = 0usize;
        for (index, entry) in entries.iter().enumerate().skip(cursor) {
            let record = encode_dirent(entry, index + 1);
            if written + record.len() > user_len.min(payload.len()) {
                if written == 0 {
                    return Err(EINVAL);
                }
                break;
            }
            payload[written..written + record.len()].copy_from_slice(record.as_slice());
            written += record.len();
            consumed += 1;
        }
        Ok((written, consumed))
    }

}

impl VfsStorage {
    fn metadata(&mut self, path: &str) -> Result<Metadata, i32> {
        if path == "/" || path == "/proc" || path == "/run" {
            return Ok(Metadata {
                kind: RemoteKind::Directory,
                len: 0,
                inode: path_inode(path.as_bytes()),
            });
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
        if let Some(entry) = self.metadata_cache.get(path) {
            return *entry;
        }
        if let Some(len) = early_system::file_len(path)? {
            let metadata = Metadata {
                kind: RemoteKind::File,
                len,
                inode: path_inode(path.as_bytes()),
            };
            self.metadata_cache.insert(path.to_string(), Ok(metadata));
            return Ok(metadata);
        }
        let result = match self.volume()?.metadata(path) {
            Ok(meta) => Ok(Metadata {
                kind: match meta.kind {
                    FatNodeKind::File => RemoteKind::File,
                    FatNodeKind::Directory => RemoteKind::Directory,
                },
                len: meta.len,
                inode: path_inode(path.as_bytes()),
            }),
            Err(err) => Err(map_fat_error(err)),
        };
        if result.is_ok()
            || result
                .as_ref()
                .is_err_and(|errno| cacheable_metadata_errno(*errno))
        {
            self.metadata_cache.insert(path.to_string(), result);
        }
        result
    }

    fn dir_entries(&mut self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let mut entries = Vec::new();
        if path == "/" {
            entries.push(DirEntry::new("dev", RemoteKind::Directory));
            entries.push(DirEntry::new("proc", RemoteKind::Directory));
            entries.push(DirEntry::new("run", RemoteKind::Directory));
        }
        if path == "/dev" || path == "/dev/input" || path == "/dev/dri" {
            match devmgrd_dir_entries(path) {
                Ok(entries) => return Ok(entries),
                Err(errno) => return Err(errno),
            }
        }
        if path == "/proc" || path == "/run" {
            return Ok(entries);
        }
        self.invalidate_caches_if_remounted();
        if let Some(cached) = self.dir_entries_cache.get(path) {
            entries.extend_from_slice(cached);
            return Ok(entries);
        }
        let fat_entries = self.volume()?.read_dir(path).map_err(map_fat_error)?;
        let resolved: Vec<DirEntry> = fat_entries.into_iter().map(DirEntry::from_fat).collect();
        self.dir_entries_cache
            .insert(path.to_string(), resolved.clone());
        entries.extend(resolved);
        Ok(entries)
    }

    fn volume(&mut self) -> Result<&FatVolume<BootBlockDevice>, i32> {
        if self.volume.is_none() {
            let device = BootBlockDevice::open().inspect_err(|&errno| {
                if !block::is_transient_storage_not_ready(errno) {
                    debug_line(&format!(
                        "vfsd: volume unavailable stage=block-info errno={errno}"
                    ));
                }
            })?;
            self.volume = Some(
                FatVolume::new(device)
                    .map_err(map_fat_error)
                    .inspect_err(|&errno| {
                        debug_line(&format!(
                            "vfsd: volume unavailable stage=fat-admission errno={errno}"
                        ));
                    })?,
            );
        }
        Ok(self.volume.as_ref().expect("volume initialized"))
    }
}
