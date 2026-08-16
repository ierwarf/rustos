//! Allocation-free lock-class dependency tracking for ring0 leaf locks.
//!
//! The tracker records observed class ordering before a spin acquisition and
//! rejects recursion or a dependency edge that closes a cycle. Raw-spin state
//! is CPU-owned and interrupt-atomic. Sleepable state is keyed by scheduler
//! task identity so it survives blocking and resumption without leaking into
//! the next task dispatched on that CPU. Logical CPU identity is a dense,
//! release-published map from architectural APIC ID; raw APIC IDs never index
//! arrays.
//!
//! - **Owner:** `nucleus-core` owns raw/sleepable lock-class accounting and
//!   the dense CPU/APIC identity used by ring0 lock diagnostics.
//! - **Boundary:** kernel owners supply one stable class and may enter only
//!   bounded nonblocking critical sections; scheduler policy is not admitted.
//! - **Lifecycle:** validate dependency → pin CPU/task and preemption depth →
//!   acquire → release class → restore the exact same CPU's depth.
//! - **Concurrency:** raw state is CPU-local, sleepable state is task-local,
//!   and the dependency graph is allocation-free and globally atomic.
//! - **Failure:** recursion, order cycles, IRQ-mode conflicts, cross-CPU/APIC
//!   release, or preemption underflow panic before broken authority proceeds.
//! - **Forbidden:** blocking, allocation, migration, dispatch, or foreign
//!   release while a raw guard remains live.
//! - **Evidence:** `scheduler-cpu-ownership`, `cpu-online-lifecycle`, and
//!   source-conformance witnesses in `formal/run-source-conformance.sh`.

use core::ops::{Deref, DerefMut};
#[cfg(rustos_boot_image)]
use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    panic::Location,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use spin::{Mutex, MutexGuard};

mod cpu_identity;
#[cfg(any(rustos_boot_image, test))]
mod dependency_graph;
#[cfg(test)]
use dependency_graph::graph_reaches;
#[cfg(rustos_boot_image)]
use dependency_graph::{
    DEPENDENCIES, VALIDATED_RAW_EDGES, VALIDATED_TASK_EDGES, dependency_reaches,
    edge_already_validated, irq_dependency_conflicts, publish_validated_edge, record_irq_usage,
};
pub(super) mod lock_profile;
mod preemption;
#[cfg(rustos_boot_image)]
mod raw_diag;
mod scheduler_diag;
#[cfg(any(rustos_boot_image, test))]
mod spin_budget;
pub mod work_budget;

pub use lock_profile::drain_lock_profile;
pub use preemption::{
    PreemptionSnapshot, preemption_depth, preemption_disabled, preemption_snapshot,
};
#[cfg(rustos_boot_image)]
use preemption::{
    cancel_pending_acquire_and_enable, disable_preemption, enable_preemption, enable_preemption_on,
};
// Only the anchored unit tests reach these directly; the production callers
// moved into `preemption` with the functions that use them.
#[cfg(test)]
use preemption::{preemption_release_is_admissible, preemption_units_match};
pub use cpu_identity::{
    bind_current_cpu_identity, current_apic_id, current_cpu_index, finalize_cpu_identities,
    hardware_apic_id, register_cpu_identity,
};
pub use scheduler_diag::{
    SchedulerDispatchWitness, SchedulerObservation, SchedulerObservationKind,
    SchedulerObservationWitness, record_scheduler_dispatch, record_scheduler_observation,
    scheduler_dispatch_witness, scheduler_observation_witness,
};
#[cfg(rustos_boot_image)]
use spin_budget::{RAW_SPIN_CYCLE_LIMIT, raw_spin_wait_exceeded};

pub const MAX_LOCK_CLASSES: usize = 64;
pub const MAX_TRACKED_CPUS: usize = 8;
#[cfg(rustos_boot_image)]
const MAX_HELD_LOCK_DEPTH: usize = 16;
#[cfg(rustos_boot_image)]
const MAX_TRACKED_TASK_LOCK_STACKS: usize = 512;
#[cfg(rustos_boot_image)]
const TASK_STACK_OCCUPANCY_WORDS: usize = MAX_TRACKED_TASK_LOCK_STACKS.div_ceil(64);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum LockClass {
    IpcEndpoint = 1,
    IpcMessage = 2,
    IpcReply = 3,
    IpcRegion = 4,
    ProcessTable = 5,
    ConsoleRegistry = 6,
    IpcTransferRegistry = 7,
    IpcDeferredDrop = 8,
    CompatLifecycle = 9,
    IpcRegionReclaim = 10,
    CompatWaitset = 11,
    InputOpenDescription = 12,
    NetdDeferredRef = 13,
    RemoteVfsRegistry = 14,
    VfsDeferredMutation = 15,
    ProcBrokerRegistry = 16,
    ProcessState = 17,
    PhysicalAllocator = 18,
    KernelPageTable = 19,
    DirectMapUpdate = 20,
    ServiceEndpointRegistry = 21,
    ServiceCallGrant = 22,
    ServiceEndpointWaiter = 23,
    FutexWaiter = 24,
    InputWaiter = 25,
    RtcSleepWaiter = 26,
    LegacyPic = 27,
    IpcEndpointQuota = 28,
    IpcRegionQuota = 29,
    ConsoleWait = 30,
    TtyWait = 31,
    DvmBlockWait = 32,
    DvmNetworkStateWait = 33,
    DvmNetworkLeaseWait = 34,
    MmioRegistryWait = 35,
    DisplayBackendWait = 36,
    Scheduler = 37,
    TlbShootdown = 38,
    LinuxThreadState = 39,
    SchedulerRunQueue = 40,
    SchedulerMailbox = 41,
    SchedulerPolicy = 42,
    PciConfigTransaction = 43,
    DeviceIoctlRouteMemo = 44,
}

