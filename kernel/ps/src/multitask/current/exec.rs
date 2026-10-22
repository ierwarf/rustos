//! Exec coordinator: original masked CPU retries; no directory or leaf spans waits.
use super::*;
use crate::multitask::{cpu_local, scheduler::exec_transaction};

pub fn exec_current_user_process(
    address_space: ProcessAddressSpace,
    mut bootstrap: super::super::UserTaskBootstrap,
) -> Option<alloc::vec::Vec<crate::user::handles::KernelHandle>> {
    const EXEC_REMOTE_QUIESCE_TIMEOUT_NS: u64 = 2_000_000_000;
    let linux_process_state = bootstrap.linux_process_state.take()?;
    let linux_memory_map = bootstrap.linux_memory_map.take()?;
    let linux_runtime_profile = bootstrap.linux_runtime_profile.take()?;
    let exec_path = alloc::string::String::from(bootstrap.exec_path());
    let new_root = address_space.root_phys().as_u64();
    let process_handle = interrupts::without_interrupts(owned_current_process_handle)?;
    let exec_reservation = super::super::process_table::begin_exec(process_handle)?;
    // Process wait completed with caller-enabled IPIs serviceable; exact
    // identity/resource producer owns admission, not directory enumeration.
    let Some((intent, current)) =
        exec_transaction::claim_exec_candidate(None, exec_reservation, false)
    else {
        super::super::process_table::cancel_exec(exec_reservation);
        return None;
    };
    assert!(current, "self exec lost own-CPU current scope");
    let Some(census) =
        super::super::process_placement::exec_membership::ExecMembership::begin(&intent)
    else {
        cancel_intent(intent);
        return None;
    };
    // Self remains dispatchable while waiting for siblings. Its owned image
    // scope, not this reschedulable barrier, closes the self dispatch gate.
    let start = crate::arch::clock::monotonic_nanos();
    loop {
        let ready = exec_transaction::quiescence::retry(&intent, &census);
        match ready {
            Some(true) => break,
            Some(false) => {}
            None => {
                // Cancellation may synchronously drain a queued exit.
                drop(census);
                cancel_intent(intent);
                return None;
            }
        }
        if crate::arch::clock::monotonic_nanos().saturating_sub(start)
            >= EXEC_REMOTE_QUIESCE_TIMEOUT_NS
        {
            panic!(
                "scheduler invariant: exec timed out quiescing remote process threads handle={process_handle:?}"
            );
        }
        // The ACK comes from tasks on this or another CPU; spinning on an
        // already-requested reschedule would hold a single CPU for a quantum.
        super::super::yield_now();
    }
    complete_retirement_side_effects();
    if !super::super::process_table::authorize_exec(exec_reservation) {
        // Cancellation may synchronously drain a queued exit.
        drop(census);
        cancel_intent(intent);
        return None;
    }
    let finalized = exec_transaction::with_census_image(
        &intent,
        &census,
        exec_reservation,
        new_root,
        true,
        |lease| {
            let staged = super::super::process_table::stage_exec_state(
                exec_reservation,
                address_space,
                linux_process_state,
                linux_memory_map,
                linux_runtime_profile,
                exec_path.as_str(),
            )?;
            census.mark_staged();
            lease.bind_staged_mm(&staged);
            let published = lease.commit_image(&mut bootstrap, None)
                .expect("authorized self exec lost its reserved scheduler target");
            Some(super::super::process_table::finalize_exec_state(
                staged, published,
            ))
        },
    );
    let Some(Some(finalized)) = finalized else {
        // Cancellation may synchronously drain a queued exit.
        drop(census);
        cancel_intent(intent);
        return None;
    };

    census.finish(&finalized);
    let (process_id, exit_pending, closed, old_state) = finalized.into_parts();
    old_state.retire_execution_state();
    kernel_mm::api::address_space::drain_retired_address_spaces(16);
    if exit_pending {
        assert!(
            terminate_user_process(process_id),
            "exec-finalized pending exit lost scheduler ownership"
        );
        super::super::request_user_return_reschedule();
    }
    complete_retirement_side_effects();
    Some(closed)
}

