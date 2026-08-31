use core::sync::atomic::{AtomicBool, Ordering};

use kernel_compat::api as compat_api;
use kernel_hal::api as hal_api;
use kernel_mm::api as mm_api;
use kernel_ps::api as ps_api;
use rustos_user_abi::pager::{
    PAGER_FAULT_ABI_VERSION, PAGER_PAGE_BYTES, PagerFaultRequestWire, VM_ACCESS_EXECUTE,
    VM_ACCESS_READ, VM_ACCESS_WRITE, VM_OBJECT_ANONYMOUS,
};

static FIRST_PAGER_RESERVATION_REJECTION: AtomicBool = AtomicBool::new(false);
static FIRST_USER_PRESENT_PAGE_FAULT: AtomicBool = AtomicBool::new(false);

fn page_fault_access(error_code: Option<u64>) -> Option<u16> {
    let error = error_code?;
    const PRESENT: u64 = 1 << 0;
    const WRITE: u64 = 1 << 1;
    const USER: u64 = 1 << 2;
    const RESERVED: u64 = 1 << 3;
    const INSTRUCTION: u64 = 1 << 4;
    const PROTECTION_KEY: u64 = 1 << 5;
    const SHADOW_STACK: u64 = 1 << 6;
    const SGX: u64 = 1 << 15;
    if error & USER == 0 || error & (PRESENT | RESERVED | PROTECTION_KEY | SHADOW_STACK | SGX) != 0
    {
        return None;
    }
    Some(if error & INSTRUCTION != 0 {
        VM_ACCESS_EXECUTE
    } else if error & WRITE != 0 {
        VM_ACCESS_WRITE
    } else {
        VM_ACCESS_READ
    })
}

fn cancel_reserved_fault(
    reservation: ps_api::PagerFaultReservation,
    binding: mm_api::frame_capability::FrameGrantBinding,
) {
    let _ = ps_api::cancel_block_current_task();
    let _ = ps_api::cancel_pager_fault(reservation.token, ps_api::PagerFaultState::FaultPending);
    let _ =
        mm_api::frame_capability::cancel_frame_grant(reservation.zeroed_frame_capability, binding);
}