#[cfg(rustos_boot_image)]
#[derive(Clone, Copy)]
struct HeldLockStack {
    classes: [u8; MAX_HELD_LOCK_DEPTH],
    len: usize,
}

#[cfg(rustos_boot_image)]
impl HeldLockStack {
    const fn new() -> Self {
        Self {
            classes: [0; MAX_HELD_LOCK_DEPTH],
            len: 0,
        }
    }
}

#[cfg(rustos_boot_image)]
struct PerCpuHeldStacks([UnsafeCell<HeldLockStack>; MAX_TRACKED_CPUS]);

#[cfg(rustos_boot_image)]
// SAFETY: each UnsafeCell has one dense CPU owner, and accessors validate that
// CPU/APIC identity while local preemption rules prevent cross-CPU aliasing.
unsafe impl Sync for PerCpuHeldStacks {}

#[cfg(rustos_boot_image)]
struct TaskHeldStack {
    owner: AtomicU64,
    stack: UnsafeCell<HeldLockStack>,
}

#[cfg(rustos_boot_image)]
impl TaskHeldStack {
    const fn new() -> Self {
        Self {
            owner: AtomicU64::new(0),
            stack: UnsafeCell::new(HeldLockStack::new()),
        }
    }
}

#[cfg(rustos_boot_image)]
// SAFETY: every task-owned stack is serialized by its atomic owner identity;
// lockdep rejects reuse or release by a foreign task/CPU generation.
unsafe impl Sync for TaskHeldStack {}

#[cfg(rustos_boot_image)]
static HELD_STACKS: PerCpuHeldStacks =
    PerCpuHeldStacks([const { UnsafeCell::new(HeldLockStack::new()) }; MAX_TRACKED_CPUS]);
#[cfg(rustos_boot_image)]
static TASK_HELD_STACKS: [TaskHeldStack; MAX_TRACKED_TASK_LOCK_STACKS] =
    [const { TaskHeldStack::new() }; MAX_TRACKED_TASK_LOCK_STACKS];
/// Registered-slot bitmap for `TASK_HELD_STACKS`.
///
/// A slot is registered only while a task holds a sleepable class, which is
/// rare, but `before_acquire` consults the table on every tracked spin
/// acquisition. Scanning every owner word touches one cache line per slot to
/// answer "not registered" almost every time. This answers the same question
/// by visiting only registered slots, and the owner word is still compared, so
/// the check keeps its exact strength.
#[cfg(rustos_boot_image)]
static TASK_STACK_OCCUPANCY: [AtomicU64; TASK_STACK_OCCUPANCY_WORDS] =
    [const { AtomicU64::new(0) }; TASK_STACK_OCCUPANCY_WORDS];
// The per-CPU statics contract in `formal/smp-source-contracts.toml` pins these
// declarations to this file; `preemption` reads them through `super`.
#[cfg(rustos_boot_image)]
static PREEMPT_DISABLE_DEPTH: [AtomicUsize; MAX_TRACKED_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_TRACKED_CPUS];
#[cfg(rustos_boot_image)]
static PREEMPT_PENDING_DEPTH: [AtomicUsize; MAX_TRACKED_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_TRACKED_CPUS];
#[cfg(rustos_boot_image)]
static IRQ_CONTEXT_DEPTH: [AtomicUsize; MAX_TRACKED_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_TRACKED_CPUS];
#[cfg(rustos_boot_image)]
static CURRENT_TASK_OWNER: [AtomicU64; MAX_TRACKED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_TRACKED_CPUS];
#[cfg(rustos_boot_image)]
static CPU_APIC_IDENTITIES: [AtomicU64; MAX_TRACKED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_TRACKED_CPUS];
#[cfg(rustos_boot_image)]
static CPU_IDENTITY_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(rustos_boot_image)]
static CPU_IDENTITIES_PUBLISHED: AtomicBool = AtomicBool::new(false);
#[derive(Clone, Copy)]
struct PendingAcquire {
    #[cfg(rustos_boot_image)]
    class: u8,
}

/// A raw spin lock with an allocation-free runtime lock-class contract.
///
/// `CLASS` is a stable class identifier, not an object identity. Different
/// instances of one class therefore contribute to the same dependency graph,
/// matching the useful lockdep property that one observed inverse ordering
/// invalidates every instance of that logical lock type.
///
/// Every target-kernel guard disables task preemption on its acquisition CPU
/// for the complete lifetime without globally masking device interrupts. A
/// guard is CPU-affine: releasing it after task migration is an immediate
/// invariant failure. Locks shared with an IRQ leaf must additionally use the
/// local `without_interrupts` wrapper at process-context call sites.
pub struct TrackedSpinLock<T: ?Sized, const CLASS: u8> {
    inner: Mutex<T>,
}

pub struct TrackedSpinGuard<'a, T: ?Sized, const CLASS: u8> {
    guard: Option<MutexGuard<'a, T>>,
    owner_cpu: usize,
    owner_apic_id: u32,
    #[cfg(rustos_boot_image)]
    acquire_file: &'static str,
    #[cfg(rustos_boot_image)]
    acquire_line: u32,
    #[cfg(rustos_boot_image)]
    acquire_preemption_depth: usize,
}

pub struct IrqContextGuard;


/// Tracks a successful, non-blocking acquisition of an externally implemented
/// lock while the caller already owns a raw-spin class or runs in IRQ context.
/// This is deliberately not available for blocking acquisition: its only
/// purpose is to keep try-lock ordering in the same dependency graph without
/// pretending that an atomic, fail-fast probe can sleep.
pub struct ExternalRawLockGuard {
    #[cfg(rustos_boot_image)]
    class: u8,
}

