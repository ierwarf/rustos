//! CPU-local scheduler ownership publication.
//!
//! - **Owner:** `kernel-ps` owns the global scheduler lock and every dense
//!   CPU's current-task publication.
//! - **Boundary:** a private CPU-local dispatch scratch slot is loaded from
//!   and returned to the invoking CPU's published current slot.  The
//!   `Scheduler` catalog never stores a shared mutable current-task field.
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
    test_publication_lock,
};

// This remains a dead-owner fail-stop, not a scheduling latency budget. A KVM
// vCPU can be descheduled by the host while it owns an otherwise bounded raw
// scheduler transaction; treating a 100 ms host pause as a kernel deadlock
// made healthy 8-vCPU boots panic. Match the existing TLB/reclaim fail-stop
// horizon while lock wait/hold profiles continue to reject this global path
// as a scalability architecture.
const SCHEDULER_LOCK_TIMEOUT_NS: u64 = 2_000_000_000;
// A dead-owner deadline must not be sampled on every spin iteration. The
// global monotonic source is an MMIO counter until the multiprocessor TSC
// admission promotes it, and under a virtualized product topology each such
// read is an exit serialized against the other CPUs. Polling it per iteration
// made every waiter actively slow the owner it was waiting for. The horizon is
// two seconds, so a fixed spin batch costs no useful detection resolution.
const SCHEDULER_LOCK_DEADLINE_POLL_SPINS: u32 = 1_024;
const NO_SCHEDULER_OWNER: usize = usize::MAX;
// ORDERING: the owner CPU is the release/acquire publication word for the
// remaining diagnostic fields. These atomics never grant scheduler authority.
static SCHEDULER_OWNER_CPU: AtomicUsize = AtomicUsize::new(NO_SCHEDULER_OWNER);
static SCHEDULER_OWNER_SLOT: AtomicUsize = AtomicUsize::new(0);
static SCHEDULER_OWNER_ACQUIRED_NS: AtomicU64 = AtomicU64::new(0);
static SCHEDULER_OWNER_CALLER: AtomicPtr<Location<'static>> = AtomicPtr::new(ptr::null_mut());

