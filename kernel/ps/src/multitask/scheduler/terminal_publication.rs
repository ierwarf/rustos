//! Owner: original terminal producer through election, side effects and final publication.
//! Boundary: exact terminal seal, retained physical context and original process turn.
//! Lifecycle: reserve/claim -> borrower drain -> withdraw -> effects -> both consumers.
//! Concurrency: leaves release before queues/signals/donation; seal excludes fresh wake loans.
//! Failure: busy election/foreign execution defer SAME producer; consumer ACK is external.
//! Forbidden: descriptor absence, copied slot receipts, or deferred emptiness as reuse grants.
//! Evidence: actual terminal/process/cleanup owners and original masked caller custody.
use super::*;
use crate::multitask::{cpu_local, current_identity as identity};
use retirement_cleanup::producer::Producer;

fn quiescent(slot: usize) -> bool {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    let own_current = cpu_local::task_execution_owner(slot)
        == Some(cpu_local::TaskExecutionOwner::Current(cpu))
        && stack_resources::cpu_leases::owns_current(cpu, slot);
    if cpu_local::task_execution_owner(slot).is_some() && !own_current {
        return false;
    }
    // Only the original own-CPU Current permit may survive final effects.
    // Native physical bank fixtures model Current without a permit; zero is
    // therefore their explicit input, not a fabricated executing grant.
    let current_ref = u32::from(own_current && identity::execution::banks::owns_current(cpu, slot));
    identity::execution::count(slot) == current_ref
}

fn defer(owner: Producer) {
    let target = cpu_local::task_execution_owner(owner.slot()).map(|role| match role {
        cpu_local::TaskExecutionOwner::Current(cpu)
        | cpu_local::TaskExecutionOwner::Transition(cpu) => cpu,
    });
    terminal_quarantine::publish(owner);
    #[cfg(not(test))]
    if let Some(cpu) = target {
        crate::multitask::irq::request_target_reschedule(cpu);
    }
    #[cfg(test)]
    let _ = target;
}

