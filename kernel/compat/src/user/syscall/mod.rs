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
        if vector == 14 {
            // A bare RIP cannot distinguish an absent VMA from an anonymous
            // frame refusal or a protected access. Preserve CR2 and x86's
            // page-fault bits for the exact terminal fault, not every retry.
            debug::record_milestone(
                debug::LogCategory::Compat,
                "linux-user-page-fault-address",
                cr2,
                error_code.unwrap_or(u64::MAX),
            );
        }
    }
    // IF is still clear in the exception callback. The original terminal
    // producer carries the fault to the runtime cleanup consumer, which may
    // close service handles and notify policy only from interruptible context.
    multitask::retire_current_user_task_due_to_fault(vector, error_code, cr2, rip, rsp)
}

#[repr(C)]
pub(crate) struct SyscallFrame {
    user_rsp: u64,
    rax: u64,
    rbx: u64,
    user_rip: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rbp: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    user_rflags: u64,
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
// Offset the scheduler context by one word so its GPR sequence and XMM image
// coincide with the syscall entry image. `user_rsp` occupies the leading word
// until the HAL initializes the normalized IRET suffix.
const SYSCALL_SCHEDULE_CONTEXT_OFFSET: usize =
    core::mem::size_of::<SyscallFrame>() - kernel_hal::api::SAVED_CONTEXT_XMM_OFFSET;
const _: [(); 8] = [(); SYSCALL_SCHEDULE_CONTEXT_OFFSET];
const _: [(); 0] = [(); core::mem::offset_of!(SyscallFrame, rax) - SYSCALL_SCHEDULE_CONTEXT_OFFSET];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, rbx) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 8];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, user_rip) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 16];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, rdx) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 24];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, rsi) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 32];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, rdi) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 40];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, rbp) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 48];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, r8) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 56];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, r9) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 64];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, r10) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 72];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, user_rflags) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 80];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, r12) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 88];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, r13) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 96];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, r14) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 104];
const _: [(); 0] =
    [(); core::mem::offset_of!(SyscallFrame, r15) - SYSCALL_SCHEDULE_CONTEXT_OFFSET - 112];
const SYSCALL_ENTRY_STACK_BYTES: usize =
    (SYSCALL_SCHEDULE_CONTEXT_OFFSET + kernel_hal::api::SAVED_CONTEXT_BYTES + 15) & !15;
const _: [(); 432] = [(); SYSCALL_ENTRY_STACK_BYTES];

/// What a register-IPC syscall that blocked through the direct schedule needs to
/// finish from its syscall frame alone. Written by the blocking code just before
/// the direct commit, read once by [`fast_ipc_parked_tail`]; meaningless while
/// `kind` is [`ParkKind::None`].
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct ParkRecord {
    pub(super) kind: u64,
    pub(super) endpoint: u64,
    pub(super) reply: u64,
    pub(super) deadline_tick: u64,
}

pub(super) mod park_kind {
    pub(in crate::user::syscall) const NONE: u64 = 0;
    /// A `REGISTER_CALL` waiting for its reply (`endpoint`, `reply`, `deadline_tick`).
    pub(in crate::user::syscall) const CALL: u64 = 1;
    /// A `REGISTER_REPLY_RECV` whose reply is published and that waits for the
    /// next request on `endpoint`.
    pub(in crate::user::syscall) const RECV: u64 = 2;
}

// Outside the normalized SavedContext suffix: never overwritten by HAL.
#[repr(C)]
struct RegisterIpcBoundary {
    active: u64,
    mxcsr: u32,
    control_word: u16,
    // The trailing two bytes are C-layout padding, not an uninitialized Rust
    // integer field: the assembly initializes only the three values above.
    park: ParkRecord,
}
const _: [(); 48] = [(); core::mem::size_of::<RegisterIpcBoundary>()];
const _: [(); 8] = [(); core::mem::offset_of!(RegisterIpcBoundary, mxcsr)];
const _: [(); 12] = [(); core::mem::offset_of!(RegisterIpcBoundary, control_word)];
const _: [(); 16] = [(); core::mem::offset_of!(RegisterIpcBoundary, park)];

unsafe fn register_ipc_boundary(frame: *mut SyscallFrame) -> *mut RegisterIpcBoundary {
    unsafe { frame.cast::<u8>().add(SYSCALL_ENTRY_STACK_BYTES).cast() }
}