#[inline]
pub fn enter_irq_context() -> IrqContextGuard {
    #[cfg(rustos_boot_image)]
    {
        // ORDERING: AcqRel publishes this CPU's IRQ nesting before any handler
        // lock acquisition and pairs with the final AcqRel guard release.
        let previous = IRQ_CONTEXT_DEPTH[current_cpu_index()].fetch_add(1, Ordering::AcqRel);
        assert!(
            previous < MAX_HELD_LOCK_DEPTH,
            "interrupt-context nesting exceeded bound"
        );
    }
    IrqContextGuard
}

#[inline]
pub fn irq_context_depth() -> usize {
    #[cfg(rustos_boot_image)]
    {
        // ORDERING: Acquire observes the CPU-local entry/release transition
        // before deciding whether task-owned lock state may be consulted.
        IRQ_CONTEXT_DEPTH[current_cpu_index()].load(Ordering::Acquire)
    }
    #[cfg(not(rustos_boot_image))]
    {
        0
    }
}

/// Number of tracked raw-spin classes held by the current CPU. Sleepable
/// locks use this as a hard boundary: waiting while a raw spin lock is held
/// can block the owner needed to release it and turns IRQ exclusion into an
/// unbounded critical section.
#[inline]
pub fn held_spin_lock_depth() -> usize {
    #[cfg(rustos_boot_image)]
    {
        return with_current_stack(|stack| stack.len);
    }
    #[cfg(not(rustos_boot_image))]
    {
        0
    }
}

/// Publish the task currently executing in process context on this CPU.
///
/// The scheduler updates this immediately before a context switch becomes
/// visible. Interrupt entry deliberately retains the interrupted task token;
/// raw-lock acquisition ignores the task-owned stack while IRQ context is
/// active.
#[inline]
pub fn set_current_task_owner(owner: u64) {
    #[cfg(rustos_boot_image)]
    {
        assert!(owner != 0, "lockdep current task owner must be nonzero");
        // ORDERING: Release publishes scheduler current-task ownership before
        // subsequent lock acquisitions load it with Acquire.
        CURRENT_TASK_OWNER[current_cpu_index()].store(owner, Ordering::Release);
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = owner;
    }
}

/// Return the scheduler-published task owner for diagnostics on this CPU.
#[inline]
pub fn current_task_owner() -> Option<u64> {
    #[cfg(rustos_boot_image)]
    {
        // ORDERING: Acquire observes the scheduler's Release publication
        // before an invariant report attributes the fault to a task.
        let owner = CURRENT_TASK_OWNER[current_cpu_index()].load(Ordering::Acquire);
        return (owner != 0).then_some(owner);
    }
    #[cfg(not(rustos_boot_image))]
    {
        None
    }
}

/// Record acquisition of one task-owned sleepable lock class.
///
/// Sleepable classes share the dependency graph with raw-spin classes.
/// Blocking acquisition while a raw class is held is forbidden; a bounded raw
/// leaf acquired after a sleepable lock contributes an ordinary dependency
/// edge and must not close a cycle. This prevents sleeping while owning
/// IRQ-relevant raw state without rejecting legitimate mutex-to-leaf ordering.
#[inline]
pub fn record_sleepable_acquire(owner: u64, class: u8) {
    #[cfg(rustos_boot_image)]
    {
        assert!(owner != 0, "sleepable lock owner must be nonzero");
        assert_eq!(
            irq_context_depth(),
            0,
            "sleepable lock acquired from interrupt context class={}",
            class
        );
        assert_eq!(
            held_spin_lock_depth(),
            0,
            "sleepable lock acquired while raw-spin class is held class={}",
            class
        );
        let class_index = validate_class(class);
        // A sleepable acquisition is charged to the CPU that made it. The two
        // assertions above already established that this is task context with
        // no raw class held, which is exactly the condition a declared ceiling
        // on a sleepable class relies on.
        work_budget::charge_acquire(current_cpu_index(), class_index);
        dependency_graph::mark_class_irq_unsafe(class_index);
        with_task_stack(owner, true, |stack| {
            assert!(
                !stack.classes[..stack.len].contains(&class),
                "recursive sleepable lock-class acquisition class={}",
                class
            );
            for held in &stack.classes[..stack.len] {
                let held_index = usize::from(*held);
                if edge_already_validated(&VALIDATED_TASK_EDGES, held_index, class_index) {
                    continue;
                }
                // ORDERING: SeqCst publishes this edge in the same total order
                // used by the following reverse-reachability check.
                DEPENDENCIES[held_index].fetch_or(1_u64 << class_index, Ordering::SeqCst);
                assert!(
                    !dependency_reaches(class_index, held_index),
                    "sleepable lock-class dependency cycle {} -> {}",
                    held,
                    class
                );
                publish_validated_edge(&VALIDATED_TASK_EDGES, held_index, class_index);
            }
            assert!(
                stack.len < stack.classes.len(),
                "sleepable lock-class nesting exceeds {}",
                MAX_HELD_LOCK_DEPTH
            );
            stack.classes[stack.len] = class;
            stack.len += 1;
        })
        .expect("sleepable lock-class stack capacity exhausted");
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = owner;
        let _ = validate_class(class);
    }
}

