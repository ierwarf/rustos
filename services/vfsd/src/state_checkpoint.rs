// SPDX-License-Identifier: MIT

impl VfsState {
    fn new() -> Self {
        Self {
            volume: None,
            cwd: BTreeMap::new(),
            handles: BTreeMap::new(),
            metadata_cache: BTreeMap::new(),
            dir_entries_cache: BTreeMap::new(),
            file_bytes_cache: BTreeMap::new(),
            file_bytes_cache_bytes: 0,
            executable_snapshot_cache: BTreeMap::new(),
            executable_snapshot_cache_bytes: 0,
            epolls: WaitSetRegistry::default(),
            checkpoint_revisions: BTreeMap::new(),
            checkpoint_operations: BTreeMap::new(),
            checkpoint_records: BTreeMap::new(),
            readiness_generation: 1,
            next_handle: 1,
            mount_generation: 1,
            cache_generation: 1,
        }
    }

    fn restore_waitset_checkpoint(&mut self) -> Result<(), i32> {
        let mut cursor = 0_u64;
        let mut records = Vec::new();
        loop {
            let response = call_rootd_checkpoint(
                COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_SCAN,
                cursor,
                None,
            )?;
            let wire_size = size_of::<ServiceCheckpointRecordWire>();
            let record_count = response.payload_len as usize / wire_size;
            if !(response.payload_len as usize).is_multiple_of(wire_size)
                || response.value1 as usize != record_count
                || cursor
                    .checked_add(record_count as u64)
                    .is_none_or(|next| response.value0 != next)
            {
                return Err(EIO);
            }
            for offset in (0..response.payload_len as usize).step_by(wire_size) {
                let record = unsafe {
                    core::ptr::read_unaligned(
                        response.payload[offset..]
                            .as_ptr()
                            .cast::<ServiceCheckpointRecordWire>(),
                    )
                };
                let key = checkpoint_revision_key(&record);
                if !valid_checkpoint_record(&record)
                    || self
                        .checkpoint_revisions
                        .insert(key, record.revision)
                        .is_some()
                    || self
                        .checkpoint_operations
                        .insert(key, (record.operation_hi, record.operation_lo))
                        .is_some()
                    || self.checkpoint_records.insert(key, record).is_some()
                {
                    return Err(EIO);
                }
                records.push(record);
            }
            if response.value1 == 0 {
                break;
            }
            if response.value0 == cursor {
                return Err(EIO);
            }
            cursor = response.value0;
        }

        for record in records.iter().filter(|record| {
            record.parent_hi == 0
                && record.parent_lo == 0
                && record.key_lo == CHECKPOINT_EPOLL_TAG
                && record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
        }) {
            if record.value_len != 0 {
                return Err(EIO);
            }
            self.epolls.restore(record.key_hi).map_err(|_| EIO)?;
        }
        for record in records.iter().filter(|record| {
            record.parent_lo == CHECKPOINT_EPOLL_TAG
                && record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
        }) {
            if record.value_len as usize != size_of::<WaitSetInterestWire>() {
                return Err(EIO);
            }
            let wire = unsafe {
                core::ptr::read_unaligned(record.value.as_ptr().cast::<WaitSetInterestWire>())
            };
            let interest = waitset_interest_from_wire(&wire).ok_or(EIO)?;
            let (key_hi, key_lo) = checkpoint_interest_key(&interest);
            if record.key_hi != key_hi || record.key_lo != key_lo {
                return Err(EIO);
            }
            self.epolls
                .add(record.parent_hi, interest)
                .map_err(|_| EIO)?;
        }
        self.restore_open_descriptions(&records)?;
        Ok(())
    }

