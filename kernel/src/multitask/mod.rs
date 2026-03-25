mod context;
mod scheduler;

use core::{
    cell::Cell,
    mem, ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use x86_64::VirtAddr;
use x86_64::instructions::{hlt, interrupts};
use x86_64::registers::rflags::RFlags;
use x86_64::registers::segmentation::{CS, SS, Segment};

use self::context::SavedContext;
use self::scheduler::Scheduler;
use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging::ProcessAddressSpace;
use crate::user::abi::UserAbi;
use crate::user::linux::{LinuxMemoryMapState, LinuxProcessState, LinuxThreadState};
use crate::user::process_state::UserProcessState;

const MAIN_THREAD_SLICE_MICROS: u64 = 1_000;
const MIN_THREAD_WEIGHT_MICROS: u64 = 1;
const MAX_THREAD_WEIGHT_MICROS: u64 = 100;
pub const DEFAULT_USER_TASK_WEIGHT_MICROS: u64 = 50;
const USER_TASK_EXEC_PATH_CAPACITY: usize = 192;
const USER_STACK_PAGE_SIZE: u64 = 4096;

static mut SCHEDULER: Scheduler = Scheduler::new();
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(0);

#[inline(always)]
unsafe fn scheduler_mut() -> &'static mut Scheduler {
    unsafe { &mut *ptr::addr_of_mut!(SCHEDULER) }
}

#[inline(always)]
unsafe fn scheduler_ref() -> &'static Scheduler {
    unsafe { &*ptr::addr_of!(SCHEDULER) }
}

fn scheduler_bootstrap_ready() -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().bootstrap_context_ready() })
}

pub fn is_initialized() -> bool {
    scheduler_bootstrap_ready()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnTaskError {
    InvalidWeightMicros,
    NoFreeTaskSlot,
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
    pub linux_thread_state: Option<LinuxThreadState>,
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
            linux_thread_state: None,
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

pub fn spawn_user_process(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    let id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
    let pit_divisor = checked_thread_pit_divisor(weight_micros)?;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();

    interrupts::without_interrupts(|| unsafe {
        scheduler_mut()
            .allocate_user_slot(
                id,
                address_space,
                bootstrap,
                pit_divisor,
                user_cs,
                user_ss,
                rflags,
                noop_task_entry,
            )
            .ok_or(SpawnTaskError::NoFreeTaskSlot)
    })?;

    Ok(id)
}

pub fn spawn_user_thread(
    bootstrap: UserTaskBootstrap,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    let id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
    let pit_divisor = checked_thread_pit_divisor(weight_micros)?;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();

    interrupts::without_interrupts(|| unsafe {
        scheduler_mut()
            .allocate_user_thread_slot(id, bootstrap, pit_divisor, user_cs, user_ss, rflags)
            .ok_or(SpawnTaskError::NoFreeTaskSlot)
    })?;

    Ok(id)
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

extern "C" fn task_entry_trampoline() -> ! {
    let task = interrupts::without_interrupts(|| unsafe { scheduler_ref().current_task_start() });
    let Some(task) = task else {
        exit_current_task();
    };

    (task.entry)(task.id);
    exit_current_task();
}

fn exit_current_task() -> ! {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exit_current_task();
    });

    loop {
        hlt();
    }
}

pub fn init() {
    unsafe {
        scheduler_mut().reset(crate::arch::pit::divisor_from_micros(
            MAIN_THREAD_SLICE_MICROS,
        ));
        scheduler_mut().prepare_current_task_execution();
    }

    crate::arch::pit::start_micros(0, MAIN_THREAD_SLICE_MICROS);
}

pub fn save_current_simd_state() {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().save_current_simd_state();
    });
}

pub fn restore_current_simd_state() {
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref().restore_current_simd_state();
    });
}

pub fn current_user_address_space() -> Option<&'static ProcessAddressSpace> {
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref()
            .current_user_address_space()
            .map(|space| &*(space as *const ProcessAddressSpace))
    })
}

pub fn current_user_abi() -> Option<UserAbi> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_user_abi() })
}

pub fn current_user_id() -> Option<u64> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_user_id() })
}

pub fn current_user_process_id() -> Option<u64> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_user_process_id() })
}

pub fn is_user_task_alive(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().is_user_task_alive(task_id) })
}

pub fn terminate_user_task(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        let requested_by_pid = scheduler_ref().current_user_id();
        scheduler_mut().terminate_user_task(task_id, requested_by_pid)
    })
}

