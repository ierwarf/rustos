use x86_64::VirtAddr;
use x86_64::instructions::interrupts;

use super::{
    MAIN_THREAD_SLICE_MICROS, NEXT_TASK_ID, SpawnTaskError, UserTaskBootstrap,
    checked_thread_pit_divisor, initial_task_rflags, kernel_task_entry_trampoline_addr,
    noop_task_entry, scheduler_mut, scheduler_ref,
};
use crate::memory::paging::ProcessAddressSpace;
use crate::user::process_state::UserProcessState;

pub fn spawn_user_process(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    spawn_user_process_inner(address_space, bootstrap, None, weight_micros, true, false)
}

pub fn spawn_user_process_with_parent(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    parent_process_id: Option<u64>,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    spawn_user_process_inner(
        address_space,
        bootstrap,
        parent_process_id,
        weight_micros,
        true,
        false,
    )
}

pub fn spawn_user_process_without_deferred_reschedule(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    spawn_user_process_inner(address_space, bootstrap, None, weight_micros, false, false)
}

pub fn spawn_user_process_suspended(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    spawn_user_process_inner(address_space, bootstrap, None, weight_micros, false, true)
}

fn spawn_user_process_inner(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    parent_process_id: Option<u64>,
    weight_micros: u64,
    defer_reschedule: bool,
    start_suspended: bool,
) -> Result<u64, SpawnTaskError> {
    if nucleus_core::util::fault_injection::should_fail("process.spawn") {
        crate::debug::warn!(process, "fault injection: process.spawn failed");
        return Err(SpawnTaskError::NoFreeTaskSlot);
    }
    let id = NEXT_TASK_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let pit_divisor = checked_thread_pit_divisor(weight_micros)?;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();
    let (spawned_from_user, slot) = interrupts::without_interrupts(|| unsafe {
        let scheduler = scheduler_mut();
        let current_is_user = scheduler.current_task_is_user_task();
        let slot = scheduler
            .allocate_user_slot(
                id,
                address_space,
                bootstrap,
                parent_process_id,
                pit_divisor,
                user_cs,
                user_ss,
                rflags,
                start_suspended,
                noop_task_entry,
            )
            .ok_or(SpawnTaskError::NoFreeTaskSlot)?;
        Ok((current_is_user, slot))
    })?;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "spawn-user-process",
        id,
        spawn_milestone_arg(slot, spawned_from_user, weight_micros),
    );

    if spawned_from_user && defer_reschedule && !start_suspended {
        super::set_next_spawn_pick_hint(id);
    }

    Ok(id)
}

pub fn spawn_user_process_state_with_parent(
    process_state: UserProcessState,
    bootstrap: UserTaskBootstrap,
    parent_process_id: Option<u64>,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    if nucleus_core::util::fault_injection::should_fail("process.spawn") {
        crate::debug::warn!(process, "fault injection: process.spawn failed");
        return Err(SpawnTaskError::NoFreeTaskSlot);
    }
    let id = NEXT_TASK_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let pit_divisor = checked_thread_pit_divisor(weight_micros)?;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();
    let (spawned_from_user, slot) = interrupts::without_interrupts(|| unsafe {
        let scheduler = scheduler_mut();
        let current_is_user = scheduler.current_task_is_user_task();
        let slot = scheduler
            .allocate_user_process_state_slot(
                id,
                process_state,
                bootstrap,
                parent_process_id,
                pit_divisor,
                user_cs,
                user_ss,
                rflags,
                noop_task_entry,
            )
            .ok_or(SpawnTaskError::NoFreeTaskSlot)?;
        Ok((current_is_user, slot))
    })?;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "spawn-user-process-state",
        id,
        spawn_milestone_arg(slot, spawned_from_user, weight_micros),
    );

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

fn spawn_milestone_arg(slot: usize, spawned_from_user: bool, weight_micros: u64) -> u64 {
    ((spawned_from_user as u64) << 63) | ((slot as u64) << 32) | (weight_micros & 0xffff_ffff)
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

pub fn start(entry: fn(u64)) -> ! {
    super::irq::install_interrupt_dispatch_callbacks();

    let saved_rsp = interrupts::without_interrupts(|| unsafe {
        NEXT_TASK_ID.store(1, core::sync::atomic::Ordering::Relaxed);
        let scheduler = scheduler_mut();
        scheduler.reset(
            crate::arch::pit::divisor_from_micros(MAIN_THREAD_SLICE_MICROS),
            entry,
            0,
            crate::arch::gdt::kernel_code_selector().0 as u64,
            crate::arch::gdt::kernel_data_selector().0 as u64,
            initial_task_rflags().bits(),
            kernel_task_entry_trampoline_addr(),
        );
        scheduler.prepare_current_task_execution();
        scheduler.current_saved_rsp()
    });

    crate::arch::pit::start_micros(0, MAIN_THREAD_SLICE_MICROS);
    crate::debug::info!(
        sched,
        "scheduler initialized slice_micros={}",
        MAIN_THREAD_SLICE_MICROS
    );
    unsafe {
        kernel_hal::api::restore_kernel_saved_context(
            saved_rsp as *mut super::context::SavedContext,
        )
    }
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