#[inline]
pub fn release_sleepable_lock(owner: u64, class: u8) {
    #[cfg(rustos_boot_image)]
    {
        let became_empty = with_task_stack(owner, false, |stack| {
            assert!(
                stack.len != 0,
                "sleepable lock-class release without acquisition"
            );
            let top = stack.classes[stack.len - 1];
            assert_eq!(
                top, class,
                "sleepable lock-class release order violation held={} released={}",
                top, class
            );
            stack.len -= 1;
            stack.classes[stack.len] = 0;
            stack.len == 0
        })
        .expect("sleepable lock-class owner is not registered");
        if became_empty {
            release_task_stack(owner);
        }
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = (owner, class);
    }
}

impl Drop for IrqContextGuard {
    fn drop(&mut self) {
        #[cfg(rustos_boot_image)]
        {
            // ORDERING: AcqRel publishes the completed IRQ handler before the
            // CPU-local nesting depth becomes observable as one level lower.
            let previous = IRQ_CONTEXT_DEPTH[current_cpu_index()].fetch_sub(1, Ordering::AcqRel);
            assert!(previous != 0, "interrupt-context depth underflow");
        }
    }
}

impl<T, const CLASS: u8> TrackedSpinLock<T, CLASS> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
        }
    }
}

impl<T: ?Sized, const CLASS: u8> TrackedSpinLock<T, CLASS> {
    #[track_caller]
    pub fn lock(&self) -> TrackedSpinGuard<'_, T, CLASS> {
        #[cfg(rustos_boot_image)]
        let acquire_site = Location::caller();
        #[cfg(not(rustos_boot_image))]
        let acquire_site = ();
        // Preemption is disabled for the guard's whole lifetime, so the index
        // derived here is stable until release -- the invariant the release
        // admissibility check independently confirms. Deriving it once is what
        // keeps the acquire cheap.
        #[cfg(rustos_boot_image)]
        let acquire_cpu = disable_preemption();
        let profile_entry = lock_profile::now();
        #[cfg(rustos_boot_image)]
        let pending = before_acquire_with_irq_tracking(acquire_cpu, CLASS, acquire_site, true);
        #[cfg(not(rustos_boot_image))]
        let pending = before_acquire(CLASS, acquire_site);
        let profile_checked = lock_profile::charge(lock_profile::LockPhase::BeforeAcquire, profile_entry);
        #[cfg(rustos_boot_image)]
        let guard = {
            let wait_start_tsc = read_tsc();
            let mut spins = 0usize;
            loop {
                if let Some(guard) = self.inner.try_lock() {
                    break guard;
                }
                spins += 1;
                let wait_cycles = if spins & 0x3ff == 0 {
                    read_tsc().saturating_sub(wait_start_tsc)
                } else {
                    0
                };
                if raw_spin_wait_exceeded(wait_cycles, spins) {
                    panic!(
                        "tracked spin lock contention exceeded duration bound class={} wait_at={}:{} spins={} wait_cycles={} cycle_limit={} preempt_depth={}",
                        CLASS,
                        acquire_site.file(),
                        acquire_site.line(),
                        spins,
                        wait_cycles,
                        RAW_SPIN_CYCLE_LIMIT,
                        preemption_depth()
                    );
                }
                spin_loop();
            }
        };
        #[cfg(not(rustos_boot_image))]
        let guard = self.inner.lock();
        let profile_taken = lock_profile::charge(lock_profile::LockPhase::Spin, profile_checked);
        #[cfg(rustos_boot_image)]
        let owner_apic_id = x86_64::instructions::interrupts::without_interrupts(|| {
            after_acquire_on(acquire_cpu, pending);
            cpu_identity::apic_id_for_index(acquire_cpu)
        });
        #[cfg(not(rustos_boot_image))]
        after_acquire(pending);
        lock_profile::charge(lock_profile::LockPhase::AfterAcquire, profile_taken);
        TrackedSpinGuard {
            guard: Some(guard),
            #[cfg(rustos_boot_image)]
            owner_cpu: acquire_cpu,
            #[cfg(not(rustos_boot_image))]
            owner_cpu: tracked_guard_owner_cpu(),
            #[cfg(rustos_boot_image)]
            owner_apic_id,
            #[cfg(not(rustos_boot_image))]
            owner_apic_id: current_apic_id(),
            #[cfg(rustos_boot_image)]
            acquire_file: acquire_site.file(),
            #[cfg(rustos_boot_image)]
            acquire_line: acquire_site.line(),
            // ORDERING: Acquire observes completed guard/pending transitions
            // before the depth this guard must match on release is recorded.
            #[cfg(rustos_boot_image)]
            acquire_preemption_depth: PREEMPT_DISABLE_DEPTH[acquire_cpu].load(Ordering::Acquire),
        }
    }

    #[track_caller]
    pub fn try_lock(&self) -> Option<TrackedSpinGuard<'_, T, CLASS>> {
        #[cfg(rustos_boot_image)]
        let acquire_cpu = current_cpu_index();
        #[cfg(rustos_boot_image)]
        let acquire_apic_id = current_apic_id();
        #[cfg(rustos_boot_image)]
        disable_preemption();
        #[cfg(rustos_boot_image)]
        let acquire_site = Location::caller();
        #[cfg(not(rustos_boot_image))]
        let acquire_site = ();
        let pending = before_acquire(CLASS, acquire_site);
        if let Some(guard) = self.inner.try_lock() {
            after_acquire(pending);
            Some(TrackedSpinGuard {
                guard: Some(guard),
                owner_cpu: tracked_guard_owner_cpu(),
                owner_apic_id: current_apic_id(),
                #[cfg(rustos_boot_image)]
                acquire_file: acquire_site.file(),
                #[cfg(rustos_boot_image)]
                acquire_line: acquire_site.line(),
                #[cfg(rustos_boot_image)]
                acquire_preemption_depth: preemption_depth(),
            })
        } else {
            #[cfg(rustos_boot_image)]
            {
                let release_cpu = current_cpu_index();
                let release_apic_id = current_apic_id();
                let depth = preemption_depth();
                assert!(
                    guard_release_is_admissible(
                        acquire_cpu,
                        acquire_apic_id,
                        release_cpu,
                        release_apic_id,
                        depth,
                    ),
                    "raw-spin failed try-lock lost preemption ownership class={} acquired_at={}:{} acquire_cpu={} acquire_apic={:#x} release_cpu={} release_apic={:#x} preempt_depth={} held_depth={} top_class={:?}",
                    CLASS,
                    acquire_site.file(),
                    acquire_site.line(),
                    acquire_cpu,
                    acquire_apic_id,
                    release_cpu,
                    release_apic_id,
                    depth,
                    held_spin_lock_depth(),
                    current_lock_class()
                );
                cancel_pending_acquire_and_enable(CLASS);
            }
            None
        }
    }

    #[track_caller]
    pub fn lock_scheduler_bounded(
        &self,
        mut timed_out: impl FnMut() -> bool,
    ) -> Option<TrackedSpinGuard<'_, T, CLASS>> {
        #[cfg(rustos_boot_image)]
        {
            if CLASS != LockClass::Scheduler as u8 {
                panic!("scheduler class required");
            }
            let irq_context = irq_context_depth() != 0;
            if irq_context {
                if preemption_disabled() {
                    panic!("bounded IRQ scheduler acquisition entered with raw lock held");
                }
                if x86_64::instructions::interrupts::are_enabled() {
                    panic!("bounded IRQ scheduler acquisition requires local IRQ exclusion");
                }
            }
            let acquire_site = Location::caller();
            let acquire_cpu = disable_preemption();
            let pending = before_acquire_with_irq_tracking(
                acquire_cpu,
                CLASS,
                acquire_site,
                !irq_context,
            );
            loop {
                while self.inner.is_locked() {
                    if timed_out() {
                        cancel_pending_acquire_and_enable(CLASS);
                        return None;
                    }
                    spin_loop();
                }
                if let Some(guard) = self.inner.try_lock() {
                    after_acquire(pending);
                    return Some(TrackedSpinGuard {
                        guard: Some(guard),
                        owner_cpu: tracked_guard_owner_cpu(),
                        owner_apic_id: current_apic_id(),
                        acquire_file: acquire_site.file(),
                        acquire_line: acquire_site.line(),
                        acquire_preemption_depth: preemption_depth(),
                    });
                }
                if timed_out() {
                    cancel_pending_acquire_and_enable(CLASS);
                    return None;
                }
                spin_loop();
            }
        }
        #[cfg(not(rustos_boot_image))]
        {
            let _ = &mut timed_out;
            Some(self.lock())
        }
    }
}

