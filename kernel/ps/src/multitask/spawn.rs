use x86_64::instructions::interrupts;

use kernel_hal::api::cpu;

use super::{
    allocate_task_id, cancel_process_spawn, checked_thread_pit_divisor, initial_task_rflags,
    kernel_task_entry_trampoline_addr, SchedulingContextAdmission, SpawnReservation,
    SpawnTaskError, UserTaskBootstrap, MAIN_THREAD_SLICE_MICROS, NEXT_TASK_ID,
};
use crate::memory::paging::ProcessAddressSpace;
use crate::user::process_state::UserProcessState;

pub fn spawn_user_process_with_scheduling_context(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    weight_micros: u64,
    admission: SchedulingContextAdmission,
    spawn_reservation: SpawnReservation,
) -> Result<u64, SpawnTaskError> {
    spawn_user_process_inner(
        address_space,
        bootstrap,
        None,
        weight_micros,
        true,
        false,
        Some(admission),
        spawn_reservation,
    )
}

pub fn spawn_user_process_without_deferred_reschedule_with_scheduling_context(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    weight_micros: u64,
    admission: SchedulingContextAdmission,
    spawn_reservation: SpawnReservation,
) -> Result<u64, SpawnTaskError> {
    spawn_user_process_inner(
        address_space,
        bootstrap,
        None,
        weight_micros,
        false,
        false,
        Some(admission),
        spawn_reservation,
    )
}

pub fn spawn_user_process_suspended_with_scheduling_context(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    weight_micros: u64,
    admission: SchedulingContextAdmission,
    spawn_reservation: SpawnReservation,
) -> Result<u64, SpawnTaskError> {
    spawn_user_process_inner(
        address_space,
        bootstrap,
        None,
        weight_micros,
        false,
        true,
        Some(admission),
        spawn_reservation,
    )
}

fn spawn_user_process_inner(
    address_space: ProcessAddressSpace,
    bootstrap: UserTaskBootstrap,
    parent_process_id: Option<u64>,
    weight_micros: u64,
    defer_reschedule: bool,
    start_suspended: bool,
    scheduling_context: Option<SchedulingContextAdmission>,
    spawn_reservation: SpawnReservation,
) -> Result<u64, SpawnTaskError> {
    let (id, pit_divisor) =
        match prepare_user_spawn(weight_micros, process_spawn_faulted(), allocate_task_id) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = cancel_process_spawn(spawn_reservation);
                return Err(error);
            }
        };
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();
    let allocation = super::scheduler::root_creation::allocate(
        id,
        super::scheduler::root_creation::Input::AddressSpace(address_space),
        bootstrap,
        parent_process_id,
        pit_divisor,
        user_cs,
        user_ss,
        rflags,
        start_suspended,
        scheduling_context,
        spawn_reservation,
    )
    .ok_or(SpawnTaskError::NoFreeTaskSlot);

    let (spawned_from_user, slot) = match allocation {
        Ok(allocation) => allocation,
        Err(error) => {
            let _ = cancel_process_spawn(spawn_reservation);
            return Err(error);
        }
    };
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

/// Publish one already-reserved process birth after its fallible address-space
/// transaction has completed. The reservation was acquired before cloning or
/// mapping and is cancelled on every scheduler/admission failure here.
pub fn spawn_user_process_state_suspended_with_parent_reservation(
    process_state: UserProcessState,
    bootstrap: UserTaskBootstrap,
    parent_process_id: Option<u64>,
    weight_micros: u64,
    reservation: SpawnReservation,
) -> Result<u64, SpawnTaskError> {
    spawn_user_process_state_inner(
        process_state,
        bootstrap,
        parent_process_id,
        weight_micros,
        true,
        reservation,
    )
}

fn spawn_user_process_state_inner(
    process_state: UserProcessState,
    bootstrap: UserTaskBootstrap,
    parent_process_id: Option<u64>,
    weight_micros: u64,
    start_suspended: bool,
    spawn_reservation: SpawnReservation,
) -> Result<u64, SpawnTaskError> {
    let (id, pit_divisor) =
        match prepare_user_spawn(weight_micros, process_spawn_faulted(), allocate_task_id) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = cancel_process_spawn(spawn_reservation);
                return Err(error);
            }
        };
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let rflags = initial_task_rflags().bits();
    let allocation = super::scheduler::root_creation::allocate(
        id,
        super::scheduler::root_creation::Input::State(process_state),
        bootstrap,
        parent_process_id,
        pit_divisor,
        user_cs,
        user_ss,
        rflags,
        start_suspended,
        None,
        spawn_reservation,
    )
    .ok_or(SpawnTaskError::NoFreeTaskSlot);

    let (spawned_from_user, slot) = match allocation {
        Ok(allocation) => allocation,
        Err(error) => {
            let _ = cancel_process_spawn(spawn_reservation);
            return Err(error);
        }
    };
    // Repeated fork is a steady-state operation. Formatting/emitting a full
    // debugcon frame here costs more than COW creation under KVM; exact spawn
    // lifecycle evidence belongs to the opt-in diagnostic profile.
    #[cfg(rustos_lifecycle_trace)]
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "spawn-user-process-state",
        id,
        spawn_milestone_arg(slot, spawned_from_user, weight_micros),
    );

    if spawned_from_user && !start_suspended {
        super::request_deferred_reschedule();
    }

    Ok(id)
}

