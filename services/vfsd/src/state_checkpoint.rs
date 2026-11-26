// SPDX-License-Identifier: MIT

impl VfsState {
    fn new() -> Self {
        Self {
            billing: None,
            cwd: BTreeMap::new(),
            handles: BTreeMap::new(),
            inotify: vfsd::inotify::InotifyRegistry::new(),
            locks: vfsd::locks::LockTable::new(),
            checkpoint_revisions: BTreeMap::new(),
            checkpoint_operations: BTreeMap::new(),
            checkpoint_records: BTreeMap::new(),
            readiness_generation: 1,
            next_handle: 1,
        }
    }

    fn restore_checkpoints(&mut self) -> Result<(), i32> {
        // Rootd's checkpoint stream is untrusted until every parent, child,
        // and capacity check succeeds. Build a new state and publish it only
        // after the complete replay.
        let mut candidate = Self::new();
        let mut cursor = 0_u64;
        let mut records = Vec::new();
        loop {
            let response = call_rootd_checkpoint(
                COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_SCAN,
                cursor,
                None,
            )?;
            // A malformed reply length is an I/O fault, not a slice panic.
            let page = rustos_svc_runtime::checkpoint_idl::decode_framed(
                response
                    .payload
                    .get(..response.payload_len as usize)
                    .ok_or(EIO)?,
            )
            .map_err(|_| EIO)?;
            let record_count = page.len();
            if response.value1 as usize != record_count
                || cursor
                    .checked_add(record_count as u64)
                    .is_none_or(|next| response.value0 != next)
            {
                return Err(EIO);
            }
            for record in page {
                let key = checkpoint_revision_key(&record);
                if !valid_checkpoint_record(&record)
                    || candidate
                        .checkpoint_revisions
                        .insert(key, record.revision)
                        .is_some()
                    || candidate
                        .checkpoint_operations
                        .insert(key, (record.operation_hi, record.operation_lo))
                        .is_some()
                    || candidate.checkpoint_records.insert(key, record).is_some()
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

        candidate.restore_open_descriptions(&records)?;
        *self = candidate;
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
                // Records are unique per (parent, key), so a keyed lookup
                // replaces a scan of every record for every path chunk.
                let child = self
                    .checkpoint_records
                    .get(&(record.key_hi, VFSD_CHECKPOINT_HANDLE_TAG, key_hi, key_lo))
                    .filter(|child| child.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0)
                    .ok_or(EIO)?;
                let expected = (path_len - path_bytes.len()).min(SERVICE_CHECKPOINT_VALUE_CAPACITY);
                if child.value_len as usize != expected {
                    return Err(EIO);
                }
                path_bytes.extend_from_slice(&child.value[..expected]);
            }
            let live_children = self
                .checkpoint_records
                .range(
                    (record.key_hi, VFSD_CHECKPOINT_HANDLE_TAG, 0, 0)
                        ..=(record.key_hi, VFSD_CHECKPOINT_HANDLE_TAG, u64::MAX, u64::MAX),
                )
                .filter(|(_, child)| child.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0)
                .count();
            if live_children != chunk_count || path_bytes.len() != path_len {
                return Err(EIO);
            }
            let path = str::from_utf8(&path_bytes).map_err(|_| EIO)?.to_string();
            if !path.starts_with('/') || path_inode(path.as_bytes()) != wire.content_identity {
                return Err(EIO);
            }
            let kind = remote_kind_from_u16(wire.kind).ok_or(EIO)?;
            let writable = (wire.status_flags & linux_abi::O_ACCMODE) != linux_abi::O_RDONLY;
            let append = (wire.status_flags & linux_abi::O_APPEND) != 0;
            if self
                .handles
                .insert(
                    record.key_hi,
                    RemoteHandle {
                        checkpoint_version: wire.version,
                        stale: vfsd::mount_table::is_volatile_mount_path(&path)
                            || (wire.version < VFSD_OPEN_CHECKPOINT_VERSION
                                && kind == RemoteKind::Directory
                                && (wire.cursor != 0 || wire.last_mutation == VFSD_OPEN_MUTATION_GETDENTS)),
                        kind,
                        mount_id: 0,
                        writable,
                        append,
                        path,
                        node: 0,
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
        self.forget_checkpoint_key(parent_key);
        // The three mirrors publish and retire the same keys. Restrict lookup
        // to this parent's contiguous child range, without scanning unrelated
        // descriptions or allocating a temporary list of their keys.
        let first = (proof.key_hi, proof.key_lo, 0, 0);
        let last = (proof.key_hi, proof.key_lo, u64::MAX, u64::MAX);
        loop {
            let key = self.checkpoint_records.range(first..=last).next().map(|(key, _)| *key);
            let Some(key) = key else { break };
            self.forget_checkpoint_key(key);
        }
    }

    fn forget_checkpoint_key(&mut self, key: CheckpointRevisionKey) {
        self.checkpoint_revisions.remove(&key);
        self.checkpoint_operations.remove(&key);
        self.checkpoint_records.remove(&key);
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
        let staging = checkpoint_handle_record(
            remote_id,
            handle,
            VFSD_OPEN_MUTATION_STAGING,
            request.arg0,
            0,
        )?;
        let final_record =
            checkpoint_handle_record(remote_id, handle, VFSD_OPEN_MUTATION_OPEN, request.arg0, 0)?;
        // Staging parent, every path chunk and the final handle record go to
        // rootd as ordered MUTATE_BATCH requests (normally exactly one IPC)
        // with the same per-record DAG validation and journal order as
        // sequential single mutations. See `vfsd::checkpoint_batch`.
        let items = vfsd::checkpoint_batch::open_description_items(
            remote_id,
            path_bytes,
            staging,
            final_record,
            |ordinal| checkpoint_suboperation(request, ordinal),
        )?;
        let mut mirror = VfsCheckpointMirror {
            revisions: &mut self.checkpoint_revisions,
            operations: &mut self.checkpoint_operations,
            records: &mut self.checkpoint_records,
        };
        vfsd::checkpoint_batch::commit_ordered(&mut mirror, &mut RootdCheckpointTransport, &items)
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

}

/// vfsd's acknowledged-checkpoint mirror, advanced only by rootd-confirmed
/// records (see `vfsd::checkpoint_batch`).
struct VfsCheckpointMirror<'a> {
    revisions: &'a mut BTreeMap<CheckpointRevisionKey, u64>,
    operations: &'a mut BTreeMap<CheckpointRevisionKey, (u64, u64)>,
    records: &'a mut BTreeMap<CheckpointRevisionKey, ServiceCheckpointRecordWire>,
}

impl vfsd::checkpoint_batch::CheckpointMirror for VfsCheckpointMirror<'_> {
    fn current(&self, key: CheckpointRevisionKey) -> Option<ServiceCheckpointRecordWire> {
        self.records.get(&key).copied()
    }

    fn publish(&mut self, record: ServiceCheckpointRecordWire) {
        let key = checkpoint_revision_key(&record);
        self.revisions.insert(key, record.revision);
        self.operations
            .insert(key, (record.operation_hi, record.operation_lo));
        self.records.insert(key, record);
    }
}

impl VfsStorage {
    fn invalidate_caches_if_remounted(&mut self) {
        if self.cache_generation != self.mount_generation {
            self.metadata_cache.clear();
            self.metadata_cache_lru.clear();
            self.dir_entries_cache.clear();
            self.dir_entries_cache_lru.clear();
            self.file_bytes_cache.clear();
            self.file_bytes_cache_lru.clear();
            self.file_bytes_cache_bytes = 0;
            self.read_maps.clear();
            self.read_maps_lru.clear();
            self.read_map_bytes = 0;
            for snapshot in self.executable_snapshot_cache.values() {
                close_fd(snapshot.fd);
            }
            self.executable_snapshot_cache.clear();
            self.executable_snapshot_cache_bytes = 0;
            self.cache_generation = self.mount_generation;
        }
    }

}