#[cfg(rustos_boot_image)]
#[inline]
fn read_tsc() -> u64 {
    // SAFETY: RDTSC has no memory operand or privilege requirement.
    unsafe { core::arch::x86_64::_rdtsc() }
}

impl<T: ?Sized, const CLASS: u8> Deref for TrackedSpinGuard<'_, T, CLASS> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard
            .as_deref()
            .expect("tracked spin guard missing before drop")
    }
}

impl<T: ?Sized, const CLASS: u8> DerefMut for TrackedSpinGuard<'_, T, CLASS> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard
            .as_deref_mut()
            .expect("tracked spin guard missing before drop")
    }
}

impl<T: ?Sized, const CLASS: u8> Drop for TrackedSpinGuard<'_, T, CLASS> {
    fn drop(&mut self) {
        let profile_entry = lock_profile::now();
        #[cfg(rustos_boot_image)]
        {
            x86_64::instructions::interrupts::without_interrupts(|| {
                let profile_masked = lock_profile::now();
                // Interrupts are masked for this whole block, so the logical
                // index cannot change inside it. Deriving it once and passing
                // it down is what makes the release cheap: the identity step
                // used to derive the same answer six times, and the actual
                // lock-word handoff below is thirty-six cycles.
                let release_cpu = current_cpu_index();
                let release_apic_id = cpu_identity::apic_id_for_index(release_cpu);
                // ORDERING: Acquire observes completed guard/pending
                // transitions before the release admissibility comparison.
                let depth = PREEMPT_DISABLE_DEPTH[release_cpu].load(Ordering::Acquire);
                let admissible = guard_release_is_admissible(
                    self.owner_cpu,
                    self.owner_apic_id,
                    release_cpu,
                    release_apic_id,
                    depth,
                ) && depth == self.acquire_preemption_depth;
                if !admissible {
                    raw_diag::write_guard_release_marker(
                        CLASS,
                        self.owner_cpu,
                        release_cpu,
                        self.owner_apic_id,
                        release_apic_id,
                        self.acquire_preemption_depth,
                        depth,
                        with_current_stack_on(release_cpu, |stack| stack.len),
                    );
                    panic!(
                        "raw-spin guard release invariant class={} acquired_at={}:{} owner_cpu={} owner_apic={:#x} release_cpu={} release_apic={:#x} acquire_depth={} release_depth={} held_depth={} top_class={:?}",
                        CLASS,
                        self.acquire_file,
                        self.acquire_line,
                        self.owner_cpu,
                        self.owner_apic_id,
                        release_cpu,
                        release_apic_id,
                        self.acquire_preemption_depth,
                        depth,
                        with_current_stack_on(release_cpu, |stack| stack.len),
                        with_current_stack_on(release_cpu, |stack| stack
                            .len
                            .checked_sub(1)
                            .map(|index| stack.classes[index]))
                    );
                }
                let profile_checked =
                    lock_profile::charge(lock_profile::LockPhase::ReleaseIdentity, profile_masked);
                drop(self.guard.take());
                let profile_unlocked =
                    lock_profile::charge(lock_profile::LockPhase::ReleaseUnlock, profile_checked);
                release_on(release_cpu, CLASS);
                let profile_popped =
                    lock_profile::charge(lock_profile::LockPhase::ReleaseStack, profile_unlocked);
                enable_preemption_on(release_cpu, CLASS);
                lock_profile::charge(lock_profile::LockPhase::ReleaseEnable, profile_popped);
            });
        }
        #[cfg(not(rustos_boot_image))]
        {
            drop(self.guard.take());
            release(CLASS);
            let _ = (self.owner_cpu, self.owner_apic_id);
        }
        lock_profile::charge(lock_profile::LockPhase::Release, profile_entry);
    }
}