fn try_handle_current_user_page_fault(
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
    _rsp: u64,
) -> hal_api::UserFaultDisposition {
    if error_code.is_some_and(|error| error & 0x5 == 0x5)
        && FIRST_USER_PRESENT_PAGE_FAULT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "pager-user-present-page-fault",
            cr2,
            rip,
        );
    }
    let Some(access) = page_fault_access(error_code) else {
        return hal_api::UserFaultDisposition::Unhandled;
    };
    let page = cr2 & !(PAGER_PAGE_BYTES - 1);
    let Ok(vma) = ps_api::current_pager_vma_snapshot(page, access) else {
        return hal_api::UserFaultDisposition::Unhandled;
    };
    if vma.task_id == 0 || vma.region.object.object_type != VM_OBJECT_ANONYMOUS {
        return hal_api::UserFaultDisposition::Unhandled;
    }
    let Some(task_generation) = vma.task_id.checked_add(1) else {
        return hal_api::UserFaultDisposition::Unhandled;
    };
    let Some(charge) = ps_api::current_pager_charge_snapshot() else {
        return hal_api::UserFaultDisposition::Unhandled;
    };
    let Some(object_delta) = page.checked_sub(vma.region.start) else {
        return hal_api::UserFaultDisposition::Unhandled;
    };
    let Some(object_offset) = vma.region.object_offset.checked_add(object_delta) else {
        return hal_api::UserFaultDisposition::Unhandled;
    };
    let deadline_ns = hal_api::arch::clock::monotonic_nanos().saturating_add(charge.period_ns);
    if deadline_ns == 0 {
        return hal_api::UserFaultDisposition::Unhandled;
    }

    let request = PagerFaultRequestWire {
        version: PAGER_FAULT_ABI_VERSION,
        access,
        fault_flags: 0,
        reserved0: 0,
        fault_token: 0,
        process_handle: vma.region.process_handle,
        process_generation: vma.region.process_generation,
        task_id: vma.task_id,
        task_generation,
        mm_generation: vma.region.mm_generation,
        vma_generation: vma.region.vma_generation,
        virtual_address: page,
        object_offset,
        deadline_ns,
        scheduling_domain: charge.scheduling_domain,
        charge_token: charge.charge_token,
        object: vma.region.object,
        reserved1: [0; 2],
    };
    let binding_template = mm_api::frame_capability::FrameGrantBinding {
        fault_token: 0,
        process_generation: request.process_generation,
        mm_generation: request.mm_generation,
        vma_generation: request.vma_generation,
        pager_epoch: request.object.pager_epoch,
    };
    let Ok(reservation) = ps_api::reserve_pager_fault_with_dispatch_grant(
        request,
        vma.region.fault_endpoint,
        |token, request| {
            let binding = mm_api::frame_capability::FrameGrantBinding {
                fault_token: token,
                ..binding_template
            };
            mm_api::frame_capability::reserve_preallocated_zeroed_frame_grant(
                binding,
                u64::from(request.object.rights),
            )
            .map(|capability| (capability, request.object.rights))
            .map_err(|_| ps_api::PagerFaultSlotError::Pressure)
        },
    ) else {
        if FIRST_PAGER_RESERVATION_REJECTION
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-fault-reservation-rejected",
                request.task_id,
                page,
            );
        }
        return hal_api::UserFaultDisposition::Unhandled;
    };
    let binding = mm_api::frame_capability::FrameGrantBinding {
        fault_token: reservation.token,
        ..binding_template
    };
    if !ps_api::arm_block_current_task_on_pager_fault(reservation.token) {
        cancel_reserved_fault(reservation, binding);
        return hal_api::UserFaultDisposition::Unhandled;
    }
    if ps_api::commit_pager_fault_block_and_yield(reservation.token) == Some(true) {
        return hal_api::UserFaultDisposition::Resumed;
    }
    cancel_reserved_fault(reservation, binding);
    hal_api::UserFaultDisposition::Unhandled
}

fn retire_current_user_task_due_to_fault(
    vector: u8,
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
    rsp: u64,
) -> hal_api::UserFaultDisposition {
    let linux_abi =
        uses_linux_fault_policy(ps_api::current_user_snapshot().map(|snapshot| snapshot.abi()));
    let disposition = if linux_abi {
        compat_api::syscall::retire_current_linux_task_due_to_fault(
            vector, error_code, cr2, rip, rsp,
        )
    } else {
        ps_api::retire_current_user_task_due_to_fault(vector, error_code, cr2, rip, rsp)
    };
    match disposition {
        ps_api::UserFaultDisposition::Resumed => hal_api::UserFaultDisposition::Resumed,
        ps_api::UserFaultDisposition::Retired => hal_api::UserFaultDisposition::Retired,
        ps_api::UserFaultDisposition::Unhandled => hal_api::UserFaultDisposition::Unhandled,
    }
}

fn uses_linux_fault_policy(abi: Option<ps_api::UserAbi>) -> bool {
    abi == Some(ps_api::UserAbi::Linux)
}

fn current_user_snapshot() -> Option<hal_api::CurrentUserSnapshot> {
    ps_api::current_user_snapshot().map(|snapshot| hal_api::CurrentUserSnapshot {
        abi: snapshot.abi(),
        thread_id: snapshot.thread_id(),
        process_id: snapshot.process_id(),
        console_session_raw: snapshot.console_session().raw(),
    })
}

fn current_debug_user_context() -> Option<nucleus_core::debug::CurrentUserLogContext> {
    ps_api::current_user_log_ids().map(|(process_id, thread_id)| {
        nucleus_core::debug::CurrentUserLogContext {
            process_id,
            thread_id,
        }
    })
}

