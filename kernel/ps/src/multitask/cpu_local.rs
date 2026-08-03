//! CPU-local scheduler ownership publication.
//!
//! - **Owner:** `kernel-ps` owns the global scheduler lock and every dense
//!   CPU's current-task publication.
//! - **Boundary:** `Scheduler::current_task` is scratch loaded from and
//!   returned to the invoking CPU's slot on every serialized access.
//! - **Lifecycle:** BSP bootstrap ownership precedes AP idle publication. A
//!   later handoff reserves the incoming slot while retaining the outgoing
//!   stack, then assembly releases the outgoing owner only after switching
//!   `rsp` to the selected frame.
//! - **Concurrency:** one global raw lock serializes selection while active
//!   current and stack-transition slots provide lock-free remote ownership.
//! - **Failure:** duplicate publication, overlapping transition, inactive AP
//!   entry, or one task on two CPUs is an immediate scheduler panic.
//! - **Forbidden:** remote lifetime code must not infer ownership from the
//!   scheduler scratch field, release an outgoing stack before the assembly
//!   commit boundary, or infer authority from inactive slot values.
//! - **Evidence:** `cpu-online-lifecycle`, `scheduler-lifecycle`, and
//!   `scheduler-cpu-ownership`.

use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use nucleus_core::util::lockdep::{LockClass, TrackedSpinGuard, TrackedSpinLock};

use super::MAX_SCHEDULER_TASKS;
use super::scheduler::Scheduler;

static SCHEDULER: TrackedSpinLock<Scheduler, { LockClass::Scheduler as u8 }> =
    TrackedSpinLock::new(Scheduler::new());
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
static TRANSITION_FROM_SLOTS: [AtomicUsize; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicUsize::new(0) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static TRANSITION_ACTIVE: [AtomicBool; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];

const SCHEDULER_LOCK_TIMEOUT_NS: u64 = 100_000_000;
const NO_SCHEDULER_OWNER: usize = usize::MAX;
// ORDERING: the owner CPU is the release/acquire publication word for the
// remaining diagnostic fields. These atomics never grant scheduler authority.
static SCHEDULER_OWNER_CPU: AtomicUsize = AtomicUsize::new(NO_SCHEDULER_OWNER);
static SCHEDULER_OWNER_SLOT: AtomicUsize = AtomicUsize::new(0);
static SCHEDULER_OWNER_ACQUIRED_NS: AtomicU64 = AtomicU64::new(0);
static SCHEDULER_OWNER_CALLER: AtomicPtr<Location<'static>> = AtomicPtr::new(ptr::null_mut());

pub(super) fn scheduler_initialized() -> bool {
    // ORDERING: Acquire observes reset, every AP idle context, and BSP current
    // execution preparation published before the one-way Release below.
    SCHEDULER_INITIALIZED.load(Ordering::Acquire)
}

pub(super) fn publish_scheduler_initialized() {
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

pub(super) struct SchedulerAccessGuard {
    guard: Option<TrackedSpinGuard<'static, Scheduler, { LockClass::Scheduler as u8 }>>,
    logical_index: usize,
    original_task: usize,
    acquired_at_ns: u64,
}

impl Deref for SchedulerAccessGuard {
    type Target = Scheduler;

    fn deref(&self) -> &Self::Target {
        self.guard
            .as_deref()
            .expect("scheduler guard missing before release")
    }
}

impl DerefMut for SchedulerAccessGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard
            .as_deref_mut()
            .expect("scheduler guard missing before release")
    }
}

impl Drop for SchedulerAccessGuard {
    fn drop(&mut self) {
        let guard = self
            .guard
            .as_mut()
            .expect("scheduler guard missing during release");
        guard.record_runtime_profile_lock_hold(
            crate::arch::clock::monotonic_nanos().saturating_sub(self.acquired_at_ns),
        );
        // ORDERING: Acquire proves this CPU completed initial slot admission.
        assert!(
            CURRENT_TASK_ACTIVE[self.logical_index].load(Ordering::Acquire),
            "scheduler invariant: CPU returned a task before current-slot admission"
        );
        assert!(
            !slot_has_remote_owner(self.logical_index, guard.current_task),
            "scheduler invariant: one task selected concurrently on two CPUs"
        );
        // ORDERING: Acquire rejects a second scheduler return before assembly
        // has release-cleared the previous outgoing stack transition.
        assert!(
            !TRANSITION_ACTIVE[self.logical_index].load(Ordering::Acquire),
            "scheduler invariant: CPU entered scheduler before prior stack handoff committed"
        );
        if guard.current_task != self.original_task {
            // ORDERING: Publish the outgoing slot before activating the
            // transition. Remote lifetime and selection paths must retain its
            // stack until assembly has installed the incoming `rsp`.
            TRANSITION_FROM_SLOTS[self.logical_index].store(self.original_task, Ordering::Release);
            TRANSITION_ACTIVE[self.logical_index].store(true, Ordering::Release);
        }
        // ORDERING: Release publishes the exact task slot selected for this
        // CPU. A changed handoff already published the outgoing transition,
        // so both stacks remain owned until the assembly commit callback.
        CURRENT_TASK_SLOTS[self.logical_index].store(guard.current_task, Ordering::Release);
        // ORDERING: Release publishes the selected slot's scheduler-derived
        // Idle class after the slot itself. Periodic interrupt leaves may use
        // this one bit only to retain an idle continuation when every local
        // and foreign runqueue publication is empty.
        // ORDERING: the following Release is the derived-class publication.
        CURRENT_TASK_IDLE[self.logical_index]
            .store(guard.current_task_is_idle_task(), Ordering::Release);
        // ORDERING: diagnostic metadata is not lock authority. Release-clear
        // its publication immediately before the tracked guard releases the
        // real lock.
        SCHEDULER_OWNER_CPU.store(NO_SCHEDULER_OWNER, Ordering::Release);
        // Drop the tracked guard explicitly so the raw lock and its
        // preemption unit are gone before any remote IPI is emitted.
        drop(
            self.guard
                .take()
                .expect("scheduler guard disappeared before unlock"),
        );
        super::irq::flush_deferred_target_reschedules();
    }
}