/// Finalize before the nonreturning saved-user-context route. Its generic
/// scheduler capture must see this task's controls and no foreign FP data.
#[inline(never)]
unsafe fn finish_register_ipc_return(frame: *mut SyscallFrame) {
    let boundary = unsafe { &mut *register_ipc_boundary(frame) };
    unsafe {
        // The IRET leaf restores the explicit XMM image, not just hardware.
        frame
            .cast::<u8>()
            .add(core::mem::size_of::<SyscallFrame>())
            .write_bytes(0, 256);
        kernel_hal::api::arch::simd::clear_ipc_registers();
        core::arch::asm!(
            "ldmxcsr [{mxcsr}]", "fldcw [{cw}]",
            mxcsr = in(reg) &boundary.mxcsr, cw = in(reg) &boundary.control_word,
            options(nostack, readonly),
        );
    }
    boundary.active = 0;
}

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
        endbr64
        swapgs
        mov gs:[8], rsp
        mov rsp, gs:[0]
        # The scheduler and CPU-local setter require a 16-byte-aligned stack
        # top. Normalize again at the architectural trust boundary so a stale
        # or independently initialized CPU-local value cannot turn an ordinary
        # aligned Rust load/store into #GP.
        and rsp, -16
        # Ordinary syscalls preserve every architectural XMM register before
        # Rust. Only the opt-in Linux register IPC ABI may discard FP data;
        # its immutable entry marker and controls live beyond SavedContext.
        #
        # Outside the named register-IPC custody boundary, nothing writes x87
        # or MXCSR -- verified on the linked image by
        # `tools/xtask/src/build/nucleus_audit.rs`. General x87/FP arithmetic
        # remains forbidden; register IPC saves and restores its own controls.
        # The ymm upper halves are not saved because the only kernel code that
        # touches them brackets itself with `kernel_hal::arch::simd`'s
        # `wide_simd_section`, which the same audit enforces.
        #
        # The complete frame plus 16-byte boundary contains either the ordinary
        # SYSRET image or one normalized SavedContext, while preserving
        # call-site alignment so Rust enters with rsp % 16 == 8 as required
        # by the x86_64 SysV ABI.
        sub rsp, {stack_bytes}
        mov qword ptr [rsp + {boundary_offset}], 0
        cmp qword ptr gs:[{register_abi_offset}], 1
        jne 2f
        cmp rax, 0x52550052
        jb 2f
        cmp rax, 0x52550055
        ja 2f
        mov qword ptr [rsp + {boundary_offset}], 1
        stmxcsr [rsp + {boundary_offset} + 8]
        fnstcw [rsp + {boundary_offset} + 12]
        jmp 3f
    2:
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
    3:
        mov [rsp + 8], rax
        mov rax, gs:[8]
        mov [rsp + 0], rax
        mov [rsp + 24], rcx
        mov [rsp + 88], r11
        mov [rsp + 48], rdi
        mov [rsp + 40], rsi
        mov [rsp + 32], rdx
        mov [rsp + 64], r8
        mov [rsp + 72], r9
        mov [rsp + 80], r10
        mov [rsp + 16], rbx
        mov [rsp + 56], rbp
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

    # A local label, not a symbol: the linked-image audit attributes x87/FP
    # custody instructions to the symbol that contains them, and these belong to
    # `syscall_entry`.
    .Lsyscall_exit_epilogue:
        cmp qword ptr [rsp + {boundary_offset}], 0
        je 4f
        call {clear_ipc_registers}
        ldmxcsr [rsp + {boundary_offset} + 8]
        fldcw [rsp + {boundary_offset} + 12]
        jmp 5f
    4:
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
    5:
        mov rdi, [rsp + 48]
        mov rsi, [rsp + 40]
        mov rdx, [rsp + 32]
        mov r8, [rsp + 64]
        mov r9, [rsp + 72]
        mov r10, [rsp + 80]
        mov rbx, [rsp + 16]
        mov rbp, [rsp + 56]
        mov r12, [rsp + 96]
        mov r13, [rsp + 104]
        mov r14, [rsp + 112]
        mov r15, [rsp + 120]
        mov r11, [rsp + 88]
        mov rcx, [rsp + 24]
        mov rax, [rsp + 8]
        mov rsp, [rsp + 0]
        swapgs
        sysretq
    .size syscall_entry, . - syscall_entry

    # A register-IPC syscall that blocked through the direct fast-IPC schedule
    # resumes here, entered by the scheduler's IRET with RSP = its syscall frame
    # (the Rust frames below it were abandoned). Everything the blocked call
    # still owed is described by the park record beside the frame.
    .global fast_ipc_parked_resume
    .type fast_ipc_parked_resume, @function
    fast_ipc_parked_resume:
        endbr64
        cld
        mov rdi, rsp
        call {parked_tail}
        cli
        jmp .Lsyscall_exit_epilogue
    .size fast_ipc_parked_resume, . - fast_ipc_parked_resume
 "#,
    frame_bytes = const core::mem::size_of::<SyscallFrame>(),
    stack_bytes = const SYSCALL_ENTRY_STACK_BYTES + core::mem::size_of::<RegisterIpcBoundary>(),
    boundary_offset = const SYSCALL_ENTRY_STACK_BYTES,
    register_abi_offset = const syscall_core::register_ipc_abi_offset(),
    clear_ipc_registers = sym kernel_hal::api::arch::simd::clear_ipc_registers,
    parked_tail = sym fast_ipc_parked_tail,
);

