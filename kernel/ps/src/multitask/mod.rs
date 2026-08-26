mod context;
mod cpu_local;
mod current;
mod current_identity;
mod deferred_wake;
mod irq;
mod process_state_lock;
mod process_table;
mod reschedule_observation;
mod retirement;
mod run_authority;
mod scheduler;
mod scheduling_api;
mod spawn;

use core::{
    cell::Cell,
    mem,
    sync::atomic::{AtomicU64, Ordering},
};

use x86_64::VirtAddr;
use x86_64::instructions::{hlt, interrupts};
use x86_64::registers::rflags::RFlags;
use x86_64::registers::segmentation::{CS, SS, Segment};

use self::cpu_local::{
    current_cpu_task_slot_admitted, publish_cpu_current_task, publish_scheduler_initialized,
    scheduler_initialized, scheduler_mut, scheduler_ref, task_slot_is_running,
};
use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging::ProcessAddressSpace;
use crate::user::abi::UserAbi;
use crate::user::linux::{
    LinuxMemoryMapState, LinuxProcessState, LinuxRuntimeProfile, LinuxThreadState,
};
use crate::user::process_state::{
    ProcessSecurityContext, UserProcessState, WindowsProcessRuntimeState, WindowsThreadRuntimeState,
};

pub use self::current::{
    activate_suspended_user_task, activate_suspended_user_tasks,
    activate_suspended_user_tasks_with_commit, any_user_process_state, arm_block_current_task,
    arm_block_current_task_on_endpoint, arm_block_current_task_on_reply,
    attach_reserved_ipc_priority, bind_ipc_priority_to_process_worker, bind_reserved_ipc_priority,
    cancel_block_current_task, cancel_ipc_priority_reservation, commit_ipc_call_handoff,
    complete_fast_ipc_reply_wake_handoff_with_custody, complete_ipc_reply_wake_handoff,
    complete_ipc_reply_wake_handoff_with_custody, complete_retired_task_cleanup,
    current_console_session, current_linux_thread_state,
    current_scheduling_context_runtime_snapshot, current_task_id,
    current_thread_may_have_pending_signals, current_user_abi, current_user_address_space,
    current_user_id, current_user_log_ids, current_user_process_id, current_user_process_identity,
    current_user_process_thread_count, current_user_snapshot, current_user_stack_state,
    current_user_thread_id, current_user_wait_binding, demote_current_user_task_to_user_class,
    exec_current_user_process, exec_user_process_by_pid, exit_current_user_process,
    exit_current_user_task, halt_current_retired_task, inherit_ipc_priority,
    is_user_process_exiting, is_user_task_alive, linux_task_affinity, linux_thread_snapshot_by_ids,
    live_user_process_identity_by_pid, live_user_process_identity_with_exact_exec_path,
    mark_user_process_exiting, mark_user_process_exiting_once, next_retired_task_cleanup,
    note_process_exit_status, parent_process_id_of, queue_linux_process_sigchld,
    queue_linux_signal, release_ipc_priorities_for_process, release_ipc_priority,
    reserve_ipc_call_donation, reserve_ipc_priority, retain_current_user_process_state,
    retire_current_user_task_due_to_fault, service_deferred_work, set_current_linux_tls_fs_base,
    set_linux_task_affinity, set_next_latency_pick_hint, set_next_pick_hint,
    set_next_process_pick_hint, set_next_spawn_pick_hint, set_next_synchronous_pick_hint,
    set_windows_current_thread_affinity, set_windows_process_affinity,
    settle_ipc_reply_scheduling_context, stop_current_linux_process,
    task_has_system_scheduling_class, terminate_user_process, terminate_user_task,
    user_log_ids_for_task, wait_for_child, wake_task, wake_user_task, windows_process_affinity,
    with_current_mm, with_current_process_credentials, with_current_process_state,
    with_current_process_state_mut, with_current_user_linux_state_mut,
    with_current_user_process_and_linux_thread_state_mut, with_current_user_process_state,
    with_current_user_process_state_mut, with_process_state_by_pid, with_process_state_by_pid_mut,
};
pub use self::process_table::{
    ProcessIdentity, SpawnReservation, cancel_spawn as cancel_process_spawn,
    reserve_spawn as reserve_process_spawn,
};
pub use self::retirement::UserFaultDisposition;
pub use self::scheduler::FastIpcCallHandoffOutcome;
pub use self::scheduler::drain_scheduler_runtime_profile;
pub use self::scheduler::smp::drain_fast_ipc_eligibility_rejections;
pub use self::scheduler::{AffinityCommit, AffinityError, ProcessAffinitySnapshot};
pub use self::scheduling_api::{SchedulingContextAdmission, SchedulingContextRuntimeSnapshot};

