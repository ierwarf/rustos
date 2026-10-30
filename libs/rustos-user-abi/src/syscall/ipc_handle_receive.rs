//! Explicit handle-bearing receives with kernel-stamped sender identity.
//!
//! The older sender-only and handle-only layouts remain unchanged. A service
//! accepting placement authority must not choose between sender authentication
//! and descriptor transfer, including on its next fused reply/receive.
use super::{
    IPC_ABI_VERSION, IPC_MAX_TRANSFER_HANDLES, IpcReplyRecvWithSenderArgs,
    ipc_reply_recv_shape_valid,
};

pub const SYS_RUSTOS_IPC_RECV_WITH_HANDLES_AND_SENDER: u64 = 0x5255_0058;
pub const SYS_RUSTOS_IPC_REPLY_RECV_WITH_HANDLES_AND_SENDER: u64 = 0x5255_0059;

/// All output ranges are validated before a message is dequeued. Descriptors
/// are published only after request, reply, sender and fd outputs copy out.
/// `recv_fds_ptr` addresses `recv_fd_capacity` i32 slots; the count is a u16.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcRecvWithHandlesAndSenderArgs {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub endpoint: u64,
    pub request_ptr: u64,
    pub request_capacity: u64,
    pub reply_cap_ptr: u64,
    pub recv_fds_ptr: u64,
    pub recv_fd_count_ptr: u64,
    pub sender_pid_ptr: u64,
    pub sender_tid_ptr: u64,
    pub recv_fd_capacity: u16,
    pub reserved1: u16,
    pub reserved2: u32,
}

/// An explicit extension of the legacy byte-only fused transaction. Reply
/// consumption retains the same precommit errno / tagged postcommit partition.
/// Every receive output, including fd slots/count, is admitted before consume.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcReplyRecvWithHandlesAndSenderArgs {
    pub reply_recv: IpcReplyRecvWithSenderArgs,
    pub recv_fds_ptr: u64,
    pub recv_fd_count_ptr: u64,
    pub recv_fd_capacity: u16,
    pub reserved0: u16,
    pub reserved1: u32,
}

/// Envelope checks only. Live endpoint ownership, pointer ranges and transfer
/// custody are checked by the kernel, not inferred from well-shaped input.
pub const fn ipc_handle_receive_shape_valid(args: &IpcRecvWithHandlesAndSenderArgs) -> bool {
    args.abi_version == IPC_ABI_VERSION
        && args.flags == 0
        && args.reserved0 == 0
        && args.reserved1 == 0
        && args.reserved2 == 0
        && args.recv_fd_capacity as usize <= IPC_MAX_TRANSFER_HANDLES
}