unsafe extern "C" {
    fn fast_ipc_parked_resume();
}

/// The resume entry for [`ParkRecord`]-described blocks.
pub(super) fn fast_ipc_parked_resume_address() -> u64 {
    paging::higher_half_addr(fast_ipc_parked_resume as *const () as usize as u64)
}

/// The syscall frame this task's current register-IPC syscall runs on.
pub(super) fn register_ipc_frame_address(frame: &SyscallFrame) -> u64 {
    core::ptr::from_ref(frame) as usize as u64
}

/// The park record beside `frame`.
///
/// # Safety
/// `frame` is the live syscall frame of the calling task's register-IPC syscall.
pub(super) unsafe fn register_ipc_park_record(frame: *mut SyscallFrame) -> *mut ParkRecord {
    unsafe { core::ptr::addr_of_mut!((*register_ipc_boundary(frame)).park) }
}

pub fn init() {
    syscall_core::init_cpu_local();
    kernel_hal::api::register_fast_ipc_resume_rip(fast_ipc_parked_resume_address());
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
    // The assembly gate, not mutable RAX (signals/results may replace it),
    // determines custody for the whole continuation, including signal IPC.
    let register_ipc = unsafe { (*register_ipc_boundary(frame)).active != 0 };
    assert_eq!(
        register_ipc,
        register_ipc_entry_expected(abi, frame.rax),
        "syscall entry ABI selector is not the installed task's ABI"
    );
    let simd_guard = if register_ipc {
        // SAFETY: entry saved controls; both return routes sanitize below.
        Some(unsafe { multitask::begin_ipc_register_simd() })
    } else {
        None
    };
    let phase = syscall_profile::charge(syscall_profile::SyscallPhase::Validate, phase);
    trace_syscall_entry(frame, abi);
    let phase = syscall_profile::charge(syscall_profile::SyscallPhase::Trace, phase);
    let result = dispatch_syscall(frame, abi);
    let phase = syscall_profile::charge(syscall_profile::SyscallPhase::Dispatch, phase);
    syscall_return(frame, register_ipc, simd_guard, result, phase)
}

/// Everything a syscall does after its body: claim a deferred reschedule, prove
/// the user-return contract again, then either return to the assembly epilogue
/// (SYSRET) or hand this same frame to the scheduler as a user context.
///
/// Shared by [`syscall_dispatch`] and [`fast_ipc_parked_tail`], so a register-IPC
/// call that resumed from a direct block leaves through the identical sequence.
#[inline(always)]
fn syscall_return(
    frame: &mut SyscallFrame,
    register_ipc: bool,
    simd_guard: Option<multitask::IpcRegisterSimdGuard>,
    result: u64,
    phase: u64,
) -> u64 {
    let schedule_user_return = multitask::claim_interruptible_syscall_reschedule();
    let phase = syscall_profile::charge(syscall_profile::SyscallPhase::RescheduleDeferred, phase);
    // Validate after the final syscall mutation and before either SYSRET or
    // publishing this same image as a scheduler-owned IRET context.
    let return_abi = validate_syscall_entry_or_terminate(frame);
    let phase = syscall_profile::charge(syscall_profile::SyscallPhase::Validate, phase);
    trace_syscall_exit(frame, return_abi, result);
    syscall_profile::charge(syscall_profile::SyscallPhase::Trace, phase);
    frame.rax = result;
    if schedule_user_return {
        // Nonreturning path: finalize while masked BEFORE the generic save,
        // and retire the task marker rather than abandoning a live guard.
        x86_64::instructions::interrupts::disable();
        if register_ipc {
            unsafe { finish_register_ipc_return(frame) };
        }
        drop(simd_guard);
        // The captured user image stays in this task's kernel stack: the
        // scheduler consumes the existing frame instead of copying SIMD state
        // into a manufactured software-interrupt continuation.
        let context_addr = frame as *mut SyscallFrame as usize + SYSCALL_SCHEDULE_CONTEXT_OFFSET;
        // SAFETY: the compile-time offset witnesses above make the initialized
        // syscall GPR/XMM image the prefix of one SavedContext. The return
        // fields have just passed the syscall boundary validation; HAL writes
        // the selector/suffix fields before publishing it to the scheduler.
        unsafe {
            kernel_hal::api::dispatch_saved_syscall_user_context(
                context_addr,
                frame.user_rsp,
                frame.user_rip,
                frame.user_rflags,
            )
        }
    }
    result
}