// Per-site acquisition census.
//
// The lock is acquired far more often than it dispatches, and the excess is
// non-dispatch traffic: wake, pick hints, donation, affinity, and lifecycle.
// Sharding only the dispatch path would therefore leave most acquisitions
// behind, so the per-CPU migration has to be aimed with a per-caller count
// rather than with the dispatch total.
//
// This is an atomic side table, not lock state: it is updated before the guard
// exists and adds a bounded linear probe, never an allocation or a second lock.
const ACQUIRE_SITE_SLOTS: usize = 64;
static ACQUIRE_SITE_CALLERS: [AtomicPtr<Location<'static>>; ACQUIRE_SITE_SLOTS] =
    [const { AtomicPtr::new(ptr::null_mut()) }; ACQUIRE_SITE_SLOTS];
static ACQUIRE_SITE_COUNTS: [AtomicU64; ACQUIRE_SITE_SLOTS] =
    [const { AtomicU64::new(0) }; ACQUIRE_SITE_SLOTS];
static ACQUIRE_SITE_OVERFLOW: AtomicU64 = AtomicU64::new(0);

/// Spreads a caller's static address over the census table.
///
/// The probe below started at slot zero for every site, so an acquisition
/// walked the whole registered prefix before finding its own counter: about
/// ten shared cache lines touched per acquisition, on every CPU, in the
/// shipping build. Hashing the address makes the common case one line, which
/// matters most at eight vCPUs where those lines are the ones bouncing.
#[inline]
fn acquire_site_bucket(key: *mut Location<'static>) -> usize {
    // Fibonacci hashing of the pointer; `Location` allocations are aligned, so
    // the low bits alone would collide across every site.
    const _: () = assert!(ACQUIRE_SITE_SLOTS.is_power_of_two());
    let mixed = (key as usize as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (mixed >> 32) as usize & (ACQUIRE_SITE_SLOTS - 1)
}

/// Charges one acquisition to its caller.
fn record_acquire_site(caller: &'static Location<'static>) {
    let key = caller as *const Location<'static> as *mut Location<'static>;
    let first = acquire_site_bucket(key);
    for probe in 0..ACQUIRE_SITE_SLOTS {
        let slot = (first + probe) % ACQUIRE_SITE_SLOTS;
        // ORDERING: Acquire observes a claim published by whichever CPU first
        // registered this site.
        let current = ACQUIRE_SITE_CALLERS[slot].load(Ordering::Acquire);
        if current == key {
            ACQUIRE_SITE_COUNTS[slot].fetch_add(1, Ordering::Relaxed);
            return;
        }
        if current.is_null()
            // ORDERING: Release publishes the claim before its count is used.
            && ACQUIRE_SITE_CALLERS[slot]
                .compare_exchange(ptr::null_mut(), key, Ordering::Release, Ordering::Acquire)
                .is_ok()
        {
            ACQUIRE_SITE_COUNTS[slot].fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    ACQUIRE_SITE_OVERFLOW.fetch_add(1, Ordering::Relaxed);
}

/// Takes the census so far, clearing counts for the next window.
///
/// Called outside the scheduler owner; it never blocks a dispatch.
pub(super) fn take_acquire_site_census() -> ([(&'static str, u32, u64); ACQUIRE_SITE_SLOTS], u64) {
    let mut census = [("", 0_u32, 0_u64); ACQUIRE_SITE_SLOTS];
    for slot in 0..ACQUIRE_SITE_SLOTS {
        let count = ACQUIRE_SITE_COUNTS[slot].swap(0, Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        // ORDERING: Acquire pairs with the claim publication above.
        let caller = ACQUIRE_SITE_CALLERS[slot].load(Ordering::Acquire);
        if caller.is_null() {
            continue;
        }
        // SAFETY: `Location::caller` returns a static allocation that outlives
        // every observer of this table.
        let caller = unsafe { &*caller };
        census[slot] = (caller.file(), caller.line(), count);
    }
    (census, ACQUIRE_SITE_OVERFLOW.swap(0, Ordering::Relaxed))
}

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
    /// Carried so the window's worst hold can name the site that produced it.
    caller: &'static Location<'static>,
    /// In-owner segment total observed at acquisition. The difference against
    /// the same total at release is what this turn actually attributed.
    phase_total_at_acquire_ns: u64,
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

/// The release-published ownership edge of one scheduler guard.
///
/// Keeping this narrow operation separate from lock profiling makes its
/// release/acquire contract directly testable while `SchedulerAccessGuard`
/// still owns its only production call site and ordering.
struct SchedulerGuardReleasePublication {
    logical_index: usize,
    original_task: usize,
}

impl SchedulerGuardReleasePublication {
    fn publish(self, selected_task: usize, selected_task_is_idle: bool) {
        if selected_task != self.original_task {
            // ORDERING: Publish the outgoing slot before activating the
            // transition. Remote lifetime and selection paths must retain its
            // stack until assembly has installed the incoming `rsp`.
            TRANSITION_FROM_SLOTS[self.logical_index].store(self.original_task, Ordering::Release);
            // ORDERING: Release activates the transition only after the
            // outgoing slot is published.
            TRANSITION_ACTIVE[self.logical_index].store(true, Ordering::Release);
        }
        // ORDERING: Release publishes the exact task slot selected for this
        // CPU. A changed handoff already published the outgoing transition,
        // so both stacks remain owned until the assembly commit callback.
        CURRENT_TASK_SLOTS[self.logical_index].store(selected_task, Ordering::Release);
        // ORDERING: Release publishes the selected slot's scheduler-derived
        // Idle class after the slot itself. Periodic interrupt leaves may use
        // this one bit only to retain an idle continuation when every local
        // and foreign runqueue publication is empty.
        // ORDERING: the following Release is the derived-class publication.
        CURRENT_TASK_IDLE[self.logical_index].store(selected_task_is_idle, Ordering::Release);
    }
}

impl Drop for SchedulerAccessGuard {
    fn drop(&mut self) {
        let guard = self
            .guard
            .as_mut()
            .expect("scheduler guard missing during release");
        let selected_task = scheduler_current_task_scratch_for_cpu(self.logical_index);
        assert!(
            selected_task < MAX_SCHEDULER_TASKS,
            "scheduler invariant: CPU-local dispatch scratch slot exceeds capacity"
        );
        let attributed_ns = guard
            .runtime_profile_phase_total_ns()
            .saturating_sub(self.phase_total_at_acquire_ns);
        guard.record_runtime_profile_lock_hold(
            crate::arch::clock::monotonic_nanos().saturating_sub(self.acquired_at_ns),
            attributed_ns,
            self.caller,
        );
        // ORDERING: Acquire proves this CPU completed initial slot admission.
        assert!(
            CURRENT_TASK_ACTIVE[self.logical_index].load(Ordering::Acquire),
            "scheduler invariant: CPU returned a task before current-slot admission"
        );
        assert!(
            !slot_has_remote_owner(self.logical_index, selected_task),
            "scheduler invariant: one task selected concurrently on two CPUs"
        );
        // ORDERING: Acquire rejects a second scheduler return before assembly
        // has release-cleared the previous outgoing stack transition.
        assert!(
            !TRANSITION_ACTIVE[self.logical_index].load(Ordering::Acquire),
            "scheduler invariant: CPU entered scheduler before prior stack handoff committed"
        );
        SchedulerGuardReleasePublication {
            logical_index: self.logical_index,
            original_task: self.original_task,
        }
        .publish(selected_task, guard.current_task_is_idle_task());
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
    record_acquire_site(caller);
    let started_at = crate::arch::clock::monotonic_nanos();
    let mut deadline_poll_spins: u32 = 0;
    let mut guard = SCHEDULER
        .lock_scheduler_bounded(|| {
            deadline_poll_spins = deadline_poll_spins.wrapping_add(1);
            deadline_poll_spins.is_multiple_of(SCHEDULER_LOCK_DEADLINE_POLL_SPINS)
                && crate::arch::clock::monotonic_nanos().saturating_sub(started_at)
                    >= SCHEDULER_LOCK_TIMEOUT_NS
        })
        .unwrap_or_else(|| {
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
        });
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
    set_scheduler_current_task_scratch_for_cpu(logical_index, original_task);
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
    let phase_total_at_acquire_ns = guard.runtime_profile_phase_total_ns();
    let deferred_wakes = super::deferred_wake::take_current_cpu(logical_index);
    let deferred_wake_count = deferred_wakes.len();
    for task_id in deferred_wakes.iter() {
        let _ = guard.wake_task(task_id);
    }
    // Charged unconditionally: the drain runs on every acquisition whether or
    // not it finds work, and an unattributed prologue would read as a
    // descheduled owner in the hold-maximum record.
    guard.record_runtime_profile_prologue(
        deferred_wake_count as u64,
        crate::arch::clock::monotonic_nanos().saturating_sub(acquired_at_ns),
    );
    SchedulerAccessGuard {
        guard: Some(guard),
        logical_index,
        original_task,
        acquired_at_ns,
        caller,
        phase_total_at_acquire_ns,
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
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    scheduler_current_task_scratch_for_cpu(logical_index)
}

/// Replaces the caller CPU's private scheduler-turn scratch slot.
///
/// Callers hold the exact CPU's scheduler dispatch exclusion.  Publication to
/// remote observers remains deferred to `SchedulerAccessGuard::drop`, after
/// the outgoing stack-transition owner is installed.
#[inline]
pub(super) fn set_scheduler_current_task_scratch(slot: usize) {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    set_scheduler_current_task_scratch_for_cpu(logical_index, slot);
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
fn set_scheduler_current_task_scratch_for_cpu(logical_index: usize, slot: usize) {
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
    // ORDERING: Release makes the completed architectural stack switch happen
    // before remote scheduler/lifetime readers may observe transition clear.
    TRANSITION_ACTIVE[logical_index].store(false, Ordering::Release);
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
        test_scheduler_guard_for_release,
    };
    use super::{
        AtomicBool, AtomicUsize, Ordering, SchedulerGuardReleasePublication, TaskExecutionOwner,
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
        let _serial = test_publication_lock();
        let _flush_reset = DeferredTargetFlushReset::new();
        let logical_index = 0;
        let original_task = super::MAX_SCHEDULER_TASKS - 5;
        let selected_task = super::MAX_SCHEDULER_TASKS - 4;
        let _publication = install_test_current_owner(logical_index, original_task);
        let target_cpu = 1;
        irq::prepare_test_deferred_target_reschedule(target_cpu);

        let guard = test_scheduler_guard_for_release(logical_index, original_task, selected_task);
        drop(guard);

        assert_eq!(irq::test_deferred_target_reschedule_pending_mask(), 0);
        assert_eq!(
            irq::test_deferred_target_reschedule_sent_mask(),
            1_u64 << target_cpu,
            "the release path must claim and validate the exact remote target"
        );
        assert_eq!(
            irq::test_deferred_target_reschedule_flush_epoch(),
            1,
            "the live SchedulerAccessGuard release must send one validated target notification"
        );
    }

    #[test]
    fn guard_switch_publishes_outgoing_transition_before_current_slot() {
        let _serial = test_publication_lock();
        let logical_index = nucleus_core::util::lockdep::MAX_TRACKED_CPUS - 1;
        let outgoing_slot = super::MAX_SCHEDULER_TASKS - 7;
        let incoming_slot = super::MAX_SCHEDULER_TASKS - 6;
        let publication = install_test_current_owner(logical_index, outgoing_slot);

        SchedulerGuardReleasePublication {
            logical_index,
            original_task: outgoing_slot,
        }
        .publish(incoming_slot, false);

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