/// # Safety
/// Caller retains original same-CPU masked IRQ/preemption through all effects.
/// Original terminal seal plus drained borrowers retain source lifetime;
/// original process census fences physical member reuse through publication.
/// Recipient wake loans/readers, not a global serializer, own recipient access.
/// Own Current is permitted only with its original physical bank; foreign
/// execution and every extra binding/Cohort borrower must drain first.
pub(super) unsafe fn guarded(mut owner: Producer, eligible_cpus: &dyn Fn(usize) -> u64) {
    let slot = owner.slot();
    let id = owner
        .task_id()
        .expect("terminal producer lacks original UID");
    assert_ne!(
        slot, ROOT_TASK_SLOT,
        "scheduler root kernel task cannot be retired"
    );
    assert!(
        stack_resources::terminal_binding_matches(slot, Some(id)),
        "terminal producer lost original resource custody"
    );
    assert_eq!(
        stack_resources::task_start::read(slot).map(|s| s.id),
        Some(id),
        "terminal producer lost slot binding before effects"
    );
    assert_eq!(
        nucleus_core::util::lockdep::irq_context_depth(),
        0,
        "task retirement finalization must run outside hardware interrupt context slot={} reason={:?}",
        slot,
        owner.reason()
    );
    // Close ALL fresh identity/CPU/Cohort admission before checking the old
    // borrower count. A Terminal50 seal alone only excludes terminal/reset,
    // kernel Executor and wake captures, not every independent constructor.
    owner = terminal_quarantine::retain_for_finalization(owner);
    if !quiescent(slot) {
        defer(owner);
        return;
    }
    let context =
        stack_resources::context::read(slot).expect("terminal producer lost retained context");
    let mut process_turn = if context.user_mode {
        let handle = context
            .process_handle
            .expect("user terminal election lacks process handle");
        let Some(turn) =
            crate::multitask::process_placement::terminal::Turn::begin(handle, (slot, id))
        else {
            defer(owner);
            return;
        };
        owner.bind_process_turn(turn.credential());
        Some(turn)
    } else {
        None
    };
    let rust_stack_pointer: usize;
    // SAFETY: scalar register read only, no memory or stack modification.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rust_stack_pointer,
        options(nomem, nostack, preserves_flags));
    }
    if rust_stack_pointer & 0xF != 0 {
        match owner.reason() {
            TaskRetireReason::UserFault { vector, rip, .. } => panic!(
                "user-fault retirement entered Rust with a misaligned call stack: vector={} rip={:#x}",
                vector, rip
            ),
            TaskRetireReason::Terminated { .. } => {
                panic!("termination retirement entered Rust with a misaligned call stack")
            }
            TaskRetireReason::CorruptedContext { .. } => {
                panic!("corrupted-context retirement entered Rust with a misaligned call stack")
            }
            TaskRetireReason::Exited => {
                panic!("task-exit retirement entered Rust with a misaligned call stack")
            }
        }
    }
    let process_terminal = process_turn.as_ref().is_some_and(|turn| turn.last());
    let linux = linux_thread_state::terminal_snapshot(&owner);
    let cleanup = context
        .user_mode
        .then(|| crate::multitask::RetiredTaskCleanup {
            task_id: id,
            process_id: context
                .process_id
                .expect("live user task retirement requires a process identity"),
            process_terminal,
            user_fault_vector: match owner.reason() {
                TaskRetireReason::UserFault { vector, .. } => Some(vector),
                _ => None,
            },
            clear_child_tid: linux.map(|state| state.clear_child_tid).unwrap_or(0),
            robust_list_head: linux.map(|state| state.robust_list_head).unwrap_or(0),
            robust_list_len: linux.map(|state| state.robust_list_len).unwrap_or(0),
        });
    owner.begin_effects();
    runqueue::retire(slot, runqueue::weight::value(slot));
    assert!(
        identity::update_live_lifecycle(slot, id, identity::LifecycleGate::Retired, true),
        "terminal producer lost exact lifecycle authority"
    );
    if let Some(turn) = process_turn.as_mut() {
        turn.mark_stopped();
    }
    // SAFETY: fresh source wake loans excluded by original seal; all old source
    // borrowers drained, and original masked CPU caller custody still retained.
    unsafe {
        linux_thread_state::transfer_terminal_sigchld(&owner, eligible_cpus);
    }
    donation_ledger::release_task(id);
    runqueue::wait::set_ready_since_ticks(slot, 0);
    runqueue::wait::set_blocked(slot, false);
    runqueue::wait::set_blocked_since_ticks(slot, 0);
    runqueue::wait::set_armed(slot, false);
    runqueue::wait::set_reason(slot, 0, 0);
    let terminal_pid = context.process_id.filter(|_| process_terminal);
    if terminal_pid.is_some() {
        let census = process_turn.as_ref().expect("process terminal lost original census");
        for &(_, member_id) in census.final_donation_members() {
            donation_ledger::release_task(member_id);
        }
    }
    let mut effects = reclaim::RetirementSideEffect::new(Some(id), terminal_pid);
    if owner.detach() {
        if let Some(handle) = context.process_handle {
            // Capture original detach payload BEFORE losing descriptor metadata.
            effects.defer_process_detach(handle);
            stack_resources::context::detach_process(slot, id, handle)
                .expect("exec sibling lost exact context binding");
            let mut metadata =
                identity::read(slot).expect("terminal detach lost original identity");
            assert_eq!(metadata.task_id, Some(id));
            metadata.process_handle = None;
            identity::publish_metadata(slot, Some(metadata));
        }
    }
    // Both consumers are visible only after every effect and detach payload.
    let publication = owner.publish(cleanup, effects);
    if let Some(turn) = process_turn {
        turn.finish(publication);
    }
}

/// # Safety
/// Same original masked IRQ/preemption custody as `guarded`.
pub(super) unsafe fn drain_guarded(eligible_cpus: &dyn Fn(usize) -> u64) {
    assert_eq!(
        nucleus_core::util::lockdep::irq_context_depth(),
        0,
        "deferred task retirement drained from interrupt context"
    );
    for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
        let id = stack_resources::task_start::read(slot).map(|s| s.id);
        let Some(owner) = terminal_quarantine::claim(slot, id) else {
            continue;
        };
        assert!(
            identity::lifecycle(slot).retired || owner.waited_for_executor(),
            "deferred retirement lost its terminal lifecycle or kernel resume loan"
        );
        // SAFETY: same caller custody and same original claimed producer.
        unsafe {
            guarded(owner, eligible_cpus);
        }
    }
}

#[cfg(test)]
pub(super) mod tests;
