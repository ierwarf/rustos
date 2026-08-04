// SPDX-License-Identifier: MIT

impl VfsState {
    fn handle_linux_request(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
    ) -> LinuxSyscallOffloadResponse {
        let mut response = linux_response_for_op(request.op);
        if let Err(errno) = validate_linux_request(request) {
            response.status = errno;
            return response;
        }
        match request.op {
            SYSCALL_OFFLOAD_OP_LINUX_STATX => self.linux_statx(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT => self.linux_newfstatat(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_READLINKAT => self.linux_readlinkat(&mut response),
            SYSCALL_OFFLOAD_OP_LINUX_ACCESS => self.linux_access(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_GETCWD => self.linux_getcwd(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_CHDIR => self.linux_chdir(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_MKDIR => self.linux_mkdir(request, &mut response),
            // Stateful fd operations require the versioned VFS IPC request's
            // operation identity. The retired generic offload envelope has no
            // such field and cannot provide crash-safe open descriptions.
            SYSCALL_OFFLOAD_OP_LINUX_OPENAT
            | SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64
            | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
            | SYSCALL_OFFLOAD_OP_LINUX_DUP
            | SYSCALL_OFFLOAD_OP_LINUX_FCNTL => response.status = EOPNOTSUPP,
            SYSCALL_OFFLOAD_OP_LINUX_MOUNT => self.linux_mount(&mut response),
            SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2 => self.linux_umount2(&mut response),
            SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT => self.linux_unlinkat(request, &mut response),
            _ => response.status = EINVAL,
        }
        response
    }

    fn handle_commercial_request(
        &mut self,
        request: &CommercialMaxProtocolRequest,
    ) -> CommercialMaxProtocolResponse {
        let mut response = CommercialMaxProtocolResponse {
            header: request.header,
            ..CommercialMaxProtocolResponse::default()
        };
        response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
        if let Err(errno) = validate_commercial_request(request) {
            response.status = errno;
            return response;
        }
        match request.header.op {
            COMMERCIAL_MAX_VFSD_OP_MOUNT_GRAPH => {
                {
                    let storage = lock_vfs_storage();
                    response.value0 = storage.mount_generation;
                    response.value1 = u64::from(storage.volume.is_some());
                }
                response.descriptor_count = 1;
                response.descriptors[0] =
                    vfs_descriptor("mount-graph", request.header.op, lock_vfs_storage().mount_generation, 0);
            }
            COMMERCIAL_MAX_VFSD_OP_PATH_RESOLVE => {
                let path = commercial_request_path(request);
                if let Some(path) = path {
                    match lock_vfs_storage().metadata(path) {
                        Ok(metadata) => {
                            response.value0 = metadata.inode;
                            response.value1 = metadata.len;
                            response.capability = vfs_capability("path-resolve", request.header.op);
                            response.descriptor_count = 1;
                            response.descriptors[0] = vfs_descriptor(
                                "path-resolve",
                                request.header.op,
                                metadata.inode,
                                metadata.len,
                            );
                        }
                        Err(errno) => response.status = errno,
                    }
                } else {
                    response.status = EINVAL;
                }
            }
            COMMERCIAL_MAX_VFSD_OP_FD_TABLE_PLAN => {
                response.value0 = self.handles.len() as u64;
                response.value1 = self.next_handle;
                response.descriptor_count = 1;
                response.descriptors[0] =
                    vfs_descriptor("fd-table", request.header.op, self.handles.len() as u64, 0);
            }
            COMMERCIAL_MAX_VFSD_OP_DIRECTORY_CURSOR => {
                fill_handle_descriptors(self, &mut response, RemoteKind::Directory);
            }
            COMMERCIAL_MAX_VFSD_OP_FILE_CURSOR => {
                fill_handle_descriptors(self, &mut response, RemoteKind::File);
            }
            COMMERCIAL_MAX_VFSD_OP_METADATA_POLICY => {
                {
                    let storage = lock_vfs_storage();
                    response.value0 = storage.metadata_cache.len() as u64;
                    response.value1 = storage.dir_entries_cache.len() as u64;
                }
                response.capability = vfs_capability("metadata-policy", request.header.op);
                response.descriptor_count = 1;
                // One acquisition: two live guards in one argument list would
                // deadlock, because the storage owner is not re-entrant.
                let (metadata_entries, dir_entries) = {
                    let storage = lock_vfs_storage();
                    (
                        storage.metadata_cache.len() as u64,
                        storage.dir_entries_cache.len() as u64,
                    )
                };
                response.descriptors[0] = vfs_descriptor(
                    "metadata-policy",
                    request.header.op,
                    metadata_entries,
                    dir_entries,
                );
            }
            _ => response.status = EINVAL,
        }
        response
    }

    fn handle_vfs_request(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        if let Err(errno) = validate_vfs_request(request) {
            response.status = errno;
            return;
        }
        match request.op {
            VFS_IPC_OP_OPENAT => self.vfs_openat(request, response),
            VFS_IPC_OP_CLOSE => self.vfs_close(request, response),
            VFS_IPC_OP_DUP => response.status = EOPNOTSUPP,
            VFS_IPC_OP_READ => self.vfs_read(request, response, None),
            VFS_IPC_OP_PREAD64 => self.vfs_read(request, response, Some(request.arg0)),
            VFS_IPC_OP_WRITE => {
                response.status = persistent_mutation_status(request.op).unwrap_or(EINVAL)
            }
            VFS_IPC_OP_LSEEK => self.vfs_lseek(request, response),
            VFS_IPC_OP_FSTAT => self.vfs_fstat(request, response),
            VFS_IPC_OP_FTRUNCATE => {
                response.status = persistent_mutation_status(request.op).unwrap_or(EINVAL)
            }
            VFS_IPC_OP_GETDENTS64 => self.vfs_getdents64(request, response),
            VFS_IPC_OP_FCNTL => self.vfs_fcntl(request, response),
            VFS_IPC_OP_CURSOR_SETTLE => self.vfs_cursor_settle(request, response),
            VFS_IPC_OP_CHECKPOINT_ACK => self.vfs_checkpoint_ack(request, response),
            VFS_IPC_OP_STATX => self.vfs_path_statx(request, response),
            VFS_IPC_OP_NEWFSTATAT => self.vfs_path_stat(request, response),
            VFS_IPC_OP_READLINKAT => response.status = ENOENT,
            VFS_IPC_OP_ACCESS => self.vfs_access(request, response),
            VFS_IPC_OP_GETCWD => self.vfs_getcwd(request, response),
            VFS_IPC_OP_CHDIR => self.vfs_chdir(request, response),
            VFS_IPC_OP_MKDIR => self.vfs_mkdir(request, response),
            VFS_IPC_OP_MOUNT => self.linux_mount_vfs(response),
            VFS_IPC_OP_UMOUNT2 => self.linux_umount_vfs(response),
            VFS_IPC_OP_UNLINKAT => self.vfs_unlinkat(request, response),
            VFS_IPC_OP_POLL_QUERY => self.vfs_poll_query(request, response),
            _ => response.status = EINVAL,
        }
    }

    fn vfs_poll_query(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let mut mutated = false;
        let mut mutated_epolls = Vec::new();
        match request.arg0 {
            VFS_POLL_QUERY_POLL => self.vfs_poll_once(request, response),
            VFS_POLL_QUERY_EPOLL_CREATE => {
                let key = checkpoint_epoll_key(request.remote_id);
                if self.checkpoint_operation_replayed(request, key) {
                    return;
                }
                let mut candidate = self.epolls.clone();
                if let Err(err) = candidate.create(request.remote_id) {
                    response.status = waitset_registry_status(err);
                    return;
                }
                let record = checkpoint_epoll_record(request.remote_id, false);
                if let Err(errno) = self.checkpoint_mutate(request, record) {
                    response.status = errno;
                    return;
                }
                self.epolls = candidate;
                mutated = true;
                mutated_epolls.push(request.remote_id);
            }
            VFS_POLL_QUERY_EPOLL_CTL => {
                self.vfs_epoll_ctl(request, response);
                mutated = response.status == 0;
                if mutated {
                    mutated_epolls.push(request.remote_id);
                }
            }
            VFS_POLL_QUERY_EPOLL_SNAPSHOT => self.vfs_epoll_snapshot(request, response),
            VFS_POLL_QUERY_EPOLL_RETIRE => {
                let key = checkpoint_epoll_key(request.remote_id);
                if self.checkpoint_operation_replayed(request, key) {
                    return;
                }
                let mut candidate = self.epolls.clone();
                if let Err(err) = candidate.retire(request.remote_id) {
                    response.status = waitset_registry_status(err);
                    return;
                }
                let record = checkpoint_epoll_record(request.remote_id, true);
                if let Err(errno) = self.checkpoint_mutate(request, record) {
                    response.status = errno;
                    return;
                }
                self.epolls = candidate;
                mutated = true;
                mutated_epolls.push(request.remote_id);
            }
            VFS_POLL_QUERY_EPOLL_PURGE_OBJECT => {
                let Ok(provider) = u16::try_from(request.arg1) else {
                    response.status = EINVAL;
                    return;
                };
                if provider == 0 || request.arg2 == 0 {
                    response.status = EINVAL;
                    return;
                }
                let interests = self.epolls.matching_interests(provider, request.arg2);
                mutated_epolls.extend(interests.iter().map(|(token, _)| *token));
                let mut candidate = self.epolls.clone();
                mutated = candidate.purge(provider, request.arg2);
                for (token, interest) in interests {
                    let record = checkpoint_interest_record(token, interest, true);
                    let key = checkpoint_revision_key(&record);
                    if !self.checkpoint_operation_replayed(request, key) {
                        if let Err(errno) = self.checkpoint_mutate(request, record) {
                            response.status = errno;
                            return;
                        }
                    }
                }
                self.epolls = candidate;
            }
            _ => response.status = EINVAL,
        }
        if mutated {
            mutated_epolls.sort_unstable();
            mutated_epolls.dedup();
            self.advance_readiness_generation(&mutated_epolls);
        }
    }

    fn vfs_poll_once(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        const POLLFD_SIZE: usize = size_of::<linux_abi::LinuxPollFd>();
        let len = request.payload_len as usize;
        if !len.is_multiple_of(POLLFD_SIZE) || len > response.payload.len() {
            response.status = EINVAL;
            return;
        }
        let mut ready = 0_u64;
        for offset in (0..len).step_by(POLLFD_SIZE) {
            let fd = i32::from_le_bytes(request.payload[offset..offset + 4].try_into().unwrap());
            let events =
                i16::from_le_bytes(request.payload[offset + 4..offset + 6].try_into().unwrap());
            let revents = if fd < 0 {
                0
            } else {
                let ready_bits = poll_ready_bits(events as u32) as i16;
                if ready_bits != 0 {
                    ready += 1;
                }
                ready_bits
            };
            response.payload[offset..offset + 6]
                .copy_from_slice(&request.payload[offset..offset + 6]);
            response.payload[offset + 6..offset + 8].copy_from_slice(&revents.to_le_bytes());
        }
        response.value = ready;
        response.payload_len = len as u32;
    }

    fn vfs_epoll_ctl(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(interest) = epoll_interest_from_request(request) else {
            response.status = EINVAL;
            return;
        };
        let tombstone = request.arg1 == linux_abi::EPOLL_CTL_DEL;
        let checkpoint = checkpoint_interest_record(request.remote_id, interest, tombstone);
        let checkpoint_key = checkpoint_revision_key(&checkpoint);
        if self.checkpoint_operation_replayed(request, checkpoint_key) {
            return;
        }
        let mut candidate = self.epolls.clone();
        let result = match request.arg1 {
            linux_abi::EPOLL_CTL_ADD => candidate.add(request.remote_id, interest),
            linux_abi::EPOLL_CTL_MOD => candidate.modify(request.remote_id, interest),
            linux_abi::EPOLL_CTL_DEL => candidate.delete(request.remote_id, interest.key),
            _ => {
                response.status = EINVAL;
                return;
            }
        };
        if let Err(err) = result {
            response.status = waitset_registry_status(err);
            return;
        }
        if let Err(errno) = self.checkpoint_mutate(request, checkpoint) {
            response.status = errno;
            return;
        }
        self.epolls = candidate;
    }

    fn vfs_epoll_snapshot(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let maxevents = request.arg1 as usize;
        let wire_size = size_of::<WaitSetInterestWire>();
        let capacity = response.payload.len() / wire_size;
        if maxevents == 0 || maxevents > WAITSET_MAX_INTERESTS || maxevents > capacity {
            response.status = EINVAL;
            return;
        }
        let interests = match self.epolls.snapshot(request.remote_id, maxevents) {
            Ok(interests) => interests,
            Err(err) => {
                response.status = waitset_registry_status(err);
                return;
            }
        };
        for (written, interest) in interests.iter().enumerate() {
            let wire = WaitSetInterestWire {
                abi_version: WAITSET_ABI_VERSION,
                provider: interest.key.provider,
                flags: 0,
                target_fd: interest.key.target_fd,
                object_id: interest.key.object_id,
                provider_epoch: interest.provider_epoch,
                events: interest.events,
                reserved0: 0,
                data: interest.data,
            };
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (&wire as *const WaitSetInterestWire).cast::<u8>(),
                    wire_size,
                )
            };
            let offset = written * wire_size;
            response.payload[offset..offset + wire_size].copy_from_slice(bytes);
        }
        response.value = interests.len() as u64;
        response.aux = self.readiness_generation;
        response.payload_len = (interests.len() * wire_size) as u32;
    }

    fn advance_readiness_generation(&mut self, object_ids: &[u64]) {
        self.readiness_generation = self
            .readiness_generation
            .checked_add(1)
            .expect("vfsd readiness generation exhausted");
        #[cfg(not(test))]
        for object_id in object_ids.iter().copied() {
            let args = WaitSetSignalBrokerArgs {
                abi_version: WAITSET_ABI_VERSION,
                provider: WAITSET_PROVIDER_VFSD,
                flags: 0,
                object_id,
                generation: self.readiness_generation,
                reserved0: 0,
            };
            let status = unsafe {
                rustos_svc_runtime::syscall::syscall1(
                    SYS_RUSTOS_WAITSET_SIGNAL_BROKER,
                    (&args as *const WaitSetSignalBrokerArgs) as u64,
                )
            };
            if status < 0 {
                panic!("vfsd readiness generation publication failed");
            }
        }
    }

    fn linux_statx(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        match lock_vfs_storage().metadata(path) {
            Ok(metadata) => {
                let statx = build_linux_statx(metadata);
                response.payload_len = LINUX_STATX_SIZE as u32;
                response.payload[..LINUX_STATX_SIZE].copy_from_slice(&statx);
            }
            Err(errno) => response.status = errno,
        }
    }

    fn linux_newfstatat(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        match lock_vfs_storage().metadata(path) {
            Ok(metadata) => {
                let stat = build_linux_stat(metadata);
                response.payload_len = LINUX_STAT_SIZE as u32;
                response.payload[..LINUX_STAT_SIZE].copy_from_slice(&stat);
            }
            Err(errno) => response.status = errno,
        }
    }

    fn linux_readlinkat(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        response.status = ENOENT;
    }

    fn linux_access(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = match lock_vfs_storage().metadata(path) {
            Ok(_) => 0,
            Err(errno) => errno,
        };
    }

    fn linux_getcwd(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let cwd = self.cwd_for_pid(request.pid);
        write_payload_bytes(response, cwd.as_bytes());
    }

    fn linux_chdir(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = self.chdir(request.pid, path);
    }

    fn linux_mkdir(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = mkdir_policy(path, request.euid);
    }

    fn linux_mount(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        response.status = match lock_vfs_storage().advance_mount_generation() {
            Ok(()) => 0,
            Err(errno) => errno,
        };
    }

    fn linux_umount2(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        response.status = match lock_vfs_storage().advance_mount_generation() {
            Ok(()) => 0,
            Err(errno) => errno,
        };
    }

    fn linux_unlinkat(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = unlink_policy(path);
    }

}
