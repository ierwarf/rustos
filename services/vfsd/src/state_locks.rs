// SPDX-License-Identifier: MIT
//
// `VFS_IPC_OP_FILE_LOCK`: marshalling between the wire and `vfsd::locks`.
// Policy (ranges, conflicts, deadlocks) lives in `services/vfsd/src/locks.rs`;
// this file only resolves the file identity, the `whence` base and the owner
// from kernel-stamped facts. The wire never names the owner pid.

impl VfsState {
    /// Lock identity of an open description: the pinned memory-filesystem node
    /// or, for a path-based volume, the identity `FILE_IDENTITY` would report.
    fn lock_file_key(&self, handle: &RemoteHandle) -> Result<vfsd::locks::FileKey, i32> {
        if handle.node != 0 {
            return Ok((handle.mount_id, handle.node));
        }
        lock_vfs_storage().lock_identity(handle.path.as_str())
    }

    /// Size used by `SEEK_END`: the live node length, never a stale handle one.
    fn lock_file_len(&self, handle: &RemoteHandle) -> u64 {
        if handle.kind != RemoteKind::File {
            return 0;
        }
        if handle.node != 0 {
            return lock_vfs_storage()
                .node_len(handle.mount_id, handle.node)
                .unwrap_or(handle.len);
        }
        lock_vfs_storage()
            .live_stat_for_path(handle.path.as_str(), handle.len)
            .0
    }

    fn vfs_file_lock(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        use rustos_user_abi::syscall::{
            VfsFileLockWire, VFS_FILE_LOCK_CMD_CANCEL_WAIT, VFS_FILE_LOCK_CMD_RELEASE_FILE,
        };

        let payload = &request.payload[..(request.payload_len as usize).min(request.payload.len())];
        let Some(wire) = VfsFileLockWire::from_bytes(payload).filter(VfsFileLockWire::is_well_formed)
        else {
            response.status = EINVAL;
            return;
        };
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            // A cancel names a thread, not a file; everything else needs one.
            if wire.cmd == VFS_FILE_LOCK_CMD_CANCEL_WAIT && request.pid != 0 {
                self.locks.cancel_wait(request.tid);
            } else {
                response.status = EBADF;
            }
            return;
        };
        if !matches!(handle.kind, RemoteKind::File | RemoteKind::Directory) {
            response.status = EINVAL;
            return;
        }
        let key = match self.lock_file_key(&handle) {
            Ok(key) => key,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        let needs_len = wire.cmd != VFS_FILE_LOCK_CMD_CANCEL_WAIT
            && wire.cmd != VFS_FILE_LOCK_CMD_RELEASE_FILE
            && wire.owner != rustos_user_abi::syscall::VFS_FILE_LOCK_OWNER_FLOCK
            && wire.whence == rustos_user_abi::syscall::VFS_FILE_LOCK_WHENCE_END;
        let ctx = vfsd::locks::LockContext {
            pid: request.pid,
            tid: request.tid,
            remote_id: request.remote_id,
            key,
            cursor: handle.cursor,
            file_len: if needs_len { self.lock_file_len(&handle) } else { 0 },
            now_ns: monotonic_nanos(),
        };
        match vfsd::locks::execute_verified(&mut self.locks, &ctx, &wire, &mut probe_lifecycle_pids) {
            Ok(Some(reply)) => write_lock_reply(response, &reply),
            Ok(None) => {}
            Err(errno) => response.status = errno,
        }
    }
}

fn write_lock_reply(response: &mut VfsIpcResponse, wire: &rustos_user_abi::syscall::VfsFileLockWire) {
    let bytes = wire.to_bytes();
    response.payload[..bytes.len()].copy_from_slice(&bytes);
    response.payload_len = bytes.len() as u32;
}