    fn restore_open_descriptions(
        &mut self,
        records: &[ServiceCheckpointRecordWire],
    ) -> Result<(), i32> {
        for record in records.iter().filter(|record| {
            record.parent_hi == 0
                && record.parent_lo == 0
                && record.key_lo == VFSD_CHECKPOINT_HANDLE_TAG
                && record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
        }) {
            if record.value_len as usize != size_of::<OpenDescriptionCheckpointWire>() {
                return Err(EIO);
            }
            let wire = unsafe {
                core::ptr::read_unaligned(
                    record
                        .value
                        .as_ptr()
                        .cast::<OpenDescriptionCheckpointWire>(),
                )
            };
            if !wire.valid(VFS_IPC_PATH_CAPACITY) {
                return Err(EIO);
            }
            // A staging parent is a recoverable interrupted OPEN. It remains
            // unavailable until an exact request completes every path chunk
            // and advances the parent to OPEN.
            if wire.last_mutation == VFSD_OPEN_MUTATION_STAGING {
                continue;
            }
            let path_len = wire.path_len as usize;
            let chunk_count = path_len.div_ceil(SERVICE_CHECKPOINT_VALUE_CAPACITY);
            let mut path_bytes = Vec::with_capacity(path_len);
            for chunk_index in 0..chunk_count {
                let (key_hi, key_lo) =
                    checkpoint_path_key(record.key_hi, chunk_index).ok_or(EIO)?;
                let child = records
                    .iter()
                    .find(|candidate| {
                        candidate.parent_hi == record.key_hi
                            && candidate.parent_lo == VFSD_CHECKPOINT_HANDLE_TAG
                            && candidate.key_hi == key_hi
                            && candidate.key_lo == key_lo
                            && candidate.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
                    })
                    .ok_or(EIO)?;
                let expected = (path_len - path_bytes.len()).min(SERVICE_CHECKPOINT_VALUE_CAPACITY);
                if child.value_len as usize != expected {
                    return Err(EIO);
                }
                path_bytes.extend_from_slice(&child.value[..expected]);
            }
            let live_children = records
                .iter()
                .filter(|candidate| {
                    candidate.parent_hi == record.key_hi
                        && candidate.parent_lo == VFSD_CHECKPOINT_HANDLE_TAG
                        && candidate.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
                })
                .count();
            if live_children != chunk_count || path_bytes.len() != path_len {
                return Err(EIO);
            }
            let path = str::from_utf8(&path_bytes).map_err(|_| EIO)?.to_string();
            if !path.starts_with('/') || path_inode(path.as_bytes()) != wire.content_identity {
                return Err(EIO);
            }
            let kind = remote_kind_from_u16(wire.kind).ok_or(EIO)?;
            if self
                .handles
                .insert(
                    record.key_hi,
                    RemoteHandle {
                        kind,
                        path,
                        cursor: wire.cursor,
                        len: wire.len,
                        refs: u64::from(wire.refs),
                        status_flags: wire.status_flags,
                        last_mutation: wire.last_mutation,
                        last_start: wire.last_start,
                        last_result: wire.last_result,
                    },
                )
                .is_some()
            {
                return Err(EIO);
            }
        }
        Ok(())
    }

    fn compact_checkpoint_proof(&mut self, proof: ServiceCheckpointRecordWire) -> Result<(), i32> {
        call_rootd_checkpoint(
            COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT,
            0,
            Some(&proof),
        )?;
        self.forget_compacted_record(proof);
        Ok(())
    }

    fn forget_compacted_record(&mut self, proof: ServiceCheckpointRecordWire) {
        let parent_key = checkpoint_revision_key(&proof);
        self.checkpoint_revisions.retain(|key, _| {
            !(*key == parent_key || (key.0, key.1) == (proof.key_hi, proof.key_lo))
        });
        self.checkpoint_operations.retain(|key, _| {
            !(*key == parent_key || (key.0, key.1) == (proof.key_hi, proof.key_lo))
        });
        self.checkpoint_records.retain(|key, _| {
            !(*key == parent_key || (key.0, key.1) == (proof.key_hi, proof.key_lo))
        });
    }

    fn checkpoint_mutate(
        &mut self,
        request: &VfsIpcRequest,
        record: ServiceCheckpointRecordWire,
    ) -> Result<bool, i32> {
        self.checkpoint_mutate_with_operation(record, request.operation_hi, request.operation_lo)
    }

    fn checkpoint_mutate_with_operation(
        &mut self,
        mut record: ServiceCheckpointRecordWire,
        operation_hi: u64,
        operation_lo: u64,
    ) -> Result<bool, i32> {
        let key = checkpoint_revision_key(&record);
        if let Some(current) = self.checkpoint_records.get(&key).copied() {
            if (current.operation_hi, current.operation_lo) == (operation_hi, operation_lo) {
                record.revision = current.revision;
                record.operation_hi = operation_hi;
                record.operation_lo = operation_lo;
                return if record == current {
                    Ok(true)
                } else {
                    Err(EIO)
                };
            }
        }
        let current = self.checkpoint_revisions.get(&key).copied().unwrap_or(0);
        record.revision = current.checked_add(1).ok_or(EOVERFLOW)?;
        record.operation_hi = operation_hi;
        record.operation_lo = operation_lo;
        let response = call_rootd_checkpoint(
            COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_MUTATE,
            0,
            Some(&record),
        )?;
        if response.payload_len != 0 || response.value0 != record.revision || response.value1 > 1 {
            return Err(EIO);
        }
        self.checkpoint_revisions.insert(key, record.revision);
        self.checkpoint_operations
            .insert(key, (record.operation_hi, record.operation_lo));
        self.checkpoint_records.insert(key, record);
        Ok(response.value1 == 1)
    }

