use super::IPC_ABI_VERSION;

/// Complete one byte-only reply and enter the next sender-authenticated
/// receive without returning to ring3 between the two phases.
///
/// The argument block is [`IpcReplyRecvWithSenderArgs`]. A non-negative return
/// is the next request length. Ordinary negative errno values mean that the old
/// reply was not consumed. Values tagged by
/// [`IPC_REPLY_RECV_COMMITTED_ERROR_BASE`] mean that the reply completed but
/// the receive phase failed; callers must never retry the old reply cap.
pub const SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER: u64 = 0x5255_0048;

/// Native IPC error tag for the post-commit half of reply-receive.
///
/// Linux reserves only -1..=-4095 for errno. The native RustOS service ABI
/// deliberately places phase-aware failures below that range so a raw-syscall
/// caller can distinguish them without a racy user-memory status write.
pub const IPC_REPLY_RECV_COMMITTED_ERROR_BASE: i64 = 4096;

/// Largest ordinary errno admitted by the reply-receive wire partition.
pub const IPC_REPLY_RECV_ERRNO_MAX: i64 = IPC_REPLY_RECV_COMMITTED_ERROR_BASE - 1;

/// Exhaustive interpretation of one raw reply-receive return value.
///
/// `Invalid` is deliberately distinct from a pre-commit error. In particular,
/// `-4096`, values at or below `-8192`, and `i64::MIN` are protocol violations;
/// treating them as retryable could duplicate a reply after an ABI mismatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcReplyRecvResultKind {
    Success,
    PreCommitError(i64),
    PostCommitError(i64),
    Invalid,
}

/// Classify the complete native result space without ambiguous retry states.
pub const fn ipc_reply_recv_result_kind(result: i64) -> IpcReplyRecvResultKind {
    if result >= 0 {
        return IpcReplyRecvResultKind::Success;
    }
    if result == i64::MIN {
        return IpcReplyRecvResultKind::Invalid;
    }
    let magnitude = -result;
    if magnitude <= IPC_REPLY_RECV_ERRNO_MAX {
        IpcReplyRecvResultKind::PreCommitError(magnitude)
    } else if magnitude > IPC_REPLY_RECV_COMMITTED_ERROR_BASE
        && magnitude <= IPC_REPLY_RECV_COMMITTED_ERROR_BASE + IPC_REPLY_RECV_ERRNO_MAX
    {
        IpcReplyRecvResultKind::PostCommitError(magnitude - IPC_REPLY_RECV_COMMITTED_ERROR_BASE)
    } else {
        IpcReplyRecvResultKind::Invalid
    }
}

/// Versioned byte-only reply + sender-authenticated receive transaction.
///
/// All response input and next-request output ranges are validated before the
/// old one-shot reply capability is consumed. `flags` and both reserved fields
/// must be zero. Handle transfer intentionally remains on the separate
/// explicit syscalls so the common service loop cannot silently drop handles.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcReplyRecvWithSenderArgs {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub endpoint: u64,
    pub reply_cap: u64,
    pub response_ptr: u64,
    pub response_len: u64,
    pub request_ptr: u64,
    pub request_capacity: u64,
    pub next_reply_cap_ptr: u64,
    pub sender_pid_ptr: u64,
    pub sender_tid_ptr: u64,
    pub reserved1: u64,
}