/// Why a target exec was refused before any image effect. Reported to the
/// committing service so a failed `execve` names the stage instead of a bare
/// "no such process".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecRefusal {
    /// The bootstrap lacks Linux process state, memory map or runtime profile.
    IncompleteBootstrap,
    /// No live Linux main thread matches the pid/tid pair.
    NoCandidate,
    /// The process refused a new exec reservation (exec or exit in progress).
    Reservation,
    /// The scheduler refused to elect the target as an exec producer.
    Claim,
    /// Another membership transaction (exit, job control) owns the process.
    Membership,
    /// Sibling quiescence was cancelled by a concurrent exit.
    Quiescence,
    /// The process table withdrew authorization (a pending exit won).
    Authorization,
    /// The image transaction refused; carries the scheduler stage code.
    Image(u8),
    /// The old address space still has an activation grant in flight.
    SealBusy,
    /// Staging refused for another reason (pin, process gone, transfer).
    Stage,
}

impl ExecRefusal {
    /// Stable numeric code for telemetry: stage in the low byte, detail above.
    pub fn code(self) -> u64 {
        match self {
            Self::IncompleteBootstrap => 1,
            Self::NoCandidate => 2,
            Self::Reservation => 3,
            Self::Claim => 4,
            Self::Membership => 5,
            Self::Quiescence => 6,
            Self::Authorization => 7,
            Self::Image(detail) => 8 | (u64::from(detail) << 8),
            Self::SealBusy => 9,
            Self::Stage => 10,
        }
    }
}

pub fn exec_user_process_by_pid(
    process_id: u64,
    thread_id: u64,
    address_space: ProcessAddressSpace,
    bootstrap: super::super::UserTaskBootstrap,
) -> Option<alloc::vec::Vec<crate::user::handles::KernelHandle>> {
    exec_user_process_by_pid_checked(process_id, thread_id, address_space, bootstrap).ok()
}

