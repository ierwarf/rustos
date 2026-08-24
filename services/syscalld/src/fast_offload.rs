//! Compact fixed-frame adapter for high-frequency syscalld requests.

use rustos_user_abi::syscall::{
    identity_is_exact_sender, LinuxSyscallOffloadFastRequest, LinuxSyscallOffloadFastResponse,
    LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, SYSCALL_OFFLOAD_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_GETEGID, SYSCALL_OFFLOAD_OP_LINUX_GETEUID,
    SYSCALL_OFFLOAD_OP_LINUX_GETGID, SYSCALL_OFFLOAD_OP_LINUX_GETUID,
};

use crate::errno;

pub fn expand_id_request(
    request: &LinuxSyscallOffloadFastRequest,
    sender_pid: u64,
    sender_tid: u64,
) -> Result<LinuxSyscallOffloadRequest, i32> {
    if request.version != SYSCALL_OFFLOAD_ABI_VERSION
        || request.reserved0 != 0
        || !identity_is_exact_sender(request.pid, request.tid, sender_pid, sender_tid)
        || !matches!(
            request.op,
            SYSCALL_OFFLOAD_OP_LINUX_GETUID
                | SYSCALL_OFFLOAD_OP_LINUX_GETGID
                | SYSCALL_OFFLOAD_OP_LINUX_GETEUID
                | SYSCALL_OFFLOAD_OP_LINUX_GETEGID
        )
    {
        return Err(errno::EINVAL);
    }
    Ok(LinuxSyscallOffloadRequest {
        version: request.version,
        op: request.op,
        pid: request.pid,
        tid: request.tid,
        uid: request.uid,
        gid: request.gid,
        euid: request.euid,
        egid: request.egid,
        ..LinuxSyscallOffloadRequest::default()
    })
}

pub fn compact_response(
    op: u16,
    response: &LinuxSyscallOffloadResponse,
) -> LinuxSyscallOffloadFastResponse {
    let mut compact = LinuxSyscallOffloadFastResponse {
        version: SYSCALL_OFFLOAD_ABI_VERSION,
        op,
        status: response.status,
        ..LinuxSyscallOffloadFastResponse::default()
    };
    let payload_len = response.payload_len as usize;
    if payload_len > compact.payload.len() {
        compact.status = errno::EOVERFLOW;
        return compact;
    }
    compact.payload_len = response.payload_len;
    compact.payload[..payload_len].copy_from_slice(&response.payload[..payload_len]);
    compact
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::*;

    #[test]
    fn compact_id_wire_is_fixed_frame_bounded_sender_exact_and_lossless() {
        assert!(size_of::<LinuxSyscallOffloadFastRequest>() <= 256);
        assert!(size_of::<LinuxSyscallOffloadFastResponse>() <= 256);
        let request = LinuxSyscallOffloadFastRequest {
            version: SYSCALL_OFFLOAD_ABI_VERSION,
            op: SYSCALL_OFFLOAD_OP_LINUX_GETUID,
            pid: 98_001,
            tid: 98_002,
            uid: 1_234,
            ..LinuxSyscallOffloadFastRequest::default()
        };
        let expanded = expand_id_request(&request, request.pid, request.tid).expect("exact sender");
        assert_eq!(expanded.uid, request.uid);
        assert!(expand_id_request(&request, request.pid, request.tid + 1).is_err());

        let mut full = LinuxSyscallOffloadResponse {
            op: request.op,
            payload_len: 4,
            ..LinuxSyscallOffloadResponse::default()
        };
        full.payload[..4].copy_from_slice(&request.uid.to_le_bytes());
        let compact = compact_response(request.op, &full);
        assert_eq!(compact.status, 0);
        assert_eq!(compact.payload_len, 4);
        assert_eq!(&compact.payload[..4], &request.uid.to_le_bytes());
    }
}
