//! Terminal-reply custody for loaderd scheduling-authority release.
//!
//! - **Owner:** loaderd owns the one-shot decision to relinquish its bootstrap
//!   System class after creating the UI server.
//! - **Boundary:** Request bytes, fused result tags, cleanup ownership, and
//!   demotion intent remain untrusted until exact classification.
//! - **Lifecycle:** A successful UI spawn records intent, a successful reply
//!   consumes it, failed/cancelled replies retain bootstrap authority, and a
//!   fused receive is admitted only after immediate post-reply work is absent.
//! - **Concurrency:** The loader service loop is the sole intent consumer.
//! - **Failure:** A negative reply never authorizes demotion; zero or malformed
//!   requests remain terminal reply obligations rather than idle receives.
//! - **Forbidden:** Request handlers must not execute self-demotion directly,
//!   delay descriptor cleanup behind a fused receive, or retry a consumed cap.
//! - **Evidence:** `scheduler-thread-demotion/SchedulerThreadDemotion` and
//!   `ipc-reply-recv-transaction/IpcReplyRecvTransaction`.

#![no_std]

use core::mem::size_of;

use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, LoaderActivateBatchRequest, LoaderSpawnRequest,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoaderWireKind {
    Commercial,
    ActivateBatch,
    Spawn,
    Malformed,
}

/// Classifies only exact versioned request container sizes. In particular, a
/// zero-byte endpoint call is a dequeued request with a live reply capability,
/// not an idle receive, and therefore must reach the terminal malformed reply.
pub fn classify_loader_wire_size(received: usize) -> LoaderWireKind {
    if received == size_of::<CommercialMaxProtocolRequest>() {
        LoaderWireKind::Commercial
    } else if received == size_of::<LoaderActivateBatchRequest>() {
        LoaderWireKind::ActivateBatch
    } else if received == size_of::<LoaderSpawnRequest>() {
        LoaderWireKind::Spawn
    } else {
        LoaderWireKind::Malformed
    }
}

/// Fused receive may block only when the completed reply has no immediate
/// descriptor cleanup or scheduling-class transition behind it.
pub fn fused_loader_reply_eligible(cleanup_fd_count: usize, demote_after_reply: bool) -> bool {
    cleanup_fd_count == 0 && !demote_after_reply
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyRecvRecoveryAction {
    None,
    RetryReply(i64),
    PostCommit(i64),
    ProtocolViolation,
}

/// Keeps pre-commit retry authority disjoint from completed-reply failures.
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

/// Returns whether the service loop may consume a previously recorded
/// post-UI demotion intent.
///
/// Keeping this predicate outside the no-main service binary gives formal
/// conformance a host-testable witness for the terminal-reply boundary.
pub fn completion_demotion_due(reply: i64, demote_after_reply: bool) -> bool {
    reply >= 0 && demote_after_reply
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_bootstrap_demotion_is_custodied_until_terminal_reply() {
        assert!(!completion_demotion_due(-22, true));
        assert!(!completion_demotion_due(0, false));
        assert!(completion_demotion_due(0, true));
    }

    #[test]
    fn zero_length_request_is_malformed_not_idle() {
        assert_eq!(classify_loader_wire_size(0), LoaderWireKind::Malformed);
        assert_eq!(
            classify_loader_wire_size(size_of::<LoaderSpawnRequest>()),
            LoaderWireKind::Spawn
        );
    }

    #[test]
    fn fused_reply_never_delays_cleanup_or_bootstrap_demotion() {
        assert!(fused_loader_reply_eligible(0, false));
        assert!(!fused_loader_reply_eligible(1, false));
        assert!(!fused_loader_reply_eligible(0, true));
    }

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