#[inline(always)]
#[track_caller]
// SAFETY: callers must exclude same-CPU scheduler reentry for the complete
// guard lifetime; the tracked global lock serializes every other CPU.
pub(super) unsafe fn scheduler_mut() -> SchedulerAccessGuard {
    let caller = Location::caller();
    assert_ne!(
        nucleus_core::util::lockdep::current_lock_class(),
        Some(LockClass::Scheduler as u8),
        "scheduler invariant: recursive scheduler entry caller={}:{}",
        caller.file(),
        caller.line()
    );
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    assert!(
        logical_index < CURRENT_TASK_SLOTS.len(),
        "scheduler invariant: logical CPU index exceeds capacity"
    );
    // ORDERING: Acquire observes a prior BSP bootstrap-slot admission.
    if logical_index == 0 && !CURRENT_TASK_ACTIVE[0].load(Ordering::Acquire) {
        // ORDERING: BSP slot zero is the pre-scheduler bootstrap owner.
        CURRENT_TASK_SLOTS[0].store(0, Ordering::Release);
        // ORDERING: Release publishes the bootstrap slot after its value.
        assert!(
            CURRENT_TASK_ACTIVE[0]
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "scheduler invariant: concurrent BSP current-task admission"
        );
    }
    // ORDERING: Acquire proves AP current-slot publication precedes entry.
    assert!(
        logical_index == 0 || CURRENT_TASK_ACTIVE[logical_index].load(Ordering::Acquire),
        "scheduler invariant: AP entered before current-task admission"
    );
    let started_at = crate::arch::clock::monotonic_nanos();
    let mut guard = loop {
        let irq_context = nucleus_core::util::lockdep::irq_context_depth() != 0;
        if irq_context {
            assert!(
                !nucleus_core::util::lockdep::preemption_disabled(),
                "scheduler invariant: IRQ attempted dispatch while raw lock held depth={} class={:?}",
                nucleus_core::util::lockdep::preemption_depth(),
                nucleus_core::util::lockdep::current_lock_class()
            );
        }
        let acquired = if irq_context {
            SCHEDULER.try_lock_preemption_gated_irq()
        } else {
            SCHEDULER.try_lock()
        };
        if let Some(guard) = acquired {
            break guard;
        }
        // Concurrent per-CPU clockevents legitimately serialize here. The
        // generic raw-spin diagnostic count is too short under a descheduled
        // vCPU, so enforce the scheduler's wall-clock failure bound instead.
        if crate::arch::clock::monotonic_nanos().saturating_sub(started_at)
            >= SCHEDULER_LOCK_TIMEOUT_NS
        {
            let now = crate::arch::clock::monotonic_nanos();
            // ORDERING: Acquire of the owner CPU observes the diagnostic
            // fields published before that CPU entered the protected body.
            let owner_cpu = SCHEDULER_OWNER_CPU.load(Ordering::Acquire);
            let owner_slot = SCHEDULER_OWNER_SLOT.load(Ordering::Relaxed);
            let owner_acquired_ns = SCHEDULER_OWNER_ACQUIRED_NS.load(Ordering::Relaxed);
            // ORDERING: the owner CPU acquire above covers this final relaxed
            // field in the diagnostic publication.
            let owner_caller = SCHEDULER_OWNER_CALLER.load(Ordering::Relaxed);
            let (owner_file, owner_line) = if owner_caller.is_null() {
                ("<unpublished>", 0)
            } else {
                // SAFETY: `Location::caller` returns a static allocation and
                // its pointer remains valid after diagnostic publication.
                let owner_caller = unsafe { &*owner_caller };
                (owner_caller.file(), owner_caller.line())
            };
            panic!(
                "scheduler invariant: global scheduler lock acquisition timed out waiter_cpu={} waiter={}:{} waited_ns={} owner_cpu={} owner_slot={} owner={}:{} held_ns={}",
                logical_index,
                caller.file(),
                caller.line(),
                now.saturating_sub(started_at),
                owner_cpu,
                owner_slot,
                owner_file,
                owner_line,
                now.saturating_sub(owner_acquired_ns),
            );
        }
        spin_loop();
    };
    let acquired_at_ns = crate::arch::clock::monotonic_nanos();
    guard.record_runtime_profile_lock_wait(acquired_at_ns.saturating_sub(started_at));
    // ORDERING: Acquire restores this CPU's last published current-task slot
    // into the lock-protected scheduler scratch field.
    let original_task = CURRENT_TASK_SLOTS[logical_index].load(Ordering::Acquire);
    // ORDERING: Acquire rejects Rust reentry until the post-RSP assembly
    // callback has release-cleared the prior outgoing stack owner.
    assert!(
        !TRANSITION_ACTIVE[logical_index].load(Ordering::Acquire),
        "scheduler invariant: Rust reentry preceded assembly stack-handoff commit"
    );
    guard.current_task = original_task;
    // ORDERING: prepare every relaxed diagnostic field before release
    // publishing the owner CPU below.
    SCHEDULER_OWNER_SLOT.store(original_task, Ordering::Relaxed);
    SCHEDULER_OWNER_ACQUIRED_NS.store(acquired_at_ns, Ordering::Relaxed);
    // ORDERING: this final relaxed field is also covered by the owner CPU's
    // following release publication.
    SCHEDULER_OWNER_CALLER.store(
        caller as *const Location<'static> as *mut Location<'static>,
        Ordering::Relaxed,
    );
    // ORDERING: Release publishes one complete diagnostic owner record after
    // the tracked scheduler lock has been acquired.
    SCHEDULER_OWNER_CPU.store(logical_index, Ordering::Release);
    // A wake raised by an IRQ that interrupted any raw owner cannot enter the
    // scheduler at that point. Consume those exact task identities only after
    // this CPU has safely acquired scheduler ownership.
    let deferred_wakes = super::deferred_wake::take_current_cpu(logical_index);
    for task_id in deferred_wakes.iter() {
        let _ = guard.wake_task(task_id);
    }
    SchedulerAccessGuard {
        guard: Some(guard),
        logical_index,
        original_task,
        acquired_at_ns,
    }
}

