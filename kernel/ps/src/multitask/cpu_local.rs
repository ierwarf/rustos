//! CPU-local scheduler ownership publication.
//!
//! - **Owner:** independent dispatch turns own every dense CPU's Current and
//!   Transition publication; no global scheduler lock exists.
//! - **Boundary:** a private CPU-local dispatch scratch slot is loaded from
//!   and returned to the invoking CPU's published current slot.  The
//!   `Scheduler` catalog never stores a shared mutable current-task field.
//! - **Lifecycle:** BSP bootstrap ownership precedes AP idle publication. A
//!   later handoff reserves the incoming slot while retaining the outgoing
//!   stack, then assembly releases the outgoing owner only after switching
//!   `rsp` to the selected frame.
//! - **Concurrency:** CPU-local reentry exclusion, original claims and physical
//!   banks retain switch custody; scalar slots are remote observations only.
//! - **Failure:** duplicate publication, overlapping transition, inactive AP
//!   entry, or one task on two CPUs is an immediate scheduler panic.
//! - **Forbidden:** remote lifetime code must not infer ownership from the
//!   scheduler scratch field, release an outgoing stack before the assembly
//!   commit boundary, or infer authority from inactive slot values.
//! - **Evidence:** `cpu-online-lifecycle`, `scheduler-lifecycle`, and
//!   `scheduler-cpu-ownership`.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nucleus_core::util::lockdep::MAX_TRACKED_CPUS;

