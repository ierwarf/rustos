//! Untimed ring-3 witnesses for the opt-in FP/vector clobber contract.
use rustos_user_abi::syscall::IpcRegisterResult;

#[repr(C, align(64))]
struct Image([u8; 576]);

/// Capture immediately after SYSCALL, before Rust may reuse caller-saved XMM.
/// This deliberately seeds physical x87/MMX and both ends of the vector bank.
pub(crate) unsafe fn checked_syscall(
    operation: u64,
    endpoint: u64,
    len: u64,
    words: [u64; 2],
    reply: u64,
    register: bool,
) -> (IpcRegisterResult, bool) {
    unsafe { checked_syscall_mode(operation, endpoint, len, words, reply, register, 0) }
}

/// Buffer slow mode must obey the identical physical-data/control contract,
/// including copy faults and rejected forwarded operations.
pub(crate) unsafe fn checked_buffer_syscall(
    operation: u64,
    args_ptr: u64,
) -> (IpcRegisterResult, bool) {
    unsafe {
        checked_syscall_mode(
            operation,
            args_ptr,
            0,
            [0; 2],
            0,
            true,
            rustos_user_abi::syscall::IPC_REGISTER_BUFFER_MODE,
        )
    }
}

unsafe fn checked_syscall_mode(
    operation: u64,
    endpoint: u64,
    len: u64,
    words: [u64; 2],
    reply: u64,
    register: bool,
    mode: u64,
) -> (IpcRegisterResult, bool) {
    let mut before = Image([0; 576]);
    let mut after = Image([0; 576]);
    let controls = [
        0x1f80u32 | ((operation as u32 & 3) << 13),
        0x037f | ((operation as u32 & 3) << 10),
    ];
    let avx = unsafe {
        let cpuid = core::arch::x86_64::__cpuid(1);
        if cpuid.ecx & (1 << 27) != 0 {
            core::arch::x86_64::_xgetbv(0) & 6 == 6
        } else {
            false
        }
    };
    let mut status = operation;
    let mut word0 = words[0];
    let mut word1 = words[1];
    let mut cap = reply;
    let mut sender_pid = mode;
    let mut sender_tid = len;
    unsafe {
        core::arch::asm!(
            "fxsave64 [r13]",
            "fninit", "fld1", "fld1", "fld1", "fld1",
            "fld1", "fld1", "fld1", "fld1",
            "ldmxcsr [r15]", "fldcw [r15 + 4]",
            "pcmpeqd xmm0, xmm0", "pcmpeqd xmm15, xmm15",
            "test r14, r14", "jz 2f",
            "vpcmpeqd ymm15, ymm15, ymm15",
            "2:", "syscall",
            "fxsave64 [r12]",
            "test r14, r14", "jz 3f",
            "vmovdqu [r12 + 512], ymm15",
            "3:", "fxrstor64 [r13]",
            in("r12") after.0.as_mut_ptr(), in("r13") before.0.as_mut_ptr(),
            in("r14") avx as u64, in("r15") controls.as_ptr(),
            inlateout("rax") status, in("rdi") endpoint,
            inlateout("rsi") sender_tid, inlateout("rdx") word0,
            inlateout("r10") word1, inlateout("r8") cap, inlateout("r9") sender_pid,
            clobber_abi("C"), options(nostack),
        );
    }
    let controls_ok = u16::from_le_bytes(after.0[0..2].try_into().unwrap()) == controls[1] as u16
        && u32::from_le_bytes(after.0[24..28].try_into().unwrap()) == controls[0];
    let data_ok = if register {
        after.0[4] == 0 // empty x87 tag word, but physical data must ALSO be zero
            && (0..8).all(|i| after.0[32 + 16*i..42 + 16*i].iter().all(|&b| b == 0))
            && after.0[160..416].iter().all(|&b| b == 0)
            && (!avx || after.0[512..544].iter().all(|&b| b == 0))
    } else {
        after.0[4] == 0xff
            && after.0[160..176].iter().all(|&b| b == 0xff)
            && after.0[400..416].iter().all(|&b| b == 0xff)
            && (!avx || after.0[512..544].iter().all(|&b| b == 0xff))
    };
    if !controls_ok || !data_ok {
        eprintln!(
            "ipc SIMD witness op={operation:#x} register={register} controls_ok={controls_ok} data_ok={data_ok} cw={:#x} mxcsr={:#x} tag={:#x} x87_nonzero={} xmm_nonzero={} ymm_nonzero={}",
            u16::from_le_bytes(after.0[0..2].try_into().unwrap()),
            u32::from_le_bytes(after.0[24..28].try_into().unwrap()),
            after.0[4],
            (0..8)
                .filter(|&i| after.0[32 + 16 * i..42 + 16 * i].iter().any(|&b| b != 0))
                .count(),
            after.0[160..416].iter().filter(|&&b| b != 0).count(),
            after.0[512..544].iter().filter(|&&b| b != 0).count()
        );
    }
    (
        IpcRegisterResult {
            status: status as i64,
            words: [word0, word1],
            reply: cap,
            sender_pid,
            sender_tid,
        },
        controls_ok && data_ok,
    )
}
