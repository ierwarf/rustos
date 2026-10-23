//! Opt-in fastcall IPC, not a change to ordinary Linux syscall preservation.
//! Default mode is pointer-free short IPC; IPC_REGISTER_BUFFER_MODE reuses an
//! allowlisted existing buffer/handle operation through IpcRegisterBufferArgs.
//!
//! Inputs: RDI endpoint (reply capability for REPLY), RSI byte length,
//! RDX/R10 little-endian payload words, R8 old reply capability for REPLY_RECV,
//! R9 zero. RECV requires RSI/RDX/R10/R8 zero. CALL/REPLY require R8 zero.
//! CALL requests must contain 1..=16 bytes; replies may contain 0..=16.
//! Outputs: RAX length or native errno (tagged post-commit for REPLY_RECV),
//! RDX/R10 payload, R8 reply capability, R9 authenticated sender PID, RSI TID.
//! Error outputs are zero. Default mode has no descriptors/pointers/timeouts;
//! buffer mode retains the existing fault-revalidated pointer/deadline contract.
//! XMM registers are caller-saved at this boundary; implementations may still
//! preserve them. All enabled FP/vector data (x87/MMX/XMM/YMM) is caller-clobbered
//! and sanitized on return; x87 status/stack state is reset. The entering x87
//! control word and MXCSR are preserved. Ordinary syscalls and asynchronous
//! context preservation retain their full-state contract.

pub const SYS_RUSTOS_IPC_REGISTER_CALL: u64 = 0x5255_0052;
pub const SYS_RUSTOS_IPC_REGISTER_RECV: u64 = 0x5255_0053;
pub const SYS_RUSTOS_IPC_REGISTER_REPLY_RECV: u64 = 0x5255_0054;
pub const SYS_RUSTOS_IPC_REGISTER_REPLY: u64 = 0x5255_0055;
pub const IPC_REGISTER_BYTES: usize = 16;

pub const fn ipc_register_shape_valid(
    operation: u64,
    len: u64,
    word0: u64,
    word1: u64,
    reply: u64,
    reserved: u64,
) -> bool {
    if reserved == super::IPC_REGISTER_BUFFER_MODE {
        return len == 0
            && word0 == 0
            && word1 == 0
            && reply == 0
            && matches!(
                operation,
                SYS_RUSTOS_IPC_REGISTER_CALL
                    | SYS_RUSTOS_IPC_REGISTER_RECV
                    | SYS_RUSTOS_IPC_REGISTER_REPLY_RECV
                    | SYS_RUSTOS_IPC_REGISTER_REPLY
            );
    }
    if reserved != 0 || len > IPC_REGISTER_BYTES as u64 {
        return false;
    }
    match operation {
        SYS_RUSTOS_IPC_REGISTER_RECV => len == 0 && word0 == 0 && word1 == 0 && reply == 0,
        SYS_RUSTOS_IPC_REGISTER_CALL => len != 0 && reply == 0,
        SYS_RUSTOS_IPC_REGISTER_REPLY => reply == 0,
        SYS_RUSTOS_IPC_REGISTER_REPLY_RECV => reply != 0,
        _ => false,
    }
}

pub fn ipc_register_pack(bytes: &[u8]) -> Option<[u64; 2]> {
    if bytes.len() > IPC_REGISTER_BYTES {
        return None;
    }
    let mut padded = [0; IPC_REGISTER_BYTES];
    padded[..bytes.len()].copy_from_slice(bytes);
    Some([
        u64::from_le_bytes(padded[..8].try_into().unwrap()),
        u64::from_le_bytes(padded[8..].try_into().unwrap()),
    ])
}

pub fn ipc_register_unpack(words: [u64; 2]) -> [u8; IPC_REGISTER_BYTES] {
    let mut bytes = [0; IPC_REGISTER_BYTES];
    bytes[..8].copy_from_slice(&words[0].to_le_bytes());
    bytes[8..].copy_from_slice(&words[1].to_le_bytes());
    bytes
}

/// Register return image. Negative `status` uses the same errno/commit tagging
/// as buffer IPC. Only a successful receive carries authenticated metadata.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcRegisterResult {
    pub status: i64,
    pub words: [u64; 2],
    pub reply: u64,
    pub sender_pid: u64,
    pub sender_tid: u64,
}

/// Execute the opt-in ABI, declaring *all* SysV caller-saved SIMD registers to
/// the compiler. The asm is a memory barrier (not `nomem`/`readonly`).
///
/// # Safety
/// The caller must obey the endpoint/reply ownership and one-shot protocol.
/// In particular a post-commit REPLY_RECV error must never retry the old reply.
#[cfg(target_arch = "x86_64")]
#[inline]
pub unsafe fn ipc_register_syscall(
    operation: u64,
    endpoint: u64,
    len: u64,
    words: [u64; 2],
    reply: u64,
) -> IpcRegisterResult {
    let (status, tid, word0, word1, next_reply, pid);
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") operation => status,
            inlateout("rdi") endpoint => _,
            inlateout("rsi") len => tid,
            inlateout("rdx") words[0] => word0,
            inlateout("r10") words[1] => word1,
            inlateout("r8") reply => next_reply,
            inlateout("r9") 0_u64 => pid,
            clobber_abi("C"),
            options(nostack),
        );
    }
    IpcRegisterResult {
        status,
        words: [word0, word1],
        reply: next_reply,
        sender_pid: pid,
        sender_tid: tid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_length_round_trips_and_zeroes_unused_bytes() {
        let source = [0xa5; IPC_REGISTER_BYTES];
        for len in 0..=IPC_REGISTER_BYTES {
            let bytes = ipc_register_unpack(ipc_register_pack(&source[..len]).unwrap());
            assert_eq!(&bytes[..len], &source[..len]);
            assert!(bytes[len..].iter().all(|&byte| byte == 0));
        }
        assert!(ipc_register_pack(&[0; IPC_REGISTER_BYTES + 1]).is_none());
    }

    #[test]
    fn rejects_unknown_oversize_reserved_and_ambiguous_reply_shapes() {
        let call = SYS_RUSTOS_IPC_REGISTER_CALL;
        assert!(ipc_register_shape_valid(call, 16, 1, 2, 0, 0));
        assert!(!ipc_register_shape_valid(call, 0, 0, 0, 0, 0));
        for len in [17, u64::MAX] {
            assert!(!ipc_register_shape_valid(call, len, 1, 2, 0, 0));
        }
        assert!(!ipc_register_shape_valid(call, 16, 1, 2, 0, 1));
        assert!(!ipc_register_shape_valid(call, 16, 1, 2, 1, 0));
        assert!(!ipc_register_shape_valid(0, 0, 0, 0, 0, 0));
        assert!(ipc_register_shape_valid(
            SYS_RUSTOS_IPC_REGISTER_RECV,
            0,
            0,
            0,
            0,
            0
        ));
        assert!(!ipc_register_shape_valid(
            SYS_RUSTOS_IPC_REGISTER_RECV,
            0,
            1,
            0,
            0,
            0
        ));
        assert!(!ipc_register_shape_valid(
            SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
            16,
            1,
            2,
            0,
            0
        ));
        assert!(ipc_register_shape_valid(
            SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
            16,
            1,
            2,
            3,
            0
        ));
        assert_eq!(
            [
                call,
                SYS_RUSTOS_IPC_REGISTER_RECV,
                SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
                SYS_RUSTOS_IPC_REGISTER_REPLY
            ],
            [0x5255_0052, 0x5255_0053, 0x5255_0054, 0x5255_0055]
        );
    }
}
