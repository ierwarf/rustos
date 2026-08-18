use x86_64::VirtAddr;
use x86_64::instructions::interrupts;

use kernel_hal::api::cpu;

use super::{
    MAIN_THREAD_SLICE_MICROS, NEXT_TASK_ID, SpawnTaskError, UserTaskBootstrap, allocate_task_id,
    checked_thread_pit_divisor, cpu_local, current_identity, initial_task_rflags,
    kernel_task_entry_trampoline_addr, noop_task_entry, publish_cpu_current_task,
    publish_scheduler_initialized, scheduler_mut, scheduler_ref,
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
    let (id, pit_divisor) =
        prepare_user_spawn(weight_micros, process_spawn_faulted(), allocate_task_id)?;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();
    let (spawned_from_user, slot) = interrupts::without_interrupts(|| unsafe {
        let mut scheduler = scheduler_mut();
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

fn process_spawn_faulted() -> bool {
    if nucleus_core::util::fault_injection::should_fail("process.spawn") {
        crate::debug::warn!(process, "fault injection: process.spawn failed");
        true
    } else {
        false
    }
}

fn allocate_task_id_after_fault_gate(
    faulted: bool,
    allocate: impl FnOnce() -> Option<u64>,
) -> Result<u64, SpawnTaskError> {
    if faulted {
        Err(SpawnTaskError::NoFreeTaskSlot)
    } else {
        allocate().ok_or(SpawnTaskError::NoFreeTaskSlot)
    }
}

fn prepare_user_spawn(
    weight_micros: u64,
    faulted: bool,
    allocate: impl FnOnce() -> Option<u64>,
) -> Result<(u64, u16), SpawnTaskError> {
    // Validate the request before consuming a non-reusable task identity.  An
    // invalid userspace weight must be a side-effect-free admission failure.
    let pit_divisor = checked_thread_pit_divisor(weight_micros)?;
    let id = allocate_task_id_after_fault_gate(faulted, allocate)?;
    Ok((id, pit_divisor))
}

pub fn spawn_user_process_state_with_parent(
    process_state: UserProcessState,
    bootstrap: UserTaskBootstrap,
    parent_process_id: Option<u64>,
    weight_micros: u64,
) -> Result<u64, SpawnTaskError> {
    let (id, pit_divisor) =
        prepare_user_spawn(weight_micros, process_spawn_faulted(), allocate_task_id)?;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();
    let (spawned_from_user, slot) = interrupts::without_interrupts(|| unsafe {
        let mut scheduler = scheduler_mut();
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
    let pit_divisor = checked_thread_pit_divisor(weight_micros)?;
    let id = allocate_task_id().ok_or(SpawnTaskError::NoFreeTaskSlot)?;
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

pub fn spawn_user_thread_suspended(bootstrap: UserTaskBootstrap) -> Result<u64, SpawnTaskError> {
    let id = allocate_task_id().ok_or(SpawnTaskError::NoFreeTaskSlot)?;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();

    let reservation =
        interrupts::without_interrupts(|| unsafe { scheduler_mut().reserve_user_thread_slot(id) })
            .ok_or(SpawnTaskError::NoFreeTaskSlot)?;

    // The scheduler reservation keeps the slot non-runnable while the
    // sleepable ProcessState owner initializes Windows TEB identifiers. No raw
    // scheduler guard is nested with ProcessStateLock in either direction.
    if let Some(thread_state) = bootstrap.windows_thread_state {
        let initialized = super::process_table::with_process_state_mut(
            reservation.process_handle,
            |_, process_state| {
                crate::user::process::initialize_windows_thread_identifiers(
                    process_state.address_space_mut(),
                    thread_state.teb_address,
                    reservation.process_id,
                    id,
                )
            },
        );
        let Some(initialized) = initialized else {
            interrupts::without_interrupts(|| unsafe {
                scheduler_mut().cancel_user_thread_slot(reservation);
            });
            return Err(SpawnTaskError::NoFreeTaskSlot);
        };
        if let Err(error) = initialized {
            interrupts::without_interrupts(|| unsafe {
                scheduler_mut().cancel_user_thread_slot(reservation);
            });
            panic!("failed to initialize windows thread ids: {:?}", error);
        }
    }

    interrupts::without_interrupts(|| unsafe {
        scheduler_mut()
            .commit_user_thread_slot(reservation, bootstrap, user_cs, user_ss, rflags)
            .ok_or(SpawnTaskError::NoFreeTaskSlot)
    })?;

    Ok(id)
}

pub fn start(entry: fn(u64)) -> ! {
    super::irq::install_interrupt_dispatch_callbacks();

    let saved_rsp = interrupts::without_interrupts(|| unsafe {
        NEXT_TASK_ID.store(1, core::sync::atomic::Ordering::Relaxed);
        let mut scheduler = scheduler_mut();
        scheduler.reset(
            crate::arch::pit::divisor_from_micros(MAIN_THREAD_SLICE_MICROS),
            entry,
            0,
            crate::arch::gdt::kernel_code_selector().0 as u64,
            crate::arch::gdt::kernel_data_selector().0 as u64,
            initial_task_rflags().bits(),
            kernel_task_entry_trampoline_addr(),
        );
        let cpu_count = cpu::discovered_count();
        assert!(
            (1..=nucleus_core::util::lockdep::MAX_TRACKED_CPUS).contains(&cpu_count),
            "scheduler invariant: admitted CPU count is outside capacity"
        );
        let mut generations = [0_u64; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
        for logical_index in 0..cpu_count {
            let logical_index =
                u8::try_from(logical_index).expect("scheduler CPU index exceeds u8 capacity");
            let snapshot = cpu::lifecycle_snapshot(logical_index)
                .expect("scheduler invariant: admitted CPU has no lifecycle slot");
            assert_eq!(
                snapshot.state,
                cpu::CpuLifecycleState::OnlineParked,
                "scheduler invariant: CPU admitted before OnlineParked"
            );
            generations[usize::from(logical_index)] = snapshot.generation;
            if logical_index == 0 {
                continue;
            }
            let id = allocate_task_id().expect("secondary idle task identity exhausted");
            let (raw_stack_base, stack_top) =
                cpu::ap_bootstrap_stack_bounds(logical_index, snapshot.generation);
            let slot = scheduler.initialize_secondary_idle(
                logical_index,
                secondary_idle_entry,
                id,
                raw_stack_base,
                stack_top,
            );
            publish_cpu_current_task(usize::from(logical_index), slot);
        }
        scheduler.prepare_current_task_execution();
        let saved_rsp = scheduler.current_saved_rsp();
        drop(scheduler);
        // ORDERING: publish the complete BSP/AP scheduler image before any
        // CPU lifecycle becomes SchedulerReady and before timer/IPI leaves may
        // enter without a redundant global-lock readiness probe.
        publish_scheduler_initialized();
        for (logical_index, generation) in generations[..cpu_count].iter().copied().enumerate() {
            cpu::transition_lifecycle(
                u8::try_from(logical_index).expect("scheduler CPU index exceeds u8 capacity"),
                generation,
                cpu::CpuLifecycleState::SchedulerReady,
            );
            crate::debug::record_milestone(
                crate::debug::LogCategory::Boot,
                "smp-cpu-scheduler-ready",
                logical_index as u64,
                generation,
            );
        }
        crate::arch::tlb::admit_current_cpu_online();
        cpu::transition_lifecycle(0, generations[0], cpu::CpuLifecycleState::Online);
        crate::debug::record_milestone(
            crate::debug::LogCategory::Boot,
            "smp-cpu-online",
            0,
            generations[0],
        );
        saved_rsp
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

fn secondary_idle_entry(_arg: u64) {
    loop {
        x86_64::instructions::hlt();
    }
}

pub fn start_secondary_cpu() -> ! {
    interrupts::disable();
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    assert!(
        logical_index > 0 && logical_index < nucleus_core::util::lockdep::MAX_TRACKED_CPUS,
        "secondary scheduler entry executed on an invalid CPU"
    );
    let logical_index =
        u8::try_from(logical_index).expect("secondary scheduler CPU index exceeds u8 capacity");
    let snapshot = cpu::lifecycle_snapshot(logical_index)
        .expect("secondary scheduler entry has no lifecycle slot");
    assert_eq!(
        snapshot.state,
        cpu::CpuLifecycleState::SchedulerReady,
        "secondary scheduler entry occurred outside SchedulerReady"
    );
    unsafe {
        scheduler_mut().prepare_secondary_idle_execution(logical_index);
    }
    crate::arch::tlb::admit_current_cpu_online();
    assert!(
        crate::arch::timer::init_current_cpu(),
        "secondary scheduler entry could not arm its local clockevent"
    );
    cpu::transition_lifecycle(
        logical_index,
        snapshot.generation,
        cpu::CpuLifecycleState::Online,
    );
    crate::debug::record_milestone(
        crate::debug::LogCategory::Boot,
        "smp-ap-online",
        u64::from(logical_index),
        snapshot.generation,
    );
    crate::debug::record_milestone(
        crate::debug::LogCategory::Boot,
        "smp-cpu-online",
        u64::from(logical_index),
        snapshot.generation,
    );
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "smp-cpu-idle-enter",
        u64::from(logical_index),
        snapshot.generation,
    );

    loop {
        interrupts::enable_and_hlt();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_fault_gate_prevents_publication() {
        let allocator_called = core::cell::Cell::new(false);
        assert_eq!(
            allocate_task_id_after_fault_gate(true, || {
                allocator_called.set(true);
                Some(7)
            }),
            Err(SpawnTaskError::NoFreeTaskSlot)
        );
        assert!(!allocator_called.get());
    }

    #[test]
    fn invalid_spawn_weight_does_not_consume_identity() {
        let allocator_called = core::cell::Cell::new(false);
        assert_eq!(
            prepare_user_spawn(0, false, || {
                allocator_called.set(true);
                Some(7)
            }),
            Err(SpawnTaskError::InvalidWeightMicros)
        );
        assert!(!allocator_called.get());
    }
}
