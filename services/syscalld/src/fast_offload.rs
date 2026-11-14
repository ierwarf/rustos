//! Compact fixed-frame adapter for high-frequency syscalld requests.

use rustos_user_abi::syscall::{
    identity_is_exact_sender, LinuxSyscallOffloadFastMemRequest, LinuxSyscallOffloadFastRequest,
    LinuxSyscallOffloadFastResponse, LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse,
    SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_BRK, SYSCALL_OFFLOAD_OP_LINUX_GETEGID,
    SYSCALL_OFFLOAD_OP_LINUX_GETEUID, SYSCALL_OFFLOAD_OP_LINUX_GETGID,
    SYSCALL_OFFLOAD_OP_LINUX_GETUID, SYSCALL_OFFLOAD_OP_LINUX_MMAP,
    SYSCALL_OFFLOAD_OP_LINUX_MPROTECT, SYSCALL_OFFLOAD_OP_LINUX_MUNMAP,
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

/// Expand a compact address-space request into the full request the handlers
/// take. The same exact-sender rule as the id getters applies: the identity in
/// the frame must be the kernel-stamped caller, so a service cannot ask for
/// another process's mappings by writing its PID into a compact frame.
pub fn expand_mem_request(
    request: &LinuxSyscallOffloadFastMemRequest,
    sender_pid: u64,
    sender_tid: u64,
) -> Result<LinuxSyscallOffloadRequest, i32> {
    if request.version != SYSCALL_OFFLOAD_ABI_VERSION
        || !identity_is_exact_sender(request.pid, request.tid, sender_pid, sender_tid)
        || !matches!(
            request.op,
            SYSCALL_OFFLOAD_OP_LINUX_MMAP
                | SYSCALL_OFFLOAD_OP_LINUX_MUNMAP
                | SYSCALL_OFFLOAD_OP_LINUX_MPROTECT
                | SYSCALL_OFFLOAD_OP_LINUX_BRK
        )
    {
        return Err(errno::EINVAL);
    }
    let mut full = LinuxSyscallOffloadRequest {
        version: request.version,
        op: request.op,
        pid: request.pid,
        tid: request.tid,
        arg0: request.addr,
        arg1: request.len,
        flags: request.flags,
        mask: request.prot,
        dirfd: request.fd,
        // Address-space operations carry the kernel lease ticket in `arg3`
        // (unused by these operations); handlers echo it to the MM broker.
        arg3: request.ticket,
        ..LinuxSyscallOffloadRequest::default()
    };
    // `mmap` carries its file offset in the first path bytes, exactly as the
    // full request does.
    full.path[0..8].copy_from_slice(&request.offset.to_le_bytes());
    full.path_len = 8;
    Ok(full)
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
    fn compact_memory_wire_is_fast_lane_sized_distinct_exact_sender_and_lossless() {
        let size = size_of::<LinuxSyscallOffloadFastMemRequest>();
        assert!(size <= 256, "must fit the IPC fast lane's inline frame");
        for other in [
            size_of::<LinuxSyscallOffloadFastRequest>(),
            size_of::<LinuxSyscallOffloadRequest>(),
        ] {
            assert_ne!(size, other, "the frame size is the wire discriminator");
        }
        let request = LinuxSyscallOffloadFastMemRequest {
            version: SYSCALL_OFFLOAD_ABI_VERSION,
            op: SYSCALL_OFFLOAD_OP_LINUX_MMAP,
            prot: 3,
            pid: 77,
            tid: 78,
            addr: 0x7000_0000,
            len: 8192,
            flags: 0x22,
            fd: u64::MAX,
            offset: 0x4000,
            ticket: 0x1234_5678,
        };
        let full = expand_mem_request(&request, 77, 78).expect("exact sender");
        assert_eq!(
            (full.op, full.pid, full.arg0, full.arg1, full.flags, full.mask, full.dirfd),
            (request.op, 77, 0x7000_0000, 8192, 0x22, 3, u64::MAX)
        );
        assert_eq!(full.arg3, 0x1234_5678);
        assert_eq!(&full.path[0..8], &0x4000_u64.to_le_bytes());
        assert_eq!(full.path_len, 8);
        // Another process's identity in the frame is refused, not honoured.
        assert!(expand_mem_request(&request, 99, 78).is_err());
        assert!(expand_mem_request(&request, 77, 99).is_err());
        // Only the address-space operations may use this frame.
        let id_op = LinuxSyscallOffloadFastMemRequest {
            op: SYSCALL_OFFLOAD_OP_LINUX_GETUID,
            ..request
        };
        assert!(expand_mem_request(&id_op, 77, 78).is_err());
        let stale = LinuxSyscallOffloadFastMemRequest { version: 0, ..request };
        assert!(expand_mem_request(&stale, 77, 78).is_err());
    }

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
