use super::*;

pub(in crate::multitask) fn with_initialized_fixture(f: impl FnOnce()) {
    let previous = super::SCHEDULER_INITIALIZED.swap(true, Ordering::AcqRel);
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            // ORDERING: restore the explicitly modeled earlier boot ACK.
            super::SCHEDULER_INITIALIZED.store(self.0, Ordering::Release);
        }
    }
    let _restore = Restore(previous);
    f();
}

static TEST_PUBLICATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes tests that temporarily install synthetic per-CPU publication.
///
/// Production ownership is CPU-local and cannot alias. Host tests all execute
/// through one process, so these white-box witnesses need the equivalent
/// exclusion while they save and restore one CPU's publication words.
pub(in super::super) fn test_publication_lock() -> std::sync::MutexGuard<'static, ()> {
    // This guards disposable host fixtures.  Preserve the next test's
    // ability to restore a clean publication snapshot after an assertion
    // failure, so a single failed witness does not mask every later failure
    // behind `PoisonError`.
    TEST_PUBLICATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(in super::super) struct TestCpuPublicationRestore {
    logical_index: usize,
    current: usize,
    active: bool,
    idle: bool,
    scratch: usize,
    transition_from: usize,
    transition_active: bool,
    turn_phase: u8,
    execution_banks: Option<super::super::current_identity::execution::banks::FixtureRestore>,
}

pub(super) fn preserve_test_cpu_publication(logical_index: usize) -> TestCpuPublicationRestore {
    assert!(
        logical_index < CURRENT_TASK_SLOTS.len(),
        "test scheduler publication CPU exceeds capacity"
    );
    TestCpuPublicationRestore {
        logical_index,
        current: CURRENT_TASK_SLOTS[logical_index].load(Ordering::Acquire),
        active: CURRENT_TASK_ACTIVE[logical_index].load(Ordering::Acquire),
        idle: CURRENT_TASK_IDLE[logical_index].load(Ordering::Acquire),
        scratch: SCHEDULER_CURRENT_TASK_SCRATCH[logical_index].load(Ordering::Acquire),
        transition_from: TRANSITION_FROM_SLOTS[logical_index].load(Ordering::Acquire),
        transition_active: TRANSITION_ACTIVE[logical_index].load(Ordering::Acquire),
        turn_phase: dispatch_turn::test_phase(logical_index),
        execution_banks: Some(super::super::current_identity::execution::banks::isolate(
            logical_index,
        )),
    }
}

impl TestCpuPublicationRestore {
    /// Models the assembly-side commit boundary for a synthetic transition.
    pub(in super::super) fn commit_assembly(&self) {
        if let Some(commit) = dispatch_turn::begin_commit(self.logical_index) {
            TRANSITION_ACTIVE[self.logical_index].store(false, Ordering::Release);
            if commit.physical_banks() {
                super::super::scheduler::stack_resources::cpu_leases::commit(self.logical_index);
            }
            super::super::current_identity::execution::banks::commit(self.logical_index);
            commit.finish();
        } else {
            // Queue-only transition fixture has no dispatch producer token.
            assert_eq!(dispatch_turn::test_phase(self.logical_index), 0);
            TRANSITION_ACTIVE[self.logical_index].store(false, Ordering::Release);
        }
    }
}

impl Drop for TestCpuPublicationRestore {
    fn drop(&mut self) {
        // Withdraw active/transition observation first so no test observer can
        // combine a restored slot with synthetic ownership from this witness.
        TRANSITION_ACTIVE[self.logical_index].store(false, Ordering::Release);
        CURRENT_TASK_ACTIVE[self.logical_index].store(false, Ordering::Release);
        CURRENT_TASK_SLOTS[self.logical_index].store(self.current, Ordering::Release);
        CURRENT_TASK_IDLE[self.logical_index].store(self.idle, Ordering::Release);
        SCHEDULER_CURRENT_TASK_SCRATCH[self.logical_index].store(self.scratch, Ordering::Release);
        TRANSITION_FROM_SLOTS[self.logical_index].store(self.transition_from, Ordering::Release);
        TRANSITION_ACTIVE[self.logical_index].store(self.transition_active, Ordering::Release);
        CURRENT_TASK_ACTIVE[self.logical_index].store(self.active, Ordering::Release);
        dispatch_turn::test_restore_phase(self.logical_index, self.turn_phase);
        drop(self.execution_banks.take());
    }
}

