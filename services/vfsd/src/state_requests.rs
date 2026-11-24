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
            // Stateful fd operations require the versioned VFS IPC request's
            // operation identity. The retired generic offload envelope has no
            // such field and cannot provide crash-safe open descriptions.
            // Namespace mutations are refused for the same reason: the
            // envelope carries caller-claimed credentials and has no parent
            // search/write/sticky check, so `VFS_IPC_OP_MKDIR`/`UNLINKAT`
            // (attested credentials, `check_parent`) are the only mutators.
            SYSCALL_OFFLOAD_OP_LINUX_MKDIR
            | SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT
            | SYSCALL_OFFLOAD_OP_LINUX_OPENAT
            | SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64
            | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
            | SYSCALL_OFFLOAD_OP_LINUX_DUP
            | SYSCALL_OFFLOAD_OP_LINUX_FCNTL => response.status = EOPNOTSUPP,
            SYSCALL_OFFLOAD_OP_LINUX_MOUNT => self.linux_mount(&mut response),
            SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2 => self.linux_umount2(&mut response),
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
                let (mount_generation, mounted) = {
                    let storage = lock_vfs_storage();
                    (storage.mount_generation, u64::from(storage.mounts.has_root()))
                };
                response.value0 = mount_generation;
                response.value1 = mounted;
                response.descriptor_count = 1;
                response.descriptors[0] =
                    vfs_descriptor("mount-graph", request.header.op, mount_generation, 0);
            }
            COMMERCIAL_MAX_VFSD_OP_PATH_RESOLVE => {
                let path = commercial_request_path(request);
                // Canonicalize before routing, as the VFS IPC path does.
                let path = path.map(|raw| normalize_absolute_path("/", raw));
                if let Some(path) = path {
                    let path = match path {
                        Ok(path) => path,
                        Err(errno) => {
                            response.status = errno;
                            return response;
                        }
                    };
                    match lock_vfs_storage().metadata(path.as_str()) {
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
                let (metadata_entries, dir_entries) = {
                    let storage = lock_vfs_storage();
                    (
                        storage.metadata_cache.len() as u64,
                        storage.dir_entries_cache.len() as u64,
                    )
                };
                response.value0 = metadata_entries;
                response.value1 = dir_entries;
                response.capability = vfs_capability("metadata-policy", request.header.op);
                response.descriptor_count = 1;
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

    fn handle_vfs_bulk_read(&mut self, reply_cap: u64, request: &VfsIpcRequest) -> i64 {
        if let Err(errno) = validate_vfs_request(request) {
            return bulk_read_worker::reply_bulk_read_status(reply_cap, errno, 0, 0);
        }
        if self.handles.get(&request.remote_id).is_some_and(|handle| handle.stale) {
            return bulk_read_worker::reply_bulk_read_status(reply_cap, ESTALE, 0, 0);
        }
        if self.handles.contains_key(&request.remote_id)
            && !self.has_funded_open_description(request.remote_id)
        {
            return bulk_read_worker::reply_bulk_read_status(reply_cap, ESTALE, 0, 0);
        }
        match self.plan_bulk_read(reply_cap, request) {
            Ok(bulk_read_worker::BulkReadPlan::Eof { file_len }) => {
                bulk_read_worker::reply_bulk_read_status(reply_cap, 0, 0, file_len)
            }
            Ok(bulk_read_worker::BulkReadPlan::Job(job)) => {
                bulk_read_worker::enqueue_bulk_read(job)
            }
            Err(errno) => bulk_read_worker::reply_bulk_read_status(reply_cap, errno, 0, 0),
        }
    }

    fn handle_vfs_request(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        if let Err(errno) = validate_vfs_request(request) {
            response.status = errno;
            return;
        }
        if request.op != VFS_IPC_OP_CLOSE
            && self.handles.get(&request.remote_id).is_some_and(|handle| handle.stale)
        {
            response.status = ESTALE;
            return;
        }
        // Restored checkpoint handles are structural descriptions, not a
        // live kernel-funded client session. After a vfsd restart the old
        // provider PID's charges have been drained by rootd. Reuse of the
        // remote ID must first perform exact OPENAT replay/admission so a
        // new provider incarnation can bind it to the current client Job.
        // Fail closed rather than silently allowing unbilled reads/ioctls.
        if self.handles.contains_key(&request.remote_id)
            && matches!(request.op,
                VFS_IPC_OP_READ | VFS_IPC_OP_PREAD64 | VFS_IPC_OP_WRITE |
                VFS_IPC_OP_PWRITE64 | VFS_IPC_OP_FSYNC |
                VFS_IPC_OP_LSEEK | VFS_IPC_OP_FSTAT | VFS_IPC_OP_GETDENTS64 |
                VFS_IPC_OP_FCNTL | VFS_IPC_OP_CURSOR_SETTLE | VFS_IPC_OP_FTRUNCATE |
                VFS_IPC_OP_FILE_IDENTITY | VFS_IPC_OP_FCHMOD | VFS_IPC_OP_FALLOCATE | VFS_IPC_OP_FCHOWN |
                VFS_IPC_OP_FILE_LOCK | VFS_IPC_OP_SHARED_BACKING |
                VFS_IPC_OP_INOTIFY_ADD_WATCH | VFS_IPC_OP_INOTIFY_RM_WATCH)
            && !self.has_funded_open_description(request.remote_id)
        {
            response.status=116; // ESTALE, forces reopen against new provider
            return;
        }
        match request.op {
            VFS_IPC_OP_OPENAT => self.vfs_openat(request, response),
            VFS_IPC_OP_CLOSE => self.vfs_close(request, response),
            VFS_IPC_OP_DUP => response.status = EOPNOTSUPP,
            VFS_IPC_OP_READ => self.vfs_read(request, response, None),
            VFS_IPC_OP_PREAD64 => self.vfs_read(request, response, Some(request.arg0)),
            VFS_IPC_OP_WRITE => self.vfs_write(request, response),
            VFS_IPC_OP_PWRITE64 => self.vfs_pwrite64(request, response),
            VFS_IPC_OP_LSEEK => self.vfs_lseek(request, response),
            VFS_IPC_OP_FSTAT => self.vfs_fstat(request, response),
            VFS_IPC_OP_FILE_IDENTITY => self.vfs_file_identity(request, response),
            VFS_IPC_OP_LIBRARY_READ => self.vfs_library_read(request, response),
            VFS_IPC_OP_FTRUNCATE => self.vfs_ftruncate(request, response),
            VFS_IPC_OP_FSYNC => self.vfs_fsync(request, response),
            VFS_IPC_OP_GETDENTS64 => self.vfs_getdents64(request, response),
            VFS_IPC_OP_FCNTL => self.vfs_fcntl(request, response),
            VFS_IPC_OP_CURSOR_SETTLE => self.vfs_cursor_settle(request, response),
            VFS_IPC_OP_CHECKPOINT_ACK => self.vfs_checkpoint_ack(request, response),
            VFS_IPC_OP_STATX => self.vfs_path_statx(request, response),
            VFS_IPC_OP_NEWFSTATAT => self.vfs_path_stat(request, response),
            VFS_IPC_OP_READLINKAT => self.vfs_readlinkat(request, response),
            VFS_IPC_OP_ACCESS => self.vfs_access(request, response),
            VFS_IPC_OP_GETCWD => self.vfs_getcwd(request, response),
            VFS_IPC_OP_CHDIR => self.vfs_chdir(request, response),
            VFS_IPC_OP_MKDIR => self.vfs_mkdir(request, response),
            VFS_IPC_OP_MOUNT => self.linux_mount_vfs(response),
            VFS_IPC_OP_UMOUNT2 => self.linux_umount_vfs(response),
            VFS_IPC_OP_UNLINKAT => self.vfs_unlinkat(request, response),
            VFS_IPC_OP_RENAMEAT2 => self.vfs_renameat2(request, response),
            VFS_IPC_OP_STATFS => self.vfs_statfs(request, response),
            VFS_IPC_OP_SYMLINKAT => self.vfs_symlinkat(request, response),
            VFS_IPC_OP_SET_ATTRS => self.vfs_set_attrs(request, response),
            VFS_IPC_OP_MKNOD => self.vfs_mknod(request, response),
            VFS_IPC_OP_CHMOD => self.vfs_chmod(request, response),
            VFS_IPC_OP_FCHMOD => self.vfs_fchmod(request, response),
            VFS_IPC_OP_FALLOCATE => self.vfs_fallocate(request, response),
            VFS_IPC_OP_CHOWN => self.vfs_chown(request, response),
            VFS_IPC_OP_FCHOWN => self.vfs_fchown(request, response),
            VFS_IPC_OP_LINKAT => self.vfs_linkat(request, response),
            VFS_IPC_OP_INOTIFY_INIT => self.vfs_inotify_init(request, response),
            VFS_IPC_OP_INOTIFY_ADD_WATCH => self.vfs_inotify_add_watch(request, response),
            VFS_IPC_OP_INOTIFY_RM_WATCH => self.vfs_inotify_rm_watch(request, response),
            VFS_IPC_OP_FILE_LOCK => self.vfs_file_lock(request, response),
            VFS_IPC_OP_SHARED_BACKING => self.vfs_shared_backing(request, response),
            VFS_IPC_OP_POLL_QUERY => self.vfs_poll_query(request, response),
            _ => response.status = EINVAL,
        }
    }

    fn vfs_poll_query(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        match request.arg0 {
            VFS_POLL_QUERY_POLL => self.vfs_poll_once(request, response),
            VFS_POLL_QUERY_POLL_KEYED => self.vfs_poll_keyed(request, response),
            // Epoll state moved to Ring0 `EpollHandle`; `vfsd` no longer hosts epoll objects.
            VFS_POLL_QUERY_EPOLL_CREATE
            | VFS_POLL_QUERY_EPOLL_CTL
            | VFS_POLL_QUERY_EPOLL_SNAPSHOT
            | VFS_POLL_QUERY_EPOLL_CLAIM
            | VFS_POLL_QUERY_EPOLL_RETIRE
            | VFS_POLL_QUERY_EPOLL_PURGE_OBJECT => response.status = EOPNOTSUPP,
            _ => response.status = EINVAL,
        }
    }

    fn vfs_poll_once(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        const POLLFD_SIZE: usize = size_of::<linux_abi::LinuxPollFd>();
        let len = request.payload_len as usize;
        if len == 0
            || !len.is_multiple_of(POLLFD_SIZE)
            || len / POLLFD_SIZE > VFS_POLL_BATCH_MAX
            || len > response.payload.len().min(request.payload.len())
        {
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
                // Regular files, directories and /dev nodes are always readable and
                // writable; they never report PRI, ERR or HUP, and poll(2) must not
                // echo those bits just because the caller asked for them.
                let ready_bits = (poll_ready_bits(events as u32)
                    & (linux_abi::EPOLLIN | linux_abi::EPOLLOUT)) as i16;
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

    fn vfs_poll_keyed(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        const ENTRY_SIZE: usize = 16;
        let len = request.payload_len as usize;
        if len == 0
            || !len.is_multiple_of(ENTRY_SIZE)
            || len / ENTRY_SIZE > VFS_POLL_KEYED_BATCH_MAX
            || len > response.payload.len().min(request.payload.len())
        {
            response.status = EINVAL;
            return;
        }
        let mut ready = 0_u64;
        for offset in (0..len).step_by(ENTRY_SIZE) {
            let fd = i32::from_le_bytes(request.payload[offset..offset + 4].try_into().unwrap());
            let events =
                i16::from_le_bytes(request.payload[offset + 4..offset + 6].try_into().unwrap());
            let remote_id =
                u64::from_le_bytes(request.payload[offset + 8..offset + 16].try_into().unwrap());
            let revents = if fd < 0 {
                0
            } else if let Some(handle) = self.handles.get(&remote_id) {
                let bits = if handle.kind == RemoteKind::Inotify {
                    if self.inotify.is_readable(remote_id).unwrap_or(false) {
                        (poll_ready_bits(events as u32) & linux_abi::EPOLLIN) as i16
                    } else {
                        0
                    }
                } else {
                    (poll_ready_bits(events as u32) & (linux_abi::EPOLLIN | linux_abi::EPOLLOUT))
                        as i16
                };
                if bits != 0 {
                    ready += 1;
                }
                bits
            } else {
                (linux_abi::POLLERR | linux_abi::POLLHUP) as i16
            };
            response.payload[offset..offset + 6]
                .copy_from_slice(&request.payload[offset..offset + 6]);
            response.payload[offset + 6..offset + 8].copy_from_slice(&revents.to_le_bytes());
            response.payload[offset + 8..offset + 16]
                .copy_from_slice(&request.payload[offset + 8..offset + 16]);
        }
        response.value = ready;
        response.aux = self.readiness_generation;
        response.payload_len = len as u32;
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
                // A server that can no longer publish readiness would leave
                // waiters asleep behind a lost wake. Fail-stop with the same
                // diagnostic-then-exit protocol netd and inputd use, so rootd
                // restarts it instead of the panic path unwinding mid-request.
                ipc::debug_line("vfsd: readiness generation publication failed");
                rustos_svc_runtime::syscall::exit_group(134);
            }
        }
    }

    /// The retired offload envelope carries raw client paths. Canonicalize
    /// them against the caller's cwd exactly like the versioned VFS IPC does,
    /// so `..`, `//` and relative forms cannot dodge the exact-path routing
    /// (`/dev`, `/proc`, `/run`) in `VfsStorage::metadata`.
    fn linux_resolved_path(
        &self,
        request: &LinuxSyscallOffloadRequest,
        path: &str,
    ) -> Result<String, i32> {
        if path.is_empty() {
            return Err(ENOENT);
        }
        let cwd = self.cwd.get(&request.pid).map(String::as_str).unwrap_or("/");
        normalize_absolute_path(cwd, path)
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
        let path = match self.linux_resolved_path(request, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        match lock_vfs_storage().metadata(path.as_str()) {
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
        let path = match self.linux_resolved_path(request, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        match lock_vfs_storage().metadata(path.as_str()) {
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
        let path = match self.linux_resolved_path(request, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        response.status = match lock_vfs_storage().metadata(path.as_str()) {
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
        // Store the canonical absolute path: a raw `a/../b` or relative cwd
        // would later be re-joined as if it were rooted.
        response.status = match self.linux_resolved_path(request, path) {
            Ok(path) => self.chdir(request.pid, path.as_str()),
            Err(errno) => errno,
        };
    }

    fn linux_mount(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        response.status = EOPNOTSUPP;
    }

    fn linux_umount2(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        response.status = EOPNOTSUPP;
    }

}