/// Entered by `fast_ipc_parked_resume` when a register-IPC call that blocked
/// through the direct fast-IPC schedule is scheduled again. Its Rust frames are
/// gone; the syscall frame and park record are all it has, and the returning
/// registers are its to fill.
#[unsafe(no_mangle)]
extern "C" fn fast_ipc_parked_tail(frame: *mut SyscallFrame) -> u64 {
    // SAFETY: the resume stub passes the syscall frame it was entered on.
    let frame = unsafe { &mut *frame };
    let phase = syscall_profile::now();
    // SAFETY: the frame is this task's live register-IPC syscall frame.
    let record = unsafe { register_ipc_park_record(frame).read() };
    // SAFETY: only the direct block parks here, and it sets the marker first.
    let boundary_active = unsafe { (*register_ipc_boundary(frame)).active != 0 };
    assert!(boundary_active, "parked register IPC resumed without its entry marker");
    unsafe { register_ipc_park_record(frame).write(ParkRecord { kind: park_kind::NONE, ..record }) };
    // SAFETY: this is the parked task's own resume entry (see the function doc).
    let simd_guard = Some(unsafe { multitask::adopt_ipc_register_simd() });
    let result = linux::ipc_ops::complete_parked_register_ipc(frame, &record);
    syscall_return(frame, true, simd_guard, result, phase)
}

