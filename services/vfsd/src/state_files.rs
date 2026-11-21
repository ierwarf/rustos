// SPDX-License-Identifier: MIT

impl VfsState {
    fn vfs_openat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        // Absolute paths are canonicalized too: the open-description path is
        // the key for device routing, caches and checkpoint replay, so a raw
        // `/dev/../x` or `//x` must never become a handle path.
        // `O_NOFOLLOW` and `O_CREAT|O_EXCL` keep a trailing symlink as the
        // opened (or colliding) name instead of following it.
        const O_NOFOLLOW: u64 = 0o400_000;
        let follow_last = request.arg0 & O_NOFOLLOW == 0
            && request.arg0 & (linux_abi::O_CREAT | linux_abi::O_EXCL)
                != (linux_abi::O_CREAT | linux_abi::O_EXCL);
        let resolved_path =
            match self.resolve_path_follow(request, request.dirfd, path, follow_last) {
                Ok(path) => path,
                Err(errno) => {
                    response.status = errno;
                    return;
                }
            };
        let path = resolved_path.as_str();
        // Kernel-fact `/proc/<pid>` nodes have no vfsd-side description: the
        // kernel builds a snapshot for the caller and holds it locally.
        if let Some(Ok(node)) = vfsd::proc_fs::lookup(path, request.pid) {
            if let Some(aux) = vfsd::proc_fs::open_delegate_for_flags(node, request.arg0) {
                match vfsd::proc_fs::delegated_open_errno(node, request.arg0) {
                    Some(errno) => response.status = errno,
                    None => response.aux = aux,
                }
                return;
            }
        }
        match self.open_remote_checkpointed(request, path, request.arg0) {
            Ok((id, handle)) => {
                response.remote_id = id;
                response.handle_kind = handle_kind_u16(handle.kind);
                response.aux = device_access_for_path(handle.path.as_str());
                write_vfs_payload_bytes(response, handle.path.as_bytes());
                // After the payload write: it sets `value` to the payload
                // length, which used to leave every open description's length
                // equal to its path length.
                response.value = handle.len;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_close(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let key = checkpoint_handle_key(request.remote_id);
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            let Some(record) = self.checkpoint_records.get(&key).copied() else {
                response.status = EBADF;
                return;
            };
            if record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0 {
                if (record.operation_hi, record.operation_lo)
                    != (request.operation_hi, request.operation_lo)
                {
                    response.status = EBADF;
                } else if let Err(errno)=self.refund_open_description(request.remote_id) {
                    // The close is already persistent, but a lost refund ACK
                    // may still be pending in the provider's local token book.
                    // A retry with the same close operation is allowed to
                    // settle precisely that receipt, never a second charge.
                    response.status = errno;
                }
                return;
            }
            if let Err(errno) = self.checkpoint_close_description(request, request.remote_id) {
                response.status = errno;
            } else if let Err(errno)=self.refund_open_description(request.remote_id) {
                response.status=errno;
            }
            return;
        };
        if cursor_mutation_prepared(&handle) {
            response.status = EBUSY;
            return;
        }
        match self.checkpoint_close_description(request, request.remote_id) {
            Ok(_) => {
                if let Some(closed) = self.handles.remove(&request.remote_id) {
                    remove_proc_snapshot(request.remote_id);
                    // Final close of the description: its OFD and flock locks.
                    self.locks.release_description(request.remote_id);
                    if closed.node != 0 {
                        lock_vfs_storage().unpin_node(closed.mount_id, closed.node);
                    } else if closed.mount_id != 0 {
                        lock_vfs_storage().note_close_node(closed.path.as_str());
                    }
                }
                // A committed tombstone is a final reference, not just a
                // request to close. The rootd receipt remains replayable.
                if let Err(errno)=self.refund_open_description(request.remote_id){
                    response.status=errno;
                }
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_read(
        &mut self,
        request: &VfsIpcRequest,
        response: &mut VfsIpcResponse,
        offset: Option<u64>,
    ) {
        let len = request.arg1.min(VFS_IPC_PAYLOAD_CAPACITY as u64) as usize;
        match self.read_remote_into(request, offset, len, &mut response.payload) {
            Ok(read) => {
                response.payload_len = read as u32;
                response.value = read as u64;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_lseek(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        if self.checkpoint_operation_replayed(request, checkpoint_handle_key(request.remote_id)) {
            if handle.last_mutation != VFSD_OPEN_MUTATION_LSEEK
                || handle.last_start != request.arg0
                || handle.last_result != request.arg1
            {
                response.status = EIO;
                return;
            }
            response.value = handle.cursor;
            return;
        }
        if cursor_mutation_prepared(&handle) {
            response.status = EBUSY;
            return;
        }
        let file_len = if handle.kind != RemoteKind::File {
            handle.len
        } else if handle.node != 0 {
            match lock_vfs_storage().node_len(handle.mount_id, handle.node) {
                Some(len) => len,
                None => {
                    response.status = ESTALE;
                    return;
                }
            }
        } else {
            lock_vfs_storage().live_len_for_path(handle.path.as_str(), handle.len)
        };
        let next = match checked_seek_position(
            handle.cursor,
            file_len,
            request.arg0 as i64,
            request.arg1,
        ) {
            Ok(next) => next,
            Err(SeekPositionError::InvalidWhence | SeekPositionError::Negative) => {
                response.status = EINVAL;
                return;
            }
            Err(SeekPositionError::Overflow) => {
                response.status = EOVERFLOW;
                return;
            }
        };
        let mut candidate = handle;
        candidate.cursor = next;
        candidate.last_mutation = VFSD_OPEN_MUTATION_LSEEK;
        candidate.last_start = request.arg0;
        candidate.last_result = request.arg1;
        if let Err(errno) = self.checkpoint_handle_state(
            request,
            request.remote_id,
            &candidate,
            VFSD_OPEN_MUTATION_LSEEK,
            request.arg0,
            request.arg1,
        ) {
            response.status = errno;
            return;
        }
        response.value = candidate.cursor;
        self.handles.insert(request.remote_id, candidate);
    }

    /// Serves one bounded chunk of a file to the pager service for the shared
    /// library cache, together with the vfsd identity of that file. Only the
    /// current pager service owner may ask, so a path alone never grants read
    /// access to anyone else; vfsd, not the pager, decides which files are on
    /// the immutable volume (`aux == 1`).
    fn vfs_library_read(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        if rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_PAGERD, request.pid) < 0 {
            response.status = EACCES;
            return;
        }
        let Ok(wanted) = usize::try_from(request.arg1) else {
            response.status = EINVAL;
            return;
        };
        let offset = request.arg0;
        if wanted > VFS_LIBRARY_READ_MAX_BYTES || offset % 4096 != 0 {
            response.status = EINVAL;
            return;
        }
        let Some(raw_path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        if !raw_path.starts_with('/') {
            response.status = EINVAL;
            return;
        }
        let path = match normalize_absolute_path("/", raw_path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let mut storage = lock_vfs_storage();
        let metadata = match storage.metadata(path.as_str()) {
            Ok(metadata) if metadata.kind == RemoteKind::File => metadata,
            Ok(_) => {
                response.status = EISDIR;
                return;
            }
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        const IDENTITY: usize = VfsFileIdentityWire::WIRE_BYTES;
        // Identity first: refusal for a non-volume file is data, not an error,
        // because the pager also reads its own (bootstrap-image) manifest here.
        let (identity, attested) = match storage.file_identity(path.as_str(), metadata.len) {
            Ok(identity) if identity.flags & VFS_FILE_IDENTITY_FLAG_IMMUTABLE_VOLUME != 0 => {
                (identity, 1)
            }
            Ok(_) | Err(EOPNOTSUPP | ENOENT | EIO | EINVAL) => (VfsFileIdentityWire::default(), 0),
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let available = metadata.len.saturating_sub(offset);
        let count = wanted.min(usize::try_from(available).unwrap_or(usize::MAX));
        let read = if count == 0 {
            0
        } else {
            match storage.read_file_slice_into(
                path.as_str(),
                metadata.len,
                offset,
                &mut response.payload[IDENTITY..IDENTITY + count],
            ) {
                Ok(read) => read,
                Err(errno) => {
                    response.status = errno;
                    return;
                }
            }
        };
        response.payload[..IDENTITY].copy_from_slice(&identity.to_bytes());
        response.payload_len = (IDENTITY + read) as u32;
        response.value = metadata.len;
        response.aux = attested;
    }

    /// Reports which immutable-volume file this open description names. Pure
    /// metadata: no cursor, checkpoint or charge state changes, so a lost or
    /// repeated request is harmless.
    fn vfs_file_identity(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id) else {
            response.status = EBADF;
            return;
        };
        if handle.kind != RemoteKind::File {
            response.status = EINVAL;
            return;
        }
        let (path, len) = (handle.path.clone(), handle.len);
        if handle.node != 0 {
            let (mount_id, node) = (handle.mount_id, handle.node);
            let storage = lock_vfs_storage();
            let Some(len) = storage.node_len(mount_id, node) else {
                response.status = ESTALE;
                return;
            };
            let bytes = VfsFileIdentityWire {
                volume: mount_id,
                object: node,
                generation: storage.mount_generation,
                storage_epoch: 0,
                mount_generation: storage.mount_generation,
                len,
                flags: VFS_FILE_IDENTITY_FLAG_MUTABLE_VOLUME,
            }
            .to_bytes();
            response.payload[..bytes.len()].copy_from_slice(&bytes);
            response.payload_len = bytes.len() as u32;
            return;
        }
        match lock_vfs_storage().file_identity(path.as_str(), len) {
            Ok(identity) => {
                let bytes = identity.to_bytes();
                response.payload[..bytes.len()].copy_from_slice(&bytes);
                response.payload_len = bytes.len() as u32;
            }
            Err(errno) => response.status = errno,
        }
    }

    /// `VFS_IPC_OP_SHARED_BACKING`: names the memfd that now holds a pinned
    /// TmpFs or FAT file's bytes so the mapping broker can map it `MAP_SHARED`.
    /// The description, not a path, selects the file, so unlink/rename races
    /// cannot redirect the mapping. A description that cannot write is refused
    /// a writable backing here as well as in the kernel. When
    /// `VFS_SHARED_BACKING_FLAG_SYNC` is set (`remote_id == 0`), flushes the
    /// shared-backed FAT file identified by `arg1` (memfd object id) to disk.
    fn vfs_shared_backing(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        use rustos_user_abi::syscall::{
            VFS_SHARED_BACKING_FLAG_SYNC, VFS_SHARED_BACKING_FLAG_WRITE,
        };
        if request.arg0 == VFS_SHARED_BACKING_FLAG_SYNC && request.remote_id == 0 {
            if let Err(errno) = lock_vfs_storage().sync_shared_backing_by_object(request.arg1) {
                response.status = errno;
            }
            return;
        }
        let Some(handle) = self.handles.get(&request.remote_id) else {
            response.status = EBADF;
            return;
        };
        let (kind, writable, mount_id, node, path) = (
            handle.kind,
            handle.writable,
            handle.mount_id,
            handle.node,
            handle.path.clone(),
        );
        let mut storage = lock_vfs_storage();
        let supports_shared = node != 0 || storage.supports_shared_backing_path(path.as_str());
        if let Err(errno) = vfsd::shared_backing_admission(
            kind == RemoteKind::File,
            request.arg0,
            writable,
            supports_shared,
        ) {
            response.status = errno;
            return;
        }
        let result = if node != 0 {
            storage.shared_backing_node(mount_id, node)
        } else {
            let want_write = request.arg0 & VFS_SHARED_BACKING_FLAG_WRITE != 0;
            storage.shared_backing_path(path.as_str(), want_write)
        };
        match result {
            Ok(backing) => {
                response.value = backing.token;
                response.aux = backing.object;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_fstat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id) else {
            response.status = EBADF;
            return;
        };
        if handle.node != 0 {
            match lock_vfs_storage().node_metadata(handle.mount_id, handle.node) {
                Ok(metadata) => {
                    response.payload_len = LINUX_STAT_SIZE as u32;
                    response.payload[..LINUX_STAT_SIZE]
                        .copy_from_slice(&build_linux_stat(metadata));
                }
                Err(errno) => response.status = errno,
            }
            return;
        }
        if has_proc_snapshot(request.remote_id) {
            if let Some(Ok(node)) = vfsd::proc_fs::lookup(handle.path.as_str(), request.pid) {
                let stat = vfsd::proc_fs::node_stat(node, request.pid, 0);
                response.payload_len = LINUX_STAT_SIZE as u32;
                response.payload[..LINUX_STAT_SIZE].copy_from_slice(&stat);
                return;
            }
        }
        let (len, inode) = if handle.kind == RemoteKind::File {
            lock_vfs_storage().live_stat_for_path(handle.path.as_str(), handle.len)
        } else {
            (handle.len, path_inode(handle.path.as_bytes()))
        };
        let mut metadata = Metadata::plain(handle.kind, len, inode);
        if handle.kind == RemoteKind::Device {
            metadata.rdev = device_rdev_for_path(handle.path.as_str());
        }
        let stat = build_linux_stat(metadata);
        response.payload_len = LINUX_STAT_SIZE as u32;
        response.payload[..LINUX_STAT_SIZE].copy_from_slice(&stat);
    }

    fn vfs_getdents64(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        let requested = request.arg1.min(response.payload.len() as u64) as usize;
        let replayed =
            self.checkpoint_operation_replayed(request, checkpoint_handle_key(request.remote_id));
        if cursor_mutation_prepared(&handle) && !replayed {
            response.status = EBUSY;
            return;
        }
        let start = if replayed {
            if handle.checkpoint_version != VFSD_OPEN_CHECKPOINT_VERSION
                || handle.last_mutation != VFSD_OPEN_MUTATION_GETDENTS
                || vfsd::directory::checkpoint_request_size(handle.last_result) != requested
            {
                response.status = EIO;
                return;
            }
            handle.last_start
        } else {
            handle.cursor
        };
        let (written, end) = match self.render_getdents_payload(
            request.remote_id,
            request.pid,
            start,
            requested,
            &mut response.payload,
        ) {
            Ok(result) => result,
            Err(errno) => {
                // The exact prepared reply is no longer reproducible. The
                // compat caller cancels this operation and restores `start`.
                response.status = if replayed { EIO } else { errno };
                return;
            }
        };
        let bytes = &response.payload[..written];
        if replayed {
            if !vfsd::directory::replay_matches(handle.last_result, handle.cursor, requested, end, bytes) {
                response.status = EIO;
                return;
            }
        } else {
            let Some(result) = vfsd::directory::checkpoint_reply(requested, bytes) else {
                response.status = EOVERFLOW;
                return;
            };
            let mut candidate = handle;
            candidate.cursor = end;
            candidate.checkpoint_version = VFSD_OPEN_CHECKPOINT_VERSION;
            candidate.last_mutation = VFSD_OPEN_MUTATION_GETDENTS;
            candidate.last_start = start;
            candidate.last_result = result;
            if let Err(errno) = self.checkpoint_handle_state(
                request,
                request.remote_id,
                &candidate,
                VFSD_OPEN_MUTATION_GETDENTS,
                start,
                candidate.last_result,
            ) {
                response.status = errno;
                return;
            }
            self.handles.insert(request.remote_id, candidate);
        }
        response.payload_len = written as u32;
        response.value = written as u64;
    }

    fn vfs_fcntl(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        const F_SETFL_MUTABLE_MASK: u64 = linux_abi::O_APPEND | linux_abi::O_NONBLOCK;
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        if cursor_mutation_prepared(&handle)
            && !self
                .checkpoint_operation_replayed(request, checkpoint_handle_key(request.remote_id))
        {
            response.status = EBUSY;
            return;
        }
        match request.arg0 {
            // Private VFS operation, not a Linux fcntl command. The kernel
            // supplies an existing funded description and a fresh remote id.
            0x5255_0001 => self.vfs_reopen_description(request, response, &handle),
            linux_abi::F_GETFL => response.value = handle.status_flags,
            linux_abi::F_SETFL => {
                if self.checkpoint_operation_replayed(
                    request,
                    checkpoint_handle_key(request.remote_id),
                ) {
                    if handle.last_mutation != VFSD_OPEN_MUTATION_FCNTL
                        || handle.last_start != request.arg0
                        || handle.last_result != request.arg1
                    {
                        response.status = EIO;
                        return;
                    }
                    response.value = handle.status_flags;
                    return;
                }
                let mut candidate = handle;
                candidate.status_flags = (candidate.status_flags & !F_SETFL_MUTABLE_MASK)
                    | (request.arg1 & F_SETFL_MUTABLE_MASK);
                candidate.append = (candidate.status_flags & linux_abi::O_APPEND) != 0;
                candidate.last_mutation = VFSD_OPEN_MUTATION_FCNTL;
                candidate.last_start = request.arg0;
                candidate.last_result = request.arg1;
                if let Err(errno) = self.checkpoint_handle_state(
                    request,
                    request.remote_id,
                    &candidate,
                    VFSD_OPEN_MUTATION_FCNTL,
                    request.arg0,
                    request.arg1,
                ) {
                    response.status = errno;
                    return;
                }
                response.value = candidate.status_flags;
                self.handles.insert(request.remote_id, candidate);
            }
            _ => response.status = EINVAL,
        }
    }

    fn vfs_reopen_description(
        &mut self,
        request: &VfsIpcRequest,
        response: &mut VfsIpcResponse,
        source: &RemoteHandle,
    ) {
        let flags = request.arg1;
        let id = request.arg2;
        let allowed = linux_abi::O_ACCMODE | linux_abi::O_APPEND | linux_abi::O_NONBLOCK
            | linux_abi::O_CLOEXEC | linux_abi::O_DIRECTORY | 0o100000;
        if id == 0 || id == request.remote_id || request.arg3 != 0 || flags & !allowed != 0 {
            response.status = EINVAL;
            return;
        }
        let access = flags & linux_abi::O_ACCMODE;
        let original = source.status_flags & linux_abi::O_ACCMODE;
        if access > linux_abi::O_RDWR
            || (access != linux_abi::O_WRONLY && original == linux_abi::O_WRONLY)
            || (access != linux_abi::O_RDONLY && original == linux_abi::O_RDONLY)
        {
            response.status = EACCES;
            return;
        }
        if source.node == 0 {
            response.status = EOPNOTSUPP;
            return;
        }
        let metadata = match lock_vfs_storage().node_metadata(source.mount_id, source.node) {
            Ok(metadata) => metadata,
            Err(errno) => { response.status = errno; return; }
        };
        let needed = (if access != linux_abi::O_WRONLY { 4 } else { 0 })
            | (if access != linux_abi::O_RDONLY { 2 } else { 0 });
        if let Err(errno) = check_metadata_permission(metadata, request, needed, false) {
            response.status = errno; return;
        }
        // Checkpoint records encode OPEN's flags in arg0; retain the same
        // suboperation token while using the requested flags for this record.
        let mut open_request = *request;
        open_request.arg0 = flags;
        let result = if let Some(existing) = self.handles.get(&id).cloned() {
            let chunks = existing.path.len().div_ceil(SERVICE_CHECKPOINT_VALUE_CAPACITY);
            let operation = checkpoint_suboperation(&open_request, chunks as u64 + 1);
            let record = self.checkpoint_records.get(&checkpoint_handle_key(id));
            if existing.stale || existing.mount_id != source.mount_id || existing.node != source.node
                || existing.status_flags != flags & !linux_abi::O_CLOEXEC
                || record.is_none_or(|r| (r.operation_hi, r.operation_lo) != operation)
            {
                Err(EINVAL)
            } else {
                Ok((id, existing))
            }
        } else {
            let retained = lock_vfs_storage().node_volume(source.mount_id)
                .and_then(|volume| volume.retain_node_id(source.node).map_err(vfsd::map_fs_error));
            match retained {
                Err(errno) => Err(errno),
                Ok(()) => {
                    let opened = self.publish_open_handle(&open_request, id, &source.path, flags,
                        metadata, (source.mount_id, source.node), false, false);
                    if opened.is_err() {
                        lock_vfs_storage().unpin_node(source.mount_id, source.node);
                    }
                    opened
                }
            }
        };
        match result {
            Ok((id, handle)) => {
                response.remote_id = id;
                response.handle_kind = handle_kind_u16(handle.kind);
                response.aux = device_access_for_path(&handle.path);
                write_vfs_payload_bytes(response, handle.path.as_bytes());
                response.value = handle.len;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_cursor_settle(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        if self.checkpoint_operation_replayed(request, checkpoint_handle_key(request.remote_id)) {
            if handle.last_mutation != VFSD_OPEN_MUTATION_STABLE {
                response.status = EIO;
            } else {
                response.value = handle.cursor;
            }
            return;
        }
        if !cursor_mutation_prepared(&handle)
            || self
                .checkpoint_operations
                .get(&checkpoint_handle_key(request.remote_id))
                .copied()
                != Some((request.arg0, request.arg1))
            || !matches!(
                request.arg2,
                VFS_CURSOR_SETTLE_COMMIT | VFS_CURSOR_SETTLE_CANCEL
            )
            || request.arg3 != 0
            || request.path_len != 0
            || request.payload_len != 0
        {
            response.status = EINVAL;
            return;
        }
        let mut candidate = handle;
        if request.arg2 == VFS_CURSOR_SETTLE_CANCEL {
            candidate.cursor = candidate.last_start;
        }
        candidate.last_mutation = VFSD_OPEN_MUTATION_STABLE;
        candidate.last_start = 0;
        candidate.last_result = 0;
        if let Err(errno) = self.checkpoint_handle_state(
            request,
            request.remote_id,
            &candidate,
            VFSD_OPEN_MUTATION_STABLE,
            0,
            0,
        ) {
            response.status = errno;
            return;
        }
        response.value = candidate.cursor;
        self.handles.insert(request.remote_id, candidate);
    }

    fn vfs_checkpoint_ack(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Ok(original_op) = u16::try_from(request.arg2) else {
            response.status = EINVAL;
            return;
        };
        if (request.arg0 == 0 && request.arg1 == 0)
            || request.fd != 0
            || request.path_len != 0
            || request.payload_len != 0
            || !matches!(original_op, VFS_IPC_OP_CLOSE | VFS_IPC_OP_POLL_QUERY)
            || (original_op == VFS_IPC_OP_CLOSE && request.arg3 != 0)
            || (original_op == VFS_IPC_OP_POLL_QUERY
                && !matches!(
                    request.arg3,
                    VFS_POLL_QUERY_EPOLL_RETIRE
                        | VFS_POLL_QUERY_EPOLL_CTL
                        | VFS_POLL_QUERY_EPOLL_PURGE_OBJECT
                ))
        {
            response.status = EINVAL;
            return;
        }
        let proofs = self
            .checkpoint_records
            .values()
            .copied()
            .filter(|record| {
                record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0
                    && (record.operation_hi, record.operation_lo) == (request.arg0, request.arg1)
                    && (original_op != VFS_IPC_OP_CLOSE
                        || (record.parent_hi == 0
                            && record.parent_lo == 0
                            && record.key_hi == request.remote_id
                            && record.key_lo == VFSD_CHECKPOINT_HANDLE_TAG))
            })
            .collect::<Vec<_>>();
        for proof in proofs {
            if let Err(errno) = self.compact_checkpoint_proof(proof) {
                response.status = errno;
                return;
            }
        }
    }

    fn vfs_path_statx(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path_follow(request, request.dirfd, path, request.flags & 0x100 == 0) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        if self.proc_path_stat(request, path.as_str(), true, response) {
            return;
        }
        match lock_vfs_storage().metadata(path.as_str()) {
            Ok(metadata) => {
                let statx = build_linux_statx(metadata);
                response.payload_len = LINUX_STATX_SIZE as u32;
                response.payload[..LINUX_STATX_SIZE].copy_from_slice(&statx);
                if request.flags & rustos_user_abi::syscall::VFS_REQUEST_FLAG_NETD_SOCKET_NODE != 0 {
                    if let Some(fs) = metadata.fs {
                        response.value = metadata.dev;
                        response.aux = fs.node_id;
                    }
                }
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_path_stat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path_follow(request, request.dirfd, path, request.flags & 0x100 == 0) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        if self.proc_path_stat(request, path.as_str(), false, response) {
            return;
        }
        match lock_vfs_storage().metadata(path.as_str()) {
            Ok(metadata) => {
                let stat = build_linux_stat(metadata);
                response.payload_len = LINUX_STAT_SIZE as u32;
                response.payload[..LINUX_STAT_SIZE].copy_from_slice(&stat);
                if request.flags & rustos_user_abi::syscall::VFS_REQUEST_FLAG_NETD_SOCKET_NODE != 0 {
                    if let Some(fs) = metadata.fs {
                        response.value = metadata.dev;
                        response.aux = fs.node_id;
                    }
                }
            }
            Err(errno) => response.status = errno,
        }
    }

    fn resolve_proc_exe_target(
        &self,
        request: &VfsIpcRequest,
        node: vfsd::proc_fs::ProcNode,
    ) -> Result<String, i32> {
        let exec = vfsd::proc_fs::attached_exec_path(&request.payload, request.payload_len as usize);
        match vfsd::proc_fs::link_target(node, request.pid, exec)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|target| normalize_absolute_path("/", target.as_str()))
        {
            Some(Ok(target)) => Ok(target),
            Some(Err(errno)) => Err(errno),
            None => Err(ENOENT),
        }
    }

    fn vfs_access(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else { response.status = EINVAL; return; };
        if request.arg0 & !7 != 0 || request.flags & !(0x200 | 0x100) != 0 { response.status = EINVAL; return; }
        let real = request.flags & 0x200 == 0;
        let mut traversal_request = *request;
        if real { traversal_request.euid = request.uid; traversal_request.egid = request.gid; }
        let path = match self.resolve_path_follow(&traversal_request, request.dirfd, path, request.flags & 0x100 == 0) {
            Ok(path) => path, Err(errno) => { response.status = errno; return; }
        };
        match vfsd::proc_fs::lookup(&path, request.pid) {
            Some(Ok(vfsd::proc_fs::ProcNode::ExeLink)) => {
                response.status = self.resolve_proc_exe_target(request, vfsd::proc_fs::ProcNode::ExeLink)
                    .and_then(|target| lock_vfs_storage().metadata(&target))
                    .and_then(|m| check_metadata_permission(m, request, request.arg0 as u32, real))
                    .err().unwrap_or(0);
            }
            Some(Ok(node)) => { if let Some(aux) = vfsd::proc_fs::kernel_only_delegate(node) { response.aux = aux; } },
            Some(Err(errno)) => response.status = errno,
            None => response.status = lock_vfs_storage().metadata(&path)
                .and_then(|m| check_metadata_permission(m, request, request.arg0 as u32, real)).err().unwrap_or(0),
        }
    }

    /// Serves `/proc` nodes for `newfstatat`/`statx`. Returns true when the
    /// path belongs to procfs and `response` is filled (success or errno).
    fn proc_path_stat(
        &mut self,
        request: &VfsIpcRequest,
        path: &str,
        statx: bool,
        response: &mut VfsIpcResponse,
    ) -> bool {
        use vfsd::proc_fs::{self, ProcNode};
        const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
        let node = match proc_fs::lookup(path, request.pid) {
            None => return false,
            Some(Err(errno)) => {
                response.status = errno;
                return true;
            }
            Some(Ok(node)) => node,
        };
        if let Some(aux) = proc_fs::kernel_only_delegate(node) {
            response.aux = aux;
            return true;
        }
        let exec = proc_fs::attached_exec_path(&request.payload, request.payload_len as usize);
        let follow = request.flags & AT_SYMLINK_NOFOLLOW == 0;
        if let (true, ProcNode::NsLink(index)) = (follow, node) {
            if statx {
                response.payload_len = LINUX_STATX_SIZE as u32;
                response.payload[..LINUX_STATX_SIZE]
                    .copy_from_slice(&proc_fs::ns_target_statx(index));
            } else {
                response.payload_len = LINUX_STAT_SIZE as u32;
                response.payload[..LINUX_STAT_SIZE]
                    .copy_from_slice(&proc_fs::ns_target_stat(index));
            }
            return true;
        }
        if follow && node == ProcNode::ExeLink {
            // Stat the target as an ordinary path; its own metadata is real.
            let target = match self.resolve_proc_exe_target(request, node) {
                Ok(target) => target,
                Err(errno) => {
                    response.status = errno;
                    return true;
                }
            };
            match lock_vfs_storage().metadata(target.as_str()) {
                Ok(metadata) if statx => {
                    response.payload_len = LINUX_STATX_SIZE as u32;
                    response.payload[..LINUX_STATX_SIZE]
                        .copy_from_slice(&build_linux_statx(metadata));
                }
                Ok(metadata) => {
                    response.payload_len = LINUX_STAT_SIZE as u32;
                    response.payload[..LINUX_STAT_SIZE]
                        .copy_from_slice(&build_linux_stat(metadata));
                }
                Err(errno) => response.status = errno,
            }
            return true;
        }
        let (shown, size) = match node {
            // `/proc/self` followed is the caller's pid directory.
            ProcNode::SelfLink if follow => (ProcNode::PidDir, 0),
            ProcNode::PidDir => (ProcNode::PidDir, 0),
            ProcNode::NsDir => (ProcNode::NsDir, 0),
            // Static kernel-fact files report size 0 like procfs; the
            // kernel-only subtrees were delegated above.
            kernel @ ProcNode::Kernel(..) => (kernel, 0),
            file if proc_fs::is_virtual_file(file) => {
                match lock_vfs_storage().proc_virtual_file_bytes(file) {
                    Ok(bytes) => (file, bytes.len() as u64),
                    Err(errno) => {
                        response.status = errno;
                        return true;
                    }
                }
            }
            link => match proc_fs::link_target(link, request.pid, exec) {
                Ok(target) => (link, target.len() as u64),
                Err(errno) => {
                    response.status = errno;
                    return true;
                }
            },
        };
        if statx {
            response.payload_len = LINUX_STATX_SIZE as u32;
            response.payload[..LINUX_STATX_SIZE]
                .copy_from_slice(&proc_fs::node_statx(shown, request.pid, size));
        } else {
            response.payload_len = LINUX_STAT_SIZE as u32;
            response.payload[..LINUX_STAT_SIZE]
                .copy_from_slice(&proc_fs::node_stat(shown, request.pid, size));
        }
        true
    }

    /// `readlinkat` on `/proc` nodes; every other path is not a symlink.
    fn vfs_readlinkat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path_follow(request, request.dirfd, path, false) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let node = match vfsd::proc_fs::lookup(path.as_str(), request.pid) {
            Some(Ok(node)) => node,
            Some(Err(errno)) => {
                response.status = errno;
                return;
            }
            // Not a procfs node: existing paths are not symlinks (EINVAL),
            // missing ones are ENOENT.
            None => {
                // Memory-filesystem symlinks are read through the volume;
                // anything else that exists is not a link (EINVAL).
                let mut storage = lock_vfs_storage();
                if storage.tmpfs_owns(path.as_str()) {
                    match storage.readlink_path(path.as_str()) {
                        Ok(target) => write_vfs_payload_bytes(response, target.as_bytes()),
                        Err(errno) => response.status = errno,
                    }
                    return;
                }
                response.status = match storage.metadata(path.as_str()) {
                    Ok(_) => EINVAL,
                    Err(errno) => errno,
                };
                return;
            }
        };
        if let Some(aux) = vfsd::proc_fs::kernel_only_delegate(node) {
            response.aux = aux;
            return;
        }
        let exec = vfsd::proc_fs::attached_exec_path(&request.payload, request.payload_len as usize);
        match vfsd::proc_fs::link_target(node, request.pid, exec) {
            Ok(target) => {
                // readlink(2) truncates silently to the caller's buffer; the
                // kernel front-end applies the user length bound.
                write_vfs_payload_bytes(response, &target);
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_getcwd(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let cwd = self.cwd_for_pid(request.pid);
        write_vfs_payload_bytes(response, cwd.as_bytes());
    }

    fn vfs_chdir(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path_follow(request, request.dirfd, path, true) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let result = lock_vfs_storage().metadata(&path).and_then(|m| check_metadata_permission(m, request, 1, false));
        if let Err(errno) = result { response.status = errno; return; }
        response.status = self.chdir(request.pid, path.as_str());
    }

    fn vfs_mkdir(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path_follow(request, request.dirfd, path, false) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let attrs = FsCreateAttrs::new(request.arg0 as u32, request.euid, request.egid);
        let mut storage = lock_vfs_storage();
        response.status = storage.mkdir_path(&path, attrs, request);
    }

    fn linux_mount_vfs(&mut self, response: &mut VfsIpcResponse) {
        response.status = EOPNOTSUPP;
    }

    fn linux_umount_vfs(&mut self, response: &mut VfsIpcResponse) {
        response.status = EOPNOTSUPP;
    }

    fn vfs_unlinkat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path_follow(request, request.dirfd, path, false) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let remove_dir = (request.flags & linux_abi::AT_REMOVEDIR as u32) != 0;
        let mut storage = lock_vfs_storage();
        if let Err(e) = check_parent(&mut storage, &path, request, true) { response.status = e; return; }
        if request.flags & rustos_user_abi::syscall::VFS_REQUEST_FLAG_NETD_SOCKET_NODE != 0 {
            // A delayed bind rollback may not unlink a replacement inode.
            if request.arg1 == 0 || request.arg2 == 0 {
                response.status = EINVAL;
                return;
            }
            match storage.metadata(path.as_str()) {
                Ok(metadata) if vfsd::socket_rollback_matches(
                    (request.arg1, request.arg2),
                    (metadata.dev, metadata.fs.map_or(0, |fs| fs.node_id)),
                    metadata.kind == RemoteKind::Socket,
                ) => {}
                Ok(_) => { response.status = ESTALE; return; }
                Err(errno) => { response.status = errno; return; }
            }
        }
        response.status = storage.unlink_path(path.as_str(), remove_dir);
    }

    fn vfs_write(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        // `write` on a read-only description is EBADF for devices too.
        if !handle.writable {
            response.status = EBADF;
            return;
        }
        if handle.kind == RemoteKind::Device {
            match handle.path.as_str() {
                "/dev/null" | "/dev/zero" | "/dev/urandom" | "/dev/random" => {
                    response.value = u64::from(request.payload_len);
                    return;
                }
                "/dev/full" => {
                    response.status = if request.payload_len == 0 { 0 } else { ENOSPC };
                    return;
                }
                _ => {
                    response.status = EINVAL;
                    return;
                }
            }
        }
        if handle.kind != RemoteKind::File {
            response.status = EISDIR;
            return;
        }
        if self.checkpoint_operation_replayed(request, checkpoint_handle_key(request.remote_id)) {
            if handle.last_mutation != VFSD_OPEN_MUTATION_STABLE {
                response.status = EIO;
                return;
            }
            response.value = handle.last_result;
            return;
        }
        if cursor_mutation_prepared(&handle) {
            response.status = EBUSY;
            return;
        }
        let len = (request.payload_len as usize).min(request.payload.len());
        let data = &request.payload[..len];
        let (start, written, new_len) = {
            let mut storage = lock_vfs_storage();
            if handle.node != 0 {
                let offset = if handle.append { None } else { Some(handle.cursor) };
                match storage.write_node(handle.mount_id, handle.node, offset, data) {
                    Ok(result) => result,
                    Err(errno) => {
                        response.status = errno;
                        return;
                    }
                }
            } else {
                let start = if handle.append {
                    storage.live_len_for_path(handle.path.as_str(), handle.len)
                } else {
                    handle.cursor
                };
                match storage.write_at_path(handle.path.as_str(), start, data) {
                    Ok((written, new_len)) => (start, written, new_len),
                    Err(errno) => {
                        response.status = errno;
                        return;
                    }
                }
            }
        };
        let mut candidate = handle;
        candidate.cursor = match vfsd::cursor_after_write(candidate.cursor, start, len, written) {
            Some(c) => c,
            None => {
                response.status = EOVERFLOW;
                return;
            }
        };
        candidate.len = new_len;
        candidate.last_mutation = VFSD_OPEN_MUTATION_STABLE;
        candidate.last_start = start;
        candidate.last_result = written as u64;
        if let Err(errno) = self.checkpoint_handle_state(
            request,
            request.remote_id,
            &candidate,
            VFSD_OPEN_MUTATION_STABLE,
            start,
            written as u64,
        ) {
            response.status = errno;
            return;
        }
        self.handles.insert(request.remote_id, candidate);
        response.value = written as u64;
    }

    fn vfs_pwrite64(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        // `write` on a read-only description is EBADF for devices too.
        if !handle.writable {
            response.status = EBADF;
            return;
        }
        if handle.kind == RemoteKind::Device {
            match handle.path.as_str() {
                "/dev/null" | "/dev/zero" | "/dev/urandom" | "/dev/random" => {
                    response.value = u64::from(request.payload_len);
                    return;
                }
                "/dev/full" => {
                    response.status = if request.payload_len == 0 { 0 } else { ENOSPC };
                    return;
                }
                _ => {
                    response.status = EINVAL;
                    return;
                }
            }
        }
        if handle.kind != RemoteKind::File {
            response.status = EISDIR;
            return;
        }
        // A negative off_t offset is EINVAL.
        if request.arg0 > i64::MAX as u64 {
            response.status = EINVAL;
            return;
        }
        let len = (request.arg1 as usize)
            .min(request.payload_len as usize)
            .min(request.payload.len());
        let data = &request.payload[..len];
        let result = if handle.node != 0 {
            lock_vfs_storage()
                .write_node(handle.mount_id, handle.node, Some(request.arg0), data)
                .map(|(_, written, new_len)| (written, new_len))
        } else {
            lock_vfs_storage().write_at_path(handle.path.as_str(), request.arg0, data)
        };
        match result {
            Ok((written, new_len)) => {
                if let Some(entry) = self.handles.get_mut(&request.remote_id) {
                    entry.len = new_len;
                }
                response.value = written as u64;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_ftruncate(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        if !handle.writable || handle.kind != RemoteKind::File {
            response.status = EINVAL;
            return;
        }
        // A length with the sign bit set is a negative off_t: EINVAL, not EFBIG.
        if request.arg0 > i64::MAX as u64 {
            response.status = EINVAL;
            return;
        }
        let result = if handle.node != 0 {
            lock_vfs_storage().truncate_node(handle.mount_id, handle.node, request.arg0)
        } else {
            lock_vfs_storage().truncate_path(handle.path.as_str(), request.arg0)
        };
        match result {
            Ok(actual_len) => {
                if let Some(entry) = self.handles.get_mut(&request.remote_id) {
                    entry.len = actual_len;
                }
                response.value = 0;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_fsync(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id) else {
            response.status = EBADF;
            return;
        };
        if handle.kind == RemoteKind::Device {
            response.status = EINVAL;
            return;
        }
        if handle.node != 0 {
            // Memory-backed: a completed write is already as durable as it
            // gets, and the pinned node needs no path to flush.
            return;
        }
        let path = handle.path.clone();
        if let Err(errno) = lock_vfs_storage().flush_path(path.as_str()) {
            response.status = errno;
        }
    }

    fn vfs_renameat2(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let payload = &request.payload[..(request.payload_len as usize).min(request.payload.len())];
        let Some((old_bytes, new_bytes)) = decode_two_path_payload(payload) else {
            response.status = EINVAL;
            return;
        };
        let (Ok(old_raw), Ok(new_raw)) = (str::from_utf8(old_bytes), str::from_utf8(new_bytes)) else {
            response.status = EINVAL;
            return;
        };
        let known_flags = linux_abi::RENAME_NOREPLACE as u64 | linux_abi::RENAME_EXCHANGE as u64;
        if request.arg1 & !known_flags != 0 {
            response.status = EINVAL;
            return;
        }
        if request.arg1 & linux_abi::RENAME_EXCHANGE as u64 != 0 {
            response.status = EOPNOTSUPP;
            return;
        }
        let no_replace = (request.arg1 & linux_abi::RENAME_NOREPLACE as u64) != 0;
        let old_path = match self.resolve_path_follow(request, request.dirfd, old_raw, false) {
            Ok(path) => path,
            Err(errno) => { response.status = errno; return; }
        };
        let mut new_request = *request;
        new_request.remote_id = request.arg2;
        let new_path = match self.resolve_path_follow(&new_request, request.arg0, new_raw, false) {
            Ok(path) => path,
            Err(errno) => { response.status = errno; return; }
        };
        let mut storage = lock_vfs_storage();
        let check = check_parent(&mut storage, &old_path, request, true)
            .and_then(|()| check_parent(&mut storage, &new_path, request, false))
            .and_then(|()| match storage.metadata(&new_path) {
                Ok(_) => check_parent(&mut storage, &new_path, request, true),
                Err(ENOENT) => Ok(()), Err(e) => Err(e),
            });
        response.status = match check { Ok(()) => storage.rename_paths(&old_path, &new_path, no_replace), Err(e) => e };
    }

    fn vfs_statfs(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let path = match request.arg0 {
            0 => {
                let Some(raw) = vfs_request_path(request) else {
                    response.status = EINVAL;
                    return;
                };
                match self.resolve_path_follow(request, request.dirfd, raw, true) {
                    Ok(p) => p,
                    Err(errno) => {
                        response.status = errno;
                        return;
                    }
                }
            }
            1 => {
                let Some(handle) = self.handles.get(&request.remote_id) else {
                    response.status = EBADF;
                    return;
                };
                handle.path.clone()
            }
            _ => {
                response.status = EINVAL;
                return;
            }
        };
        match lock_vfs_storage().statfs_path(path.as_str()) {
            Ok(wire) => {
                let bytes = wire.to_bytes();
                response.payload[..bytes.len()].copy_from_slice(&bytes);
                response.payload_len = bytes.len() as u32;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_symlinkat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let payload = &request.payload[..(request.payload_len as usize).min(request.payload.len())];
        let Some((target_bytes, link_bytes)) = decode_two_path_payload(payload) else {
            response.status = EINVAL;
            return;
        };
        let (Ok(target), Ok(link_raw)) = (str::from_utf8(target_bytes), str::from_utf8(link_bytes)) else {
            response.status = EINVAL;
            return;
        };
        let link_path = match self.resolve_path_follow(request, request.dirfd, link_raw, false) {
            Ok(p) => p,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let attrs = FsCreateAttrs::new(0o777, request.euid, request.egid);
        let mut storage = lock_vfs_storage();
        response.status = match check_parent(&mut storage, &link_path, request, false) { Ok(()) => storage.symlink_path(target, &link_path, attrs), Err(e) => e };
    }

    fn vfs_set_attrs(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        use rustos_user_abi::syscall::{VFS_ATTR_ATIME, VFS_ATTR_MTIME, VFS_ATTR_ATIME_NOW, VFS_ATTR_MTIME_NOW};
        let mask = request.arg0;
        if mask & !0x3f != 0
            || (mask & VFS_ATTR_ATIME_NOW != 0 && mask & VFS_ATTR_ATIME == 0)
            || (mask & VFS_ATTR_MTIME_NOW != 0 && mask & VFS_ATTR_MTIME == 0)
            || (mask & 8 != 0 && mask & 3 != 0)
            || (mask & VFS_ATTR_ATIME != 0 && mask & VFS_ATTR_ATIME_NOW == 0 && request.arg3 as u32 >= 1_000_000_000)
            || (mask & VFS_ATTR_MTIME != 0 && mask & VFS_ATTR_MTIME_NOW == 0 && (request.arg3 >> 32) >= 1_000_000_000)
        {
            response.status = EINVAL;
            return;
        }
        let path;
        // A path request carries its directory base in `remote_id` too, so the
        // open-description form (futimens) is the one with no path at all.
        let target = if request.path_len == 0 && request.remote_id != 0 {
            let Some(handle) = self.handles.get(&request.remote_id) else {
                response.status = EBADF;
                return;
            };
            if handle.node != 0 {
                AttrTarget::Node(handle.mount_id, handle.node)
            } else {
                path = handle.path.clone();
                AttrTarget::Path(path.as_str())
            }
        } else {
            let Some(raw) = vfs_request_path(request) else {
                response.status = EINVAL;
                return;
            };
            // `utimensat(.., AT_SYMLINK_NOFOLLOW)` stamps the link itself.
            let follow_last = request.flags & linux_abi::AT_SYMLINK_NOFOLLOW as u32 == 0;
            path = match self.resolve_path_follow(request, request.dirfd, raw, follow_last) {
                Ok(p) => p,
                Err(errno) => {
                    response.status = errno;
                    return;
                }
            };
            AttrTarget::Path(path.as_str())
        };
        let mut storage = lock_vfs_storage();
        let metadata = match target {
            AttrTarget::Node(mount, node) => storage.node_metadata(mount, node),
            AttrTarget::Path(path) => storage.metadata(path),
        };
        match metadata {
            Ok(metadata) if mask != 0 && {
                let both_now = mask == VFS_ATTR_ATIME | VFS_ATTR_MTIME | VFS_ATTR_ATIME_NOW | VFS_ATTR_MTIME_NOW;
                match metadata.fs {
                    Some(fs) => !vfsd::permissions::set_times_allowed(fs, credentials(request, false), both_now),
                    // Plain metadata is reported as uid/gid 0 with the mode
                    // synthesized by stat; apply the same ownership policy.
                    None => request.euid != 0 && !(both_now && util::mode_bits(metadata.kind) & if request.egid == 0 { 0o020 } else { 0o002 } != 0),
                }
            } => {
                let both_now = mask == VFS_ATTR_ATIME | VFS_ATTR_MTIME | VFS_ATTR_ATIME_NOW | VFS_ATTR_MTIME_NOW;
                response.status = if both_now { EACCES } else { EPERM };
            }
            Ok(_) => response.status = storage.set_attrs_path(
                target,
                request.arg0,
                request.arg1 as i64,
                request.arg2 as i64,
                request.arg3,
            ),
            Err(errno) => response.status = errno,
        }
    }

    /// `VFS_IPC_OP_MKNOD`: create a socket (or empty regular) inode at a path.
    fn vfs_mknod(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(raw) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path_follow(request, request.dirfd, raw, false) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let attrs = FsCreateAttrs::new(request.arg0 as u32, request.euid, request.egid);
        let mut storage = lock_vfs_storage();
        response.status = match check_parent(&mut storage, &path, request, false) { Ok(()) => storage.mknod_path(&path, request.arg0, attrs), Err(e) => e };
        if response.status == 0
            && request.flags & rustos_user_abi::syscall::VFS_REQUEST_FLAG_NETD_SOCKET_NODE != 0
        {
            match storage.metadata(path.as_str()) {
                Ok(metadata) => {
                    if let Some(fs) = metadata.fs {
                        response.value = metadata.dev;
                        response.aux = fs.node_id;
                    } else {
                        response.status = EIO;
                    }
                }
                Err(errno) => response.status = errno,
            }
        }
    }

    /// `VFS_IPC_OP_CHMOD`: permission bits of the node at a path.
    fn vfs_chmod(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(raw) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path_follow(request, request.dirfd, raw, true) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let mut storage = lock_vfs_storage();
        match storage.metadata(path.as_str()) {
            Ok(metadata) if check_owner(metadata, request).is_err() => {
                response.status = EPERM;
            }
            Ok(_) => response.status = storage.chmod_path(path.as_str(), request.arg0 as u32),
            Err(errno) => response.status = errno,
        }
    }

    /// `VFS_IPC_OP_FCHMOD`: permission bits of an open description's node.
    fn vfs_fchmod(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id) else {
            response.status = EBADF;
            return;
        };
        if handle.node != 0 {
            let (mount_id, node) = (handle.mount_id, handle.node);
            let mut storage = lock_vfs_storage();
            match storage.node_metadata(mount_id, node) {
                Ok(metadata) if check_owner(metadata, request).is_err() => response.status = EPERM,
                Ok(_) => response.status = storage.chmod_node(mount_id, node, request.arg0 as u32),
                Err(errno) => response.status = errno,
            }
        } else if handle.kind == RemoteKind::Device {
            response.status = EINVAL;
        } else {
            let path = handle.path.clone();
            let mut storage = lock_vfs_storage();
            response.status = match storage.metadata(&path).and_then(|m| check_owner(m, request)) {
                Ok(()) => storage.chmod_path(&path, request.arg0 as u32), Err(e) => e,
            };
        }
    }

    /// `(uid, gid)` of a `CHOWN`/`FCHOWN` request; `None` is "unchanged".
    fn chown_ids(request: &VfsIpcRequest) -> Option<(Option<u32>, Option<u32>)> {
        use rustos_user_abi::syscall::VFS_ID_UNCHANGED;
        let id = |raw: u64| match raw {
            VFS_ID_UNCHANGED => Some(None),
            raw => u32::try_from(raw).ok().map(Some),
        };
        Some((id(request.arg0)?, id(request.arg1)?))
    }

    fn owner_credentials(request: &VfsIpcRequest) -> vfsd::permissions::OwnerCredentials {
        vfsd::permissions::OwnerCredentials { euid: request.euid, egid: request.egid, gid: request.gid }
    }

    /// `VFS_IPC_OP_CHOWN`: owner and group of the node at a path.
    fn vfs_chown(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some((uid, gid)) = Self::chown_ids(request) else {
            response.status = EINVAL;
            return;
        };
        if request.flags & !rustos_user_abi::syscall::VFS_CHOWN_FLAG_NOFOLLOW != 0 {
            response.status = EINVAL;
            return;
        }
        let Some(raw) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let follow_last = request.flags & rustos_user_abi::syscall::VFS_CHOWN_FLAG_NOFOLLOW == 0;
        let path = match self.resolve_path_follow(request, request.dirfd, raw, follow_last) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        response.status = lock_vfs_storage().chown_target(
            AttrTarget::Path(path.as_str()),
            Self::owner_credentials(request),
            uid,
            gid,
        );
    }

    /// `VFS_IPC_OP_FCHOWN`: owner and group of an open description's node.
    fn vfs_fchown(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some((uid, gid)) = Self::chown_ids(request) else {
            response.status = EINVAL;
            return;
        };
        let Some(handle) = self.handles.get(&request.remote_id) else {
            response.status = EBADF;
            return;
        };
        let cred = Self::owner_credentials(request);
        if handle.node != 0 {
            let (mount_id, node) = (handle.mount_id, handle.node);
            response.status =
                lock_vfs_storage().chown_target(AttrTarget::Node(mount_id, node), cred, uid, gid);
        } else if handle.kind == RemoteKind::Device {
            response.status = EINVAL;
        } else {
            let path = handle.path.clone();
            response.status =
                lock_vfs_storage().chown_target(AttrTarget::Path(path.as_str()), cred, uid, gid);
        }
    }

    /// `VFS_IPC_OP_LINKAT`: a new hard link to a path or to an open description.
    fn vfs_linkat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        use rustos_user_abi::syscall::VFS_LINKAT_FLAG_SOURCE_DESCRIPTION;
        let payload = &request.payload[..(request.payload_len as usize).min(request.payload.len())];
        let Some((old_bytes, new_bytes)) = decode_two_path_payload(payload) else {
            response.status = EINVAL;
            return;
        };
        let (Ok(old_raw), Ok(new_raw)) = (str::from_utf8(old_bytes), str::from_utf8(new_bytes)) else {
            response.status = EINVAL;
            return;
        };
        let by_description = request.flags & VFS_LINKAT_FLAG_SOURCE_DESCRIPTION != 0;
        if request.flags & !VFS_LINKAT_FLAG_SOURCE_DESCRIPTION != 0
            || request.arg1 & !(linux_abi::AT_SYMLINK_FOLLOW) != 0
            || (by_description && !old_raw.is_empty())
            || new_raw.is_empty()
        {
            response.status = EINVAL;
            return;
        }
        let mut new_request = *request;
        new_request.remote_id = request.arg2;
        let new_path = match self.resolve_path_follow(&new_request, request.arg0, new_raw, false) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let creds = credentials(request, false);
        if by_description {
            let Some(handle) = self.handles.get(&request.remote_id) else {
                response.status = EBADF;
                return;
            };
            if handle.node == 0 || handle.kind != RemoteKind::File {
                // Only a pinned file node can gain a name; FAT, devices and
                // directories cannot.
                response.status = EPERM;
                return;
            }
            let (mount_id, node) = (handle.mount_id, handle.node);
            let mut storage = lock_vfs_storage();
            response.status = match storage.node_metadata(mount_id, node) {
                Ok(meta) if meta.fs.is_some_and(|fs| !vfsd::permissions::hardlink_source_allowed(fs, creds)) => EPERM,
                Ok(_) => match check_parent(&mut storage, &new_path, request, false) {
                    Ok(()) => storage.link_node_path(mount_id, node, &new_path),
                    Err(errno) => errno,
                },
                Err(errno) => errno,
            };
            return;
        }
        if old_raw.is_empty() {
            response.status = ENOENT;
            return;
        }
        let follow_last = request.arg1 & linux_abi::AT_SYMLINK_FOLLOW != 0;
        let old_path = match self.resolve_path_follow(request, request.dirfd, old_raw, follow_last) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let mut storage = lock_vfs_storage();
        response.status = match storage.metadata(old_path.as_str()) {
            Ok(meta) if meta.kind == RemoteKind::Directory => EPERM,
            Ok(meta) if meta.fs.is_some_and(|fs| !vfsd::permissions::hardlink_source_allowed(fs, creds)) => EPERM,
            Ok(_) => match check_parent(&mut storage, &new_path, request, false) {
                Ok(()) => storage.link_paths(&old_path, &new_path),
                Err(errno) => errno,
            },
            Err(errno) => errno,
        };
    }

    /// `VFS_IPC_OP_FALLOCATE`: mode 0 and `KEEP_SIZE` on a memory-filesystem
    /// file; reply `value` is the resulting length.
    fn vfs_fallocate(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id) else {
            response.status = EBADF;
            return;
        };
        if !handle.writable {
            response.status = EBADF;
            return;
        }
        if handle.kind != RemoteKind::File {
            response.status = ENODEV;
            return;
        }
        let Some(keep_size) = rustos_user_abi::syscall::vfs_fallocate_keep_size(request.arg0) else {
            response.status = EOPNOTSUPP;
            return;
        };
        if handle.node == 0 {
            response.status = EOPNOTSUPP;
            return;
        }
        // Negative offset or length (sign bit set) is EINVAL; an in-range pair
        // whose end overflows off_t stays EFBIG from the volume.
        if request.arg1 > i64::MAX as u64 || request.arg2 > i64::MAX as u64 {
            response.status = EINVAL;
            return;
        }
        let (mount_id, node) = (handle.mount_id, handle.node);
        match lock_vfs_storage().fallocate_node(mount_id, node, keep_size, request.arg1, request.arg2) {
            Ok(len) => {
                if let Some(entry) = self.handles.get_mut(&request.remote_id) {
                    entry.len = len;
                }
                response.value = len;
            }
            Err(errno) => response.status = errno,
        }
    }

}
