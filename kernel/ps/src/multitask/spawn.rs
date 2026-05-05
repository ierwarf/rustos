use x86_64::VirtAddr;
use x86_64::instructions::interrupts;

use super::{
    MAIN_THREAD_SLICE_MICROS, NEXT_TASK_ID, SpawnTaskError, UserTaskBootstrap,
    checked_thread_pit_divisor, initial_task_rflags, noop_task_entry, scheduler_mut, scheduler_ref,
};
use crate::memory::paging::ProcessAddressSpace;
use crate::user::process_state::UserProcessState;

pub fn spawn_user_process(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    spawn_user_process_with_parent(address_space, bootstrap, None, weight_micros)
}

pub fn spawn_user_process_with_parent(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    parent_process_id: Option<u64>,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    let id = NEXT_TASK_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let pit_divisor = checked_thread_pit_divisor(weight_micros)?;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();
    let spawned_from_user = interrupts::without_interrupts(|| unsafe {
        let scheduler = scheduler_mut();
        let current_is_user = scheduler.current_task_is_user_task();
        scheduler
            .allocate_user_slot(
                id,
                address_space,
                bootstrap,
                parent_process_id,
                pit_divisor,
                user_cs,
                user_ss,
                rflags,
                noop_task_entry,
            )
            .map(|_| current_is_user)
            .ok_or(SpawnTaskError::NoFreeTaskSlot)
    })?;

    if spawned_from_user {
        super::request_deferred_reschedule();
    }

    Ok(id)
}

pub fn spawn_kernel_process(
    process_state: UserProcessState,
    entry: VirtAddr,
    arg0: u64,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    let id = NEXT_TASK_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let pit_divisor = checked_thread_pit_divisor(weight_micros)?;
    let kernel_cs = crate::arch::gdt::kernel_code_selector().0 as u64;
    let kernel_ss = crate::arch::gdt::kernel_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();

    interrupts::without_interrupts(|| unsafe {
        scheduler_mut()
            .allocate_kernel_process_slot(
                id,
                process_state,
                entry,
                arg0,
                pit_divisor,
                kernel_cs,
                kernel_ss,
                rflags,
            )
            .ok_or(SpawnTaskError::NoFreeTaskSlot)
    })?;

    Ok(id)
}

pub fn spawn_user_thread(
    bootstrap: UserTaskBootstrap,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    let id = NEXT_TASK_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
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

pub fn init() {
    super::irq::install_interrupt_dispatch_callbacks();

    unsafe {
        scheduler_mut().reset(crate::arch::pit::divisor_from_micros(
            MAIN_THREAD_SLICE_MICROS,
        ));
        scheduler_mut().prepare_current_task_execution();
    }

    crate::arch::pit::start_micros(0, MAIN_THREAD_SLICE_MICROS);
    crate::debug::info!(
        sched,
        "scheduler initialized slice_micros={}",
        MAIN_THREAD_SLICE_MICROS
    );
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