    fn checkpoint_operation_replayed(
        &self,
        request: &VfsIpcRequest,
        key: CheckpointRevisionKey,
    ) -> bool {
        self.checkpoint_operations.get(&key).copied()
            == Some((request.operation_hi, request.operation_lo))
    }

    fn checkpoint_open_description(
        &mut self,
        request: &VfsIpcRequest,
        remote_id: u64,
        handle: &RemoteHandle,
    ) -> Result<(), i32> {
        let path_bytes = handle.path.as_bytes();
        if path_bytes.is_empty() || path_bytes.len() > VFS_IPC_PATH_CAPACITY {
            return Err(EINVAL);
        }
        let chunk_count = path_bytes.len().div_ceil(SERVICE_CHECKPOINT_VALUE_CAPACITY);
        let staging = checkpoint_handle_record(
            remote_id,
            handle,
            VFSD_OPEN_MUTATION_STAGING,
            request.arg0,
            0,
        )?;
        let (operation_hi, operation_lo) = checkpoint_suboperation(request, 0);
        self.checkpoint_mutate_with_operation(staging, operation_hi, operation_lo)?;
        for chunk_index in 0..chunk_count {
            let start = chunk_index * SERVICE_CHECKPOINT_VALUE_CAPACITY;
            let end = (start + SERVICE_CHECKPOINT_VALUE_CAPACITY).min(path_bytes.len());
            let record = checkpoint_path_record(remote_id, chunk_index, &path_bytes[start..end])?;
            let (operation_hi, operation_lo) =
                checkpoint_suboperation(request, chunk_index as u64 + 1);
            self.checkpoint_mutate_with_operation(record, operation_hi, operation_lo)?;
        }
        let final_record =
            checkpoint_handle_record(remote_id, handle, VFSD_OPEN_MUTATION_OPEN, request.arg0, 0)?;
        let (operation_hi, operation_lo) = checkpoint_suboperation(request, chunk_count as u64 + 1);
        self.checkpoint_mutate_with_operation(final_record, operation_hi, operation_lo)?;
        Ok(())
    }

    fn checkpoint_handle_state(
        &mut self,
        request: &VfsIpcRequest,
        remote_id: u64,
        handle: &RemoteHandle,
        mutation: u16,
        last_start: u64,
        last_result: u64,
    ) -> Result<bool, i32> {
        let record =
            checkpoint_handle_record(remote_id, handle, mutation, last_start, last_result)?;
        self.checkpoint_mutate(request, record)
    }

    fn checkpoint_close_description(
        &mut self,
        request: &VfsIpcRequest,
        remote_id: u64,
    ) -> Result<ServiceCheckpointRecordWire, i32> {
        let tombstone = checkpoint_handle_tombstone(remote_id)?;
        self.checkpoint_mutate(request, tombstone)?;
        let key = checkpoint_revision_key(&tombstone);
        self.checkpoint_records.get(&key).copied().ok_or(EIO)
    }

    fn invalidate_caches_if_remounted(&mut self) {
        if self.cache_generation != self.mount_generation {
            self.metadata_cache.clear();
            self.dir_entries_cache.clear();
            self.file_bytes_cache.clear();
            self.file_bytes_cache_bytes = 0;
            for snapshot in self.executable_snapshot_cache.values() {
                close_fd(snapshot.fd);
            }
            self.executable_snapshot_cache.clear();
            self.executable_snapshot_cache_bytes = 0;
            self.cache_generation = self.mount_generation;
        }
    }

    fn advance_mount_generation(&mut self) -> Result<(), i32> {
        self.mount_generation = checked_next_generation(self.mount_generation).ok_or(EOVERFLOW)?;
        Ok(())
    }

}