pub fn block_current_user_task() -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().block_current_user_task() })
}

pub fn wake_user_task(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().wake_user_task(task_id) })
}

pub fn current_console_session() -> ConsoleSessionHandle {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_console_session() })
}

pub fn with_current_user_linux_state_mut<R>(
    f: impl FnOnce(
        u64,
        u64,
        UserAbi,
        &mut ProcessAddressSpace,
        &mut Option<LinuxProcessState>,
        &mut Option<LinuxThreadState>,
    ) -> R,
) -> Option<R> {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().with_current_user_linux_state_mut(f)
    })
}

pub fn with_current_user_process_state_mut<R>(
    f: impl FnOnce(u64, UserAbi, &mut UserProcessState) -> R,
) -> Option<R> {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().with_current_user_process_state_mut(f)
    })
}

pub fn retire_current_user_task_due_to_fault(
    vector: u8,
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
    rsp: u64,
) -> UserFaultDisposition {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().retire_current_user_task_due_to_fault(vector, error_code, cr2, rip, rsp)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserFaultDisposition {
    Resumed,
    Retired,
    Unhandled,
}

pub fn halt_current_retired_task() -> ! {
    loop {
        interrupts::enable_and_hlt();
    }
}

pub fn exit_current_user_task() -> ! {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exit_current_task();
    });
    halt_current_retired_task()
}

pub fn current_last_error() -> u32 {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_last_error() })
}

pub fn set_current_last_error(value: u32) {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().set_current_last_error(value);
    });
}

pub fn service_deferred_work() -> usize {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().reap_inactive_retired_slots() })
}

unsafe extern "C" {
    fn timer_interrupt_handler();
    fn rtc_scheduler_interrupt_handler();
    fn software_schedule_interrupt_handler();
}

pub fn timer_interrupt_handler_addr() -> u64 {
    crate::memory::paging::higher_half_addr(timer_interrupt_handler as *const () as usize as u64)
}

pub fn rtc_interrupt_handler_addr() -> u64 {
    crate::memory::paging::higher_half_addr(
        rtc_scheduler_interrupt_handler as *const () as usize as u64,
    )
}

pub fn software_schedule_interrupt_handler_addr() -> u64 {
    crate::memory::paging::higher_half_addr(
        software_schedule_interrupt_handler as *const () as usize as u64,
    )
}

#[unsafe(no_mangle)]
extern "C" fn timer_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    if !scheduler_bootstrap_ready() {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_1_OFFSET);
        return context_ptr;
    }

    let current_rsp = context_ptr as usize;
    let next_rsp = unsafe {
        let scheduler = scheduler_mut();
        crate::driver::linux::runtime::tick_jiffies(1);
        scheduler.save_current_simd_state();
        let (next_rsp, next_pit_divisor) = scheduler.on_timer_interrupt(current_rsp);
        crate::arch::pit::set_divisor(0, next_pit_divisor);
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        next_rsp
    };

    crate::arch::pic::send_eoi(crate::arch::pic::PIC_1_OFFSET);
    next_rsp as *mut SavedContext
}

#[unsafe(no_mangle)]
extern "C" fn rtc_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    if !scheduler_bootstrap_ready() {
        crate::arch::rtc::on_interrupt();
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
        return context_ptr;
    }

    let current_rsp = context_ptr as usize;
    let next_rsp = unsafe {
        let scheduler = scheduler_mut();
        crate::driver::linux::runtime::tick_jiffies(1);
        scheduler.save_current_simd_state();
        let (next_rsp, _next_pit_divisor) = scheduler.on_timer_interrupt(current_rsp);
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        next_rsp
    };

    crate::arch::rtc::on_interrupt();
    crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
    next_rsp as *mut SavedContext
}

#[unsafe(no_mangle)]
extern "C" fn software_schedule_interrupt_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    if !scheduler_bootstrap_ready() {
        return context_ptr;
    }

    let current_rsp = context_ptr as usize;
    let next_rsp = unsafe {
        let scheduler = scheduler_mut();
        scheduler.save_current_simd_state();
        let (next_rsp, _next_pit_divisor) = scheduler.on_timer_interrupt(current_rsp);
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        next_rsp
    };

    next_rsp as *mut SavedContext
}

pub fn yield_now() {
    interrupts::without_interrupts(|| unsafe {
        software_schedule_trap();
    });
}

unsafe extern "C" {
    fn software_schedule_trap();
}
