use x86_64::VirtAddr;
use x86_64::registers::model_specific::{GsBase, KernelGsBase};

use crate::memory::paging;

const SYSCALL_STACK_SIZE: usize = 64 * 1024;
const USER_GS_BASE_DEFAULT: u64 = 0;

#[repr(C, align(16))]
struct SyscallCpuLocal {
    kernel_stack_top: u64,
    user_rsp: u64,
    linux_compat_current_task: u64,
    linux_compat_stack_guard: u64,
}

#[repr(align(16))]
/// Early CPU-local stack used before the scheduler installs the current task's
/// kernel stack. This is bootstrap substrate, not a substitute execution path.
struct SyscallBootstrapStack([u8; SYSCALL_STACK_SIZE]);

const _: [(); 0x10] = [(); core::mem::offset_of!(SyscallCpuLocal, linux_compat_current_task)];
const _: [(); 0x18] = [(); core::mem::offset_of!(SyscallCpuLocal, linux_compat_stack_guard)];

static mut SYSCALL_CPU_LOCAL: SyscallCpuLocal = SyscallCpuLocal {
    kernel_stack_top: 0,
    user_rsp: 0,
    linux_compat_current_task: 0,
    linux_compat_stack_guard: 0,
};
static mut SYSCALL_BOOTSTRAP_STACK: SyscallBootstrapStack =
    SyscallBootstrapStack([0; SYSCALL_STACK_SIZE]);

pub fn init_cpu_local() {
    let stack_base =
        unsafe { paging::higher_half_addr(core::ptr::addr_of!(SYSCALL_BOOTSTRAP_STACK.0) as u64) };
    unsafe {
        SYSCALL_CPU_LOCAL.kernel_stack_top = stack_base + SYSCALL_STACK_SIZE as u64;
    }
    prepare_for_context_return(false, USER_GS_BASE_DEFAULT);
}

pub fn set_linux_compat_current_task_ptr(current_task_ptr: usize) {
    unsafe {
        SYSCALL_CPU_LOCAL.linux_compat_current_task = current_task_ptr as u64;
    }
}

pub fn set_linux_compat_stack_guard(stack_guard: u64) {
    unsafe {
        SYSCALL_CPU_LOCAL.linux_compat_stack_guard = stack_guard;
    }
}

pub fn activate_linux_compat_cpu_local() {
    prepare_for_context_return(false, USER_GS_BASE_DEFAULT);
}

pub fn with_kernel_gs_base<T>(f: impl FnOnce() -> T) -> T {
    let current_gs_base = GsBase::read();
    let kernel_gs_base = KernelGsBase::read();
    if current_gs_base == kernel_gs_base {
        return f();
    }

    GsBase::write(kernel_gs_base);
    let result = f();
    GsBase::write(current_gs_base);
    result
}

pub const fn linux_compat_current_task_offset() -> usize {
    core::mem::offset_of!(SyscallCpuLocal, linux_compat_current_task)
}

pub const fn linux_compat_stack_guard_offset() -> usize {
    core::mem::offset_of!(SyscallCpuLocal, linux_compat_stack_guard)
}

pub fn set_kernel_stack_top(kernel_stack_top: u64) {
    if kernel_stack_top == 0 {
        return;
    }
    assert_eq!(
        kernel_stack_top & 0xF,
        0,
        "syscall entry kernel stack top must be 16-byte aligned"
    );

    unsafe {
        SYSCALL_CPU_LOCAL.kernel_stack_top = kernel_stack_top;
    }
}

pub fn prepare_for_context_return(returning_to_user: bool, user_gs_base: u64) {
    let kernel_gs_base = VirtAddr::new(unsafe { syscall_cpu_local_addr() });
    let user_gs_base = VirtAddr::new(user_gs_base);
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