pub const fn ipc_handle_reply_recv_shape_valid(args: &IpcReplyRecvWithHandlesAndSenderArgs) -> bool {
    ipc_reply_recv_shape_valid(&args.reply_recv)
        && args.reserved0 == 0
        && args.reserved1 == 0
        && args.recv_fd_capacity as usize <= IPC_MAX_TRANSFER_HANDLES
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn authenticated_handle_receive_envelope_is_exact() {
        let args = IpcRecvWithHandlesAndSenderArgs {
            abi_version: kani::any(), flags: kani::any(), reserved0: kani::any(),
            reserved1: kani::any(), reserved2: kani::any(), recv_fd_capacity: kani::any(),
            ..IpcRecvWithHandlesAndSenderArgs::default()
        };
        kani::cover!(ipc_handle_receive_shape_valid(&args));
        kani::cover!(!ipc_handle_receive_shape_valid(&args));
        assert_eq!(ipc_handle_receive_shape_valid(&args),
            args.abi_version == IPC_ABI_VERSION && args.flags == 0 && args.reserved0 == 0
            && args.reserved1 == 0 && args.reserved2 == 0
            && args.recv_fd_capacity as usize <= IPC_MAX_TRANSFER_HANDLES);
    }

    #[kani::proof]
    fn authenticated_handle_reply_receive_envelope_is_exact() {
        let args = IpcReplyRecvWithHandlesAndSenderArgs {
            reply_recv: IpcReplyRecvWithSenderArgs {
                abi_version: kani::any(), flags: kani::any(), reserved0: kani::any(), reserved1: kani::any(),
                ..IpcReplyRecvWithSenderArgs::default()
            },
            reserved0: kani::any(), reserved1: kani::any(), recv_fd_capacity: kani::any(),
            ..IpcReplyRecvWithHandlesAndSenderArgs::default()
        };
        kani::cover!(ipc_handle_reply_recv_shape_valid(&args));
        kani::cover!(!ipc_handle_reply_recv_shape_valid(&args));
        assert_eq!(ipc_handle_reply_recv_shape_valid(&args),
            ipc_reply_recv_shape_valid(&args.reply_recv) && args.reserved0 == 0
            && args.reserved1 == 0 && args.recv_fd_capacity as usize <= IPC_MAX_TRANSFER_HANDLES);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn authenticated_handle_receive_layouts_preserve_legacy_prefixes() {
        assert_eq!(size_of::<IpcRecvWithHandlesAndSenderArgs>(), 80);
        assert_eq!(align_of::<IpcRecvWithHandlesAndSenderArgs>(), 8);
        assert_eq!(offset_of!(IpcRecvWithHandlesAndSenderArgs, endpoint), 8);
        assert_eq!(offset_of!(IpcRecvWithHandlesAndSenderArgs, recv_fd_capacity), 72);
        assert_eq!(size_of::<IpcReplyRecvWithHandlesAndSenderArgs>(), 112);
        assert_eq!(offset_of!(IpcReplyRecvWithHandlesAndSenderArgs, reply_recv), 0);
        assert_eq!(offset_of!(IpcReplyRecvWithHandlesAndSenderArgs, recv_fds_ptr), 88);
        assert_eq!(size_of::<super::super::IpcRecvWithHandlesArgs>(), 56);
        assert_eq!(size_of::<IpcReplyRecvWithSenderArgs>(), 88);
        assert_eq!(SYS_RUSTOS_IPC_RECV_WITH_HANDLES_AND_SENDER, 0x5255_0058);
        assert_eq!(SYS_RUSTOS_IPC_REPLY_RECV_WITH_HANDLES_AND_SENDER, 0x5255_0059);
    }

    #[test]
    fn authenticated_handle_receive_envelopes_reject_unknown_fields_and_excess_capacity() {
        let receive = IpcRecvWithHandlesAndSenderArgs {
            abi_version: IPC_ABI_VERSION, recv_fd_capacity: IPC_MAX_TRANSFER_HANDLES as u16,
            ..IpcRecvWithHandlesAndSenderArgs::default()
        };
        assert!(ipc_handle_receive_shape_valid(&receive));
        for bad in [
            IpcRecvWithHandlesAndSenderArgs { abi_version: 0, ..receive },
            IpcRecvWithHandlesAndSenderArgs { flags: 1, ..receive },
            IpcRecvWithHandlesAndSenderArgs { reserved0: 1, ..receive },
            IpcRecvWithHandlesAndSenderArgs { reserved1: 1, ..receive },
            IpcRecvWithHandlesAndSenderArgs { reserved2: 1, ..receive },
            IpcRecvWithHandlesAndSenderArgs { recv_fd_capacity: (IPC_MAX_TRANSFER_HANDLES + 1) as u16, ..receive },
        ] { assert!(!ipc_handle_receive_shape_valid(&bad)); }
        let fused = IpcReplyRecvWithHandlesAndSenderArgs {
            reply_recv: IpcReplyRecvWithSenderArgs { abi_version: IPC_ABI_VERSION, ..IpcReplyRecvWithSenderArgs::default() },
            recv_fd_capacity: IPC_MAX_TRANSFER_HANDLES as u16,
            ..IpcReplyRecvWithHandlesAndSenderArgs::default()
        };
        assert!(ipc_handle_reply_recv_shape_valid(&fused));
        for bad in [
            IpcReplyRecvWithHandlesAndSenderArgs { reply_recv: IpcReplyRecvWithSenderArgs::default(), ..fused },
            IpcReplyRecvWithHandlesAndSenderArgs { reserved0: 1, ..fused },
            IpcReplyRecvWithHandlesAndSenderArgs { reserved1: 1, ..fused },
            IpcReplyRecvWithHandlesAndSenderArgs { recv_fd_capacity: (IPC_MAX_TRANSFER_HANDLES + 1) as u16, ..fused },
        ] { assert!(!ipc_handle_reply_recv_shape_valid(&bad)); }
    }
}
