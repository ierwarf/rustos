//! Stateless regular-file read admission and cursor-sync helpers for `vfsd`.
//!
//! Lives in the library target so `cargo test -p vfsd --lib` executes the exact
//! state-transition and validation logic used by `state_storage.rs` and
//! `state_files.rs` (the `vfsd` binary target sets `test = false`).

use rustos_user_abi::syscall::{VFS_LSEEK_FLAG_SYNC_CURSOR, VFS_WRITE_FLAG_CURSOR_SYNC};

use crate::{
    EAGAIN, EINVAL, EIO, EOPNOTSUPP, VFSD_OPEN_MUTATION_GETDENTS, VFSD_OPEN_MUTATION_READ,
    VFSD_OPEN_MUTATION_STABLE,
};

const EBUSY: i32 = 16;

/// Outcome of validating a `VFS_CURSOR_SETTLE_SYNC` request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettleSyncOutcome {
    /// Exact operation id already committed at the matching cursor; reply
    /// idempotently without re-checkpointing to `rootd`.
    ReplayHit(u64),
    /// Commit `cursor` as `VFSD_OPEN_MUTATION_STABLE` and checkpoint to `rootd`.
    CommitCursor(u64),
}

/// Returns `true` when an open description currently holds a prepared
/// reversible cursor mutation (`getdents64` or legacy pre-migration `read`).
pub const fn is_cursor_mutation_prepared(last_mutation: u16) -> bool {
    matches!(
        last_mutation,
        VFSD_OPEN_MUTATION_READ | VFSD_OPEN_MUTATION_GETDENTS
    )
}

/// Validates the offset for a regular-file read (`VFS_IPC_OP_READ` vs
/// `VFS_IPC_OP_PREAD64`).
///
/// Regular files must use stateless positional reads (`PREAD64` / `BULK_READ`)
/// and never mutate `handle.cursor` or emit `VFSD_OPEN_MUTATION_READ`
/// checkpoints; legacy `VFS_IPC_OP_READ` on a regular file is rejected with
/// `EOPNOTSUPP`.
pub const fn admit_regular_file_read_offset(offset: Option<u64>) -> Result<u64, i32> {
    match offset {
        None => Err(EOPNOTSUPP),
        Some(off) if off > i64::MAX as u64 => Err(EINVAL),
        Some(off) => Ok(off),
    }
}

/// Validates and plans a `VFS_CURSOR_SETTLE_SYNC` request for a regular file.
#[allow(clippy::too_many_arguments)]
pub const fn plan_cursor_settle_sync(
    is_regular_file: bool,
    last_mutation: u16,
    current_cursor: u64,
    replayed: bool,
    arg0: u64,
    arg1: u64,
    arg3: u64,
    path_len: u32,
    payload_len: u32,
) -> Result<SettleSyncOutcome, i32> {
    if !is_regular_file
        || arg0 > i64::MAX as u64
        || arg1 != 0
        || arg3 != 0
        || path_len != 0
        || payload_len != 0
    {
        return Err(EINVAL);
    }
    if replayed {
        if last_mutation != VFSD_OPEN_MUTATION_STABLE || current_cursor != arg0 {
            return Err(EIO);
        }
        return Ok(SettleSyncOutcome::ReplayHit(current_cursor));
    }
    if is_cursor_mutation_prepared(last_mutation) {
        return Err(EBUSY);
    }
    Ok(SettleSyncOutcome::CommitCursor(arg0))
}

/// Resolves the starting cursor for `VFS_IPC_OP_LSEEK`.
///
/// When `VFS_LSEEK_FLAG_SYNC_CURSOR` is set, `arg2` carries the live kernel
/// cursor (validated `<= i64::MAX`) so `SEEK_CUR` computes relative to it.
pub const fn resolve_lseek_base_cursor(
    flags: u32,
    arg2: u64,
    handle_cursor: u64,
) -> Result<u64, i32> {
    if flags & VFS_LSEEK_FLAG_SYNC_CURSOR != 0 {
        if arg2 > i64::MAX as u64 {
            return Err(EINVAL);
        }
        Ok(arg2)
    } else {
        Ok(handle_cursor)
    }
}

/// Resolves the live cursor for `VFS_IPC_OP_WRITE`.
///
/// When `VFS_WRITE_FLAG_CURSOR_SYNC` is set, `arg0` carries the live kernel
/// cursor (validated `<= i64::MAX`) used for non-`O_APPEND` writes and for
/// zero-byte cursor preservation.
pub const fn resolve_write_base_cursor(
    flags: u32,
    arg0: u64,
    handle_cursor: u64,
) -> Result<u64, i32> {
    if flags & VFS_WRITE_FLAG_CURSOR_SYNC != 0 {
        if arg0 > i64::MAX as u64 {
            return Err(EINVAL);
        }
        Ok(arg0)
    } else {
        Ok(handle_cursor)
    }
}

