//! x86_64 syscall-entry CPU-local state.
//!
//! - **Owner:** `kernel-ps` owns the CPU-private syscall entry record and
//!   bootstrap kernel stack; the scheduler owns task-specific replacement
//!   stack values.
//! - **Boundary:** `IA32_GS_BASE`/`IA32_KERNEL_GS_BASE` publish one exact
//!   logical CPU's record to assembly entry code.
//! - **Lifecycle:** A CPU claims and initializes its slot once, release
//!   publishes it, and only then installs that slot in the GS MSRs.
//! - **Concurrency:** A CPU mutates only its own slot with interrupts excluded;
//!   no remote CPU may borrow or reset it.
//! - **Failure:** Duplicate initialization, an unknown logical CPU, use before
//!   publication, or a malformed kernel stack is a kernel invariant panic.
//! - **Forbidden:** No shared bootstrap stack, shared user RSP scratch word,
//!   raw APIC indexing, or GS publication of a different CPU's record.
//! - **Evidence:** `cpu-online-lifecycle`.
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, Ordering};

use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::registers::model_specific::{GsBase, KernelGsBase};

use crate::memory::paging;

const SYSCALL_STACK_SIZE: usize = 64 * 1024;
const USER_GS_BASE_DEFAULT: u64 = 0;
const MAX_SUPPORTED_CPUS: usize = nucleus_core::util::lockdep::MAX_TRACKED_CPUS;
const CPU_LOCAL_EMPTY: u8 = 0;
const CPU_LOCAL_BUILDING: u8 = 1;
const CPU_LOCAL_LIVE: u8 = 2;

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

struct SyscallCpuLocalMemory(UnsafeCell<SyscallCpuLocal>);

// SAFETY: every element is permanently assigned to one dense logical CPU.
// Accessors verify the current CPU and exclude local interrupts before mutation.
unsafe impl Sync for SyscallCpuLocalMemory {}

struct SyscallBootstrapStackMemory(UnsafeCell<SyscallBootstrapStack>);

// SAFETY: the bootstrap stack is written only by hardware stack operations on
// its owning CPU after that CPU publishes the corresponding CPU-local record.
unsafe impl Sync for SyscallBootstrapStackMemory {}

static SYSCALL_CPU_LOCALS: [SyscallCpuLocalMemory; MAX_SUPPORTED_CPUS] = [const {
    SyscallCpuLocalMemory(UnsafeCell::new(SyscallCpuLocal {
        kernel_stack_top: 0,
        user_rsp: 0,
        linux_compat_current_task: 0,
        linux_compat_stack_guard: 0,
    }))
}; MAX_SUPPORTED_CPUS];
static SYSCALL_BOOTSTRAP_STACKS: [SyscallBootstrapStackMemory; MAX_SUPPORTED_CPUS] = [const {
    SyscallBootstrapStackMemory(UnsafeCell::new(SyscallBootstrapStack(
        [0; SYSCALL_STACK_SIZE],
    )))
};
    MAX_SUPPORTED_CPUS];
static SYSCALL_CPU_LOCAL_STATES: [AtomicU8; MAX_SUPPORTED_CPUS] =
    [const { AtomicU8::new(CPU_LOCAL_EMPTY) }; MAX_SUPPORTED_CPUS];

pub fn init_cpu_local() {
    let logical_index = current_logical_index();
    let state = &SYSCALL_CPU_LOCAL_STATES[logical_index];
    // ORDERING: AcqRel claims the unique initialization epoch. Any prior state
    // is a duplicate or concurrent owner and therefore an internal bug.
    assert!(
        state
            .compare_exchange(
                CPU_LOCAL_EMPTY,
                CPU_LOCAL_BUILDING,
                // ORDERING: the winner acquires prior slot state and exclusively
                // owns all following initialization writes.
                Ordering::AcqRel,
                // ORDERING: a failed claim observes the published owner state
                // before reporting the invariant violation.
                Ordering::Acquire,
            )
            .is_ok(),
        "syscall CPU-local invariant: logical CPU {logical_index} initialized twice"
    );

    let stack_ptr = SYSCALL_BOOTSTRAP_STACKS[logical_index].0.get();
    // SAFETY: the slot has just been claimed by this CPU and remains static for
    // the kernel lifetime. higher_half_addr applies the established kernel map.
    let stack_base =
        unsafe { paging::higher_half_addr(core::ptr::addr_of!((*stack_ptr).0) as u64) };
    let local_ptr = SYSCALL_CPU_LOCALS[logical_index].0.get();
    // SAFETY: this CPU uniquely owns the BUILDING slot and GS cannot name it
    // until the Release publication below completes.
    unsafe {
        local_ptr.write(SyscallCpuLocal {
            kernel_stack_top: stack_base + SYSCALL_STACK_SIZE as u64,
            user_rsp: 0,
            linux_compat_current_task: 0,
            linux_compat_stack_guard: 0,
        });
    }
    // ORDERING: all record and stack-address initialization happens-before an
    // Acquire load by the CPU-local accessors.
    state.store(CPU_LOCAL_LIVE, Ordering::Release);
    prepare_for_context_return(false, USER_GS_BASE_DEFAULT);
}

pub fn set_linux_compat_current_task_ptr(current_task_ptr: usize) {
    with_current_cpu_local_mut(|local| {
        local.linux_compat_current_task = current_task_ptr as u64;
    });
}