/// A raw guard is CPU-affine and owns at least one nonzero nesting unit until
/// release. Checking both facts before unlocking keeps a broken handoff
/// fail-closed instead of exposing protected state and panicking afterwards.
#[inline]
#[cfg(any(rustos_boot_image, test))]
const fn guard_release_is_admissible(
    owner_cpu: usize,
    owner_apic_id: u32,
    release_cpu: usize,
    release_apic_id: u32,
    preemption_depth: usize,
) -> bool {
    owner_cpu == release_cpu && owner_apic_id == release_apic_id && preemption_depth != 0
}



#[inline]
fn tracked_guard_owner_cpu() -> usize {
    #[cfg(rustos_boot_image)]
    {
        current_cpu_index()
    }
    #[cfg(not(rustos_boot_image))]
    {
        0
    }
}




#[inline]
pub fn current_lock_class() -> Option<u8> {
    #[cfg(rustos_boot_image)]
    {
        with_current_stack(|stack| stack.len.checked_sub(1).map(|index| stack.classes[index]))
    }
    #[cfg(not(rustos_boot_image))]
    {
        None
    }
}

#[inline]
pub fn current_task_sleepable_lock_class() -> Option<u8> {
    #[cfg(rustos_boot_image)]
    {
        // ORDERING: Acquire observes the scheduler's Release publication of
        // the exact task whose sleepable stack may be inspected.
        let owner = CURRENT_TASK_OWNER[current_cpu_index()].load(Ordering::Acquire);
        if owner == 0 {
            return None;
        }
        return with_task_stack(owner, false, |stack| {
            stack.len.checked_sub(1).map(|index| stack.classes[index])
        })
        .flatten();
    }
    #[cfg(not(rustos_boot_image))]
    {
        None
    }
}

/// Record an already successful external try-lock as a raw, non-sleeping
/// acquisition. The caller must release the underlying lock before dropping
/// the returned guard.
#[inline]
#[track_caller]
pub fn record_external_raw_lock(class: u8) -> ExternalRawLockGuard {
    #[cfg(rustos_boot_image)]
    {
        disable_preemption();
        let pending = before_acquire(class, Location::caller());
        after_acquire(pending);
        ExternalRawLockGuard { class }
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = validate_class(class);
        ExternalRawLockGuard {}
    }
}

impl Drop for ExternalRawLockGuard {
    fn drop(&mut self) {
        #[cfg(rustos_boot_image)]
        {
            x86_64::instructions::interrupts::without_interrupts(|| {
                release(self.class);
                enable_preemption(self.class);
            });
        }
    }
}






fn validate_class(class: u8) -> usize {
    let class = usize::from(class);
    assert!(
        class != 0 && class < MAX_LOCK_CLASSES,
        "invalid lock class {}",
        class
    );
    class
}

#[cfg(rustos_boot_image)]
fn before_acquire(class: u8, acquire_site: &'static Location<'static>) -> PendingAcquire {
    before_acquire_with_irq_tracking(current_cpu_index(), class, acquire_site, true)
}

#[cfg(rustos_boot_image)]
/// `cpu` is the index the caller already derived; preemption is disabled for
/// the guard's lifetime, so it is stable for the whole acquire.
fn before_acquire_with_irq_tracking(
    cpu: usize,
    class: u8,
    acquire_site: &'static Location<'static>,
    track_irq_usage: bool,
) -> PendingAcquire {
    let class_index = validate_class(class);
    work_budget::charge_acquire(cpu, class_index);
    let profile_entry = lock_profile::now();
    if track_irq_usage {
        record_irq_usage(class_index, acquire_site);
    }
    let profile_irq = lock_profile::charge(lock_profile::LockPhase::BeforeIrqUsage, profile_entry);
    // ORDERING: Acquire observes the CPU-local entry/release transition before
    // deciding whether task-owned lock state may be consulted.
    if IRQ_CONTEXT_DEPTH[cpu].load(Ordering::Acquire) == 0 {
        // ORDERING: Acquire observes the scheduler's task-owner publication
        // before attributing any sleepable-to-raw dependency.
        let owner = CURRENT_TASK_OWNER[cpu].load(Ordering::Acquire);
        if owner != 0 {
            with_task_stack(owner, false, |stack| {
                for held in &stack.classes[..stack.len] {
                    let held_index = usize::from(*held);
                    if edge_already_validated(&VALIDATED_TASK_EDGES, held_index, class_index) {
                        continue;
                    }
                    // ORDERING: SeqCst publishes the edge before the reverse
                    // reachability query in the global lock graph.
                    DEPENDENCIES[held_index].fetch_or(1_u64 << class_index, Ordering::SeqCst);
                    assert!(
                        !dependency_reaches(class_index, held_index),
                        "sleepable-to-raw lock-class dependency cycle {} -> {} acquire={}:{}",
                        held,
                        class,
                        acquire_site.file(),
                        acquire_site.line(),
                    );
                    publish_validated_edge(&VALIDATED_TASK_EDGES, held_index, class_index);
                }
            });
        }
    }
    let profile_task = lock_profile::charge(lock_profile::LockPhase::BeforeTaskEdges, profile_irq);
    with_current_stack_on(cpu, |stack| {
        assert!(
            !stack.classes[..stack.len].contains(&class),
            "recursive lock-class acquisition class={} acquire={}:{}",
            class,
            acquire_site.file(),
            acquire_site.line()
        );
        for held in &stack.classes[..stack.len] {
            let held_index = usize::from(*held);
            if edge_already_validated(&VALIDATED_RAW_EDGES, held_index, class_index) {
                continue;
            }
            // ORDERING: SeqCst publishes the raw dependency before conflict
            // and cycle checks read the globally ordered graph.
            DEPENDENCIES[held_index].fetch_or(1_u64 << class_index, Ordering::SeqCst);
            assert!(
                !irq_dependency_conflicts(held_index, class_index),
                "lock-class IRQ dependency conflict {} -> {}",
                held,
                class
            );
            assert!(
                !dependency_reaches(class_index, held_index),
                "lock-class dependency cycle {} -> {}",
                held,
                class
            );
            publish_validated_edge(&VALIDATED_RAW_EDGES, held_index, class_index);
        }
    });
    lock_profile::charge(lock_profile::LockPhase::BeforeRawEdges, profile_task);
    PendingAcquire { class }
}

