#![no_std]

pub mod affinity_policy;
pub mod errno;
pub mod fast_offload;
pub mod mmap_policy;
pub mod vma_policy;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyRecvRecoveryAction {
    None,
    RetryReply(i64),
    PostCommit(i64),
    ProtocolViolation,
}

/// Keeps the only retry-safe reply-receive result disjoint from a receive
/// failure that arrived after the old one-shot reply capability was consumed.
pub fn classify_reply_recv_recovery(result: i64) -> ReplyRecvRecoveryAction {
    match rustos_svc_runtime::ipc::reply_recv_result_kind(result) {
        rustos_user_abi::syscall::IpcReplyRecvResultKind::Success => ReplyRecvRecoveryAction::None,
        rustos_user_abi::syscall::IpcReplyRecvResultKind::PreCommitError(errno) => {
            ReplyRecvRecoveryAction::RetryReply(errno)
        }
        rustos_user_abi::syscall::IpcReplyRecvResultKind::PostCommitError(errno) => {
            ReplyRecvRecoveryAction::PostCommit(errno)
        }
        rustos_user_abi::syscall::IpcReplyRecvResultKind::Invalid => {
            ReplyRecvRecoveryAction::ProtocolViolation
        }
    }
}

#[cfg(test)]
mod reply_recv_tests {
    use super::*;

    #[test]
    fn reply_recv_recovery_retries_only_a_proven_live_reply() {
        assert_eq!(
            classify_reply_recv_recovery(-22),
            ReplyRecvRecoveryAction::RetryReply(22)
        );
        assert_eq!(
            classify_reply_recv_recovery(-4097),
            ReplyRecvRecoveryAction::PostCommit(1)
        );
        assert_eq!(
            classify_reply_recv_recovery(-4096),
            ReplyRecvRecoveryAction::ProtocolViolation
        );
        assert_eq!(
            classify_reply_recv_recovery(0),
            ReplyRecvRecoveryAction::None
        );
    }
}