/// Maps `EAGAIN` check for `vfs_read` if needed by callers; keeps `EAGAIN` used
/// cleanly across configurations.
pub const fn input_device_read_errno() -> i32 {
    EAGAIN
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;
    use rustos_user_abi::linux as linux_abi;
    use rustos_user_abi::syscall::{
        ServiceCheckpointRecordWire, VfsIpcRequest, VfsIpcResponse, VFS_BULK_READ_MAX_BYTES,
        VFS_CURSOR_SETTLE_CANCEL, VFS_CURSOR_SETTLE_COMMIT, VFS_CURSOR_SETTLE_SYNC,
        VFS_IPC_ABI_VERSION, VFS_IPC_HANDLE_KIND_DEVICE, VFS_IPC_HANDLE_KIND_DIR,
        VFS_IPC_HANDLE_KIND_FILE, VFS_IPC_HANDLE_KIND_INOTIFY, VFS_IPC_OP_BULK_READ,
        VFS_IPC_OP_CURSOR_SETTLE, VFS_IPC_OP_LSEEK, VFS_IPC_OP_PREAD64, VFS_IPC_OP_READ,
        VFS_IPC_OP_WRITE, VFS_IPC_PATH_CAPACITY, VFS_IPC_PAYLOAD_CAPACITY,
    };

    use crate::{
        checked_seek_position, cursor_after_write, validate_bulk_read_request_bounds,
        OpenDescriptionCheckpointWire, SeekPositionError, VFSD_CHECKPOINT_HANDLE_TAG,
        VFSD_OPEN_CHECKPOINT_VERSION, VFSD_OPEN_MUTATION_LSEEK, VFSD_OPEN_MUTATION_OPEN,
        VFSD_OPEN_MUTATION_STABLE,
    };

    const EBADF: i32 = 9;
    const EOVERFLOW: i32 = 75;

    #[derive(Clone)]
    struct TestHandle {
        kind: u16,
        writable: bool,
        append: bool,
        path: String,
        cursor: u64,
        len: u64,
        status_flags: u64,
        last_mutation: u16,
        last_start: u64,
        last_result: u64,
    }

    #[derive(Default)]
    struct MockRootd {
        mutate_calls: usize,
        records: BTreeMap<(u64, u64), ServiceCheckpointRecordWire>,
    }

    impl MockRootd {
        fn mutate(
            &mut self,
            remote_id: u64,
            handle: &TestHandle,
            mutation: u16,
            last_start: u64,
            last_result: u64,
            op_hi: u64,
            op_lo: u64,
        ) {
            self.mutate_calls += 1;
            let key = (remote_id, VFSD_CHECKPOINT_HANDLE_TAG);
            let prev_rev = self.records.get(&key).map_or(0, |r| r.revision);
            let wire = OpenDescriptionCheckpointWire {
                version: VFSD_OPEN_CHECKPOINT_VERSION,
                kind: handle.kind,
                last_mutation: mutation,
                reserved0: 0,
                path_len: handle.path.len() as u32,
                refs: 1,
                cursor: handle.cursor,
                len: handle.len,
                status_flags: handle.status_flags,
                content_identity: 0x1234_5678,
                last_start,
                last_result,
            };
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (&wire as *const OpenDescriptionCheckpointWire).cast::<u8>(),
                    core::mem::size_of::<OpenDescriptionCheckpointWire>(),
                )
            };
            let mut rec = ServiceCheckpointRecordWire {
                key_hi: remote_id,
                key_lo: VFSD_CHECKPOINT_HANDLE_TAG,
                operation_hi: op_hi,
                operation_lo: op_lo,
                revision: prev_rev + 1,
                value_len: bytes.len() as u32,
                ..ServiceCheckpointRecordWire::default()
            };
            rec.value[..bytes.len()].copy_from_slice(bytes);
            self.records.insert(key, rec);
        }
    }

    struct TestVfs {
        handles: BTreeMap<u64, TestHandle>,
        ops: BTreeMap<u64, (u64, u64)>,
        files: BTreeMap<String, Vec<u8>>,
        inotify_events: BTreeMap<u64, Vec<u8>>,
        rootd: MockRootd,
    }

    impl TestVfs {
        fn new() -> Self {
            Self {
                handles: BTreeMap::new(),
                ops: BTreeMap::new(),
                files: BTreeMap::new(),
                inotify_events: BTreeMap::new(),
                rootd: MockRootd::default(),
            }
        }

        fn open_file(&mut self, id: u64, path: &str, data: Vec<u8>, writable: bool, append: bool) {
            let len = data.len() as u64;
            self.files.insert(path.to_string(), data);
            let status_flags = if writable {
                linux_abi::O_RDWR
            } else {
                linux_abi::O_RDONLY
            } | if append { linux_abi::O_APPEND } else { 0 };
            let handle = TestHandle {
                kind: VFS_IPC_HANDLE_KIND_FILE,
                writable,
                append,
                path: path.to_string(),
                cursor: 0,
                len,
                status_flags,
                last_mutation: VFSD_OPEN_MUTATION_OPEN,
                last_start: status_flags,
                last_result: 0,
            };
            self.rootd
                .mutate(id, &handle, VFSD_OPEN_MUTATION_OPEN, status_flags, 0, 1, id);
            self.ops.insert(id, (1, id));
            self.handles.insert(id, handle);
        }

        fn open_non_file(&mut self, id: u64, kind: u16, path: &str) {
            let handle = TestHandle {
                kind,
                writable: false,
                append: false,
                path: path.to_string(),
                cursor: 0,
                len: 0,
                status_flags: 0,
                last_mutation: VFSD_OPEN_MUTATION_OPEN,
                last_start: 0,
                last_result: 0,
            };
            self.ops.insert(id, (1, id));
            self.handles.insert(id, handle);
        }

        fn restart_from_rootd(&self) -> Self {
            let mut restarted = Self::new();
            restarted.files = self.files.clone();
            restarted.rootd.mutate_calls = self.rootd.mutate_calls;
            restarted.rootd.records = self.rootd.records.clone();
            for (&(id, tag), rec) in &self.rootd.records {
                assert_eq!(tag, VFSD_CHECKPOINT_HANDLE_TAG);
                let wire = unsafe {
                    core::ptr::read_unaligned(
                        rec.value.as_ptr().cast::<OpenDescriptionCheckpointWire>(),
                    )
                };
                assert!(wire.valid(VFS_IPC_PATH_CAPACITY));
                let orig = self.handles.get(&id).unwrap();
                restarted.ops.insert(id, (rec.operation_hi, rec.operation_lo));
                restarted.handles.insert(
                    id,
                    TestHandle {
                        kind: wire.kind,
                        writable: (wire.status_flags & linux_abi::O_ACCMODE) != linux_abi::O_RDONLY,
                        append: (wire.status_flags & linux_abi::O_APPEND) != 0,
                        path: orig.path.clone(),
                        cursor: wire.cursor,
                        len: wire.len,
                        status_flags: wire.status_flags,
                        last_mutation: wire.last_mutation,
                        last_start: wire.last_start,
                        last_result: wire.last_result,
                    },
                );
            }
            restarted
        }

        fn dispatch_read(&mut self, req: &VfsIpcRequest) -> VfsIpcResponse {
            let mut resp = VfsIpcResponse {
                version: VFS_IPC_ABI_VERSION,
                op: req.op,
                ..VfsIpcResponse::default()
            };
            let offset = match req.op {
                VFS_IPC_OP_READ => None,
                VFS_IPC_OP_PREAD64 => Some(req.arg0),
                _ => {
                    resp.status = EINVAL;
                    return resp;
                }
            };
            let Some(handle) = self.handles.get(&req.remote_id) else {
                resp.status = EBADF;
                return resp;
            };
            if handle.kind == VFS_IPC_HANDLE_KIND_INOTIFY {
                if offset.is_some() {
                    resp.status = 29; // ESPIPE
                    return resp;
                }
                let ev = self.inotify_events.get(&req.remote_id).cloned().unwrap_or_default();
                let n = (req.arg1 as usize).min(ev.len()).min(VFS_IPC_PAYLOAD_CAPACITY);
                resp.payload[..n].copy_from_slice(&ev[..n]);
                resp.payload_len = n as u32;
                resp.value = n as u64;
                return resp;
            }
            if handle.kind == VFS_IPC_HANDLE_KIND_DEVICE && handle.path == "/dev/zero" {
                let n = (req.arg1 as usize).min(VFS_IPC_PAYLOAD_CAPACITY);
                resp.payload[..n].fill(0);
                resp.payload_len = n as u32;
                resp.value = n as u64;
                return resp;
            }
            if handle.kind != VFS_IPC_HANDLE_KIND_FILE {
                resp.status = 21; // EISDIR
                return resp;
            }
            let start = match admit_regular_file_read_offset(offset) {
                Ok(start) => start,
                Err(errno) => {
                    resp.status = errno;
                    return resp;
                }
            };
            let data = self.files.get(&handle.path).unwrap();
            let avail = (data.len() as u64).saturating_sub(start);
            let want = (req.arg1 as usize)
                .min(usize::try_from(avail).unwrap_or(usize::MAX))
                .min(VFS_IPC_PAYLOAD_CAPACITY);
            resp.payload[..want].copy_from_slice(&data[start as usize..start as usize + want]);
            resp.payload_len = want as u32;
            resp.value = want as u64;
            resp
        }

        fn dispatch_bulk_read(&self, req: &VfsIpcRequest) -> Result<Vec<u8>, i32> {
            assert_eq!(req.op, VFS_IPC_OP_BULK_READ);
            let handle = self.handles.get(&req.remote_id).ok_or(EBADF)?;
            if handle.kind != VFS_IPC_HANDLE_KIND_FILE {
                return Err(EOPNOTSUPP);
            }
            let data = self.files.get(&handle.path).unwrap();
            let count = validate_bulk_read_request_bounds(
                req.arg0,
                req.arg1,
                data.len() as u64,
                VFS_BULK_READ_MAX_BYTES,
            )?;
            Ok(data[req.arg0 as usize..req.arg0 as usize + count].to_vec())
        }

        fn dispatch_cursor_settle(&mut self, req: &VfsIpcRequest) -> VfsIpcResponse {
            let mut resp = VfsIpcResponse {
                version: VFS_IPC_ABI_VERSION,
                op: VFS_IPC_OP_CURSOR_SETTLE,
                ..VfsIpcResponse::default()
            };
            let Some(handle) = self.handles.get(&req.remote_id).cloned() else {
                resp.status = EBADF;
                return resp;
            };
            let replayed = self.ops.get(&req.remote_id).copied()
                == Some((req.operation_hi, req.operation_lo));
            if req.arg2 == VFS_CURSOR_SETTLE_SYNC {
                match plan_cursor_settle_sync(
                    handle.kind == VFS_IPC_HANDLE_KIND_FILE,
                    handle.last_mutation,
                    handle.cursor,
                    replayed,
                    req.arg0,
                    req.arg1,
                    req.arg3,
                    req.path_len,
                    req.payload_len,
                ) {
                    Ok(SettleSyncOutcome::ReplayHit(cursor)) => {
                        resp.value = cursor;
                    }
                    Ok(SettleSyncOutcome::CommitCursor(cursor)) => {
                        let mut candidate = handle;
                        candidate.cursor = cursor;
                        candidate.last_mutation = VFSD_OPEN_MUTATION_STABLE;
                        candidate.last_start = 0;
                        candidate.last_result = 0;
                        self.rootd.mutate(
                            req.remote_id,
                            &candidate,
                            VFSD_OPEN_MUTATION_STABLE,
                            0,
                            0,
                            req.operation_hi,
                            req.operation_lo,
                        );
                        self.ops.insert(req.remote_id, (req.operation_hi, req.operation_lo));
                        self.handles.insert(req.remote_id, candidate);
                        resp.value = cursor;
                    }
                    Err(errno) => resp.status = errno,
                }
                return resp;
            }
            if replayed {
                if handle.last_mutation != VFSD_OPEN_MUTATION_STABLE {
                    resp.status = EIO;
                } else {
                    resp.value = handle.cursor;
                }
                return resp;
            }
            if !is_cursor_mutation_prepared(handle.last_mutation)
                || self.ops.get(&req.remote_id).copied() != Some((req.arg0, req.arg1))
                || !matches!(req.arg2, VFS_CURSOR_SETTLE_COMMIT | VFS_CURSOR_SETTLE_CANCEL)
                || req.arg3 != 0
                || req.path_len != 0
                || req.payload_len != 0
            {
                resp.status = EINVAL;
                return resp;
            }
            let mut candidate = handle;
            if req.arg2 == VFS_CURSOR_SETTLE_CANCEL {
                candidate.cursor = candidate.last_start;
            }
            candidate.last_mutation = VFSD_OPEN_MUTATION_STABLE;
            candidate.last_start = 0;
            candidate.last_result = 0;
            self.rootd.mutate(
                req.remote_id,
                &candidate,
                VFSD_OPEN_MUTATION_STABLE,
                0,
                0,
                req.operation_hi,
                req.operation_lo,
            );
            self.ops.insert(req.remote_id, (req.operation_hi, req.operation_lo));
            resp.value = candidate.cursor;
            self.handles.insert(req.remote_id, candidate);
            resp
        }

        fn dispatch_lseek(&mut self, req: &VfsIpcRequest) -> VfsIpcResponse {
            let mut resp = VfsIpcResponse {
                version: VFS_IPC_ABI_VERSION,
                op: VFS_IPC_OP_LSEEK,
                ..VfsIpcResponse::default()
            };
            let Some(handle) = self.handles.get(&req.remote_id).cloned() else {
                resp.status = EBADF;
                return resp;
            };
            let base_cursor = match resolve_lseek_base_cursor(req.flags, req.arg2, handle.cursor) {
                Ok(c) => c,
                Err(errno) => {
                    resp.status = errno;
                    return resp;
                }
            };
            if self.ops.get(&req.remote_id).copied() == Some((req.operation_hi, req.operation_lo)) {
                if handle.last_mutation != VFSD_OPEN_MUTATION_LSEEK
                    || handle.last_start != req.arg0
                    || handle.last_result != req.arg1
                {
                    resp.status = EIO;
                } else {
                    resp.value = handle.cursor;
                }
                return resp;
            }
            if is_cursor_mutation_prepared(handle.last_mutation) {
                resp.status = EBUSY;
                return resp;
            }
            let file_len = self
                .files
                .get(&handle.path)
                .map_or(handle.len, |b| b.len() as u64);
            let next = match checked_seek_position(base_cursor, file_len, req.arg0 as i64, req.arg1)
            {
                Ok(n) => n,
                Err(SeekPositionError::InvalidWhence | SeekPositionError::Negative) => {
                    resp.status = EINVAL;
                    return resp;
                }
                Err(SeekPositionError::Overflow) => {
                    resp.status = EOVERFLOW;
                    return resp;
                }
            };
            let mut candidate = handle;
            candidate.cursor = next;
            candidate.last_mutation = VFSD_OPEN_MUTATION_LSEEK;
            candidate.last_start = req.arg0;
            candidate.last_result = req.arg1;
            self.rootd.mutate(
                req.remote_id,
                &candidate,
                VFSD_OPEN_MUTATION_LSEEK,
                req.arg0,
                req.arg1,
                req.operation_hi,
                req.operation_lo,
            );
            self.ops.insert(req.remote_id, (req.operation_hi, req.operation_lo));
            resp.value = candidate.cursor;
            self.handles.insert(req.remote_id, candidate);
            resp
        }

        fn dispatch_write(&mut self, req: &VfsIpcRequest) -> VfsIpcResponse {
            let mut resp = VfsIpcResponse {
                version: VFS_IPC_ABI_VERSION,
                op: VFS_IPC_OP_WRITE,
                ..VfsIpcResponse::default()
            };
            let Some(handle) = self.handles.get(&req.remote_id).cloned() else {
                resp.status = EBADF;
                return resp;
            };
            if !handle.writable {
                resp.status = EBADF;
                return resp;
            }
            let sync_cursor = (req.flags & VFS_WRITE_FLAG_CURSOR_SYNC) != 0;
            let base_cursor = match resolve_write_base_cursor(req.flags, req.arg0, handle.cursor) {
                Ok(c) => c,
                Err(errno) => {
                    resp.status = errno;
                    return resp;
                }
            };
            if self.ops.get(&req.remote_id).copied() == Some((req.operation_hi, req.operation_lo)) {
                if handle.last_mutation != VFSD_OPEN_MUTATION_STABLE {
                    resp.status = EIO;
                } else {
                    resp.value = handle.last_result;
                    resp.aux = handle.cursor;
                }
                return resp;
            }
            if is_cursor_mutation_prepared(handle.last_mutation) {
                resp.status = EBUSY;
                return resp;
            }
            let len = (req.payload_len as usize).min(req.payload.len());
            if len == 0 {
                if sync_cursor {
                    let mut candidate = handle;
                    candidate.cursor = base_cursor;
                    candidate.last_mutation = VFSD_OPEN_MUTATION_STABLE;
                    candidate.last_start = base_cursor;
                    candidate.last_result = 0;
                    self.rootd.mutate(
                        req.remote_id,
                        &candidate,
                        VFSD_OPEN_MUTATION_STABLE,
                        base_cursor,
                        0,
                        req.operation_hi,
                        req.operation_lo,
                    );
                    self.ops.insert(req.remote_id, (req.operation_hi, req.operation_lo));
                    resp.value = 0;
                    resp.aux = candidate.cursor;
                    self.handles.insert(req.remote_id, candidate);
                } else {
                    resp.value = 0;
                    resp.aux = handle.cursor;
                }
                return resp;
            }
            let data = &req.payload[..len];
            let file = self.files.get_mut(&handle.path).unwrap();
            let start = if handle.append {
                file.len() as u64
            } else {
                base_cursor
            };
            if cursor_after_write(base_cursor, start, len, len).is_none() {
                resp.status = EOVERFLOW;
                return resp;
            }
            let end = start as usize + len;
            if end > file.len() {
                file.resize(end, 0);
            }
            file[start as usize..end].copy_from_slice(data);
            let new_len = file.len() as u64;
            let mut candidate = handle;
            candidate.cursor = cursor_after_write(base_cursor, start, len, len).unwrap();
            candidate.len = new_len;
            candidate.last_mutation = VFSD_OPEN_MUTATION_STABLE;
            candidate.last_start = start;
            candidate.last_result = len as u64;
            self.rootd.mutate(
                req.remote_id,
                &candidate,
                VFSD_OPEN_MUTATION_STABLE,
                start,
                len as u64,
                req.operation_hi,
                req.operation_lo,
            );
            self.ops.insert(req.remote_id, (req.operation_hi, req.operation_lo));
            resp.value = len as u64;
            resp.aux = candidate.cursor;
            self.handles.insert(req.remote_id, candidate);
            resp
        }
    }

    fn req(op: u16, remote_id: u64, op_lo: u64) -> VfsIpcRequest {
        VfsIpcRequest {
            op,
            remote_id,
            operation_hi: 10,
            operation_lo: op_lo,
            ..VfsIpcRequest::default()
        }
    }

    #[test]
    fn sequential_pread64_and_bulk_read_emit_zero_rootd_checkpoints_and_reject_file_read() {
        let mut vfs = TestVfs::new();
        let content: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
        vfs.open_file(7, "/data.bin", content.clone(), false, false);
        let initial_checkpoints = vfs.rootd.mutate_calls;
        assert_eq!(initial_checkpoints, 1);

        // Sequential PREAD64 calls across the entire file + EOF.
        let mut offset = 0_u64;
        let mut assembled = Vec::new();
        for seq in 2_u64..10 {
            let mut r = req(VFS_IPC_OP_PREAD64, 7, seq);
            r.arg0 = offset;
            r.arg1 = 256;
            let resp = vfs.dispatch_read(&r);
            assert_eq!(resp.status, 0);
            if resp.value == 0 {
                break;
            }
            assembled.extend_from_slice(&resp.payload[..resp.payload_len as usize]);
            offset += resp.value;
        }
        assert_eq!(assembled, content);
        assert_eq!(vfs.rootd.mutate_calls, initial_checkpoints);
        assert_eq!(vfs.handles.get(&7).unwrap().cursor, 0);

        // Sequential BULK_READ calls produce zero checkpoints and keep cursor unchanged.
        let mut bulk_assembled = Vec::new();
        for chunk_off in [0_u64, 512, 1024] {
            let mut r = req(VFS_IPC_OP_BULK_READ, 7, 20 + chunk_off);
            r.arg0 = chunk_off;
            r.arg1 = 512;
            let bytes = vfs.dispatch_bulk_read(&r).unwrap();
            bulk_assembled.extend_from_slice(&bytes);
        }
        assert_eq!(bulk_assembled, content);
        assert_eq!(vfs.rootd.mutate_calls, initial_checkpoints);
        assert_eq!(vfs.handles.get(&7).unwrap().cursor, 0);

        // Legacy VFS_IPC_OP_READ on regular files is rejected without checkpointing.
        let mut legacy_read = req(VFS_IPC_OP_READ, 7, 99);
        legacy_read.arg1 = 64;
        let legacy_resp = vfs.dispatch_read(&legacy_read);
        assert_eq!(legacy_resp.status, EOPNOTSUPP);
        assert_eq!(vfs.rootd.mutate_calls, initial_checkpoints);

        // VFS_IPC_OP_READ continues working for Inotify and Device handles.
        vfs.open_non_file(8, VFS_IPC_HANDLE_KIND_INOTIFY, "/anon_inode/inotify");
        vfs.inotify_events.insert(8, vec![1, 2, 3, 4]);
        let mut ino_read = req(VFS_IPC_OP_READ, 8, 100);
        ino_read.arg1 = 16;
        let ino_resp = vfs.dispatch_read(&ino_read);
        assert_eq!(ino_resp.status, 0);
        assert_eq!(ino_resp.value, 4);
        assert_eq!(&ino_resp.payload[..4], &[1, 2, 3, 4]);

        vfs.open_non_file(9, VFS_IPC_HANDLE_KIND_DEVICE, "/dev/zero");
        let mut dev_read = req(VFS_IPC_OP_READ, 9, 101);
        dev_read.arg1 = 8;
        let dev_resp = vfs.dispatch_read(&dev_read);
        assert_eq!(dev_resp.status, 0);
        assert_eq!(dev_resp.value, 8);
        assert_eq!(&dev_resp.payload[..8], &[0; 8]);
        assert_eq!(vfs.rootd.mutate_calls, initial_checkpoints);

        // Legacy VFSD_OPEN_MUTATION_READ remains decodeable in checkpoint wires.
        let legacy_wire = OpenDescriptionCheckpointWire {
            version: VFSD_OPEN_CHECKPOINT_VERSION,
            kind: VFS_IPC_HANDLE_KIND_FILE,
            last_mutation: VFSD_OPEN_MUTATION_READ,
            reserved0: 0,
            path_len: 9,
            refs: 1,
            cursor: 128,
            len: 1024,
            status_flags: 0,
            content_identity: 42,
            last_start: 0,
            last_result: 128,
        };
        assert!(legacy_wire.valid(VFS_IPC_PATH_CAPACITY));
    }

    #[test]
    fn cursor_settle_sync_is_durable_replay_idempotent_and_validates_inputs() {
        let mut vfs = TestVfs::new();
        vfs.open_file(7, "/sync.bin", vec![0; 8192], true, false);
        vfs.open_non_file(8, VFS_IPC_HANDLE_KIND_DIR, "/dir");
        vfs.open_non_file(9, VFS_IPC_HANDLE_KIND_DEVICE, "/dev/null");
        vfs.open_non_file(10, VFS_IPC_HANDLE_KIND_INOTIFY, "/anon_inode/inotify");

        // Reject non-regular-file handles with EINVAL.
        for bad_id in [8_u64, 9, 10] {
            let mut r = req(VFS_IPC_OP_CURSOR_SETTLE, bad_id, 2);
            r.arg0 = 100;
            r.arg2 = VFS_CURSOR_SETTLE_SYNC;
            assert_eq!(vfs.dispatch_cursor_settle(&r).status, EINVAL);
        }

        // Reject arg0 > i64::MAX with EINVAL.
        let mut neg_off = req(VFS_IPC_OP_CURSOR_SETTLE, 7, 3);
        neg_off.arg0 = (i64::MAX as u64) + 1;
        neg_off.arg2 = VFS_CURSOR_SETTLE_SYNC;
        assert_eq!(vfs.dispatch_cursor_settle(&neg_off).status, EINVAL);

        // Reject prepared handles with EBUSY.
        vfs.handles.get_mut(&7).unwrap().last_mutation = VFSD_OPEN_MUTATION_READ;
        let mut busy_req = req(VFS_IPC_OP_CURSOR_SETTLE, 7, 4);
        busy_req.arg0 = 4096;
        busy_req.arg2 = VFS_CURSOR_SETTLE_SYNC;
        assert_eq!(vfs.dispatch_cursor_settle(&busy_req).status, EBUSY);
        vfs.handles.get_mut(&7).unwrap().last_mutation = VFSD_OPEN_MUTATION_OPEN;

        // Directory COMMIT and CANCEL continue working as before.
        vfs.handles.get_mut(&8).unwrap().last_mutation = crate::VFSD_OPEN_MUTATION_GETDENTS;
        vfs.handles.get_mut(&8).unwrap().last_start = 12;
        vfs.handles.get_mut(&8).unwrap().cursor = 24;
        vfs.ops.insert(8, (55, 66));
        let mut dir_cancel = req(VFS_IPC_OP_CURSOR_SETTLE, 8, 5);
        dir_cancel.arg0 = 55;
        dir_cancel.arg1 = 66;
        dir_cancel.arg2 = VFS_CURSOR_SETTLE_CANCEL;
        let dir_resp = vfs.dispatch_cursor_settle(&dir_cancel);
        assert_eq!(dir_resp.status, 0);
        assert_eq!(dir_resp.value, 12);

        // Successful VFS_CURSOR_SETTLE_SYNC checkpoints to rootd and survives restart.
        let before_sync = vfs.rootd.mutate_calls;
        let mut sync_req = req(VFS_IPC_OP_CURSOR_SETTLE, 7, 77);
        sync_req.arg0 = 4096;
        sync_req.arg2 = VFS_CURSOR_SETTLE_SYNC;
        let sync_resp = vfs.dispatch_cursor_settle(&sync_req);
        assert_eq!(sync_resp.status, 0);
        assert_eq!(sync_resp.value, 4096);
        assert_eq!(vfs.rootd.mutate_calls, before_sync + 1);

        let mut restarted = vfs.restart_from_rootd();
        let restored = restarted.handles.get(&7).unwrap();
        assert_eq!(restored.cursor, 4096);
        assert_eq!(restored.last_mutation, VFSD_OPEN_MUTATION_STABLE);
        assert_eq!(restored.last_start, 0);
        assert_eq!(restored.last_result, 0);

        // Replay with same (operation_hi, operation_lo) is idempotent with 0 new checkpoints.
        let replay_resp = restarted.dispatch_cursor_settle(&sync_req);
        assert_eq!(replay_resp.status, 0);
        assert_eq!(replay_resp.value, 4096);
        assert_eq!(restarted.rootd.mutate_calls, before_sync + 1);

        // Replay with mismatched arg0 fails with EIO.
        let mut mismatch_req = sync_req;
        mismatch_req.arg0 = 4097;
        assert_eq!(restarted.dispatch_cursor_settle(&mismatch_req).status, EIO);
    }

    #[test]
    fn lseek_and_write_with_cursor_sync_flags_update_rootd_and_return_aux() {
        let mut vfs = TestVfs::new();
        vfs.open_file(7, "/rw.bin", b"0123456789".to_vec(), true, false);
        let initial_checkpoints = vfs.rootd.mutate_calls;

        // Invalid sync cursor (> i64::MAX) is rejected by both lseek and write.
        let mut bad_seek = req(VFS_IPC_OP_LSEEK, 7, 2);
        bad_seek.flags = VFS_LSEEK_FLAG_SYNC_CURSOR;
        bad_seek.arg0 = 0;
        bad_seek.arg1 = 1; // SEEK_CUR
        bad_seek.arg2 = (i64::MAX as u64) + 1;
        assert_eq!(vfs.dispatch_lseek(&bad_seek).status, EINVAL);

        let mut bad_write = req(VFS_IPC_OP_WRITE, 7, 3);
        bad_write.flags = VFS_WRITE_FLAG_CURSOR_SYNC;
        bad_write.arg0 = (i64::MAX as u64) + 1;
        bad_write.payload_len = 2;
        bad_write.payload[..2].copy_from_slice(b"ab");
        assert_eq!(vfs.dispatch_write(&bad_write).status, EINVAL);
        assert_eq!(vfs.rootd.mutate_calls, initial_checkpoints);

        // Simulate PREAD64 advancing kernel cursor to 6 while handle.cursor is 0,
        // then lseek(SEEK_CUR, -2) with VFS_LSEEK_FLAG_SYNC_CURSOR.
        let mut seek_cur = req(VFS_IPC_OP_LSEEK, 7, 4);
        seek_cur.flags = VFS_LSEEK_FLAG_SYNC_CURSOR;
        seek_cur.arg0 = (-2_i64) as u64;
        seek_cur.arg1 = 1; // SEEK_CUR
        seek_cur.arg2 = 6;
        let seek_resp = vfs.dispatch_lseek(&seek_cur);
        assert_eq!(seek_resp.status, 0);
        assert_eq!(seek_resp.value, 4);
        assert_eq!(vfs.handles.get(&7).unwrap().cursor, 4);
        assert_eq!(
            vfs.handles.get(&7).unwrap().last_mutation,
            VFSD_OPEN_MUTATION_LSEEK
        );

        // SEEK_SET and SEEK_END override cursor regardless of arg2.
        let mut seek_end = req(VFS_IPC_OP_LSEEK, 7, 5);
        seek_end.flags = VFS_LSEEK_FLAG_SYNC_CURSOR;
        seek_end.arg0 = (-3_i64) as u64;
        seek_end.arg1 = 2; // SEEK_END
        seek_end.arg2 = 1;
        let end_resp = vfs.dispatch_lseek(&seek_end);
        assert_eq!(end_resp.status, 0);
        assert_eq!(end_resp.value, 7);

        let mut seek_set = req(VFS_IPC_OP_LSEEK, 7, 6);
        seek_set.flags = VFS_LSEEK_FLAG_SYNC_CURSOR;
        seek_set.arg0 = 2;
        seek_set.arg1 = 0; // SEEK_SET
        seek_set.arg2 = 9;
        let set_resp = vfs.dispatch_lseek(&seek_set);
        assert_eq!(set_resp.status, 0);
        assert_eq!(set_resp.value, 2);

        // Non-O_APPEND write with VFS_WRITE_FLAG_CURSOR_SYNC writes at arg0=5
        // (even though handle.cursor is 2) and returns written in value, new cursor in aux.
        let mut write_req = req(VFS_IPC_OP_WRITE, 7, 7);
        write_req.flags = VFS_WRITE_FLAG_CURSOR_SYNC;
        write_req.arg0 = 5;
        write_req.payload_len = 3;
        write_req.payload[..3].copy_from_slice(b"XYZ");
        let write_resp = vfs.dispatch_write(&write_req);
        assert_eq!(write_resp.status, 0);
        assert_eq!(write_resp.value, 3);
        assert_eq!(write_resp.aux, 8);
        assert_eq!(vfs.files.get("/rw.bin").unwrap().as_slice(), b"01234XYZ89");
        assert_eq!(vfs.handles.get(&7).unwrap().cursor, 8);

        // Zero-byte write with VFS_WRITE_FLAG_CURSOR_SYNC preserves arg0 in aux and rootd.
        let mut zero_write = req(VFS_IPC_OP_WRITE, 7, 8);
        zero_write.flags = VFS_WRITE_FLAG_CURSOR_SYNC;
        zero_write.arg0 = 9;
        zero_write.payload_len = 0;
        let zero_resp = vfs.dispatch_write(&zero_write);
        assert_eq!(zero_resp.status, 0);
        assert_eq!(zero_resp.value, 0);
        assert_eq!(zero_resp.aux, 9);
        assert_eq!(vfs.handles.get(&7).unwrap().cursor, 9);

        // O_APPEND write with VFS_WRITE_FLAG_CURSOR_SYNC writes at live EOF atomically,
        // ignoring arg0 as write offset, and sets response.aux = eof + written.
        vfs.open_file(11, "/append.bin", b"hello".to_vec(), true, true);
        let mut append_req = req(VFS_IPC_OP_WRITE, 11, 9);
        append_req.flags = VFS_WRITE_FLAG_CURSOR_SYNC;
        append_req.arg0 = 2; // live kernel cursor before O_APPEND write
        append_req.payload_len = 6;
        append_req.payload[..6].copy_from_slice(b"_world");
        let append_resp = vfs.dispatch_write(&append_req);
        assert_eq!(append_resp.status, 0);
        assert_eq!(append_resp.value, 6);
        assert_eq!(append_resp.aux, 11);
        assert_eq!(
            vfs.files.get("/append.bin").unwrap().as_slice(),
            b"hello_world"
        );
        assert_eq!(vfs.handles.get(&11).unwrap().cursor, 11);

        // Replay of O_APPEND write returns both value and aux without re-appending.
        let append_replay = vfs.dispatch_write(&append_req);
        assert_eq!(append_replay.status, 0);
        assert_eq!(append_replay.value, 6);
        assert_eq!(append_replay.aux, 11);
        assert_eq!(
            vfs.files.get("/append.bin").unwrap().as_slice(),
            b"hello_world"
        );
    }
}