#[cfg(not(rustos_boot_image))]
fn before_acquire(class: u8, _acquire_site: ()) -> PendingAcquire {
    let _ = validate_class(class);
    PendingAcquire {}
}

#[cfg(rustos_boot_image)]
fn after_acquire(pending: PendingAcquire) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        after_acquire_on(current_cpu_index(), pending)
    });
}

/// `after_acquire` for a caller that already masked interrupts and derived its
/// logical index.
#[cfg(rustos_boot_image)]
fn after_acquire_on(cpu: usize, pending: PendingAcquire) {
    {
        with_current_stack_on(cpu, |stack| {
            assert!(
                stack.len < stack.classes.len(),
                "lock-class nesting exceeds {}",
                MAX_HELD_LOCK_DEPTH
            );
            stack.classes[stack.len] = pending.class;
            stack.len += 1;
        });
        let previous = PREEMPT_PENDING_DEPTH[cpu].fetch_sub(1, Ordering::Relaxed);
        assert!(previous != 0, "raw-spin pending-acquire depth underflow");
    }
}

#[cfg(not(rustos_boot_image))]
fn after_acquire(_pending: PendingAcquire) {}

#[cfg(rustos_boot_image)]
fn release(class: u8) {
    x86_64::instructions::interrupts::without_interrupts(|| release_on(current_cpu_index(), class));
}

/// `release` for a caller that already masked interrupts and derived its index.
#[cfg(rustos_boot_image)]
fn release_on(cpu: usize, class: u8) {
    with_current_stack_on(cpu, |stack| {
        assert!(stack.len != 0, "lock-class release without acquisition");
        let top = stack.classes[stack.len - 1];
        assert_eq!(
            top, class,
            "lock-class release order violation held={} released={}",
            top, class
        );
        stack.len -= 1;
        stack.classes[stack.len] = 0;
    });
}

#[cfg(not(rustos_boot_image))]
fn release(_class: u8) {}

#[cfg(rustos_boot_image)]
fn with_current_stack<R>(f: impl FnOnce(&mut HeldLockStack) -> R) -> R {
    x86_64::instructions::interrupts::without_interrupts(|| {
        with_current_stack_on(current_cpu_index(), f)
    })
}

/// The held-stack accessor for a caller that already masked interrupts and
/// derived its logical index.
///
/// The masking and the derivation are the caller's obligation here, which is
/// the point: the release path masks once and derived the same index six
/// times, at roughly two hundred cycles apiece.
#[cfg(rustos_boot_image)]
fn with_current_stack_on<R>(cpu: usize, f: impl FnOnce(&mut HeldLockStack) -> R) -> R {
    // SAFETY: one CPU owns its stack and the caller holds interrupts masked for
    // the complete mutation. RustOS does not migrate a running kernel frame.
    f(unsafe { &mut *HELD_STACKS.0[cpu].get() })
}

/// Finds `owner`'s registered stack slot, visiting only registered slots.
#[cfg(rustos_boot_image)]
fn find_task_stack(owner: u64) -> Option<(usize, &'static TaskHeldStack)> {
    for (word_index, word) in TASK_STACK_OCCUPANCY.iter().enumerate() {
        // ORDERING: Acquire observes the owner publication that preceded this
        // occupancy bit being set.
        let mut bits = word.load(Ordering::Acquire);
        while let Some(bit) = next_occupied_bit(&mut bits) {
            let index = word_index * 64 + bit;
            let entry = &TASK_HELD_STACKS[index];
            // ORDERING: Acquire observes the complete task-owned lock stack
            // published before its owner slot was released or retained.
            if entry.owner.load(Ordering::Acquire) == owner {
                return Some((index, entry));
            }
        }
    }
    None
}

