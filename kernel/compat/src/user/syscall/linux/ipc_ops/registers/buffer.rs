//! Fastcall buffer slow mode. Snapshot a fault-revalidated argument block,
//! check its exact entry family, then execute the original IPC handler once.
//! No retry, new authority, altered deadline, or handle-transfer shortcut.

use super::*;
use rustos_user_abi::syscall::{IpcRegisterBufferArgs, ipc_register_buffer_entry};

pub(super) fn dispatch(frame: &mut SyscallFrame, entry: u64, args_ptr: u64) -> u64 {
    let request = match usermem::read_current_user_struct::<IpcRegisterBufferArgs>(args_ptr) {
        Ok(request) => request,
        Err(error) => return linux_errno(address_space_error_to_linux_errno(error)),
    };
    dispatch_snapshot(frame, entry, request, dispatch_linux_rustos_ipc_syscall)
}

fn dispatch_snapshot(
    frame: &mut SyscallFrame,
    entry: u64,
    request: IpcRegisterBufferArgs,
    execute: impl FnOnce(&mut SyscallFrame) -> u64,
) -> u64 {
    clear_register_result(frame);
    if ipc_register_buffer_entry(request.operation) != Some(entry) {
        return linux_errno(LINUX_EINVAL);
    }
    let args_ptr = frame.rdi;
    frame.rax = request.operation;
    [
        frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
    ] = request.args;
    // The allowlist cannot name another fastcall or a non-IPC operation.
    // Preserve the original handler's result, including post-commit tags.
    let result = execute(frame);
    frame.rax = entry;
    frame.rdi = args_ptr;
    clear_register_result(frame);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::*;

    fn frame(entry: u64) -> SyscallFrame {
        // All fields are u64; this is only a native-test frame.
        let mut frame: SyscallFrame = unsafe { core::mem::zeroed() };
        frame.rax = entry;
        frame.rdi = 0xfeed;
        frame.user_rsp = 0x1234;
        frame.user_rip = 0x5678;
        frame.rbx = 0x9abc;
        frame
    }

    #[test]
    fn buffer_mode_rejects_foreign_family_recursion_and_non_ipc_before_dispatch() {
        for operation in [
            SYS_RUSTOS_IPC_REPLY,
            SYS_RUSTOS_IPC_REGISTER_CALL,
            SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
            39,
            102,
            u64::MAX,
        ] {
            let mut frame = frame(SYS_RUSTOS_IPC_REGISTER_CALL);
            let result = dispatch_snapshot(
                &mut frame,
                SYS_RUSTOS_IPC_REGISTER_CALL,
                IpcRegisterBufferArgs {
                    operation,
                    args: [1, 2, 3, 4, 5, 6],
                },
                |_| panic!("invalid buffer operation consumed authority"),
            );
            assert_eq!(result as i64, -LINUX_EINVAL);
            assert_eq!(
                [frame.rdx, frame.r10, frame.r8, frame.r9, frame.rsi],
                [0; 5]
            );
        }
    }

    #[test]
    fn buffer_mode_forwards_all_six_words_once_and_preserves_commit_result() {
        let cases = [
            (SYS_RUSTOS_IPC_REGISTER_CALL, SYS_RUSTOS_IPC_CALL_BOUNDED),
            (
                SYS_RUSTOS_IPC_REGISTER_CALL,
                SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED,
            ),
            (
                SYS_RUSTOS_IPC_REGISTER_RECV,
                SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED,
            ),
            (
                SYS_RUSTOS_IPC_REGISTER_RECV,
                SYS_RUSTOS_IPC_RECV_WITH_HANDLES,
            ),
            (
                SYS_RUSTOS_IPC_REGISTER_REPLY,
                SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
            ),
            (
                SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
                SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER,
            ),
        ];
        for (entry, operation) in cases {
            for result in [
                16,
                (-LINUX_EPERM) as u64,
                ipc_reply_recv::ipc_reply_recv_committed_error(LINUX_EOVERFLOW),
            ] {
                let mut frame = frame(entry);
                let mut calls = 0;
                let returned = dispatch_snapshot(
                    &mut frame,
                    entry,
                    IpcRegisterBufferArgs {
                        operation,
                        args: [11, 22, 33, 44, 55, 66],
                    },
                    |forwarded| {
                        calls += 1;
                        assert_eq!(forwarded.rax, operation);
                        assert_eq!(
                            [
                                forwarded.rdi,
                                forwarded.rsi,
                                forwarded.rdx,
                                forwarded.r10,
                                forwarded.r8,
                                forwarded.r9
                            ],
                            [11, 22, 33, 44, 55, 66]
                        );
                        result
                    },
                );
                assert_eq!(calls, 1);
                assert_eq!(returned, result);
                assert_eq!(frame.rax, entry);
                assert_eq!(frame.rdi, 0xfeed);
                assert_eq!(
                    (frame.user_rsp, frame.user_rip, frame.rbx),
                    (0x1234, 0x5678, 0x9abc)
                );
                assert_eq!(
                    [frame.rdx, frame.r10, frame.r8, frame.r9, frame.rsi],
                    [0; 5]
                );
            }
        }
    }
}
