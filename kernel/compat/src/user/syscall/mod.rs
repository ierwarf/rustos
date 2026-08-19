use core::arch::global_asm;
use x86_64::VirtAddr;
use x86_64::registers::control::{Efer, EferFlags};
use x86_64::registers::model_specific::{LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

use crate::arch::gdt;
use crate::debug;
use crate::memory::paging;
use crate::multitask;
use crate::user::abi::UserAbi;
use kernel_ps::api::syscall as syscall_core;

mod syscall_profile;
pub use syscall_profile::drain_syscall_profile;

pub(crate) mod linux;
pub(crate) mod windows;

const SYSCALL_ERR_INVALID: u64 = u64::MAX;
const USER_RFLAGS_RESERVED_BIT_1: u64 = 1 << 1;
const GENERAL_PROTECTION_VECTOR: u8 = 13;
const USER_RFLAGS_FORBIDDEN_MASK: u64 = RFlags::TRAP_FLAG.bits()
    | RFlags::DIRECTION_FLAG.bits()
    | RFlags::NESTED_TASK.bits()
    | RFlags::IOPL_LOW.bits()
    | RFlags::IOPL_HIGH.bits()
    | RFlags::VIRTUAL_8086_MODE.bits();

pub(crate) fn retire_current_linux_task_due_to_fault(
    vector: u8,
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
    rsp: u64,
) -> multitask::UserFaultDisposition {
    let faulting_process = multitask::current_user_process_id();
    let faulting_thread = multitask::current_user_thread_id().unwrap_or(0);
    if let Some(process_id) = faulting_process {
        debug::record_milestone(
            debug::LogCategory::Compat,
            "linux-user-fault",
            (process_id << 32) | (faulting_thread & u32::MAX as u64),
            ((vector as u64) << 56) | (rip & 0x00ff_ffff_ffff_ffff),
        );
    }
    let final_process_thread = faulting_process.and_then(|process_id| {
        (multitask::current_user_process_thread_count().unwrap_or(1) <= 1).then_some(process_id)
    });
    let disposition =
        multitask::retire_current_user_task_due_to_fault(vector, error_code, cr2, rip, rsp);
    if let Some(process_id) = fault_termination_process(disposition, final_process_thread) {
        linux::record_linux_process_fault_termination(process_id, vector);
    }
    disposition
}

pub(crate) fn cleanup_retired_task_runtime_state(
    task_id: u64,
    process_id: u64,
    process_terminal: bool,
    clear_child_tid: u64,
    robust_list_head: u64,
    robust_list_len: u64,
) -> usize {
    linux::cleanup_retired_task_runtime_state(
        task_id,
        process_id,
        process_terminal,
        clear_child_tid,
        robust_list_head,
        robust_list_len,
    )
}

fn fault_termination_process(
    disposition: multitask::UserFaultDisposition,
    final_process_thread: Option<u64>,
) -> Option<u64> {
    (disposition == multitask::UserFaultDisposition::Retired)
        .then_some(final_process_thread)
        .flatten()
}

#[repr(C)]
pub(crate) struct SyscallFrame {
    user_rsp: u64,
    user_rip: u64,
    user_rflags: u64,
    rax: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SysretReturnContract {
    user_rip: u64,
    user_rsp: u64,
    user_rflags: u64,
}

const _: [(); 128] = [(); core::mem::size_of::<SyscallFrame>()];
const _: [(); 0] = [(); core::mem::size_of::<SyscallFrame>() % 16];
const SYSCALL_ENTRY_XMM_BYTES: usize = 16 * 16;
const SYSCALL_ENTRY_STACK_BYTES: usize =
    core::mem::size_of::<SyscallFrame>() + SYSCALL_ENTRY_XMM_BYTES;
const _: [(); 384] = [(); SYSCALL_ENTRY_STACK_BYTES];

pub use syscall_core::{
    activate_linux_compat_cpu_local, linux_compat_current_task_offset,
    linux_compat_stack_guard_offset, prepare_for_context_return, set_kernel_stack_top,
    set_linux_compat_current_task_ptr, set_linux_compat_stack_guard, with_kernel_gs_base,
};
global_asm!(
    r#"
    .global syscall_entry
    .type syscall_entry, @function
    syscall_entry:
        swapgs
        mov gs:[8], rsp
        mov rsp, gs:[0]
        # The scheduler and CPU-local setter require a 16-byte-aligned stack
        # top. Normalize again at the architectural trust boundary so a stale
        # or independently initialized CPU-local value cannot turn an ordinary
        # aligned Rust load/store into #GP.
        and rsp, -16
        # Preserve every architectural XMM register before the first Rust
        # instruction. This is the whole of the syscall path's FPU custody:
        # xmm0-xmm15 are the only registers ordinary kernel code can disturb.
        #
        # x87 and MXCSR are not saved because nothing writes them -- verified on
        # the linked image by `tools/xtask/src/build/nucleus_audit.rs`, which
        # fails the build on any x87 or floating-point arithmetic instruction.
        # The ymm upper halves are not saved because the only kernel code that
        # touches them brackets itself with `kernel_hal::arch::simd`'s
        # `wide_simd_section`, which the same audit enforces.
        #
        # The complete 384-byte allocation preserves call-site alignment so
        # Rust enters with rsp % 16 == 8 as required by the x86_64 SysV ABI.
        sub rsp, {stack_bytes}

        # SIMD-ENTRY-SAVE-BEGIN
        movdqu [rsp + {frame_bytes} + 0x00], xmm0
        movdqu [rsp + {frame_bytes} + 0x10], xmm1
        movdqu [rsp + {frame_bytes} + 0x20], xmm2
        movdqu [rsp + {frame_bytes} + 0x30], xmm3
        movdqu [rsp + {frame_bytes} + 0x40], xmm4
        movdqu [rsp + {frame_bytes} + 0x50], xmm5
        movdqu [rsp + {frame_bytes} + 0x60], xmm6
        movdqu [rsp + {frame_bytes} + 0x70], xmm7
        movdqu [rsp + {frame_bytes} + 0x80], xmm8
        movdqu [rsp + {frame_bytes} + 0x90], xmm9
        movdqu [rsp + {frame_bytes} + 0xA0], xmm10
        movdqu [rsp + {frame_bytes} + 0xB0], xmm11
        movdqu [rsp + {frame_bytes} + 0xC0], xmm12
        movdqu [rsp + {frame_bytes} + 0xD0], xmm13
        movdqu [rsp + {frame_bytes} + 0xE0], xmm14
        movdqu [rsp + {frame_bytes} + 0xF0], xmm15
        # SIMD-ENTRY-SAVE-END

        mov [rsp + 24], rax
        mov rax, gs:[8]
        mov [rsp + 0], rax
        mov [rsp + 8], rcx
        mov [rsp + 16], r11
        mov [rsp + 32], rdi
        mov [rsp + 40], rsi
        mov [rsp + 48], rdx
        mov [rsp + 56], r8
        mov [rsp + 64], r9
        mov [rsp + 72], r10
        mov [rsp + 80], rbx
        mov [rsp + 88], rbp
        mov [rsp + 96], r12
        mov [rsp + 104], r13
        mov [rsp + 112], r14
        mov [rsp + 120], r15

        cld
        # `syscall` masks IF via IA32_FMASK. Re-enable interrupts while running
        # the Rust syscall body so blocking VFS/storage paths do not spin with
        # the timer disabled. Disable again before restoring the sysret frame.
        sti
        mov rdi, rsp
        call syscall_dispatch
        cli

        # SIMD-ENTRY-RESTORE-BEGIN
        movdqu xmm0, [rsp + {frame_bytes} + 0x00]
        movdqu xmm1, [rsp + {frame_bytes} + 0x10]
        movdqu xmm2, [rsp + {frame_bytes} + 0x20]
        movdqu xmm3, [rsp + {frame_bytes} + 0x30]
        movdqu xmm4, [rsp + {frame_bytes} + 0x40]
        movdqu xmm5, [rsp + {frame_bytes} + 0x50]
        movdqu xmm6, [rsp + {frame_bytes} + 0x60]
        movdqu xmm7, [rsp + {frame_bytes} + 0x70]
        movdqu xmm8, [rsp + {frame_bytes} + 0x80]
        movdqu xmm9, [rsp + {frame_bytes} + 0x90]
        movdqu xmm10, [rsp + {frame_bytes} + 0xA0]
        movdqu xmm11, [rsp + {frame_bytes} + 0xB0]
        movdqu xmm12, [rsp + {frame_bytes} + 0xC0]
        movdqu xmm13, [rsp + {frame_bytes} + 0xD0]
        movdqu xmm14, [rsp + {frame_bytes} + 0xE0]
        movdqu xmm15, [rsp + {frame_bytes} + 0xF0]
        # SIMD-ENTRY-RESTORE-END

        mov rdi, [rsp + 32]
        mov rsi, [rsp + 40]
        mov rdx, [rsp + 48]
        mov r8, [rsp + 56]
        mov r9, [rsp + 64]
        mov r10, [rsp + 72]
        mov rbx, [rsp + 80]
        mov rbp, [rsp + 88]
        mov r12, [rsp + 96]
        mov r13, [rsp + 104]
        mov r14, [rsp + 112]
        mov r15, [rsp + 120]
        mov r11, [rsp + 16]
        mov rcx, [rsp + 8]
        mov rsp, [rsp + 0]
        swapgs
        sysretq
    .size syscall_entry, . - syscall_entry
 "#,
    frame_bytes = const core::mem::size_of::<SyscallFrame>(),
    stack_bytes = const SYSCALL_ENTRY_STACK_BYTES,
);

pub fn init() {
    syscall_core::init_cpu_local();
    let entry_addr = paging::higher_half_addr(syscall_entry as *const () as usize as u64);
    let syscall_mask = RFlags::INTERRUPT_FLAG | RFlags::TRAP_FLAG | RFlags::DIRECTION_FLAG;

    unsafe {
        Efer::write(
            Efer::read() | EferFlags::SYSTEM_CALL_EXTENSIONS | EferFlags::NO_EXECUTE_ENABLE,
        );
        Star::write(
            gdt::user_code_selector(),
            gdt::user_data_selector(),
            gdt::kernel_code_selector(),
            gdt::kernel_data_selector(),
        )
        .expect("syscall STAR selectors must be valid");
        LStar::write(VirtAddr::new(entry_addr));
    }
    SFMask::write(syscall_mask);
}

unsafe extern "C" {
    fn syscall_entry();
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(frame: *mut SyscallFrame) -> u64 {
    let frame = unsafe { &mut *frame };
    let phase = syscall_profile::now();
    let abi = validate_syscall_entry_or_terminate(frame);
    let phase = syscall_profile::charge(syscall_profile::SyscallPhase::Validate, phase);
    trace_syscall_entry(frame, abi);
    let phase = syscall_profile::charge(syscall_profile::SyscallPhase::Trace, phase);
    let result = dispatch_syscall(frame, abi);
    let phase = syscall_profile::charge(syscall_profile::SyscallPhase::Dispatch, phase);
    // The syscall body runs with IF=1, so this software interrupt preserves
    // an interruptible kernel continuation. The scheduler may switch here and
    // later resume the exact syscall before restoring the entering user SIMD
    // image. This closes hot-syscall starvation without a high-rate PIT retry.
    multitask::reschedule_deferred_from_interruptible_syscall();
    let phase = syscall_profile::charge(syscall_profile::SyscallPhase::RescheduleDeferred, phase);
    // The syscall frame stayed live on the task's kernel stack across that
    // possible continuation. Revalidate the exact SYSRET boundary after the
    // last resume so a stale pre-schedule decision can never authorize return.
    //
    // The user's XMM image rode that continuation on this task's own kernel
    // stack, so it needs no separate custody: the stack is the save area, and
    // the scheduler's per-task XSAVE pair covers the register file across the
    // switch.
    let return_abi = validate_syscall_entry_or_terminate(frame);
    let phase = syscall_profile::charge(syscall_profile::SyscallPhase::Validate, phase);
    trace_syscall_exit(frame, return_abi, result);
    syscall_profile::charge(syscall_profile::SyscallPhase::Trace, phase);
    result
}

fn dispatch_syscall(frame: &mut SyscallFrame, abi: UserAbi) -> u64 {
    match abi {
        UserAbi::Linux => linux::dispatch_linux_syscall(frame),
        UserAbi::Windows => windows::dispatch_syscall(frame),
    }
}

fn syscall_frame_security_check(frame: &SyscallFrame) -> bool {
    multitask::current_user_abi().is_some() && syscall_return_contract(frame).is_some()
}

pub(super) fn validate_syscall_entry_or_terminate(frame: &SyscallFrame) -> UserAbi {
    let Some(abi) = multitask::current_user_abi() else {
        panic!(
            "rejected syscall without active user context: rip={:#x} rsp={:#x} rflags={:#x} nr={:#x}",
            frame.user_rip, frame.user_rsp, frame.user_rflags, frame.rax,
        );
    };

    if syscall_return_contract(frame).is_none() {
        let (process_id, thread_id) = multitask::current_user_log_ids().unwrap_or_default();
        debug::println!(
            "terminating task due to unsafe syscall return contract: pid={} tid={} rip={:#x} rsp={:#x} rflags={:#x} nr={:#x}",
            process_id,
            thread_id,
            frame.user_rip,
            frame.user_rsp,
            frame.user_rflags,
            frame.rax,
        );
        let disposition = retire_current_linux_task_due_to_fault(
            GENERAL_PROTECTION_VECTOR,
            Some(0),
            0,
            frame.user_rip,
            frame.user_rsp,
        );
        match disposition {
            multitask::UserFaultDisposition::Retired => {}
            multitask::UserFaultDisposition::Resumed
            | multitask::UserFaultDisposition::Unhandled => {
                panic!(
                    "invalid syscall return contract could not retire current task: rip={:#x} rsp={:#x}",
                    frame.user_rip, frame.user_rsp
                );
            }
        }
        multitask::halt_current_retired_task();
    }

    abi
}

fn syscall_return_contract(frame: &SyscallFrame) -> Option<SysretReturnContract> {
    let contract = SysretReturnContract {
        user_rip: frame.user_rip,
        user_rsp: frame.user_rsp,
        user_rflags: frame.user_rflags,
    };
    if !user_address_is_sysret_safe(contract.user_rip)
        || !user_address_is_sysret_safe(contract.user_rsp)
        || !user_rflags_are_sysret_safe(contract.user_rflags)
    {
        return None;
    }
    Some(contract)
}

fn user_rflags_are_sysret_safe(rflags: u64) -> bool {
    (rflags & USER_RFLAGS_RESERVED_BIT_1) != 0 && (rflags & USER_RFLAGS_FORBIDDEN_MASK) == 0
}

fn user_address_is_sysret_safe(addr: u64) -> bool {
    // `sysretq` only returns to the low canonical half on this kernel. Our Linux user
    // address space is intentionally kept there, so make that contract explicit here.
    (paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&addr)
        && ((addr >> 47) & 0x1) == 0
}

#[cfg(rustos_log_syscall_debug)]
fn trace_syscall_entry(frame: &SyscallFrame, abi: UserAbi) {
    if !debug::enabled!(syscall, debug) {
        return;
    }

    let Some(snapshot) = multitask::current_user_snapshot() else {
        debug::debug!(
            syscall,
            alloc::format!(
                "entry abi={:?} pid=? tid=? nr={:#x} rip={:#x} rsp={:#x} rflags={:#x}",
                abi,
                frame.rax,
                frame.user_rip,
                frame.user_rsp,
                frame.user_rflags,
            )
            .as_str()
        );
        return;
    };

    debug::debug!(
        syscall,
        alloc::format!(
            "entry abi={:?} pid={} tid={} nr={:#x} rip={:#x} rsp={:#x} rflags={:#x}",
            abi,
            snapshot.process_id(),
            snapshot.thread_id(),
            frame.rax,
            frame.user_rip,
            frame.user_rsp,
            frame.user_rflags,
        )
        .as_str()
    );
}

#[cfg(not(rustos_log_syscall_debug))]
fn trace_syscall_entry(_frame: &SyscallFrame, _abi: UserAbi) {}

#[cfg(rustos_log_syscall_debug)]
fn trace_syscall_exit(frame: &SyscallFrame, abi: UserAbi, result: u64) {
    if !debug::enabled!(syscall, debug) {
        return;
    }

    let Some(snapshot) = multitask::current_user_snapshot() else {
        debug::debug!(
            syscall,
            alloc::format!(
                "exit abi={:?} nr={:#x} ret={:#x} rip={:#x} rsp={:#x} rflags={:#x}",
                abi,
                frame.rax,
                result,
                frame.user_rip,
                frame.user_rsp,
                frame.user_rflags,
            )
            .as_str()
        );
        return;
    };

    debug::debug!(
        syscall,
        alloc::format!(
            "exit abi={:?} pid={} tid={} nr={:#x} ret={:#x} rip={:#x} rsp={:#x} rflags={:#x}",
            abi,
            snapshot.process_id(),
            snapshot.thread_id(),
            frame.rax,
            result,
            frame.user_rip,
            frame.user_rsp,
            frame.user_rflags,
        )
        .as_str()
    );
}

#[cfg(not(rustos_log_syscall_debug))]
fn trace_syscall_exit(_frame: &SyscallFrame, _abi: UserAbi, _result: u64) {}

#[cfg(test)]
mod tests {
    use super::{
        SyscallFrame, fault_termination_process, syscall_return_contract,
        user_address_is_sysret_safe, user_rflags_are_sysret_safe,
    };
    use crate::memory::paging;
    use crate::multitask::UserFaultDisposition;

    #[test]
    fn only_retired_final_thread_commits_fault_termination() {
        assert_eq!(
            fault_termination_process(UserFaultDisposition::Resumed, Some(41)),
            None
        );
        assert_eq!(
            fault_termination_process(UserFaultDisposition::Unhandled, Some(41)),
            None
        );
        assert_eq!(
            fault_termination_process(UserFaultDisposition::Retired, None),
            None
        );
        assert_eq!(
            fault_termination_process(UserFaultDisposition::Retired, Some(41)),
            Some(41)
        );
    }

    fn valid_frame() -> SyscallFrame {
        SyscallFrame {
            user_rsp: paging::USER_SPACE_BASE + 0x4000,
            user_rip: paging::USER_SPACE_BASE + 0x2000,
            user_rflags: 0x202,
            rax: 0,
            rdi: 0,
            rsi: 0,
            rdx: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
        }
    }

    #[test]
    fn sysret_contract_accepts_low_canonical_user_addresses() {
        let frame = valid_frame();
        assert!(user_address_is_sysret_safe(frame.user_rip));
        assert!(user_address_is_sysret_safe(frame.user_rsp));
        assert!(user_rflags_are_sysret_safe(frame.user_rflags));
        assert!(syscall_return_contract(&frame).is_some());
    }

    #[test]
    fn sysret_contract_rejects_noncanonical_or_kernel_addresses() {
        let mut frame = valid_frame();
        frame.user_rip = paging::USER_SPACE_END_EXCLUSIVE;
        assert!(syscall_return_contract(&frame).is_none());

        frame = valid_frame();
        frame.user_rsp = 0xffff_8000_0000_1000;
        assert!(syscall_return_contract(&frame).is_none());
    }

    #[test]
    fn sysret_contract_rejects_forbidden_rflags() {
        let mut frame = valid_frame();
        frame.user_rflags = 0x200;
        assert!(!user_rflags_are_sysret_safe(frame.user_rflags));
        assert!(syscall_return_contract(&frame).is_none());

        frame = valid_frame();
        frame.user_rflags |= x86_64::registers::rflags::RFlags::TRAP_FLAG.bits();
        assert!(syscall_return_contract(&frame).is_none());
    }

    #[test]
    fn sysret_validation_follows_last_interruptible_resume() {
        let source = include_str!("mod.rs");
        let dispatch = source
            .split("extern \"C\" fn syscall_dispatch")
            .nth(1)
            .and_then(|rest| rest.split("fn dispatch_syscall").next())
            .expect("syscall dispatch body");
        let resume = dispatch
            .find("multitask::reschedule_deferred_from_interruptible_syscall();")
            .expect("interruptible scheduler tail");
        let validation = dispatch
            .find("let return_abi = validate_syscall_entry_or_terminate(frame);")
            .expect("post-resume SYSRET validation");
        assert!(
            resume < validation,
            "SYSRET validation must follow the last possible continuation resume"
        );
    }

    #[test]
    fn syscall_entry_preserves_xmm_before_any_rust_dispatch() {
        let source = include_str!("mod.rs");
        let save = source
            .split("# SIMD-ENTRY-SAVE-BEGIN")
            .nth(1)
            .and_then(|rest| rest.split("# SIMD-ENTRY-SAVE-END").next())
            .expect("assembly XMM entry save");
        let restore = source
            .split("# SIMD-ENTRY-RESTORE-BEGIN")
            .nth(1)
            .and_then(|rest| rest.split("# SIMD-ENTRY-RESTORE-END").next())
            .expect("assembly XMM return restore");
        let dispatch = source.find("call syscall_dispatch").expect("Rust dispatch");
        let save_begin = source
            .find("# SIMD-ENTRY-SAVE-BEGIN")
            .expect("XMM save marker");
        let restore_begin = source
            .find("# SIMD-ENTRY-RESTORE-BEGIN")
            .expect("XMM restore marker");

        assert_eq!(save.matches("movdqu [rsp +").count(), 16);
        assert_eq!(restore.matches("movdqu xmm").count(), 16);
        assert!(save_begin < dispatch);
        assert!(dispatch < restore_begin);
    }

    #[test]
    fn the_entry_stub_is_the_whole_of_the_syscall_paths_fpu_custody() {
        // A per-syscall XSAVE/XRSTOR pair used to sit inside `syscall_dispatch`,
        // covering x87, MXCSR, and the ymm upper halves as well as XMM, at a
        // measured 829 ticks of every syscall. It is gone. What replaced it is
        // not a cheaper save: it is the checked absence of anything to save.
        //
        // Two of the three gaps are empty in the linked image -- no x87, no
        // floating-point arithmetic -- and the third is bracketed at its two
        // call sites. `tools/xtask/src/build/nucleus_audit.rs` fails the build
        // if any of that stops holding.
        //
        // This witness is what stops the pair from being reintroduced by reflex
        // the next time someone reads the entry stub and notices it saves only
        // sixteen registers.
        let source = include_str!("mod.rs");
        let dispatch = source
            .split("extern \"C\" fn syscall_dispatch(")
            .nth(1)
            .and_then(|rest| rest.split("\nfn dispatch_syscall(").next())
            .expect("syscall_dispatch body");
        for reintroduced in [
            "SyscallUserSimdSnapshot",
            "syscall_simd",
            "save_state",
            "restore_state",
            "SimdCapture",
            "SimdRestore",
        ] {
            assert!(
                !dispatch.contains(reintroduced),
                "syscall_dispatch reintroduced a per-syscall FPU snapshot: {reintroduced}",
            );
        }
        // The entry stub has to say why it saves only the narrow image, and
        // name the check that keeps that true.
        let stub = source
            .split("syscall_entry:")
            .nth(1)
            .expect("entry stub body");
        assert!(stub.contains("nucleus_audit.rs"));
        assert!(stub.contains("wide_simd_section"));
        // The image rides the blocked continuation on the task's own stack.
        assert!(dispatch.contains("kernel stack"));
    }
}