#[cfg(rustos_boot_image)]
fn with_task_stack<R>(
    owner: u64,
    create: bool,
    f: impl FnOnce(&mut HeldLockStack) -> R,
) -> Option<R> {
    if let Some((_, entry)) = find_task_stack(owner) {
        // SAFETY: a task cannot execute concurrently on two CPUs, and its
        // stack remains registered until the last held class is released.
        return Some(f(unsafe { &mut *entry.stack.get() }));
    }
    if !create {
        return None;
    }
    let (index, entry) = TASK_HELD_STACKS.iter().enumerate().find(|(_, entry)| {
        // ORDERING: AcqRel claims one empty stack exclusively; Acquire failure
        // observes the winning task owner before another slot is considered.
        entry
            .owner
            .compare_exchange(0, owner, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    })?;
    // ORDERING: Release publishes the owner word before a scan can observe the
    // slot, so a reader that sees the bit also sees the owner it belongs to.
    TASK_STACK_OCCUPANCY[index / 64].fetch_or(1_u64 << (index % 64), Ordering::Release);
    // SAFETY: the successful owner publication gives this task exclusive
    // ownership of the empty stack until release_task_stack().
    Some(f(unsafe { &mut *entry.stack.get() }))
}

#[cfg(rustos_boot_image)]
fn task_stack_depth(owner: u64) -> usize {
    if owner == 0 {
        return 0;
    }
    with_task_stack(owner, false, |stack| stack.len).unwrap_or(0)
}

#[cfg(rustos_boot_image)]
fn release_task_stack(owner: u64) {
    // ORDERING: Acquire identifies the exact live owner publication before its
    // empty stack is validated; Release below makes the slot reusable.
    let (index, entry) = find_task_stack(owner).expect("sleepable lock-class owner disappeared");
    // SAFETY: the task owns this entry and has just observed an empty stack.
    assert_eq!(unsafe { (*entry.stack.get()).len }, 0);
    // ORDERING: the occupancy bit clears before the owner word, so the slot
    // leaves every scan while it is still unclaimable. Clearing in the other
    // order would let this release erase a new owner's freshly set bit.
    TASK_STACK_OCCUPANCY[index / 64].fetch_and(!(1_u64 << (index % 64)), Ordering::AcqRel);
    // ORDERING: Release publishes all final stack mutations before a new task
    // may acquire and reuse this owner slot.
    entry.owner.store(0, Ordering::Release);
}

/// Takes the lowest set bit out of an occupancy word.
#[cfg(any(rustos_boot_image, test))]
#[inline]
fn next_occupied_bit(bits: &mut u64) -> Option<usize> {
    if *bits == 0 {
        return None;
    }
    let bit = bits.trailing_zeros() as usize;
    *bits &= *bits - 1;
    Some(bit)
}

#[cfg(test)]
mod tests {
    use super::cpu_identity::{decode_cpu_token, select_cpu_index};
    use super::{
        graph_reaches, guard_release_is_admissible, preemption_release_is_admissible,
        preemption_units_match,
    };

    #[test]
    fn occupancy_bits_are_visited_in_ascending_slot_order() {
        let mut bits = (1_u64 << 0) | (1 << 3) | (1 << 63);
        let mut seen = alloc::vec::Vec::new();
        while let Some(bit) = super::next_occupied_bit(&mut bits) {
            seen.push(bit);
        }
        assert_eq!(seen, [0, 3, 63]);
        assert_eq!(bits, 0);
    }

    #[test]
    fn an_empty_occupancy_word_visits_nothing() {
        // This is the common case: no task holds a sleepable class, so a
        // tracked spin acquisition must not walk the stack table at all.
        let mut bits = 0_u64;
        assert_eq!(super::next_occupied_bit(&mut bits), None);
    }

    #[test]
    fn dependency_walk_detects_transitive_cycle_edge() {
        let mut rows = [0_u64; 64];
        rows[1] = 1 << 2;
        rows[2] = 1 << 3;
        assert!(graph_reaches(1, 3, |node| rows[node]));
        assert!(!graph_reaches(3, 1, |node| rows[node]));
        rows[3] = 1 << 1;
        assert!(graph_reaches(3, 2, |node| rows[node]));
    }

    #[test]
    fn dense_apic_identity_map_does_not_index_by_raw_apic_id() {
        let identities = [1_u64, u64::from(0x1234_u32) + 1, 8];
        assert_eq!(select_cpu_index(identities, 0), Some(0));
        assert_eq!(select_cpu_index(identities, 0x1234), Some(1));
        assert_eq!(select_cpu_index(identities, 7), Some(2));
        assert_eq!(select_cpu_index(identities, 2), None);
        assert_eq!(decode_cpu_token(0, 3), None);
        assert_eq!(decode_cpu_token(1, 3), Some(0));
        assert_eq!(decode_cpu_token(3, 3), Some(2));
        assert_eq!(decode_cpu_token(4, 3), None);
    }

    #[test]
    fn tracked_guard_release_requires_same_cpu_apic_and_positive_depth() {
        assert!(guard_release_is_admissible(1, 0x1234, 1, 0x1234, 1));
        assert!(guard_release_is_admissible(1, 0x1234, 1, 0x1234, 3));
        assert!(!guard_release_is_admissible(1, 0x1234, 0, 0, 1));
        assert!(!guard_release_is_admissible(1, 0x1234, 1, 0x4321, 1));
        assert!(!guard_release_is_admissible(1, 0x1234, 1, 0x1234, 0));
    }

    #[test]
    fn pending_acquire_units_cannot_consume_a_held_guard_pin() {
        assert!(preemption_units_match(1, 1, 0));
        assert!(preemption_units_match(1, 0, 1));
        assert!(preemption_units_match(2, 1, 1));
        assert!(!preemption_units_match(0, 1, 0));
        assert!(preemption_release_is_admissible(2, 1, 0));
        assert!(preemption_release_is_admissible(2, 0, 1));
        assert!(!preemption_release_is_admissible(1, 1, 0));
    }
}