use super::MAX_SCHEDULER_TASKS;
// ORDERING: This is a one-way scheduler publication. Interrupt leaves need
// only know whether the fully initialized scheduler may be entered; taking the
// global lock merely to read its root slot doubled every SMP timer/IPI
// transaction.
static SCHEDULER_INITIALIZED: AtomicBool = AtomicBool::new(false);
static CURRENT_TASK_SLOTS: [AtomicUsize; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicUsize::new(0) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static CURRENT_TASK_ACTIVE: [AtomicBool; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static CURRENT_TASK_IDLE: [AtomicBool; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
// This is private working state for one CPU's dispatch turn.  Unlike
// `CURRENT_TASK_SLOTS`, it is never remote lifetime or execution authority:
// remote readers use the release-published current/transition pair below.
// Keeping it out of `Scheduler` removes the last mutable current-task scratch
// word from the lifecycle catalog without publishing an incoming task before
// the assembly RSP switch commits.
static SCHEDULER_CURRENT_TASK_SCRATCH: [AtomicUsize;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicUsize::new(0) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static TRANSITION_FROM_SLOTS: [AtomicUsize; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicUsize::new(0) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static TRANSITION_ACTIVE: [AtomicBool; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];

#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(super) use test_support::{
    TestCpuPublicationRestore, install_test_current_owner, install_test_transition_owner,
    with_test_current_scalar, with_initialized_fixture,
    test_publication_lock, with_test_activation_return, with_test_rejected_activation_return,
};

/// Compatibility telemetry for the deleted global lock. There are no global
/// scheduler acquisitions in any profile; CPU-local owners report separately.
pub(super) fn take_acquire_site_census() -> ([(&'static str, u32, u64); 64], u64) {
    ([("", 0, 0); 64], 0)
}

pub(super) fn scheduler_initialized() -> bool {
    // ORDERING: Acquire observes reset, every AP idle context, and BSP current
    // execution preparation published before the one-way Release below.
    SCHEDULER_INITIALIZED.load(Ordering::Acquire)
}

pub(super) fn publish_scheduler_initialized(image: &super::scheduler::bootstrap::ReadyImage<'_>) {
    image.assert_complete();
    // ORDERING: Release publishes the fully initialized image exactly once.
    // The scheduler has no supported teardown; duplicate initialization is a
    // lifecycle violation rather than a state transition that can be retried.
    // ORDERING: Acquire on failure distinguishes the duplicate publisher.
    assert!(
        SCHEDULER_INITIALIZED
            .compare_exchange(false, true, Ordering::Release, Ordering::Acquire)
            .is_ok(),
        "scheduler invariant: initialized lifecycle published twice"
    );
}

pub(super) mod dispatch_turn;
pub(super) mod irq_entry;

/// The publication edge immediately before a scheduler returns to assembly.
///
/// It deliberately owns no lock and changes no scheduling policy. The
/// assembly commit still clears the outgoing transition only after installing
/// the incoming RSP. Keeping that edge reusable is the prerequisite for a
/// future local dispatcher to preserve the same stack-lifetime protocol.
pub(super) struct CpuSwitchPublication {
    cpu: usize,
    from: usize,
    to: usize,
    to_idle: bool,
}

impl CpuSwitchPublication {
    /// Constructs the sole pre-return CPU-switch publication sequence.
    /// The original CPU-local turn uses this constructor instead of copying
    /// its Release stores.
    fn new(cpu: usize, from: usize, to: usize, to_idle: bool) -> Self {
        assert!(
            cpu < MAX_TRACKED_CPUS,
            "scheduler CPU exceeds publication capacity"
        );
        assert!(
            from < MAX_SCHEDULER_TASKS,
            "outgoing scheduler slot exceeds capacity"
        );
        assert!(
            to < MAX_SCHEDULER_TASKS,
            "incoming scheduler slot exceeds capacity"
        );
        Self {
            cpu,
            from,
            to,
            to_idle,
        }
    }

    fn publish_before_return(self) {
        super::current_identity::execution::banks::publish(self.cpu, self.from, self.to);
        // Allocation custody crosses this edge before publication can expose
        // the outgoing task's absence. No leaf lock enters assembly.
        super::scheduler::stack_resources::cpu_leases::publish(self.cpu, self.from, self.to);
        self.publish_scalars();
    }

    /// Integer-only protocol projection, explicitly selected by its fixture;
    /// real native resource turns execute publish_before_return unchanged.
    #[cfg(test)]
    fn publish_phase_model(self) {
        super::current_identity::execution::banks::publish(self.cpu, self.from, self.to);
        self.publish_scalars();
    }

    fn publish_scalars(self) {
        if self.to != self.from {
            // ORDERING: Publish the outgoing slot before activating the
            // transition. Remote lifetime and selection paths must retain its
            // stack until assembly has installed the incoming `rsp`.
            TRANSITION_FROM_SLOTS[self.cpu].store(self.from, Ordering::Release);
            // ORDERING: Release activates the transition only after the
            // outgoing slot is published.
            TRANSITION_ACTIVE[self.cpu].store(true, Ordering::Release);
        }
        // ORDERING: Release publishes the exact task slot selected for this
        // CPU. A changed handoff already published the outgoing transition,
        // so both stacks remain owned until the assembly commit callback.
        CURRENT_TASK_SLOTS[self.cpu].store(self.to, Ordering::Release);
        // ORDERING: Release publishes the selected slot's scheduler-derived
        // Idle class after the slot itself. Periodic interrupt leaves may use
        // this one bit only to retain an idle continuation when every local
        // and foreign runqueue publication is empty.
        CURRENT_TASK_IDLE[self.cpu].store(self.to_idle, Ordering::Release);
    }
}

pub(super) fn assert_initial_image_absent() {
    assert!(
        CURRENT_TASK_ACTIVE
            .iter()
            // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
            .all(|active| !active.load(Ordering::Acquire)),
        "bootstrap overlaps published Current"
    );
}

/// Exact image scope qualifies this turn; no directory is acquired or renamed.
pub(super) fn with_bootstrap_turn<R>(
    preemption: nucleus_core::util::lockdep::DispatchPreemption<'static>,
    scope: &super::scheduler::bootstrap::HardwareScope<'_>,
    idle: bool,
    install: impl FnOnce(&dispatch_turn::CpuDispatchTurn<'static>) -> R,
) -> R {
    let cpu = preemption.cpu();
    scope.assert_current(cpu);
    let slot = scope.slot();
    let mut turn = dispatch_turn::begin(preemption, slot);
    turn.mask_irqs();
    let result = install(&turn);
    scope.assert_current(cpu);
    turn.publish_return(slot, idle).after_directory_unlock();
    result
}

/// Exact exec image owns this unchanged-Current control return. No directory,
/// next-task scratch or fresh selection authority participates.
pub(super) fn with_exec_image_turn<R>(
    preemption: nucleus_core::util::lockdep::DispatchPreemption<'static>,
    image: &super::scheduler::exec_transaction::ExecImageLease<'_>,
    install: impl FnOnce(&nucleus_core::util::lockdep::DispatchPreemption<'static>) -> R,
) -> Option<R> {
    let cpu = preemption.cpu();
    let slot = image.cpu_actor(cpu)?;
    let mut turn = dispatch_turn::begin(preemption, slot);
    turn.mask_irqs();
    let result = install(turn.preemption());
    let idle = image.finish_cpu_actor(cpu, slot);
    let completion = turn.publish_return(slot, idle);
    super::irq::flush_deferred_target_reschedules();
    completion.after_directory_unlock();
    Some(result)
}

pub(super) fn publish_cpu_current_task(image: &super::scheduler::bootstrap::InitialCpu<'_>) {
    image.assert_publication();
    let logical_index = image.cpu();
    let slot = image.slot();
    assert!(
        logical_index < CURRENT_TASK_SLOTS.len() && slot < MAX_SCHEDULER_TASKS,
        "scheduler invariant: invalid per-CPU current-task publication"
    );
    // ORDERING: Acquire rejects duplicate publication of an active CPU slot.
    assert!(
        !CURRENT_TASK_ACTIVE[logical_index].load(Ordering::Acquire),
        "scheduler invariant: CPU current-task slot published twice"
    );
    assert!(
        !slot_has_remote_owner(logical_index, slot),
        "scheduler invariant: initial task already runs on another CPU"
    );
    #[cfg(not(test))]
    {
        super::scheduler::stack_resources::cpu_leases::initialize(image);
    }
    // ORDERING: Release makes the initialized idle context visible before the
    // AP is allowed to enter scheduler code.
    CURRENT_TASK_SLOTS[logical_index].store(slot, Ordering::Release);
    // The private scratch starts from the same initial context, but it is not
    // an execution-owner publication.  The Release store above remains the
    // only initial current-task authority observed by another CPU.
    set_scheduler_current_task_scratch_for_cpu(logical_index, slot);
    // ORDERING: this Release follows the exact slot publication above. AP
    // initial publications are private idle tasks; BSP slot zero is still
    // bootstrap work here and becomes Idle only after `mark_root_idle`.
    CURRENT_TASK_IDLE[logical_index].store(logical_index != 0, Ordering::Release);
    // ORDERING: Release publishes ownership only after the exact task slot.
    CURRENT_TASK_ACTIVE[logical_index].store(true, Ordering::Release);
}

/// Returns the caller CPU's private scheduler-turn scratch slot.
///
/// This word is protected by the caller's scheduler dispatch exclusion and is
/// deliberately not a substitute for `CURRENT_TASK_SLOTS`: only the latter,
/// together with `TRANSITION_*`, grants remote execution/lifetime authority.
#[inline]
pub(super) fn scheduler_current_task_scratch() -> usize {
    let logical_index = scheduler_turn_logical_index();
    scheduler_current_task_scratch_for_cpu(logical_index)
}

/// Replaces the caller CPU's private scheduler-turn scratch slot.
///
/// Callers hold the exact CPU's scheduler dispatch exclusion.  Publication to
/// remote observers remains deferred to the original CPU turn's return, after
/// the outgoing stack-transition owner is installed.
#[inline]
pub(super) fn set_scheduler_current_task_scratch(slot: usize) {
    let logical_index = scheduler_turn_logical_index();
    set_scheduler_current_task_scratch_for_cpu(logical_index, slot);
}

/// Exact private scratch authority belongs to this CPU's unique local turn.
/// The global owner CPU is diagnostic publication only; a foreign CPU can no
/// longer derive scratch permission from whichever directory writer holds it.
#[inline]
pub(super) fn scheduler_turn_logical_index() -> usize {
    dispatch_turn::current_cpu()
}

pub(in crate::multitask) fn has_placement(cpu: usize, slot: usize) -> bool {
    dispatch_turn::has_placement(cpu, slot)
}

pub(in crate::multitask) fn current_placement_scope(
    cpu: usize,
    slot: usize,
) -> Option<Option<super::scheduler::PlacementGrant>> {
    dispatch_turn::current_placement_scope(cpu, slot)
}
pub(in crate::multitask) fn retain_placement(cpu: usize, grant: super::scheduler::PlacementGrant) {
    dispatch_turn::retain_placement(cpu, grant);
}

pub(super) fn can_retain_mm_activation(
    cpu: usize,
    incoming: &super::scheduler::stack_resources::activation::ActivationCredential,
) -> bool {
    dispatch_turn::can_retain_activation(cpu, incoming)
}

pub(super) fn can_retain_mm_identity(
    cpu: usize,
    slot: usize,
    task_id: u64,
    mm: &super::process_table::MmExecutionPin,
) -> bool {
    dispatch_turn::can_retain_identity(cpu, slot, task_id, mm)
}
pub(super) fn retain_mm_activation(
    cpu: usize,
    incoming: super::scheduler::stack_resources::activation::ActivationCredential,
) {
    dispatch_turn::retain_activation(cpu, incoming);
}

#[inline]
fn scheduler_current_task_scratch_for_cpu(logical_index: usize) -> usize {
    assert!(
        logical_index < SCHEDULER_CURRENT_TASK_SCRATCH.len(),
        "scheduler invariant: CPU-local dispatch scratch CPU exceeds capacity"
    );
    // ORDERING: this is same-CPU scratch behind the raw scheduler exclusion;
    // a remote reader must instead acquire the release-published owner state.
    SCHEDULER_CURRENT_TASK_SCRATCH[logical_index].load(Ordering::Relaxed)
}

#[inline]
pub(in crate::multitask) fn set_scheduler_current_task_scratch_for_cpu(logical_index: usize, slot: usize) {
    assert!(
        logical_index < SCHEDULER_CURRENT_TASK_SCRATCH.len() && slot < MAX_SCHEDULER_TASKS,
        "scheduler invariant: invalid CPU-local dispatch scratch publication"
    );
    // ORDERING: see `scheduler_current_task_scratch_for_cpu`; this local
    // working value is never a cross-CPU ownership publication.
    SCHEDULER_CURRENT_TASK_SCRATCH[logical_index].store(slot, Ordering::Relaxed);
}

pub(super) fn current_cpu_task_is_idle(logical_index: usize) -> bool {
    if logical_index >= CURRENT_TASK_IDLE.len() {
        return false;
    }
    // ORDERING: Acquire observes the exact current-slot publication that
    // precedes this derived class bit.
    CURRENT_TASK_IDLE[logical_index].load(Ordering::Acquire)
}

/// The slot the asking CPU is currently running, or `None` when the slot has
/// not been admitted yet. Callers must already have interrupts masked, which
/// is what keeps the answer from changing underneath them.
///
/// The logical index is derived once and threaded into the admission test.
/// Deriving it twice was not merely redundant: the admission and the slot read
/// would answer for two separately derived CPUs, so the saving is incidental to
/// making the pair name one CPU by construction.
pub(super) fn current_cpu_task_slot() -> Option<usize> {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    if !cpu_task_slot_admitted(logical_index) {
        return None;
    }
    let slot = CURRENT_TASK_SLOTS.get(logical_index)?;
    // ORDERING: Acquire pairs with the current-slot publication in dispatch
    // and in `publish_cpu_current_task`.
    Some(slot.load(Ordering::Acquire))
}

/// Reports whether the calling CPU may safely enter current-task snapshot
/// paths. Early AP boot and panic/log formatting use this as a non-blocking
/// authority check before touching the scheduler lock.
pub(super) fn current_cpu_task_slot_admitted() -> bool {
    cpu_task_slot_admitted(nucleus_core::util::lockdep::current_cpu_index())
}

/// The admission test above, against an already-derived logical index.
#[inline]
fn cpu_task_slot_admitted(logical_index: usize) -> bool {
    logical_index < CURRENT_TASK_ACTIVE.len()
        // ORDERING: Acquire pairs with initial current-slot publication and
        // prevents a snapshot from observing an uninitialized slot value.
        && CURRENT_TASK_ACTIVE[logical_index].load(Ordering::Acquire)
        // ORDERING: A current-task observation cannot be attributed safely
        // until assembly has release-cleared the outgoing stack transition.
        && !TRANSITION_ACTIVE[logical_index].load(Ordering::Acquire)
}

/// The slot `cpu` has published as current, if it has admitted one.
///
/// Unlike `current_cpu_task_slot`, this answers for any CPU rather than the
/// caller's, and it deliberately does not exclude an active stack transition:
/// the outgoing task of a transition is still executing as far as ownership is
/// concerned, which is exactly the window the runnable bit has to stay correct
/// across.
pub(super) fn published_current_slot(cpu: usize) -> Option<usize> {
    // ORDERING: Acquire pairs with initial current-slot publication.
    if !CURRENT_TASK_ACTIVE.get(cpu)?.load(Ordering::Acquire) {
        return None;
    }
    // ORDERING: Acquire pairs with the current-slot publication in dispatch.
    Some(CURRENT_TASK_SLOTS.get(cpu)?.load(Ordering::Acquire))
}

/// Releases the outgoing stack only after the interrupt stub has changed
/// `rsp` to the incoming saved frame. This callback must remain allocation-
/// free, lock-free, and callable with interrupts disabled.
pub(super) extern "C" fn commit_context_switch() {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    assert!(
        logical_index < TRANSITION_ACTIVE.len(),
        "scheduler invariant: stack-handoff commit CPU exceeds capacity"
    );
    let Some(commit) = dispatch_turn::begin_commit(logical_index) else {
        return;
    };
    // ORDERING: Release makes the completed architectural stack switch happen
    // before remote scheduler/lifetime readers may observe transition clear.
    TRANSITION_ACTIVE[logical_index].store(false, Ordering::Release);
    if commit.physical_banks() {
        super::scheduler::stack_resources::cpu_leases::commit(logical_index);
    }
    super::current_identity::execution::banks::commit(logical_index);
    // Keep local reentry closed until the outgoing allocation has also been
    // released on the incoming RSP. A duplicate cannot spend a producer turn.
    commit.finish();
}

pub(super) fn task_slot_is_running(slot: usize) -> bool {
    task_execution_owner(slot).is_some()
}

pub(super) fn task_running_cpu(slot: usize) -> Option<usize> {
    task_execution_owner(slot).map(TaskExecutionOwner::cpu)
}

/// Identifies why a task slot is still owned by a CPU.
///
/// `Transition` is deliberately distinct from `Current`: the outgoing task
/// has already published a reusable saved frame, but its old kernel stack is
/// still live until the assembly switch release-clears `TRANSITION_ACTIVE`.
/// A concurrent wake must make that task runnable for the next dispatch; it
/// must not apply the consumed-frame rule used for a currently executing task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TaskExecutionOwner {
    Current(usize),
    Transition(usize),
}

impl TaskExecutionOwner {
    const fn cpu(self) -> usize {
        match self {
            Self::Current(cpu) | Self::Transition(cpu) => cpu,
        }
    }
}

pub(super) fn task_execution_owner(slot: usize) -> Option<TaskExecutionOwner> {
    task_execution_owner_in(
        &CURRENT_TASK_SLOTS,
        &CURRENT_TASK_ACTIVE,
        &TRANSITION_FROM_SLOTS,
        &TRANSITION_ACTIVE,
        slot,
    )
}

fn slot_has_remote_owner(logical_index: usize, slot: usize) -> bool {
    slot_has_owner_in(
        &CURRENT_TASK_SLOTS,
        &CURRENT_TASK_ACTIVE,
        &TRANSITION_FROM_SLOTS,
        &TRANSITION_ACTIVE,
        logical_index,
        slot,
    )
}

fn slot_has_owner_in(
    slots: &[AtomicUsize],
    active: &[AtomicBool],
    transition_slots: &[AtomicUsize],
    transition_active: &[AtomicBool],
    excluded_cpu: usize,
    slot: usize,
) -> bool {
    assert_eq!(slots.len(), active.len());
    assert_eq!(slots.len(), transition_slots.len());
    assert_eq!(slots.len(), transition_active.len());
    slots
        .iter()
        .zip(active)
        .zip(transition_slots)
        .zip(transition_active)
        .enumerate()
        .any(
            |(index, (((current, active), transition), transition_is_active))| {
                index != excluded_cpu
                // ORDERING: Acquire observes admission before its task slot.
                && active.load(Ordering::Acquire)
                && (current.load(Ordering::Acquire) == slot
                    || (transition_is_active.load(Ordering::Acquire)
                        && transition.load(Ordering::Acquire) == slot))
            },
        )
}

#[cfg(test)]
fn slot_is_running_in(
    slots: &[AtomicUsize],
    active: &[AtomicBool],
    transition_slots: &[AtomicUsize],
    transition_active: &[AtomicBool],
    slot: usize,
) -> bool {
    task_execution_owner_in(slots, active, transition_slots, transition_active, slot).is_some()
}

fn task_execution_owner_in(
    slots: &[AtomicUsize],
    active: &[AtomicBool],
    transition_slots: &[AtomicUsize],
    transition_active: &[AtomicBool],
    slot: usize,
) -> Option<TaskExecutionOwner> {
    assert_eq!(slots.len(), active.len());
    assert_eq!(slots.len(), transition_slots.len());
    assert_eq!(slots.len(), transition_active.len());
    let mut owner = None;
    for (logical_index, (((current, active), transition), transition_is_active)) in slots
        .iter()
        .zip(active)
        .zip(transition_slots)
        .zip(transition_active)
        .enumerate()
    {
        // ORDERING: Acquire observes admission before its task slot.
        let admitted = active.load(Ordering::Acquire);
        let owns_current = admitted && current.load(Ordering::Acquire) == slot;
        // ORDERING: Acquire of the transition flag observes its outgoing slot
        // publication and retains that stack across the assembly switch.
        let owns_transition = admitted
            && transition_is_active.load(Ordering::Acquire)
            && transition.load(Ordering::Acquire) == slot;
        if owns_current || owns_transition {
            // A slot may briefly be both the published current and the
            // transition-from slot on the *same* CPU. Prefer Transition: its
            // saved frame has been published and a wake must survive the
            // imminent transition clear. Seeing the slot on two CPUs remains
            // a fatal ownership violation.
            let observed = if owns_transition {
                TaskExecutionOwner::Transition(logical_index)
            } else {
                TaskExecutionOwner::Current(logical_index)
            };
            assert!(
                owner.replace(observed).is_none(),
                "scheduler invariant: one task has duplicate current/transition owners"
            );
        }
    }
    owner
}

#[cfg(test)]
mod tests {
    use super::super::irq;
    use super::test_support::{
        install_test_current_owner, install_test_transition_owner_with_admission,
    };
    use super::{
        AtomicBool, AtomicUsize, CpuSwitchPublication, Ordering, TaskExecutionOwner,
        slot_has_owner_in, slot_is_running_in, task_execution_owner, task_execution_owner_in,
        test_publication_lock,
    };
    use std::panic::{AssertUnwindSafe, catch_unwind};

    struct DeferredTargetFlushReset;

    impl DeferredTargetFlushReset {
        fn new() -> Self {
            irq::reset_test_deferred_target_reschedule_flush_epoch();
            Self
        }
    }

    impl Drop for DeferredTargetFlushReset {
        fn drop(&mut self) {
            irq::reset_test_deferred_target_reschedule_flush_epoch();
        }
    }

    #[test]
    fn current_task_ownership_ignores_offline_slots_and_is_cpu_distinct() {
        let slots = [
            AtomicUsize::new(0),
            AtomicUsize::new(17),
            AtomicUsize::new(29),
        ];
        let active = [
            AtomicBool::new(true),
            AtomicBool::new(true),
            AtomicBool::new(false),
        ];
        let transition_slots = [
            AtomicUsize::new(0),
            AtomicUsize::new(17),
            AtomicUsize::new(0),
        ];
        let transition_active = [
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicBool::new(false),
        ];
        assert!(slot_is_running_in(
            &slots,
            &active,
            &transition_slots,
            &transition_active,
            0
        ));
        assert!(slot_is_running_in(
            &slots,
            &active,
            &transition_slots,
            &transition_active,
            17
        ));
        assert_eq!(
            task_execution_owner_in(&slots, &active, &transition_slots, &transition_active, 17),
            Some(TaskExecutionOwner::Current(1))
        );
        assert!(!slot_is_running_in(
            &slots,
            &active,
            &transition_slots,
            &transition_active,
            29
        ));
        assert!(slot_has_owner_in(
            &slots,
            &active,
            &transition_slots,
            &transition_active,
            2,
            17
        ));
        assert!(!slot_has_owner_in(
            &slots,
            &active,
            &transition_slots,
            &transition_active,
            1,
            17
        ));

        // ORDERING: Publish the outgoing stack transition before the incoming
        // current slot, exactly as the original CPU turn's publication does.
        transition_slots[1].store(17, Ordering::Release);
        transition_active[1].store(true, Ordering::Release);
        slots[1].store(23, Ordering::Release);
        assert!(slot_is_running_in(
            &slots,
            &active,
            &transition_slots,
            &transition_active,
            17
        ));
        assert!(slot_is_running_in(
            &slots,
            &active,
            &transition_slots,
            &transition_active,
            23
        ));
        assert_eq!(
            task_execution_owner_in(&slots, &active, &transition_slots, &transition_active, 17),
            Some(TaskExecutionOwner::Transition(1))
        );

        // ORDERING: Assembly clears the transition only after changing `rsp`.
        transition_active[1].store(false, Ordering::Release);
        assert!(!slot_is_running_in(
            &slots,
            &active,
            &transition_slots,
            &transition_active,
            17
        ));

        let _serial = test_publication_lock();
        let inactive_cpu = nucleus_core::util::lockdep::MAX_TRACKED_CPUS - 2;
        let active_cpu = nucleus_core::util::lockdep::MAX_TRACKED_CPUS - 1;
        let retained_slot = super::MAX_SCHEDULER_TASKS - 3;
        let _inactive = install_test_transition_owner_with_admission(
            inactive_cpu,
            retained_slot - 1,
            retained_slot,
            false,
        );
        let _active = install_test_transition_owner_with_admission(
            active_cpu,
            retained_slot,
            retained_slot - 2,
            true,
        );
        assert_eq!(
            task_execution_owner(retained_slot),
            Some(TaskExecutionOwner::Current(active_cpu)),
            "an inactive CPU's stale transition must not collide with the active current owner"
        );
    }

    #[test]
    fn duplicate_current_or_transition_owner_panics() {
        let slots = [AtomicUsize::new(41), AtomicUsize::new(52)];
        let active = [AtomicBool::new(true), AtomicBool::new(true)];
        let transition_slots = [AtomicUsize::new(0), AtomicUsize::new(41)];
        let transition_active = [AtomicBool::new(false), AtomicBool::new(true)];

        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = task_execution_owner_in(
                    &slots,
                    &active,
                    &transition_slots,
                    &transition_active,
                    41,
                );
            }))
            .is_err(),
            "one slot published by two current/transition owners must fail-stop"
        );
    }

    #[test]
    fn scheduler_guard_release_flushes_deferred_target_notification() {
        // Keep the historical mutation witness ID, but exercise the actual
        // independent IRQ owner. No global-lock fixture or scalar Current can
        // stand in for this born allocation's original physical/UID banks.
        super::super::scheduler::irq_entry_tests::current(|_| {
            let _flush_reset = DeferredTargetFlushReset::new();
            let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
            scope.mask_irqs();
            let entry = super::irq_entry::DispatchEntry::from_scope(scope);
            let target_cpu = 1;
            irq::prepare_test_deferred_target_reschedule(target_cpu);
            drop(entry);

            assert_eq!(irq::test_deferred_target_reschedule_pending_mask(), 0);
            assert_eq!(
                irq::test_deferred_target_reschedule_sent_mask(),
                1_u64 << target_cpu,
                "the release path must claim and validate the exact remote target"
            );
            assert_eq!(
                irq::test_deferred_target_reschedule_flush_epoch(),
                1,
                "the original IRQ turn must send one validated target notification"
            );
        });
    }

    #[test]
    fn guard_switch_publishes_outgoing_transition_before_current_slot() {
        let _serial = test_publication_lock();
        let logical_index = nucleus_core::util::lockdep::MAX_TRACKED_CPUS - 1;
        let outgoing_slot = super::MAX_SCHEDULER_TASKS - 7;
        let incoming_slot = super::MAX_SCHEDULER_TASKS - 6;
        let publication = install_test_current_owner(logical_index, outgoing_slot);

        CpuSwitchPublication {
            cpu: logical_index,
            from: outgoing_slot,
            to: incoming_slot,
            to_idle: false,
        }
        .publish_phase_model();

        assert_eq!(
            task_execution_owner(outgoing_slot),
            Some(TaskExecutionOwner::Transition(logical_index)),
            "the outgoing stack must become a transition owner before incoming current publication"
        );
        assert_eq!(
            task_execution_owner(incoming_slot),
            Some(TaskExecutionOwner::Current(logical_index)),
            "the same live publication must install the selected current slot"
        );
        publication.commit_assembly();
        assert_eq!(task_execution_owner(outgoing_slot), None);
    }
}
