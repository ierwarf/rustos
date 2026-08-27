use super::*;

static TEST_PUBLICATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TEST_SCHEDULER_FOR_GUARD_RELEASE: TrackedSpinLock<
    Scheduler,
    { LockClass::Scheduler as u8 },
> = TrackedSpinLock::new(Scheduler::new());

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
    scheduler_owner_cpu: usize,
    scheduler_owner_slot: usize,
    scheduler_owner_acquired_ns: u64,
    scheduler_owner_caller: *mut Location<'static>,
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
        scheduler_owner_cpu: SCHEDULER_OWNER_CPU.load(Ordering::Acquire),
        scheduler_owner_slot: SCHEDULER_OWNER_SLOT.load(Ordering::Acquire),
        scheduler_owner_acquired_ns: SCHEDULER_OWNER_ACQUIRED_NS.load(Ordering::Acquire),
        scheduler_owner_caller: SCHEDULER_OWNER_CALLER.load(Ordering::Acquire),
    }
}

impl TestCpuPublicationRestore {
    /// Models the assembly-side commit boundary for a synthetic transition.
    pub(in super::super) fn commit_assembly(&self) {
        TRANSITION_ACTIVE[self.logical_index].store(false, Ordering::Release);
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
        SCHEDULER_OWNER_SLOT.store(self.scheduler_owner_slot, Ordering::Release);
        SCHEDULER_OWNER_ACQUIRED_NS.store(self.scheduler_owner_acquired_ns, Ordering::Release);
        SCHEDULER_OWNER_CALLER.store(self.scheduler_owner_caller, Ordering::Release);
        SCHEDULER_OWNER_CPU.store(self.scheduler_owner_cpu, Ordering::Release);
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

/// Installs a synthetic current owner before a test drives the real guard
/// release publication. The caller retains `test_publication_lock` until the
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

/// Produces an actual `SchedulerAccessGuard` backed by isolated test scheduler
/// storage, so its production `Drop` path can be witnessed without touching
/// IRQ/MMIO-backed boot state.
#[track_caller]
pub(super) fn test_scheduler_guard_for_release(
    logical_index: usize,
    original_task: usize,
    selected_task: usize,
) -> SchedulerAccessGuard {
    assert!(logical_index < CURRENT_TASK_SLOTS.len());
    assert!(original_task < MAX_SCHEDULER_TASKS && selected_task < MAX_SCHEDULER_TASKS);
    assert_eq!(
        CURRENT_TASK_SLOTS[logical_index].load(Ordering::Acquire),
        original_task,
        "test guard original slot must match the installed CPU owner"
    );
    let guard = TEST_SCHEDULER_FOR_GUARD_RELEASE
        .lock_scheduler_bounded(|| false)
        .expect("test scheduler guard acquisition cannot time out");
    let phase_total_at_acquire_ns = guard.runtime_profile_phase_total_ns();
    set_scheduler_current_task_scratch_for_cpu(logical_index, selected_task);
    SchedulerAccessGuard {
        guard: Some(guard),
        logical_index,
        original_task,
        acquired_at_ns: crate::arch::clock::monotonic_nanos(),
        caller: Location::caller(),
        phase_total_at_acquire_ns,
    }
}
