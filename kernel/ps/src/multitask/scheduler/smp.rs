//! Secondary-CPU idle admission and first scheduler entry.
//!
//! - **Owner:** the BSP creates one disjoint idle slot per admitted AP; the AP
//!   may enter only its own slot after the lifecycle reaches `SchedulerReady`.
//! - **Boundary:** HAL-owned bootstrap stacks remain AP idle stacks until a
//!   complete interrupt frame publishes a resumable continuation.
//! - **Lifecycle:** BSP publication precedes `SchedulerReady`; each AP proves
//!   its slot and stack before publishing `Online`.
//! - **Concurrency:** the global scheduler lock serializes idle-slot creation
//!   and each AP touches only its previously published current slot.
//! - **Failure:** identity, stack, lifecycle, or slot mismatches are fatal
//!   scheduler invariants.
//! - **Forbidden:** no shared idle slot, cross-CPU idle selection, or AP
//!   dispatch before its first complete interrupt continuation.
//! - **Evidence:** `cpu-online-lifecycle`, `scheduler-lifecycle`, and
//!   `smp-reschedule-ipi-lifecycle`.

#[cfg(rustos_ipc_phase_profile)]
use core::sync::atomic::{AtomicU64, Ordering};

use super::*;

const FAST_IPC_ELIGIBILITY_REJECTION_COUNT: usize = 10;
#[cfg(rustos_ipc_phase_profile)]
static FAST_IPC_ELIGIBILITY_REJECTIONS: [AtomicU64; FAST_IPC_ELIGIBILITY_REJECTION_COUNT] =
    [const { AtomicU64::new(0) }; FAST_IPC_ELIGIBILITY_REJECTION_COUNT];

#[repr(usize)]
#[derive(Clone, Copy)]
pub(super) enum FastIpcEligibilityRejection {
    BudgetCpu = 0,
    ContextBudget = 1,
    DomainBudget = 2,
    ForeignExecutionOwner = 3,
    RootCpu = 4,
    IdleCpu = 5,
    Affinity = 6,
    JobStopped = 7,
    ExecQuiesced = 8,
    InvalidContext = 9,
}

pub(super) fn record_fast_ipc_eligibility_rejection(reason: FastIpcEligibilityRejection) {
    #[cfg(rustos_ipc_phase_profile)]
    FAST_IPC_ELIGIBILITY_REJECTIONS[reason as usize].fetch_add(1, Ordering::Relaxed);
    #[cfg(not(rustos_ipc_phase_profile))]
    let _ = reason;
}

pub fn drain_fast_ipc_eligibility_rejections() -> [u64; FAST_IPC_ELIGIBILITY_REJECTION_COUNT] {
    #[cfg(rustos_ipc_phase_profile)]
    {
        let mut counters = [0; FAST_IPC_ELIGIBILITY_REJECTION_COUNT];
        for (index, counter) in FAST_IPC_ELIGIBILITY_REJECTIONS.iter().enumerate() {
            counters[index] = counter.swap(0, Ordering::Relaxed);
        }
        counters
    }
    #[cfg(not(rustos_ipc_phase_profile))]
    [0; FAST_IPC_ELIGIBILITY_REJECTION_COUNT]
}

fn candidate_has_foreign_execution_owner(
    candidate_slot: usize,
    current_slot: usize,
    current_cpu: usize,
    running_cpu: Option<usize>,
) -> bool {
    running_cpu.is_some_and(|owner_cpu| candidate_slot != current_slot || owner_cpu != current_cpu)
}


pub(super) const fn idle_slot_for_cpu(cpu: usize) -> Option<usize> {
    if cpu >= nucleus_core::util::lockdep::MAX_TRACKED_CPUS {
        return None;
    }
    if cpu == 0 { Some(ROOT_TASK_SLOT) } else { MAX_TASK.checked_sub(cpu) }
}

pub(super) fn remote_task_requires_quiescence(
    slot: usize,
    current_slot: usize,
    task_is_running: bool,
) -> bool {
    slot != current_slot
        && (task_is_running || super::super::current_identity::execution::count(slot) != 0)
}

#[cfg(test)]
mod tests {
    use super::{
        candidate_has_foreign_execution_owner, idle_slot_for_cpu, remote_task_requires_quiescence,
    };
    use crate::memory::paging::ProcessAddressSpace;
    use crate::multitask::{UserTaskBootstrap, noop_task_entry};
    use crate::user::abi::UserAbi;

    #[test]
    fn secondary_idle_slot_layout_is_cpu_exact_and_outside_the_root() {
        let capacity = nucleus_core::util::lockdep::MAX_TRACKED_CPUS;
        assert!(capacity <= super::MAX_TASK);
        assert_eq!(idle_slot_for_cpu(0), Some(super::ROOT_TASK_SLOT));
        for cpu in 1..capacity {
            let slot = idle_slot_for_cpu(cpu).expect("supported AP has a reserved idle slot");
            assert_eq!(slot, super::MAX_TASK - cpu);
            assert!(slot > super::ROOT_TASK_SLOT);
            for other_cpu in 1..cpu {
                assert_ne!(slot, idle_slot_for_cpu(other_cpu).unwrap());
            }
        }
        assert_eq!(idle_slot_for_cpu(capacity), None);
        assert_eq!(idle_slot_for_cpu(usize::MAX), None);
    }

    #[test]
    fn remote_or_transition_owned_task_is_not_schedulable() {
        assert!(!candidate_has_foreign_execution_owner(7, 7, 1, Some(1)));
        assert!(candidate_has_foreign_execution_owner(7, 7, 1, Some(0)));
        assert!(candidate_has_foreign_execution_owner(7, 3, 1, Some(1)));
        assert!(candidate_has_foreign_execution_owner(7, 3, 1, Some(0)));
        assert!(!candidate_has_foreign_execution_owner(7, 3, 1, None));
    }

    #[test]
    fn remote_retirement_waits_only_for_another_cpus_running_slot() {
        assert!(remote_task_requires_quiescence(7, 3, true));
        assert!(!remote_task_requires_quiescence(3, 3, true));
        assert!(!remote_task_requires_quiescence(7, 3, false));
    }

    #[test]
    fn source_migration_requires_exact_runnable_local_owner_and_target_affinity() {
        super::super::dispatcher::maintenance::tests::legacy_witnesses::source_eligibility();
    }
}