/// Upper bound for schedulable task identities and cross-crate bounded registries.
pub const MAX_SCHEDULER_TASKS: usize = scheduler::MAX_TASK;

pub use self::irq::{
    commit_block_current_task_and_yield, commit_fast_ipc_call_handoff_and_yield,
    rtc_interrupt_handler_addr, software_schedule_interrupt_handler_addr,
    timer_interrupt_handler_addr, yield_now,
};
#[allow(unused_imports)]
pub(crate) use self::irq::{
    cond_resched, request_deferred_reschedule, request_user_return_reschedule,
    reschedule_deferred_from_interruptible_syscall, reschedule_if_requested,
};
pub use self::spawn::{
    spawn_user_process_state_suspended_with_parent_reservation,
    spawn_user_process_suspended_with_scheduling_context,
    spawn_user_process_with_scheduling_context,
    spawn_user_process_without_deferred_reschedule_with_scheduling_context,
    spawn_user_thread_suspended, start, start_secondary_cpu,
};

// A one-millisecond periodic preemption edge spent a material fraction of a
// vCPU re-entering the scheduler even when the local runqueue had no competing
// task.  Four milliseconds keeps the worst-case fair-class dispatch latency
// well inside a 60 Hz frame while cutting periodic scheduler entries by 75%.
// Explicit wakeups, IPC handoffs, and remote mailbox notifications still use
// immediate software/IPI safe points and are not delayed by this quantum.
const MAIN_THREAD_SLICE_MICROS: u64 = 4_000;
const MIN_THREAD_WEIGHT_MICROS: u64 = 1;
const MAX_THREAD_WEIGHT_MICROS: u64 = 10_000;
const INTERACTIVE_PIT_DIVISOR_FLAG: u16 = 1 << 15;
pub const DEFAULT_USER_TASK_WEIGHT_MICROS: u64 = 100;
const USER_TASK_EXEC_PATH_CAPACITY: usize = 192;
const USER_STACK_PAGE_SIZE: u64 = 4096;

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(0);
static USER_RETURN_RESCHEDULE_ARMED: [AtomicU64; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicU64::new(0) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];

fn allocate_task_id_from(counter: &AtomicU64) -> Option<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .ok()
}

fn allocate_task_id() -> Option<u64> {
    allocate_task_id_from(&NEXT_TASK_ID)
}

pub fn is_initialized() -> bool {
    scheduler_initialized()
}