fn spawn_milestone_arg(slot: usize, spawned_from_user: bool, weight_micros: u64) -> u64 {
    ((spawned_from_user as u64) << 63) | ((slot as u64) << 32) | (weight_micros & 0xffff_ffff)
}

pub fn spawn_user_thread_suspended(bootstrap: UserTaskBootstrap) -> Result<u64, SpawnTaskError> {
    let id = allocate_task_id().ok_or(SpawnTaskError::NoFreeTaskSlot)?;
    let selectors = (
        crate::arch::gdt::user_code_selector().0 as u64,
        crate::arch::gdt::user_data_selector().0 as u64,
        initial_task_rflags().bits(),
    );
    // Only a scalar receipt survives the first safe point on a killable user.
    // The durable row and then an actual kernel Executor own the real Ticket,
    // original MM and sleepable TEB effects through complete Slot/cancel ACK.
    let receipt = super::scheduler::thread_creation::work::submit(id, bootstrap, selectors)
        .ok_or(SpawnTaskError::NoFreeTaskSlot)?;
    loop {
        match super::scheduler::thread_creation::work::poll(receipt) {
            Some(true) => return Ok(id),
            Some(false) => return Err(SpawnTaskError::NoFreeTaskSlot),
            None => {
                // Kernel callers can help under their own kill-safe Executor;
                // user callers may only yield and observe, never claim work.
                if let Some(executor) =
                    super::scheduler::retirement_cleanup::executor::capture_current()
                {
                    super::scheduler::thread_creation::work::service(&executor);
                }
                super::irq::yield_now();
            }
        }
    }
}

pub fn start(entry: fn(u64)) -> ! {
    super::irq::install_interrupt_dispatch_callbacks();
    let saved_rsp = interrupts::without_interrupts(|| {
        let mut image = super::scheduler::bootstrap::Bootstrap::begin();
        NEXT_TASK_ID.store(1, core::sync::atomic::Ordering::Relaxed);
        image.reset_root(
            crate::arch::pit::divisor_from_micros(MAIN_THREAD_SLICE_MICROS),
            entry,
            crate::arch::gdt::kernel_code_selector().0 as u64,
            crate::arch::gdt::kernel_data_selector().0 as u64,
            initial_task_rflags().bits(),
            kernel_task_entry_trampoline_addr(),
        );
        for index in 1..cpu::discovered_count() {
            let id = allocate_task_id().expect("secondary idle task identity exhausted");
            image.initialize_ap(index as u8, secondary_idle_entry, id);
        }
        image.prepare_and_publish()
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
    super::scheduler::bootstrap::enter_secondary();
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

    #[test]
    fn process_state_spawn_has_no_unreserved_production_alias() {
        let spawn_source = include_str!("spawn.rs")
            .split_once("mod tests {")
            .expect("spawn tests remain below production entries")
            .0;
        let scheduler_source = include_str!("scheduler.rs");
        assert!(!spawn_source.contains("spawn_user_process_state_with_parent("));
        assert!(!spawn_source.contains("spawn_user_process_state_suspended_with_parent("));
        assert!(!spawn_source.contains("spawn_reservation: Option<SpawnReservation>"));
        assert!(
            !scheduler_source
                .contains("spawn_reservation: Option<process_table::SpawnReservation>")
        );
        let owner = include_str!("scheduler/root_creation.rs");
        assert!(spawn_source.contains("root_creation::Input::State(process_state)"));
        assert!(owner.contains("reservation: process_table::SpawnReservation"));
        assert!(owner.contains("RootPublication::prepare(reservation, ticket"));
        assert!(owner.contains("root.publish_process(parent, state)"));
        assert!(owner.contains("root.cancel()"));
        assert!(!scheduler_source.contains("process_table::create_process_with_parent("));
    }
}
