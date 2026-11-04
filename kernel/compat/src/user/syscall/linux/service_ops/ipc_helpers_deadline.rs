//! Foreground VFS deadline and replay classification.
//!
//! A dequeued service request may keep executing after its caller expires.
//! Therefore one attempt owns the complete remaining rail; only an early
//! transport break may retry. A real timeout transfers exact mutation
//! ownership to the bounded housekeeping queue in `ipc_helpers`.

use super::{
    LINUX_ENOSYS, LINUX_EPIPE, NETD_IPC_OP_REF_ACK, SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
    SYSCALL_OFFLOAD_OP_LINUX_DUP, SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
    SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR, SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, VFS_IPC_OP_POLL_QUERY, VfsIpcRequest, current_remote_vfs_handle,
    ipc_ops,
};
// Referenced only by this file's tests, which classify the poll-socket
// timeout alongside the ops the production paths above use.
#[cfg(test)]
use super::{
    SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
    SYSCALL_OFFLOAD_OP_LINUX_RECVMSG, SYSCALL_OFFLOAD_OP_LINUX_SENDMSG,
    SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
};
use alloc::string::String;

pub(in crate::user::syscall::linux::service_ops) fn remaining_service_timeout_ms(
    total: u64,
    elapsed: u64,
) -> Option<u64> {
    (elapsed < total).then(|| total - elapsed)
}

pub(in crate::user::syscall::linux::service_ops) fn retryable_early_service_transport_error(
    errno: i64,
) -> bool {
    matches!(errno, LINUX_EPIPE | LINUX_ENOSYS)
}

pub(in crate::user::syscall::linux::service_ops) fn netd_timeout_class(
    op: u16,
) -> ipc_ops::ServiceIpcClass {
    // A stream send/receive is a state mutation for deadline purposes: netd
    // keeps executing a request whose caller timed out, so an expired rail
    // leaves the bytes moved (or not) with no way to tell. A 16/100 ms rail
    // turned an ordinary netd stall on one CPU into a broken Wayland stream.
    if super::super::vfs_meta::socket_op_moves_stream_bytes(op)
        || matches!(
            op,
            SYSCALL_OFFLOAD_OP_LINUX_DUP | SYSCALL_OFFLOAD_OP_LINUX_CLOSE | NETD_IPC_OP_REF_ACK
                | SYSCALL_OFFLOAD_OP_LINUX_SOCKET | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
        )
    {
        ipc_ops::ServiceIpcClass::StateMutation
    } else {
        ipc_ops::ServiceIpcClass::ReadinessQuery
    }
}

pub(in crate::user::syscall::linux::service_ops) fn vfs_request_log_detail(
    request: &VfsIpcRequest,
) -> Option<String> {
    if request.path_len != 0 {
        let path_len = usize::try_from(request.path_len).ok()?;
        if path_len > request.path.len() {
            return None;
        }
        let path = core::str::from_utf8(&request.path[..path_len]).ok()?;
        return Some(alloc::format!("path={path}"));
    }
    if request.op == VFS_IPC_OP_POLL_QUERY {
        return Some(alloc::format!(
            "poll_query={} token={}",
            request.arg0,
            request.remote_id
        ));
    }
    current_remote_vfs_handle(request.fd)
        .map(|remote| alloc::format!("fd={} path={}", request.fd, remote.path()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user::syscall::linux::LINUX_ETIMEDOUT;

    #[test]
    fn replay_safe_foreground_attempt_owns_the_complete_remaining_deadline() {
        assert_eq!(remaining_service_timeout_ms(16, 0), Some(16));
        assert_eq!(remaining_service_timeout_ms(16, 1), Some(15));
        assert_eq!(remaining_service_timeout_ms(16, 15), Some(1));
        assert_eq!(remaining_service_timeout_ms(16, 16), None);
        assert_eq!(remaining_service_timeout_ms(16, 17), None);

        assert!(retryable_early_service_transport_error(LINUX_EPIPE));
        assert!(retryable_early_service_transport_error(LINUX_ENOSYS));
        assert!(!retryable_early_service_transport_error(LINUX_ETIMEDOUT));
    }

    #[test]
    fn netd_reference_mutations_use_state_mutation_lease() {
        for op in [
            SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
            SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
            SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
            SYSCALL_OFFLOAD_OP_LINUX_DUP,
            SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
            NETD_IPC_OP_REF_ACK,
        ] {
            assert!(matches!(
                netd_timeout_class(op),
                ipc_ops::ServiceIpcClass::StateMutation
            ));
        }
        for op in [
            SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
            SYSCALL_OFFLOAD_OP_LINUX_SENDMSG,
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
            SYSCALL_OFFLOAD_OP_LINUX_RECVMSG,
        ] {
            assert!(matches!(
                netd_timeout_class(op),
                ipc_ops::ServiceIpcClass::StateMutation
            ));
        }
        assert!(matches!(
            netd_timeout_class(SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET),
            ipc_ops::ServiceIpcClass::ReadinessQuery
        ));
    }

    #[test]
    fn vfs_timeout_diagnostic_identifies_the_exact_epoll_control_operation() {
        let request = VfsIpcRequest {
            op: VFS_IPC_OP_POLL_QUERY,
            arg0: super::super::VFS_POLL_QUERY_EPOLL_CREATE,
            remote_id: 73,
            ..VfsIpcRequest::default()
        };
        assert_eq!(
            vfs_request_log_detail(&request).as_deref(),
            Some("poll_query=2 token=73")
        );
    }
}