fn heartbeat_snapshot() -> hal_api::HeartbeatSnapshot {
    let input = crate::io_services::input_debug_snapshot();
    hal_api::HeartbeatSnapshot {
        userspace_display_active: crate::io_services::userspace_display_active(),
        input: hal_api::InputEventQueueDebugSnapshot {
            pointer_packet_submits: input.pointer_packet_submits,
            read_calls: input.read_calls,
            read_events: input.read_events,
            lock_active: input.lock_active,
            lock_last_seq: input.lock_last_seq,
            queued: input.queued,
            pending_coalesced: input.pending_coalesced,
            pending_pointer_position: input.pending_pointer_position,
            dropped_discrete: input.dropped_discrete,
            dropped_lossy: input.dropped_lossy,
        },
        linux_irq_owner_count: 0,
        linux_irq_total_depth: 0,
        linux_input_lock_active: false,
        linux_input_lock_last_seq: 0,
    }
}

pub fn register() {
    nucleus_core::debug::register_runtime_hooks(nucleus_core::debug::DebugRuntimeHooks {
        ticks: Some(hal_api::arch::rtc::ticks),
        ticks_per_second: Some(hal_api::arch::rtc::ticks_per_second),
        current_user_context: Some(current_debug_user_context),
    });
    hal_api::register_task_hooks(hal_api::TaskHooks {
        // Normal-time dispatch and reply adoption are registered in the
        // same closed lifecycle (`crate::pager::service_deferred_work`
        // runs from nucleus housekeeping), and the MM broker publishes the
        // pager VMA before an anonymous range becomes faultable, so demand
        // resolution is attempted before terminal retirement. A range that
        // no pager owns still returns `Unhandled` and falls through to the
        // unchanged retirement policy below.
        try_handle_current_user_page_fault: Some(try_handle_current_user_page_fault),
        retire_current_user_task_due_to_fault: Some(retire_current_user_task_due_to_fault),
        halt_current_retired_task: Some(ps_api::halt_current_retired_task),
        current_user_snapshot: Some(current_user_snapshot),
        is_scheduler_initialized: Some(ps_api::is_initialized),
        current_task_id: Some(ps_api::current_task_id),
        arm_block_current_task: Some(ps_api::arm_block_current_task),
        cancel_block_current_task: Some(ps_api::cancel_block_current_task),
        commit_block_current_task_and_yield: Some(ps_api::commit_block_current_task_and_yield),
        wake_user_task: Some(ps_api::wake_user_task),
        yield_now: Some(ps_api::yield_now),
    });
    hal_api::register_interrupt_hooks(hal_api::InterruptHooks {
        dispatch_pic_irq: None,
    });
    hal_api::register_heartbeat_hooks(hal_api::HeartbeatHooks {
        snapshot: Some(heartbeat_snapshot),
    });
}

#[cfg(test)]
mod tests {
    use super::{page_fault_access, uses_linux_fault_policy};
    use kernel_ps::api::UserAbi;
    use rustos_user_abi::pager::{VM_ACCESS_EXECUTE, VM_ACCESS_READ, VM_ACCESS_WRITE};

    #[test]
    fn linux_fault_policy_is_not_applied_to_windows_abi() {
        assert!(uses_linux_fault_policy(Some(UserAbi::Linux)));
        assert!(!uses_linux_fault_policy(Some(UserAbi::Windows)));
        assert!(!uses_linux_fault_policy(None));
    }

    #[test]
    fn anonymous_nonpresent_fault_access_is_exact() {
        assert_eq!(page_fault_access(Some(1 << 2)), Some(VM_ACCESS_READ));
        assert_eq!(
            page_fault_access(Some((1 << 2) | (1 << 1))),
            Some(VM_ACCESS_WRITE)
        );
        assert_eq!(
            page_fault_access(Some((1 << 2) | (1 << 1) | (1 << 4))),
            Some(VM_ACCESS_EXECUTE)
        );
    }

    #[test]
    fn protection_and_extended_x86_faults_stay_on_the_retirement_path() {
        assert_eq!(page_fault_access(None), None);
        assert_eq!(page_fault_access(Some(0)), None);
        for forbidden in [1_u64 << 0, 1 << 3, 1 << 5, 1 << 6, 1 << 15] {
            assert_eq!(page_fault_access(Some((1 << 2) | forbidden)), None);
        }
    }
}