/// Accept only the exact public reply-receive envelope before ring0 consumes
/// reply authority. Pointer/range and endpoint-owner validation remains in the
/// kernel because it depends on the live address space and capability graph.
pub const fn ipc_reply_recv_shape_valid(args: &IpcReplyRecvWithSenderArgs) -> bool {
    args.abi_version == IPC_ABI_VERSION
        && args.flags == 0
        && args.reserved0 == 0
        && args.reserved1 == 0
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn arbitrary_args() -> IpcReplyRecvWithSenderArgs {
        IpcReplyRecvWithSenderArgs {
            abi_version: kani::any(),
            flags: kani::any(),
            reserved0: kani::any(),
            reserved1: kani::any(),
            ..IpcReplyRecvWithSenderArgs::default()
        }
    }

    #[kani::proof]
    fn accepted_reply_recv_shape_is_exact() {
        let args = arbitrary_args();
        let accepted = ipc_reply_recv_shape_valid(&args);
        kani::cover!(accepted);
        kani::cover!(!accepted);
        if accepted {
            assert_eq!(args.abi_version, IPC_ABI_VERSION);
            assert_eq!(args.flags, 0);
            assert_eq!(args.reserved0, 0);
            assert_eq!(args.reserved1, 0);
        }
    }

    #[kani::proof]
    fn malformed_reply_recv_shape_is_never_accepted() {
        let args = arbitrary_args();
        let malformed = args.abi_version != IPC_ABI_VERSION
            || args.flags != 0
            || args.reserved0 != 0
            || args.reserved1 != 0;
        kani::assume(malformed);
        kani::cover!(malformed);
        assert!(!ipc_reply_recv_shape_valid(&args));
    }

    #[kani::proof]
    fn reply_recv_result_partition_is_exact() {
        let result: i64 = kani::any();
        match ipc_reply_recv_result_kind(result) {
            IpcReplyRecvResultKind::Success => {
                kani::cover!();
                assert!(result >= 0);
            }
            IpcReplyRecvResultKind::PreCommitError(errno) => {
                kani::cover!();
                assert!((1..=IPC_REPLY_RECV_ERRNO_MAX).contains(&errno));
                assert_eq!(result, -errno);
            }
            IpcReplyRecvResultKind::PostCommitError(errno) => {
                kani::cover!();
                assert!((1..=IPC_REPLY_RECV_ERRNO_MAX).contains(&errno));
                assert_eq!(result, -(IPC_REPLY_RECV_COMMITTED_ERROR_BASE + errno));
            }
            IpcReplyRecvResultKind::Invalid => {
                kani::cover!();
                assert!(
                    result == i64::MIN
                        || result == -IPC_REPLY_RECV_COMMITTED_ERROR_BASE
                        || result
                            <= -(IPC_REPLY_RECV_COMMITTED_ERROR_BASE
                                + IPC_REPLY_RECV_ERRNO_MAX
                                + 1)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn reply_recv_wire_shape_and_error_partition_are_stable() {
        assert_eq!(size_of::<IpcReplyRecvWithSenderArgs>(), 88);
        let args = IpcReplyRecvWithSenderArgs {
            abi_version: IPC_ABI_VERSION,
            ..IpcReplyRecvWithSenderArgs::default()
        };
        assert_eq!(args.flags, 0);
        assert_eq!(args.reserved0, 0);
        assert_eq!(args.reserved1, 0);
        assert!(ipc_reply_recv_shape_valid(&args));
        assert!(!ipc_reply_recv_shape_valid(&IpcReplyRecvWithSenderArgs {
            flags: 1,
            ..args
        }));
        assert_eq!(IPC_REPLY_RECV_COMMITTED_ERROR_BASE, 4096);
        assert_eq!(IPC_REPLY_RECV_ERRNO_MAX, 4095);
        assert_eq!(SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER, 0x5255_0048);
        assert_eq!(
            ipc_reply_recv_result_kind(-4095),
            IpcReplyRecvResultKind::PreCommitError(4095)
        );
        assert_eq!(
            ipc_reply_recv_result_kind(-4097),
            IpcReplyRecvResultKind::PostCommitError(1)
        );
        assert_eq!(
            ipc_reply_recv_result_kind(-8191),
            IpcReplyRecvResultKind::PostCommitError(4095)
        );
        assert_eq!(
            ipc_reply_recv_result_kind(-4096),
            IpcReplyRecvResultKind::Invalid
        );
        assert_eq!(
            ipc_reply_recv_result_kind(-8192),
            IpcReplyRecvResultKind::Invalid
        );
    }
}