pub fn mark_root_idle() {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().mark_root_idle();
    });
    crate::debug::record_milestone(crate::debug::LogCategory::Sched, "smp-cpu-idle-enter", 0, 1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnTaskError {
    InvalidWeightMicros,
    NoFreeTaskSlot,
}

pub use retirement::RetiredTaskCleanup;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentUserSnapshot {
    abi: UserAbi,
    thread_id: u64,
    process_id: u64,
    process_generation: u64,
    console_session: ConsoleSessionHandle,
    security: ProcessSecurityContext,
}

pub struct RetainedCurrentUserProcessState {
    process_id: u64,
    abi: UserAbi,
    process: process_table::ProcessRef,
}

pub struct RetainedCurrentUserAddressSpace {
    abi: UserAbi,
    process_id: u64,
    thread_id: u64,
    identity: ProcessIdentity,
    process: process_table::ProcessRef,
}

pub struct CurrentKernelStackScope {
    previous: (u64, u64),
}

pub enum WaitChildResult {
    Exited { pid: u64, status: i32 },
    StateChanged { pid: u64, status: i32 },
    Pending,
    NoMatchingChild,
}

impl RetainedCurrentUserProcessState {
    pub const fn process_id(&self) -> u64 {
        self.process_id
    }

    pub const fn abi(&self) -> UserAbi {
        self.abi
    }

    pub fn with_process_state<R>(&self, f: impl FnOnce(&UserProcessState) -> R) -> R {
        self.process
            .with_visible_state(|_, state| f(state))
            .expect("retained current process crossed an exec staging boundary")
    }

    pub fn with_address_space<R>(&self, f: impl FnOnce(&ProcessAddressSpace) -> R) -> R {
        self.process
            .with_visible_state(|_, state| f(state.address_space()))
            .expect("retained current address space crossed an exec staging boundary")
    }
}

impl CurrentKernelStackScope {
    pub fn enter(base: u64, top: u64) -> Option<Self> {
        let previous = interrupts::without_interrupts(|| unsafe {
            scheduler_mut().set_current_alternate_kernel_stack(base, top)
        })?;
        Some(Self { previous })
    }
}

impl Drop for CurrentKernelStackScope {
    fn drop(&mut self) {
        interrupts::without_interrupts(|| unsafe {
            scheduler_mut().restore_current_alternate_kernel_stack(self.previous);
        });
    }
}

impl RetainedCurrentUserAddressSpace {
    pub const fn abi(&self) -> UserAbi {
        self.abi
    }

    pub const fn process_id(&self) -> u64 {
        self.process_id
    }

    pub const fn thread_id(&self) -> u64 {
        self.thread_id
    }

    pub const fn identity(&self) -> ProcessIdentity {
        self.identity
    }

    pub fn with_process_state<R>(&self, f: impl FnOnce(&UserProcessState) -> R) -> R {
        self.process
            .with_exact_visible_state(self.identity, |_, state| f(state))
            .expect("retained current process crossed its exact MM generation")
    }

    pub fn with_address_space<R>(&self, f: impl FnOnce(&ProcessAddressSpace) -> R) -> R {
        self.process
            .with_exact_visible_state(self.identity, |_, state| f(state.address_space()))
            .expect("retained current address space crossed its exact MM generation")
    }

    pub fn try_with_address_space<R>(
        &self,
        f: impl FnOnce(&ProcessAddressSpace) -> R,
    ) -> Option<R> {
        self.process
            .with_exact_visible_state(self.identity, |_, state| f(state.address_space()))
    }
}

impl SpawnTaskError {
    pub fn summary(&self) -> &'static str {
        match self {
            Self::InvalidWeightMicros => "thread weight is outside the supported range",
            Self::NoFreeTaskSlot => "scheduler task table is full",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UserTaskRegisters {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct UserStackState {
    pub reserve_start: u64,
    pub reserve_end: u64,
    pub committed_start: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct LinuxThreadSnapshot {
    pub process_id: u64,
    pub thread_id: u64,
    pub console_session: ConsoleSessionHandle,
    pub user_stack: Option<UserStackState>,
    pub thread_state: LinuxThreadState,
}

impl UserStackState {
    pub const fn new(reserve_start: u64, reserve_end: u64, committed_start: u64) -> Self {
        Self {
            reserve_start,
            reserve_end,
            committed_start,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UserTaskBootstrap {
    pub abi: UserAbi,
    pub entry: VirtAddr,
    pub stack_pointer: VirtAddr,
    pub registers: UserTaskRegisters,
    pub user_stack: Option<UserStackState>,
    pub linux_process_state: Option<LinuxProcessState>,
    pub linux_memory_map: Option<LinuxMemoryMapState>,
    pub linux_runtime_profile: Option<LinuxRuntimeProfile>,
    pub linux_thread_state: Option<LinuxThreadState>,
    pub windows_runtime: Option<WindowsProcessRuntimeState>,
    pub windows_thread_state: Option<WindowsThreadRuntimeState>,
    pub console_session: ConsoleSessionHandle,
    pub logical_admin: bool,
    exec_path: [u8; USER_TASK_EXEC_PATH_CAPACITY],
    exec_path_len: usize,
}

impl UserTaskBootstrap {
    pub fn new(abi: UserAbi, entry: VirtAddr, stack_pointer: VirtAddr) -> Self {
        Self {
            abi,
            entry,
            stack_pointer,
            registers: UserTaskRegisters::default(),
            user_stack: None,
            linux_process_state: None,
            linux_memory_map: None,
            linux_runtime_profile: None,
            linux_thread_state: None,
            windows_runtime: None,
            windows_thread_state: None,
            console_session: ConsoleSessionHandle::SYSTEM,
            logical_admin: false,
            exec_path: [0; USER_TASK_EXEC_PATH_CAPACITY],
            exec_path_len: 0,
        }
    }

    pub fn set_exec_path(&mut self, exec_path: &str) {
        self.exec_path.fill(0);
        self.exec_path_len = 0;
        for byte in exec_path.bytes() {
            if self.exec_path_len == self.exec_path.len() {
                break;
            }
            self.exec_path[self.exec_path_len] = match byte {
                b' '..=b'~' => byte,
                _ => b'?',
            };
            self.exec_path_len += 1;
        }
    }

    pub fn exec_path(&self) -> &str {
        core::str::from_utf8(&self.exec_path[..self.exec_path_len]).unwrap_or("")
    }
}

pub struct Thread {
    entry: fn(u64),
    id: u64,
    pit_divisor: u16,
    slot: Cell<Option<usize>>,
}

impl Thread {
    pub fn new(entry: fn(u64), weight_micros: u64) -> Self {
        Self {
            entry: kernel_fn_in_higher_half(entry),
            id: allocate_task_id().expect("kernel task identity exhausted"),
            pit_divisor: checked_thread_pit_divisor(weight_micros)
                .expect("kernel thread weight must satisfy scheduler limits"),
            slot: Cell::new(None),
        }
    }

    pub fn start(&self) {
        interrupts::without_interrupts(|| unsafe {
            if self.slot.get().is_some() {
                return;
            }

            let cs = CS::get_reg().0 as u64;
            let ss = SS::get_reg().0 as u64;
            let rflags = initial_task_rflags().bits();
            let slot = scheduler_mut()
                .allocate_kernel_slot(
                    self.entry,
                    self.id,
                    self.pit_divisor,
                    cs,
                    ss,
                    rflags,
                    kernel_task_entry_trampoline_addr(),
                )
                .expect("No free task slot");
            self.slot.set(Some(slot));
        });
    }
}

#[cfg(test)]
mod identity_tests;

fn checked_thread_pit_divisor(weight_micros: u64) -> Result<u16, SpawnTaskError> {
    if !thread_weight_is_valid(weight_micros) {
        return Err(SpawnTaskError::InvalidWeightMicros);
    }

    let value = weight_micros & rustos_user_abi::syscall::TASK_WEIGHT_VALUE_MASK;
    let divisor = crate::arch::pit::divisor_from_micros(value);
    Ok(
        if weight_micros & rustos_user_abi::syscall::TASK_WEIGHT_INTERACTIVE_FLAG != 0 {
            divisor | INTERACTIVE_PIT_DIVISOR_FLAG
        } else {
            divisor
        },
    )
}

pub const fn thread_weight_is_valid(weight_micros: u64) -> bool {
    let value = weight_micros & rustos_user_abi::syscall::TASK_WEIGHT_VALUE_MASK;
    let known_bits = rustos_user_abi::syscall::TASK_WEIGHT_VALUE_MASK
        | rustos_user_abi::syscall::TASK_WEIGHT_INTERACTIVE_FLAG;
    weight_micros & !known_bits == 0
        && value >= MIN_THREAD_WEIGHT_MICROS
        && value <= MAX_THREAD_WEIGHT_MICROS
}

fn initial_task_rflags() -> RFlags {
    const RESERVED_BIT_1: u64 = 1 << 1;
    RFlags::from_bits_retain(RESERVED_BIT_1 | RFlags::INTERRUPT_FLAG.bits())
}

fn kernel_fn_in_higher_half(entry: fn(u64)) -> fn(u64) {
    let high_addr = crate::memory::paging::higher_half_addr(entry as usize as u64);
    unsafe { mem::transmute::<usize, fn(u64)>(high_addr as usize) }
}

fn kernel_task_entry_trampoline_addr() -> u64 {
    crate::memory::paging::higher_half_addr(task_entry_trampoline as *const () as usize as u64)
}

fn noop_task_entry(_id: u64) {
    loop {
        hlt();
    }
}

// ASSEMBLY: Initial task stacks name this trampoline by address; there is no
// ordinary Rust call edge for dead-code analysis to observe.
#[allow(dead_code)]
extern "C" fn task_entry_trampoline() -> ! {
    let task = interrupts::without_interrupts(|| unsafe { scheduler_ref().current_task_start() });
    let Some(task) = task else {
        current::exit_current_task();
    };

    (task.entry)(task.id);
    current::exit_current_task();
}
