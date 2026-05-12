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

pub(crate) mod linux;
// RING3-MIGRATION-REFERENCE: Windows syscall dispatch source is commented in
// `user/syscall/windows/*` while the service-owned NT/Win32 path is designed.

const SYSCALL_ERR_INVALID: u64 = u64::MAX;
const USER_RFLAGS_RESERVED_BIT_1: u64 = 1 << 1;
const GENERAL_PROTECTION_VECTOR: u8 = 13;
const USER_RFLAGS_FORBIDDEN_MASK: u64 = RFlags::TRAP_FLAG.bits()
    | RFlags::DIRECTION_FLAG.bits()
    | RFlags::NESTED_TASK.bits()
    | RFlags::IOPL_LOW.bits()
    | RFlags::IOPL_HIGH.bits()
    | RFlags::VIRTUAL_8086_MODE.bits();

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
        sub rsp, 128

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
 "#
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
    let abi = validate_syscall_entry_or_terminate(frame);
    let user_rip_before_dispatch = frame.user_rip;
    let user_rsp_before_dispatch = frame.user_rsp;
    trace_syscall_entry(frame, abi);
    multitask::save_current_simd_state();
    let result = dispatch_syscall(frame, abi);
    multitask::restore_current_simd_state();
    let return_abi = validate_syscall_entry_or_terminate(frame);
    let exec_transition_applied =
        frame.user_rip != user_rip_before_dispatch || frame.user_rsp != user_rsp_before_dispatch;
    if exec_transition_applied {
        multitask::clear_deferred_reschedule_request();
    } else {
        multitask::reschedule_if_requested();
    }
    trace_syscall_exit(frame, return_abi, result);
    result
}

fn dispatch_syscall(frame: &mut SyscallFrame, abi: UserAbi) -> u64 {
    match abi {
        UserAbi::Linux => linux::dispatch_linux_syscall(frame),
        UserAbi::Windows => SYSCALL_ERR_INVALID,
    }
}

fn syscall_frame_security_check(frame: &SyscallFrame) -> bool {
    multitask::current_user_snapshot().is_some() && syscall_return_contract(frame).is_some()
}

pub(super) fn validate_syscall_entry_or_terminate(frame: &SyscallFrame) -> UserAbi {
    let Some(snapshot) = multitask::current_user_snapshot() else {
        panic!(
            "rejected syscall without active user context: rip={:#x} rsp={:#x} rflags={:#x} nr={:#x}",
            frame.user_rip, frame.user_rsp, frame.user_rflags, frame.rax,
        );
    };

    if syscall_return_contract(frame).is_none() {
        debug::println!(
            "terminating task due to unsafe syscall return contract: pid={} tid={} rip={:#x} rsp={:#x} rflags={:#x} nr={:#x}",
            snapshot.process_id(),
            snapshot.thread_id(),
            frame.user_rip,
            frame.user_rsp,
            frame.user_rflags,
            frame.rax,
        );
        let disposition = multitask::retire_current_user_task_due_to_fault(
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

    snapshot.abi()
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
        SyscallFrame, syscall_return_contract, user_address_is_sysret_safe,
        user_rflags_are_sysret_safe,
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
}
