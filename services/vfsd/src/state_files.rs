// SPDX-License-Identifier: MIT

impl VfsState {
    fn vfs_openat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let resolved_path;
        let path = if path.starts_with('/') {
            path
        } else {
            resolved_path = match self.resolve_path(request, request.pid, request.dirfd, path) {
                Ok(path) => path,
                Err(errno) => {
                    response.status = errno;
                    return;
                }
            };
            resolved_path.as_str()
        };
        match self.open_remote_checkpointed(request, path, request.arg0) {
            Ok((id, handle)) => {
                response.remote_id = id;
                response.handle_kind = handle_kind_u16(handle.kind);
                response.value = handle.len;
                response.aux = device_access_for_path(handle.path.as_str());
                write_vfs_payload_bytes(response, handle.path.as_bytes());
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
                }
                return;
            }
            if let Err(errno) = self.checkpoint_close_description(request, request.remote_id) {
                response.status = errno;
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
                    VFS_POLL_QUERY_EPOLL_UNREF
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
        match self.metadata(path.as_str()) {
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
        match self.metadata(path.as_str()) {
            Ok(metadata) => {
                let stat = build_linux_stat(metadata);
                response.payload_len = LINUX_STAT_SIZE as u32;
                response.payload[..LINUX_STAT_SIZE].copy_from_slice(&stat);
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_access(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = match self.resolve_path(request, request.pid, request.dirfd, path) {
            Ok(path) => self.metadata(path.as_str()).err().unwrap_or(0),
            Err(errno) => errno,
        };
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
        response.status = mkdir_policy(path.as_str(), request.euid);
    }

    fn linux_mount_vfs(&mut self, response: &mut VfsIpcResponse) {
        response.status = match self.advance_mount_generation() {
            Ok(()) => 0,
            Err(errno) => errno,
        };
    }

    fn linux_umount_vfs(&mut self, response: &mut VfsIpcResponse) {
        response.status = match self.advance_mount_generation() {
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
