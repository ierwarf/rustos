use core::arch::global_asm;
use x86_64::VirtAddr;
use x86_64::registers::control::{Efer, EferFlags};
use x86_64::registers::model_specific::{GsBase, KernelGsBase, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

use crate::gdt;
use crate::multitask;
use crate::paging;
use crate::user::abi::UserAbi;

pub(crate) mod linux;
pub(crate) mod windows;

const SYSCALL_STACK_SIZE: usize = 16 * 1024;
const SYSCALL_ERR_INVALID: u64 = u64::MAX;
const USER_GS_BASE_DEFAULT: u64 = 0;
const USER_RFLAGS_RESERVED_BIT_1: u64 = 1 << 1;
const USER_RFLAGS_FORBIDDEN_MASK: u64 = RFlags::TRAP_FLAG.bits()
    | RFlags::DIRECTION_FLAG.bits()
    | RFlags::NESTED_TASK.bits()
    | RFlags::IOPL_LOW.bits()
    | RFlags::IOPL_HIGH.bits()
    | RFlags::VIRTUAL_8086_MODE.bits();

#[repr(C, align(16))]
struct SyscallCpuLocal {
    kernel_stack_top: u64,
    user_rsp: u64,
}

#[repr(align(16))]
struct SyscallFallbackStack([u8; SYSCALL_STACK_SIZE]);

#[repr(C)]
struct SyscallFrame {
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

const _: [(); 128] = [(); core::mem::size_of::<SyscallFrame>()];

static mut SYSCALL_CPU_LOCAL: SyscallCpuLocal = SyscallCpuLocal {
    kernel_stack_top: 0,
    user_rsp: 0,
};
static mut SYSCALL_FALLBACK_STACK: SyscallFallbackStack =
    SyscallFallbackStack([0; SYSCALL_STACK_SIZE]);
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
        mov rdi, rsp
        call syscall_dispatch

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
    let (_cpu_local_addr, stack_base) = unsafe {
        (
            syscall_cpu_local_addr(),
            paging::higher_half_addr(core::ptr::addr_of!(SYSCALL_FALLBACK_STACK.0) as u64),
        )
    };

    unsafe {
        SYSCALL_CPU_LOCAL.kernel_stack_top = stack_base + SYSCALL_STACK_SIZE as u64;
    }

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
    prepare_for_context_return(false);
}

pub fn set_kernel_stack_top(kernel_stack_top: u64) {
    if kernel_stack_top == 0 {
        return;
    }

    unsafe {
        SYSCALL_CPU_LOCAL.kernel_stack_top = kernel_stack_top;
    }
}

pub fn prepare_for_context_return(returning_to_user: bool) {
    let kernel_gs_base = VirtAddr::new(unsafe { syscall_cpu_local_addr() });
    let user_gs_base = VirtAddr::new(USER_GS_BASE_DEFAULT);
    if returning_to_user {
        GsBase::write(user_gs_base);
        KernelGsBase::write(kernel_gs_base);
    } else {
        GsBase::write(kernel_gs_base);
        KernelGsBase::write(user_gs_base);
    }
}

unsafe fn syscall_cpu_local_addr() -> u64 {
    paging::higher_half_addr(core::ptr::addr_of!(SYSCALL_CPU_LOCAL) as u64)
}

unsafe extern "C" {
    fn syscall_entry();
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(frame: *mut SyscallFrame) -> u64 {
    let frame = unsafe { &mut *frame };
    multitask::save_current_fx_state();
    let result = dispatch_syscall(frame);
    multitask::restore_current_fx_state();
    result
}

fn dispatch_syscall(frame: &SyscallFrame) -> u64 {
    match multitask::current_user_abi() {
        Some(UserAbi::Linux) => linux::dispatch_linux_syscall(frame),
        Some(UserAbi::Windows) => windows::dispatch_syscall(frame),
        None => SYSCALL_ERR_INVALID,
    }
}

fn syscall_frame_security_check(frame: &SyscallFrame) -> bool {
    multitask::current_user_address_space().is_some()
        && user_address_in_range(frame.user_rip)
        && user_address_in_range(frame.user_rsp)
        && (frame.user_rflags & USER_RFLAGS_RESERVED_BIT_1) != 0
        && (frame.user_rflags & USER_RFLAGS_FORBIDDEN_MASK) == 0
}

fn user_address_in_range(addr: u64) -> bool {
    (paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&addr)
}