/// Installs one synthetic outgoing stack owner for a scheduler wake witness.
/// The caller must retain `test_publication_lock` for this restore guard's
/// lifetime so no other test sees the temporary CPU-local ownership.
pub(in super::super) fn install_test_transition_owner(
    logical_index: usize,
    incoming_slot: usize,
    outgoing_slot: usize,
) -> TestCpuPublicationRestore {
    install_test_transition_owner_with_admission(logical_index, incoming_slot, outgoing_slot, true)
}

/// Installs one synthetic transition owner with an explicit CPU admission bit.
/// This lets the ownership witness prove that a stale inactive CPU cannot
/// retain a stack merely because its old publication words remain populated.
pub(super) fn install_test_transition_owner_with_admission(
    logical_index: usize,
    incoming_slot: usize,
    outgoing_slot: usize,
    admitted: bool,
) -> TestCpuPublicationRestore {
    let restore = preserve_test_cpu_publication(logical_index);
    assert!(
        incoming_slot < MAX_SCHEDULER_TASKS && outgoing_slot < MAX_SCHEDULER_TASKS,
        "test scheduler transition slot exceeds capacity"
    );
    CURRENT_TASK_SLOTS[logical_index].store(incoming_slot, Ordering::Release);
    CURRENT_TASK_ACTIVE[logical_index].store(admitted, Ordering::Release);
    CURRENT_TASK_IDLE[logical_index].store(false, Ordering::Release);
    SCHEDULER_CURRENT_TASK_SCRATCH[logical_index].store(incoming_slot, Ordering::Release);
    TRANSITION_FROM_SLOTS[logical_index].store(outgoing_slot, Ordering::Release);
    TRANSITION_ACTIVE[logical_index].store(true, Ordering::Release);
    restore
}

/// Installs a scalar-only current owner for protocol and negative witnesses.
/// The caller retains `test_publication_lock` until the
/// restore guard drops.
pub(in super::super) fn install_test_current_owner(
    logical_index: usize,
    slot: usize,
) -> TestCpuPublicationRestore {
    let restore = preserve_test_cpu_publication(logical_index);
    assert!(
        slot < MAX_SCHEDULER_TASKS,
        "test scheduler slot exceeds capacity"
    );
    CURRENT_TASK_SLOTS[logical_index].store(slot, Ordering::Release);
    CURRENT_TASK_ACTIVE[logical_index].store(true, Ordering::Release);
    CURRENT_TASK_IDLE[logical_index].store(false, Ordering::Release);
    SCHEDULER_CURRENT_TASK_SCRATCH[logical_index].store(slot, Ordering::Relaxed);
    TRANSITION_FROM_SLOTS[logical_index].store(0, Ordering::Release);
    TRANSITION_ACTIVE[logical_index].store(false, Ordering::Release);
    restore
}

/// Changes ONLY the scalar Current for a negative witness; actual allocation,
/// execution, MM and placement banks remain retained. Caller owns the native
/// publication fixture lock. Drop restores even on a failed assertion.
pub(in super::super) fn with_test_current_scalar<R>(cpu: usize, slot: usize, f: impl FnOnce() -> R) -> R {
    struct Restore(usize, usize);
    impl Drop for Restore {
        fn drop(&mut self) { CURRENT_TASK_SLOTS[self.0].store(self.1, Ordering::Release); }
    }
    let _restore = Restore(cpu, CURRENT_TASK_SLOTS[cpu].swap(slot, Ordering::AcqRel));
    f()
}

/// Host-only phase witness; no physical CR3/RSP/IRQ or funding refinement.
pub(crate) fn with_test_activation_return(
    cpu: usize,
    from: usize,
    to: usize,
    mut inspect: impl FnMut(u8),
) {
    let restore = install_test_current_owner(cpu, from);
    let turn = dispatch_turn::begin_phase_model(
        nucleus_core::util::lockdep::DispatchPreemption::enter_model(cpu),
        from,
    );
    inspect(0);
    let completion = turn.publish_return(to, false);
    inspect(1);
    completion.after_directory_unlock();
    inspect(2);
    restore.commit_assembly();
    inspect(3);
}

pub(crate) fn with_test_rejected_activation_return(
    cpu: usize,
    from: usize,
    to: usize,
    prepare: impl FnOnce(),
    inspect_poison: impl FnOnce(),
) {
    extern crate std;
    let _restore = install_test_current_owner(cpu, from);
    let turn = dispatch_turn::begin_phase_model(
        nucleus_core::util::lockdep::DispatchPreemption::enter_model(cpu),
        from,
    );
    prepare();
    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        turn.publish_return(to, false)
    }));
    assert!(rejected.is_err());
    assert_eq!(dispatch_turn::test_phase(cpu), 4);
    inspect_poison(); // fixture recovery has not released quarantined custody
}
