//! Strict private snapshot-job reply validation, independent of transport.
use rustos_user_abi::syscall::*;
#[derive(Debug, Eq, PartialEq)]
pub enum SnapshotReply { Pending { id: u64, completed: u64, total: u64 }, Ready, Failed(i32) }

pub fn validate_response(r: &VfsExecutableSnapshotResponse, op: u16, id: u64, handles: u16) -> Result<SnapshotReply, i32> {
    let invalid = 22;
    if r.version != VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION || r.op != op || r.reserved0 != 0 || r.status < 0
        || !matches!(op, VFS_EXECUTABLE_SNAPSHOT_OP_OPEN | VFS_EXECUTABLE_SNAPSHOT_OP_POLL)
    { return Err(invalid); }
    if r.status != 0 && r.status != VFS_EXECUTABLE_SNAPSHOT_PENDING {
        return if handles == 0 { Ok(SnapshotReply::Failed(r.status)) } else { Err(invalid) };
    }
    if r.job_id == 0 || (id != 0 && id != r.job_id) { return Err(invalid); }
    if r.status == VFS_EXECUTABLE_SNAPSHOT_PENDING {
        if handles != 0 || r.mount_generation != 0 || r.resolved_path_len != 0
            || r.completed_bytes > r.file_bytes || r.file_bytes > VFS_EXECUTABLE_SNAPSHOT_MAX_FILE_BYTES
            || (op == VFS_EXECUTABLE_SNAPSHOT_OP_OPEN && (r.completed_bytes != 0 || r.file_bytes != 0))
        { return Err(invalid); }
        return Ok(SnapshotReply::Pending { id: r.job_id, completed: r.completed_bytes, total: r.file_bytes });
    }
    if op != VFS_EXECUTABLE_SNAPSHOT_OP_POLL || id == 0 || handles != 1
        || r.file_bytes == 0 || r.file_bytes > VFS_EXECUTABLE_SNAPSHOT_MAX_FILE_BYTES
        || r.completed_bytes != r.file_bytes || r.mount_generation == 0
        || r.resolved_path_len == 0 || r.resolved_path_len as usize > r.resolved_path.len()
    { return Err(invalid); }
    let path = core::str::from_utf8(&r.resolved_path[..r.resolved_path_len as usize]).map_err(|_| invalid)?;
    if !path.starts_with('/') || path.as_bytes().contains(&0) { return Err(invalid); }
    Ok(SnapshotReply::Ready)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn begin_cannot_publish_and_pending_cannot_smuggle_a_descriptor() {
        let mut r = VfsExecutableSnapshotResponse { version: VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION,
            op: VFS_EXECUTABLE_SNAPSHOT_OP_OPEN, status: VFS_EXECUTABLE_SNAPSHOT_PENDING,
            job_id: 7, ..VfsExecutableSnapshotResponse::default() };
        assert!(matches!(validate_response(&r, r.op, 0, 0), Ok(SnapshotReply::Pending { id: 7, .. })));
        assert!(validate_response(&r, r.op, 0, 1).is_err());
        r.status = 0;
        assert!(validate_response(&r, r.op, 0, 1).is_err());
        r.op = VFS_EXECUTABLE_SNAPSHOT_OP_POLL; r.status = VFS_EXECUTABLE_SNAPSHOT_PENDING;
        assert!(validate_response(&r, r.op, 8, 0).is_err());
        r.completed_bytes = 1;
        assert!(validate_response(&r, r.op, 7, 0).is_err());
    }
    #[test]
    fn only_exact_complete_canonical_metadata_with_one_handle_is_terminal() {
        let mut r = VfsExecutableSnapshotResponse { version: VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION,
            op: VFS_EXECUTABLE_SNAPSHOT_OP_POLL, job_id: 7, file_bytes: 9,
            completed_bytes: 9, mount_generation: 1, resolved_path_len: 2,
            ..VfsExecutableSnapshotResponse::default() };
        r.resolved_path[..2].copy_from_slice(b"/a");
        assert_eq!(validate_response(&r, r.op, 7, 1), Ok(SnapshotReply::Ready));
        assert!(validate_response(&r, r.op, 7, 0).is_err());
        r.completed_bytes = 8;
        assert!(validate_response(&r, r.op, 7, 1).is_err());
        r.status = 5;
        assert_eq!(validate_response(&r, r.op, 7, 0), Ok(SnapshotReply::Failed(5)));
        assert!(validate_response(&r, r.op, 7, 1).is_err());
    }
}