pub fn set_linux_compat_stack_guard(stack_guard: u64) {
    with_current_cpu_local_mut(|local| {
        local.linux_compat_stack_guard = stack_guard;
    });
}

pub fn activate_linux_compat_cpu_local() {
    prepare_for_context_return(false, USER_GS_BASE_DEFAULT);
}

pub fn with_kernel_gs_base<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(|| {
        let current_gs_base = GsBase::read();
        let kernel_gs_base = KernelGsBase::read();
        if current_gs_base == kernel_gs_base {
            return f();
        }

        GsBase::write(kernel_gs_base);
        let result = f();
        GsBase::write(current_gs_base);
        result
    })
}

pub const fn linux_compat_current_task_offset() -> usize {
    core::mem::offset_of!(SyscallCpuLocal, linux_compat_current_task)
}

pub const fn linux_compat_stack_guard_offset() -> usize {
    core::mem::offset_of!(SyscallCpuLocal, linux_compat_stack_guard)
}

pub fn set_kernel_stack_top(kernel_stack_top: u64) {
    assert_ne!(
        kernel_stack_top, 0,
        "syscall entry kernel stack top must be non-zero"
    );
    assert_eq!(
        kernel_stack_top & 0xF,
        0,
        "syscall entry kernel stack top must be 16-byte aligned"
    );

    with_current_cpu_local_mut(|local| {
        local.kernel_stack_top = kernel_stack_top;
    });
}

pub fn prepare_for_context_return(returning_to_user: bool, user_gs_base: u64) {
    let logical_index = current_logical_index();
    assert_cpu_local_live(logical_index);
    // SAFETY: the exact logical CPU slot is live and remains allocated for the
    // kernel lifetime; the value is consumed only as an MSR base address.
    let kernel_gs_base = VirtAddr::new(unsafe { syscall_cpu_local_addr(logical_index) });
    let user_gs_base = VirtAddr::new(user_gs_base);
    interrupts::without_interrupts(|| {
        if returning_to_user {
            GsBase::write(user_gs_base);
            KernelGsBase::write(kernel_gs_base);
        } else {
            GsBase::write(kernel_gs_base);
            KernelGsBase::write(user_gs_base);
        }
    });
}

fn current_logical_index() -> usize {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    assert!(
        logical_index < MAX_SUPPORTED_CPUS,
        "syscall CPU-local invariant: logical CPU index exceeds capacity"
    );
    logical_index
}

fn assert_cpu_local_live(logical_index: usize) {
    // ORDERING: Acquire observes the complete record before any field access or
    // GS publication on this CPU.
    assert_eq!(
        SYSCALL_CPU_LOCAL_STATES[logical_index].load(Ordering::Acquire),
        CPU_LOCAL_LIVE,
        "syscall CPU-local invariant: logical CPU {logical_index} used before publication"
    );
}

fn with_current_cpu_local_mut<T>(f: impl FnOnce(&mut SyscallCpuLocal) -> T) -> T {
    interrupts::without_interrupts(|| {
        let logical_index = current_logical_index();
        assert_cpu_local_live(logical_index);
        let local_ptr = SYSCALL_CPU_LOCALS[logical_index].0.get();
        // SAFETY: interrupts are excluded and only the executing logical CPU
        // can select and mutate this permanently assigned slot.
        f(unsafe { &mut *local_ptr })
    })
}

unsafe fn syscall_cpu_local_addr(logical_index: usize) -> u64 {
    paging::higher_half_addr(SYSCALL_CPU_LOCALS[logical_index].0.get() as u64)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SUPPORTED_CPUS, SYSCALL_BOOTSTRAP_STACKS, SYSCALL_CPU_LOCALS, SYSCALL_STACK_SIZE,
        SyscallCpuLocal,
    };

    #[test]
    fn cpu_local_records_and_bootstrap_stacks_are_aligned_and_disjoint() {
        assert_eq!(core::mem::offset_of!(SyscallCpuLocal, kernel_stack_top), 0);
        assert_eq!(core::mem::offset_of!(SyscallCpuLocal, user_rsp), 8);
        assert_eq!(
            core::mem::offset_of!(SyscallCpuLocal, linux_compat_current_task),
            0x10
        );
        assert_eq!(
            core::mem::offset_of!(SyscallCpuLocal, linux_compat_stack_guard),
            0x18
        );

        let mut locals = [0_usize; MAX_SUPPORTED_CPUS];
        let mut stacks = [0_usize; MAX_SUPPORTED_CPUS];
        for cpu in 0..MAX_SUPPORTED_CPUS {
            locals[cpu] = SYSCALL_CPU_LOCALS[cpu].0.get() as usize;
            stacks[cpu] = SYSCALL_BOOTSTRAP_STACKS[cpu].0.get() as usize;
            assert_eq!(locals[cpu] & 0xf, 0);
            assert_eq!(stacks[cpu] & 0xf, 0);
            assert!(!locals[..cpu].contains(&locals[cpu]));
            assert!(!stacks[..cpu].contains(&stacks[cpu]));
            assert!(
                locals[cpu] < stacks[cpu]
                    || locals[cpu] >= stacks[cpu].saturating_add(SYSCALL_STACK_SIZE)
            );
        }
    }
}
