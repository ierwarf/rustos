mod context;
mod current;
mod irq;
mod process_table;
mod scheduler;
mod spawn;

use core::{
    cell::Cell,
    mem, ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use x86_64::VirtAddr;
use x86_64::instructions::{hlt, interrupts};
use x86_64::registers::rflags::RFlags;
use x86_64::registers::segmentation::{CS, SS, Segment};

use self::scheduler::Scheduler;
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
    any_user_process_state, block_current_task, block_current_user_task, current_last_error,
    current_linux_thread_state, current_task_id, current_user_address_space, current_user_id,
    current_user_process_id, current_user_snapshot, current_user_stack_state,
    current_user_thread_id, exec_current_user_process, exit_current_user_task,
    halt_current_retired_task, is_user_task_alive, note_process_exit_status,
    parent_process_id_of, queue_linux_signal,
    retain_current_user_process_state, retire_current_user_task_due_to_fault,
    service_deferred_work, set_current_last_error, terminate_user_task, wait_for_child, wake_task,
    wake_user_task, with_current_mm, with_current_process_credentials, with_current_process_state,
    with_current_process_state_mut, with_current_user_linux_state_mut,
    with_current_user_process_and_linux_thread_state_mut, with_current_user_process_state,
    with_current_user_process_state_mut, with_current_user_windows_thread_state_mut,
    with_process_state_by_pid_mut,
};
#[allow(unused_imports)]
pub(crate) use self::current::{current_console_session, set_current_console_session};
#[allow(unused_imports)]
pub(crate) use self::irq::{
    clear_deferred_reschedule_request, request_deferred_reschedule, reschedule_if_requested,
};
pub use self::irq::{
    rtc_interrupt_handler_addr, software_schedule_interrupt_handler_addr,
    timer_interrupt_handler_addr, yield_now,
};
pub use self::spawn::{
    restore_current_simd_state, save_current_simd_state, spawn_kernel_process, spawn_user_process,
    spawn_user_process_with_parent, spawn_user_thread, start,
};

const MAIN_THREAD_SLICE_MICROS: u64 = 1_000;
const MIN_THREAD_WEIGHT_MICROS: u64 = 1;
const MAX_THREAD_WEIGHT_MICROS: u64 = 100;
pub const DEFAULT_USER_TASK_WEIGHT_MICROS: u64 = 50;
const USER_TASK_EXEC_PATH_CAPACITY: usize = 192;
const USER_STACK_PAGE_SIZE: u64 = 4096;

static mut SCHEDULER: Scheduler = Scheduler::new();
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(0);
static DEFERRED_RESCHEDULE_REQUESTED: AtomicU64 = AtomicU64::new(0);

#[inline(always)]
unsafe fn scheduler_mut() -> &'static mut Scheduler {
    unsafe { &mut *ptr::addr_of_mut!(SCHEDULER) }
}

#[inline(always)]
unsafe fn scheduler_ref() -> &'static Scheduler {
    unsafe { &*ptr::addr_of!(SCHEDULER) }
}

fn scheduler_initialized() -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().initialized() })
}