#[inline(always)]
#[track_caller]
// SAFETY: identical to `scheduler_mut`; the returned guard retains exclusive
// scheduler access even when the caller only reads.
pub(super) unsafe fn scheduler_ref() -> SchedulerAccessGuard {
    // SAFETY: the caller of this function supplied the required exclusion.
    unsafe { scheduler_mut() }
}

pub(super) fn publish_cpu_current_task(logical_index: usize, slot: usize) {
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
    // ORDERING: Release makes the initialized idle context visible before the
    // AP is allowed to enter scheduler code.
    CURRENT_TASK_SLOTS[logical_index].store(slot, Ordering::Release);
    // ORDERING: this Release follows the exact slot publication above. AP
    // initial publications are private idle tasks; BSP slot zero is still
    // bootstrap work here and becomes Idle only after `mark_root_idle`.
    CURRENT_TASK_IDLE[logical_index].store(logical_index != 0, Ordering::Release);
    // ORDERING: Release publishes ownership only after the exact task slot.
    CURRENT_TASK_ACTIVE[logical_index].store(true, Ordering::Release);
}

pub(super) fn current_cpu_task_is_idle(logical_index: usize) -> bool {
    if logical_index >= CURRENT_TASK_IDLE.len() {
        return false;
    }
    // ORDERING: Acquire observes the exact current-slot publication that
    // precedes this derived class bit.
    CURRENT_TASK_IDLE[logical_index].load(Ordering::Acquire)
}

/// Reports whether the calling CPU may safely enter current-task snapshot
/// paths. Early AP boot and panic/log formatting use this as a non-blocking
/// authority check before touching the scheduler lock.
pub(super) fn current_cpu_task_slot_admitted() -> bool {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    logical_index < CURRENT_TASK_ACTIVE.len()
        // ORDERING: Acquire pairs with initial current-slot publication and
        // prevents a snapshot from observing an uninitialized slot value.
        && CURRENT_TASK_ACTIVE[logical_index].load(Ordering::Acquire)
        // ORDERING: A current-task observation cannot be attributed safely
        // until assembly has release-cleared the outgoing stack transition.
        && !TRANSITION_ACTIVE[logical_index].load(Ordering::Acquire)
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
    // ORDERING: Release makes the completed architectural stack switch happen
    // before remote scheduler/lifetime readers may observe transition clear.
    TRANSITION_ACTIVE[logical_index].store(false, Ordering::Release);
}

pub(super) fn task_slot_is_running(slot: usize) -> bool {
    task_execution_owner_in(
        &CURRENT_TASK_SLOTS,
        &CURRENT_TASK_ACTIVE,
        &TRANSITION_FROM_SLOTS,
        &TRANSITION_ACTIVE,
        slot,
    )
    .is_some()
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
    use super::{
        AtomicBool, AtomicUsize, Ordering, TaskExecutionOwner, slot_has_owner_in,
        slot_is_running_in, task_execution_owner_in,
    };

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
        // current slot, exactly as `SchedulerAccessGuard::drop` does.
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
    }
}
