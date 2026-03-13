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
use crate::paging::ProcessAddressSpace;

const MAIN_THREAD_SLICE_MICROS: u64 = 1_000;
const MIN_THREAD_WEIGHT_MICROS: u64 = 1;
const MAX_THREAD_WEIGHT_MICROS: u64 = 100;

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
    entry: VirtAddr,
    user_stack_top: VirtAddr,
    weight_micros: u64,
    arg0: u64,
    arg1: u64,
) -> Result<u64, SpawnTaskError> {
    let id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
    let pit_divisor = checked_thread_pit_divisor(weight_micros)?;
    let user_cs = crate::gdt::user_code_selector().0 as u64;
    let user_ss = crate::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();

    interrupts::without_interrupts(|| unsafe {
        scheduler_mut()
            .allocate_user_slot(
                id,
                address_space,
                entry,
                user_stack_top,
                pit_divisor,
                user_cs,
                user_ss,
                rflags,
                arg0,
                arg1,
                noop_task_entry,
            )
            .ok_or(SpawnTaskError::NoFreeTaskSlot)
    })?;

    Ok(id)
}

fn checked_thread_pit_divisor(weight_micros: u64) -> Result<u16, SpawnTaskError> {
    if !(MIN_THREAD_WEIGHT_MICROS..=MAX_THREAD_WEIGHT_MICROS).contains(&weight_micros) {
        return Err(SpawnTaskError::InvalidWeightMicros);
    }

    Ok(crate::pit::divisor_from_micros(weight_micros))
}

fn initial_task_rflags() -> RFlags {
    const RESERVED_BIT_1: u64 = 1 << 1;
    RFlags::from_bits_retain(RESERVED_BIT_1 | RFlags::INTERRUPT_FLAG.bits())
}

fn kernel_fn_in_higher_half(entry: fn(u64)) -> fn(u64) {
    let high_addr = crate::paging::higher_half_addr(entry as usize as u64);
    unsafe { mem::transmute::<usize, fn(u64)>(high_addr as usize) }
}

fn kernel_task_entry_trampoline_addr() -> u64 {
    crate::paging::higher_half_addr(task_entry_trampoline as *const () as usize as u64)
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
        scheduler_mut().reset(crate::pit::divisor_from_micros(MAIN_THREAD_SLICE_MICROS));
        scheduler_mut().prepare_current_task_execution();
    }

    crate::pit::start_micros(0, MAIN_THREAD_SLICE_MICROS);
}

pub fn save_current_fx_state() {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().save_current_fx_state();
    });
}

pub fn restore_current_fx_state() {
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref().restore_current_fx_state();
    });
}

pub fn current_user_address_space() -> Option<&'static ProcessAddressSpace> {
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref()
            .current_user_address_space()
            .map(|space| &*(space as *const ProcessAddressSpace))
    })
}

pub fn retire_current_user_task_due_to_fault(
    vector: u8,
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().retire_current_user_task_due_to_fault(vector, error_code, cr2, rip)
    })
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

unsafe extern "C" {
    fn timer_interrupt_handler();
}

pub fn timer_interrupt_handler_addr() -> u64 {
    crate::paging::higher_half_addr(timer_interrupt_handler as *const () as usize as u64)
}

#[unsafe(no_mangle)]
extern "C" fn timer_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    let current_rsp = context_ptr as usize;
    let next_rsp = unsafe {
        let scheduler = scheduler_mut();
        scheduler.save_current_fx_state();
        let (next_rsp, next_pit_divisor) = scheduler.on_timer_interrupt(current_rsp);
        crate::pit::set_divisor(0, next_pit_divisor);
        scheduler.prepare_current_task_execution();
        scheduler.reap_inactive_retired_slots();
        scheduler.restore_current_fx_state();
        next_rsp
    };

    crate::input::poll_fallback();
    crate::pic::send_eoi(crate::pic::PIC_1_OFFSET);
    next_rsp as *mut SavedContext
}