#[allow(dead_code)]
pub fn is_initialized() -> bool {
    scheduler_initialized()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnTaskError {
    InvalidWeightMicros,
    NoFreeTaskSlot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentUserSnapshot {
    abi: UserAbi,
    thread_id: u64,
    process_id: u64,
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
    process: process_table::ProcessRef,
}

pub struct CurrentKernelStackScope {
    previous: (u64, u64),
}

pub enum WaitChildResult {
    Exited { pid: u64, status: i32 },
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

    pub fn process_state(&self) -> &UserProcessState {
        self.process.state()
    }

    pub fn address_space(&self) -> &ProcessAddressSpace {
        self.process.state().address_space()
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

    pub fn process_state(&self) -> &UserProcessState {
        self.process.state()
    }

    pub fn address_space(&self) -> &ProcessAddressSpace {
        self.process.state().address_space()
    }
}

impl CurrentUserSnapshot {
    pub(crate) const fn new(
        abi: UserAbi,
        thread_id: u64,
        process_id: u64,
        console_session: ConsoleSessionHandle,
        security: ProcessSecurityContext,
    ) -> Self {
        Self {
            abi,
            thread_id,
            process_id,
            console_session,
            security,
        }
    }

    pub const fn abi(self) -> UserAbi {
        self.abi
    }

    pub const fn thread_id(self) -> u64 {
        self.thread_id
    }

    pub const fn process_id(self) -> u64 {
        self.process_id
    }

    pub const fn console_session(self) -> ConsoleSessionHandle {
        self.console_session
    }

    pub const fn security(self) -> ProcessSecurityContext {
        self.security
    }
}

impl SpawnTaskError {
    #[allow(dead_code)]
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

#[derive(Debug, Clone, Copy, Default)]
pub struct UserStackState {
    pub reserve_start: u64,
    pub reserve_end: u64,
    pub committed_start: u64,
}

impl UserStackState {
    pub const fn new(reserve_start: u64, reserve_end: u64, committed_start: u64) -> Self {
        Self {
            reserve_start,
            reserve_end,
            committed_start,
        }
    }

    pub fn contains_reserved_address(self, addr: u64) -> bool {
        addr >= self.reserve_start && addr < self.committed_start
    }

    pub fn contains_stack_pointer(self, rsp: u64) -> bool {
        rsp >= self.reserve_start && rsp < self.reserve_end
    }

    pub fn grow_to_include_fault(&mut self, fault_addr: u64) -> Option<(u64, u64, usize)> {
        let fault_page = fault_addr & !(USER_STACK_PAGE_SIZE - 1);
        if fault_page < self.reserve_start || fault_page >= self.committed_start {
            return None;
        }

        let previous_committed_start = self.committed_start;
        let page_count = ((previous_committed_start - fault_page) / USER_STACK_PAGE_SIZE) as usize;
        if page_count == 0 {
            return None;
        }

        self.committed_start = fault_page;
        Some((fault_page, previous_committed_start, page_count))
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

#[allow(dead_code)]
impl Thread {
    pub fn new(entry: fn(u64), weight_micros: u64) -> Self {
        Self {
            entry: kernel_fn_in_higher_half(entry),
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
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

    #[allow(dead_code)]
    pub fn stop(&self) {
        interrupts::without_interrupts(|| unsafe {
            let Some(slot) = self.slot.replace(None) else {
                return;
            };

            scheduler_mut().clear_slot(slot);
        });
    }
}

fn checked_thread_pit_divisor(weight_micros: u64) -> Result<u16, SpawnTaskError> {
    if !thread_weight_is_valid(weight_micros) {
        return Err(SpawnTaskError::InvalidWeightMicros);
    }

    Ok(crate::arch::pit::divisor_from_micros(weight_micros))
}

pub const fn thread_weight_is_valid(weight_micros: u64) -> bool {
    weight_micros >= MIN_THREAD_WEIGHT_MICROS && weight_micros <= MAX_THREAD_WEIGHT_MICROS
}

fn initial_task_rflags() -> RFlags {
    const RESERVED_BIT_1: u64 = 1 << 1;
    RFlags::from_bits_retain(RESERVED_BIT_1 | RFlags::INTERRUPT_FLAG.bits())
}

#[allow(dead_code)]
fn kernel_fn_in_higher_half(entry: fn(u64)) -> fn(u64) {
    let high_addr = crate::memory::paging::higher_half_addr(entry as usize as u64);
    unsafe { mem::transmute::<usize, fn(u64)>(high_addr as usize) }
}

#[allow(dead_code)]
fn kernel_task_entry_trampoline_addr() -> u64 {
    crate::memory::paging::higher_half_addr(task_entry_trampoline as *const () as usize as u64)
}

fn noop_task_entry(_id: u64) {
    loop {
        hlt();
    }
}

#[allow(dead_code)]
extern "C" fn task_entry_trampoline() -> ! {
    let task = interrupts::without_interrupts(|| unsafe { scheduler_ref().current_task_start() });
    let Some(task) = task else {
        current::exit_current_task();
    };

    (task.entry)(task.id);
    current::exit_current_task();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserFaultDisposition {
    Resumed,
    Retired,
    Unhandled,
}
