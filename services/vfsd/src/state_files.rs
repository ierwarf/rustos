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
        let resolved_path = match self.resolve_path(request, request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let path = resolved_path.as_str();
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
                self.handles.remove(&request.remote_id);
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
        let len = (request.arg1 as usize).min(VFS_IPC_PAYLOAD_CAPACITY);
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
        let next = match checked_seek_position(
            handle.cursor,
            handle.len,
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
        let wanted = request.arg1 as usize;
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
            Ok(identity) => (identity, 1),
            Err(EOPNOTSUPP | ENOENT | EIO | EINVAL) => (VfsFileIdentityWire::default(), 0),
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
        match lock_vfs_storage().file_identity(path.as_str(), len) {
            Ok(identity) => {
                let bytes = identity.to_bytes();
                response.payload[..bytes.len()].copy_from_slice(&bytes);
                response.payload_len = bytes.len() as u32;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_fstat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id) else {
            response.status = EBADF;
            return;
        };
        let stat = build_linux_stat(Metadata {
            kind: handle.kind,
            len: handle.len,
            inode: path_inode(handle.path.as_bytes()),
        });
        response.payload_len = LINUX_STAT_SIZE as u32;
        response.payload[..LINUX_STAT_SIZE].copy_from_slice(&stat);
    }

    fn vfs_getdents64(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        let requested = (request.arg1 as usize).min(response.payload.len());
        let replayed =
            self.checkpoint_operation_replayed(request, checkpoint_handle_key(request.remote_id));
        if cursor_mutation_prepared(&handle) && !replayed {
            response.status = EBUSY;
            return;
        }
        let start = if replayed {
            let (recorded_requested, _) = unpack_u32_pair(handle.last_result);
            if handle.last_mutation != VFSD_OPEN_MUTATION_GETDENTS
                || recorded_requested as usize != requested
            {
                response.status = EIO;
                return;
            }
            handle.last_start
        } else {
            handle.cursor
        };
        let start_index = match usize::try_from(start) {
            Ok(start) => start,
            Err(_) => {
                response.status = EOVERFLOW;
                return;
            }
        };
        let (written, consumed) = match self.render_getdents_payload(
            request.remote_id,
            start_index,
            requested,
            &mut response.payload,
        ) {
            Ok(result) => result,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        if replayed {
            let (_, recorded_consumed) = unpack_u32_pair(handle.last_result);
            if consumed != recorded_consumed as usize {
                response.status = EIO;
                return;
            }
        } else {
            let mut candidate = handle;
            candidate.cursor = match candidate.cursor.checked_add(consumed as u64) {
                Some(cursor) => cursor,
                None => {
                    response.status = EOVERFLOW;
                    return;
                }
            };
            candidate.last_mutation = VFSD_OPEN_MUTATION_GETDENTS;
            candidate.last_start = start;
            candidate.last_result = match pack_u32_pair(requested, consumed) {
                Ok(result) => result,
                Err(errno) => {
                    response.status = errno;
                    return;
                }
            };
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
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
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
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_path_stat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
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
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = match self.resolve_path(request, request.pid, request.dirfd, path) {
            Ok(path) => match vfsd::proc_fs::lookup(path.as_str(), request.pid) {
                Some(Ok(vfsd::proc_fs::ProcNode::ExeLink)) => {
                    match self.resolve_proc_exe_target(request, vfsd::proc_fs::ProcNode::ExeLink) {
                        Ok(target) => lock_vfs_storage().metadata(target.as_str()).err().unwrap_or(0),
                        Err(errno) => errno,
                    }
                }
                Some(Ok(_)) => 0,
                Some(Err(errno)) => errno,
                None => lock_vfs_storage().metadata(path.as_str()).err().unwrap_or(0),
            },
            Err(errno) => errno,
        };
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
        let exec = proc_fs::attached_exec_path(&request.payload, request.payload_len as usize);
        let follow = request.flags & AT_SYMLINK_NOFOLLOW == 0;
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
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
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
                response.status = match lock_vfs_storage().metadata(path.as_str()) {
                    Ok(_) => EINVAL,
                    Err(errno) => errno,
                };
                return;
            }
        };
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
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        response.status = self.chdir(request.pid, path.as_str());
    }

    fn vfs_mkdir(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        response.status = lock_vfs_storage().run_dirs.mkdir(path.as_str(), request.euid);
    }

    fn linux_mount_vfs(&mut self, response: &mut VfsIpcResponse) {
        response.status = match lock_vfs_storage().advance_mount_generation() {
            Ok(()) => 0,
            Err(errno) => errno,
        };
    }

    fn linux_umount_vfs(&mut self, response: &mut VfsIpcResponse) {
        response.status = match lock_vfs_storage().advance_mount_generation() {
            Ok(()) => 0,
            Err(errno) => errno,
        };
    }

    fn vfs_unlinkat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        response.status = unlink_policy(path.as_str());
    }

}