pub fn exec_user_process_by_pid_checked(
    process_id: u64,
    thread_id: u64,
    address_space: ProcessAddressSpace,
    mut bootstrap: super::super::UserTaskBootstrap,
) -> Result<alloc::vec::Vec<crate::user::handles::KernelHandle>, ExecRefusal> {
    const EXEC_REMOTE_QUIESCE_TIMEOUT_NS: u64 = 2_000_000_000;
    let (Some(linux_process_state), Some(linux_memory_map), Some(linux_runtime_profile)) = (
        bootstrap.linux_process_state.take(),
        bootstrap.linux_memory_map.take(),
        bootstrap.linux_runtime_profile.take(),
    ) else {
        return Err(ExecRefusal::IncompleteBootstrap);
    };
    let exec_path = alloc::string::String::from(bootstrap.exec_path());
    let new_root = address_space.root_phys().as_u64();
    let (slot, process_handle) = exec_transaction::peer_exec_candidate(process_id, thread_id)
        .ok_or(ExecRefusal::NoCandidate)?;
    let exec_reservation = super::super::process_table::begin_exec(process_handle)
        .ok_or(ExecRefusal::Reservation)?;
    let Some((intent, current)) = exec_transaction::claim_exec_candidate(
        Some((slot, process_id, thread_id)),
        exec_reservation,
        true,
    ) else {
        super::super::process_table::cancel_exec(exec_reservation);
        return Err(ExecRefusal::Claim);
    };
    let Some(census) =
        super::super::process_placement::exec_membership::ExecMembership::begin(&intent)
    else {
        cancel_intent(intent);
        return Err(ExecRefusal::Membership);
    };
    let start = crate::arch::clock::monotonic_nanos();
    loop {
        let ready = exec_transaction::quiescence::retry(&intent, &census);
        match ready {
            Some(true) => break,
            Some(false) => {}
            None => {
                // Cancellation may synchronously drain a queued exit.
                drop(census);
                cancel_intent(intent);
                return Err(ExecRefusal::Quiescence);
            }
        }
        if crate::arch::clock::monotonic_nanos().saturating_sub(start)
            >= EXEC_REMOTE_QUIESCE_TIMEOUT_NS
        {
            panic!(
                "scheduler invariant: target exec timed out quiescing remote threads process={process_id} thread={thread_id}"
            );
        }
        // The ACK comes from tasks on this or another CPU; spinning on an
        // already-requested reschedule would hold a single CPU for a quantum.
        super::super::yield_now();
    }
    complete_retirement_side_effects();
    if !super::super::process_table::authorize_exec(exec_reservation) {
        // Cancellation may synchronously drain a queued exit.
        drop(census);
        cancel_intent(intent);
        return Err(ExecRefusal::Authorization);
    }
    // The stopped target can still be the outgoing side of its CPU's last
    // switch: a resident IPC lane hands the CPU to the peer (this committer)
    // without a scheduler turn, so the transition stays published. A yield is
    // a scheduler entry on that CPU and completes it; the image transaction
    // refuses a target that still has an execution owner.
    exec_transaction::evict_parked_target(slot);
    let owner_wait_start = crate::arch::clock::monotonic_nanos();
    while (cpu_local::task_execution_owner(slot).is_some()
        || exec_transaction::image_bank_busy(slot))
        && crate::arch::clock::monotonic_nanos().saturating_sub(owner_wait_start)
            < EXEC_REMOTE_QUIESCE_TIMEOUT_NS
    {
        super::super::yield_now();
    }
    let _ = exec_transaction::take_image_refusal();
    let mut stage_refusal = None;
    let finalized = exec_transaction::with_census_image(
        &intent,
        &census,
        exec_reservation,
        new_root,
        current,
        |lease| {
            let staged = match super::super::process_table::stage_exec_state_checked(
                exec_reservation,
                address_space,
                linux_process_state,
                linux_memory_map,
                linux_runtime_profile,
                exec_path.as_str(),
            ) {
                Ok(staged) => staged,
                Err(refusal) => {
                    stage_refusal = Some(refusal);
                    return None;
                }
            };
            census.mark_staged();
            lease.bind_staged_mm(&staged);
            let published = lease.commit_image(&mut bootstrap, Some((process_id, thread_id)))
                .expect("authorized target exec lost its reserved scheduler slot");
            Some(super::super::process_table::finalize_exec_state(
                staged, published,
            ))
        },
    );
    let Some(Some(finalized)) = finalized else {
        // Cancellation may synchronously drain a queued exit.
        drop(census);
        cancel_intent(intent);
        return Err(match stage_refusal {
            Some(super::super::process_table::StageRefusal::SealBusy) => ExecRefusal::SealBusy,
            Some(_) => ExecRefusal::Stage,
            None => ExecRefusal::Image(exec_transaction::take_image_refusal()),
        });
    };

    census.finish(&finalized);
    let (finalized_pid, exit_pending, closed, old_state) = finalized.into_parts();
    old_state.retire_execution_state();
    kernel_mm::api::address_space::drain_retired_address_spaces(16);
    if exit_pending {
        assert_eq!(finalized_pid, process_id);
        assert!(
            terminate_user_process(process_id),
            "target exec-finalized pending exit lost scheduler ownership"
        );
    }
    complete_retirement_side_effects();
    Ok(closed)
}

fn cancel_intent(intent: super::super::current_identity::exec_intent::Intent) {
    let preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    let current = cpu_local::current_cpu_task_slot() == Some(intent.slot());
    let (_, process_id, terminal) =
        intent.cancel_with(preemption, super::super::process_table::cancel_exec_owned);
    if terminal {
        assert!(
            terminate_user_process(process_id),
            "exec cancellation pending exit lost scheduler ownership"
        );
        if current {
            super::super::request_user_return_reschedule();
        }
        complete_retirement_side_effects();
    }
}