fn register_ipc_entry_expected(abi: UserAbi, operation: u64) -> bool {
    use rustos_user_abi::syscall::{
        SYS_RUSTOS_IPC_REGISTER_CALL, SYS_RUSTOS_IPC_REGISTER_RECV, SYS_RUSTOS_IPC_REGISTER_REPLY,
        SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
    };
    abi == UserAbi::Linux
        && matches!(
            operation,
            SYS_RUSTOS_IPC_REGISTER_CALL
                | SYS_RUSTOS_IPC_REGISTER_RECV
                | SYS_RUSTOS_IPC_REGISTER_REPLY_RECV
                | SYS_RUSTOS_IPC_REGISTER_REPLY
        )
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
        SyscallFrame, UserAbi, register_ipc_entry_expected, syscall_return_contract,
        user_address_is_sysret_safe, user_rflags_are_sysret_safe,
    };
    use crate::memory::paging;
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
    fn scheduled_user_return_is_validated_before_context_publication() {
        let source = include_str!("mod.rs");
        let dispatch = source
            .split("extern \"C\" fn syscall_dispatch")
            .nth(1)
            .and_then(|rest| rest.split("fn dispatch_syscall").next())
            .expect("syscall dispatch body");
        let claim = dispatch
            .find("multitask::claim_interruptible_syscall_reschedule();")
            .expect("interruptible scheduler claim");
        let validation = dispatch
            .find("let return_abi = validate_syscall_entry_or_terminate(frame);")
            .expect("final user-return validation");
        let publication = dispatch
            .find("kernel_hal::api::dispatch_saved_syscall_user_context(")
            .expect("saved user-context publication");
        assert!(
            claim < validation && validation < publication,
            "the direct scheduler context must contain the finally validated user return"
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
    fn register_ipc_entry_elision_requires_exact_linux_operation() {
        for operation in 0x52550052..=0x52550055 {
            assert!(register_ipc_entry_expected(UserAbi::Linux, operation));
            assert!(!register_ipc_entry_expected(UserAbi::Windows, operation));
        }
        for operation in [0, 39, 0x52550051, 0x52550056, u64::MAX] {
            assert!(!register_ipc_entry_expected(UserAbi::Linux, operation));
        }
    }

    #[test]
    fn register_ipc_finalizes_before_both_user_return_routes() {
        let source = include_str!("mod.rs");
        let stub = source.split("syscall_entry:").nth(1).unwrap();
        let gate = stub.split("# SIMD-ENTRY-SAVE-BEGIN").next().unwrap();
        assert!(gate.contains("gs:[{register_abi_offset}]"));
        assert!(gate.contains("jmp 3f"));
        assert!(source.contains("register_ipc_entry_expected(abi, frame.rax)"));
        assert!(gate.contains("cmp rax, 0x52550052"));
        assert!(gate.contains("cmp rax, 0x52550055"));
        assert!(gate.contains("stmxcsr"));
        assert!(gate.contains("fnstcw"));
        let epilogue = stub.split("call syscall_dispatch").nth(1).unwrap();
        assert!(
            epilogue.find("cli").unwrap() < epilogue.find("call {clear_ipc_registers}").unwrap()
        );
        assert!(epilogue.contains("ldmxcsr [rsp + {boundary_offset} + 8]"));
        assert!(epilogue.contains("fldcw [rsp + {boundary_offset} + 12]"));
        let tail = source.split("if schedule_user_return {").nth(1).unwrap();
        let finalize = tail.find("finish_register_ipc_return(frame)").unwrap();
        let publish = tail.find("dispatch_saved_syscall_user_context(").unwrap();
        assert!(tail.find("interrupts::disable()").unwrap() < finalize);
        assert!(finalize < tail.find("drop(simd_guard)").unwrap());
        assert!(tail.find("drop(simd_guard)").unwrap() < publish);
        let finalize = source
            .split("unsafe fn finish_register_ipc_return(")
            .nth(1)
            .unwrap();
        assert!(
            finalize
                .split("global_asm!")
                .next()
                .unwrap()
                .contains("write_bytes(0, 256)")
        );
    }

    #[test]
    fn direct_block_resume_leaves_through_the_one_syscall_epilogue() {
        let source = include_str!("mod.rs");
        let stub = source
            .split("fast_ipc_parked_resume:")
            .nth(1)
            .and_then(|rest| rest.split(".size fast_ipc_parked_resume").next())
            .expect("direct-block resume stub");
        // Entered by IRET (or the lane tail's jump) on the syscall frame: it may
        // only run the shared tail and then take the existing epilogue, never a
        // second copy of the SYSRET sequence.
        assert!(stub.contains("call {parked_tail}"));
        assert!(stub.contains("jmp .Lsyscall_exit_epilogue"));
        assert!(!stub.contains("sysretq") && !stub.contains("swapgs"));
        let entry = source
            .split("syscall_entry:")
            .nth(1)
            .and_then(|rest| rest.split(".size syscall_entry").next())
            .expect("syscall entry stub");
        let label = entry.find(".Lsyscall_exit_epilogue:").expect("epilogue label");
        let boundary = entry
            .find("cmp qword ptr [rsp + {boundary_offset}], 0")
            .expect("register IPC exit gate");
        assert!(entry.find("call syscall_dispatch").unwrap() < label);
        assert!(label < boundary, "the label must precede the register-IPC exit gate");
        // A label, not a symbol: the linked-image FPU audit attributes by symbol.
        assert!(!entry.contains(".global syscall_exit_epilogue"));
    }

    #[test]
    fn parked_tail_shares_the_syscall_return_and_readopts_the_simd_marker() {
        let source = include_str!("mod.rs");
        let tail = source
            .split("extern \"C\" fn fast_ipc_parked_tail(")
            .nth(1)
            .and_then(|rest| rest.split("fn register_ipc_entry_expected").next())
            .expect("parked tail");
        let adopt = tail.find("adopt_ipc_register_simd()").expect("marker re-adoption");
        let complete = tail.find("complete_parked_register_ipc(").expect("completion");
        let leave = tail.find("syscall_return(frame, true, simd_guard").expect("shared return");
        assert!(adopt < complete && complete < leave);
        // The same function ends `syscall_dispatch`, so signals, reschedule
        // claims and the user-return contract cannot diverge between the routes.
        assert!(source.contains("    syscall_return(frame, register_ipc, simd_guard, result, phase)\n}"));
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
