//! Buffer slow mode of the opt-in fastcall boundary. This changes FP/vector
//! custody, not the underlying IPC operation, pointer validation or wire data.
//! Large messages, handles and finite deadlines execute their existing handler
//! once; a completed operation is never replayed as a fallback.
//!
//! RAX is one of REGISTER_{CALL,RECV,REPLY_RECV,REPLY}, RDI points to the
//! immutable block below, R9=IPC_REGISTER_BUFFER_MODE, other inputs are zero.
//! Only RAX carries a result; the register payload/metadata outputs are zero.

use super::*;

pub const IPC_REGISTER_BUFFER_MODE: u64 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcRegisterBufferArgs {
    pub operation: u64,
    pub args: [u64; 6],
}

const _: [(); 56] = [(); core::mem::size_of::<IpcRegisterBufferArgs>()];
const _: [(); 8] = [(); core::mem::offset_of!(IpcRegisterBufferArgs, args)];

/// Closed data-plane allowlist. In particular no Linux syscall, registration,
/// service-owner validation or another fastcall can be tunneled through here.
pub const fn ipc_register_buffer_entry(operation: u64) -> Option<u64> {
    match operation {
        SYS_RUSTOS_IPC_CALL
        | SYS_RUSTOS_IPC_CALL_BOUNDED
        | SYS_RUSTOS_IPC_CALL_WITH_HANDLES
        | SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED => Some(SYS_RUSTOS_IPC_REGISTER_CALL),
        SYS_RUSTOS_IPC_RECV
        | SYS_RUSTOS_IPC_TRY_RECV
        | SYS_RUSTOS_IPC_RECV_WITH_SENDER
        | SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER
        | SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED
        | SYS_RUSTOS_IPC_RECV_WITH_HANDLES => Some(SYS_RUSTOS_IPC_REGISTER_RECV),
        SYS_RUSTOS_IPC_REPLY => Some(SYS_RUSTOS_IPC_REGISTER_REPLY),
        SYS_RUSTOS_IPC_REPLY_WITH_HANDLES => Some(SYS_RUSTOS_IPC_REGISTER_REPLY),
        SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER => Some(SYS_RUSTOS_IPC_REGISTER_REPLY_RECV),
        _ => None,
    }
}

/// Use the existing IPC buffer operation with the fastcall compiler-clobber
/// contract. `None` means the operation is not IPC and no syscall was executed.
///
/// # Safety
/// All arguments obey the selected original syscall's pointer/capability and
/// one-shot ownership contract. The block remains immutable across SYSCALL.
#[cfg(target_arch = "x86_64")]
#[inline]
pub unsafe fn ipc_register_buffer_syscall(operation: u64, args: [u64; 6]) -> Option<i64> {
    let entry = ipc_register_buffer_entry(operation)?;
    let block = IpcRegisterBufferArgs { operation, args };
    let status;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") entry => status,
            inlateout("rdi") &block as *const IpcRegisterBufferArgs as u64 => _,
            inlateout("rsi") 0_u64 => _,
            inlateout("rdx") 0_u64 => _,
            inlateout("r10") 0_u64 => _,
            inlateout("r8") 0_u64 => _,
            inlateout("r9") IPC_REGISTER_BUFFER_MODE => _,
            clobber_abi("C"),
            options(nostack),
        );
    }
    Some(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_mode_is_closed_to_data_plane_and_exact_entry_family() {
        let groups: &[(u64, &[u64])] = &[
            (
                SYS_RUSTOS_IPC_REGISTER_CALL,
                &[
                    SYS_RUSTOS_IPC_CALL,
                    SYS_RUSTOS_IPC_CALL_BOUNDED,
                    SYS_RUSTOS_IPC_CALL_WITH_HANDLES,
                    SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED,
                ],
            ),
            (
                SYS_RUSTOS_IPC_REGISTER_RECV,
                &[
                    SYS_RUSTOS_IPC_RECV,
                    SYS_RUSTOS_IPC_TRY_RECV,
                    SYS_RUSTOS_IPC_RECV_WITH_SENDER,
                    SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER,
                    SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED,
                    SYS_RUSTOS_IPC_RECV_WITH_HANDLES,
                ],
            ),
            (
                SYS_RUSTOS_IPC_REGISTER_REPLY,
                &[SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_REPLY_WITH_HANDLES],
            ),
            (
                SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
                &[SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER],
            ),
        ];
        for &(entry, operations) in groups {
            for &operation in operations {
                assert_eq!(ipc_register_buffer_entry(operation), Some(entry));
            }
            assert!(ipc_register_shape_valid(
                entry,
                0,
                0,
                0,
                0,
                IPC_REGISTER_BUFFER_MODE
            ));
            assert!(!ipc_register_shape_valid(entry, 0, 0, 0, 0, 2));
            assert!(!ipc_register_shape_valid(
                entry,
                1,
                0,
                0,
                0,
                IPC_REGISTER_BUFFER_MODE
            ));
            assert!(!ipc_register_shape_valid(
                entry,
                0,
                1,
                0,
                0,
                IPC_REGISTER_BUFFER_MODE
            ));
            assert!(!ipc_register_shape_valid(
                entry,
                0,
                0,
                1,
                0,
                IPC_REGISTER_BUFFER_MODE
            ));
            assert!(!ipc_register_shape_valid(
                entry,
                0,
                0,
                0,
                1,
                IPC_REGISTER_BUFFER_MODE
            ));
        }
        for operation in [
            0,
            39,
            102,
            SYS_RUSTOS_IPC_ENDPOINT_CREATE,
            SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
            SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER,
            SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT,
            SYS_RUSTOS_IPC_REGISTER_CALL,
            SYS_RUSTOS_IPC_REGISTER_RECV,
            SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
            SYS_RUSTOS_IPC_REGISTER_REPLY,
            u64::MAX,
        ] {
            assert_eq!(ipc_register_buffer_entry(operation), None);
        }
        assert!(!ipc_register_shape_valid(
            39,
            0,
            0,
            0,
            0,
            IPC_REGISTER_BUFFER_MODE
        ));
    }
}
