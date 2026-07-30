//! Allocation-free lock-class dependency tracking for ring0 leaf locks.
//!
//! The tracker records observed class ordering before a spin acquisition and
//! rejects recursion or a dependency edge that closes a cycle. Raw-spin state
//! is CPU-owned and interrupt-atomic. Sleepable state is keyed by scheduler
//! task identity so it survives blocking and resumption without leaking into
//! the next task dispatched on the BSP.

use core::ops::{Deref, DerefMut};
#[cfg(rustos_boot_image)]
use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    panic::Location,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use spin::{Mutex, MutexGuard};

pub const MAX_LOCK_CLASSES: usize = 64;
#[cfg(rustos_boot_image)]
const MAX_TRACKED_CPUS: usize = 1;
#[cfg(rustos_boot_image)]
const MAX_HELD_LOCK_DEPTH: usize = 16;
#[cfg(rustos_boot_image)]
const MAX_TRACKED_TASK_LOCK_STACKS: usize = 512;
#[cfg(rustos_boot_image)]
const SPIN_LIMIT: usize = 100_000;

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
unsafe impl Sync for TaskHeldStack {}

#[cfg(rustos_boot_image)]
static DEPENDENCIES: [AtomicU64; MAX_LOCK_CLASSES] =
    [const { AtomicU64::new(0) }; MAX_LOCK_CLASSES];
#[cfg(rustos_boot_image)]
static HELD_STACKS: PerCpuHeldStacks =
    PerCpuHeldStacks([const { UnsafeCell::new(HeldLockStack::new()) }; MAX_TRACKED_CPUS]);
#[cfg(rustos_boot_image)]
static TASK_HELD_STACKS: [TaskHeldStack; MAX_TRACKED_TASK_LOCK_STACKS] =
    [const { TaskHeldStack::new() }; MAX_TRACKED_TASK_LOCK_STACKS];
#[cfg(rustos_boot_image)]
static PREEMPT_DISABLE_DEPTH: AtomicUsize = AtomicUsize::new(0);
#[cfg(rustos_boot_image)]
static IRQ_CONTEXT_DEPTH: AtomicUsize = AtomicUsize::new(0);
#[cfg(rustos_boot_image)]
static IRQ_SAFE_CLASSES: AtomicU64 = AtomicU64::new(0);
#[cfg(rustos_boot_image)]
static IRQ_UNSAFE_CLASSES: AtomicU64 = AtomicU64::new(0);
#[cfg(rustos_boot_image)]
static CURRENT_TASK_OWNER: AtomicU64 = AtomicU64::new(0);
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
/// RustOS is currently BSP-only. Every target-kernel guard disables task
/// preemption for its complete lifetime without globally masking device
/// interrupts. Locks shared with an IRQ leaf must additionally use the local
/// `without_interrupts` wrapper at their process-context call sites. This keeps
/// unrelated clock/input/device latency independent of ordinary process and
/// IPC critical sections. An SMP port must replace the single BSP counter with
/// per-CPU preemption and current-task accounting before enabling another CPU.
pub struct TrackedSpinLock<T: ?Sized, const CLASS: u8> {
    inner: Mutex<T>,
}

pub struct TrackedSpinGuard<'a, T: ?Sized, const CLASS: u8> {
    guard: Option<MutexGuard<'a, T>>,
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
        let previous = IRQ_CONTEXT_DEPTH.fetch_add(1, Ordering::AcqRel);
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
        IRQ_CONTEXT_DEPTH.load(Ordering::Acquire)
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
/// The BSP scheduler updates this immediately before a context switch becomes
/// visible. Interrupt entry deliberately retains the interrupted task token;
/// raw-lock acquisition ignores the task-owned stack while IRQ context is
/// active. SMP enablement must make this value per-CPU before bringing an AP
/// online.
#[inline]
pub fn set_current_task_owner(owner: u64) {
    #[cfg(rustos_boot_image)]
    {
        assert!(owner != 0, "lockdep current task owner must be nonzero");
        CURRENT_TASK_OWNER.store(owner, Ordering::Release);
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = owner;
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
        IRQ_UNSAFE_CLASSES.fetch_or(1_u64 << class_index, Ordering::SeqCst);
        with_task_stack(owner, true, |stack| {
            assert!(
                !stack.classes[..stack.len].contains(&class),
                "recursive sleepable lock-class acquisition class={}",
                class
            );
            for held in &stack.classes[..stack.len] {
                let held_index = usize::from(*held);
                DEPENDENCIES[held_index].fetch_or(1_u64 << class_index, Ordering::SeqCst);
                assert!(
                    !dependency_reaches(class_index, held_index),
                    "sleepable lock-class dependency cycle {} -> {}",
                    held,
                    class
                );
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
            let previous = IRQ_CONTEXT_DEPTH.fetch_sub(1, Ordering::AcqRel);
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
        #[cfg(rustos_boot_image)]
        disable_preemption();
        let pending = before_acquire(CLASS, acquire_site);
        #[cfg(rustos_boot_image)]
        let guard = {
            let spin_limit = SPIN_LIMIT;
            let mut spins = 0usize;
            loop {
                if let Some(guard) = self.inner.try_lock() {
                    break guard;
                }
                spins += 1;
                if spins >= spin_limit {
                    panic!(
                        "tracked spin lock contention exceeded bound class={} wait_at={}:{} spins={} preempt_depth={}",
                        CLASS,
                        acquire_site.file(),
                        acquire_site.line(),
                        spins,
                        preemption_depth()
                    );
                }
                spin_loop();
            }
        };
        #[cfg(not(rustos_boot_image))]
        let guard = self.inner.lock();
        after_acquire(pending);
        TrackedSpinGuard { guard: Some(guard) }
    }

    #[track_caller]
    pub fn try_lock(&self) -> Option<TrackedSpinGuard<'_, T, CLASS>> {
        #[cfg(rustos_boot_image)]
        disable_preemption();
        #[cfg(rustos_boot_image)]
        let acquire_site = Location::caller();
        #[cfg(not(rustos_boot_image))]
        let acquire_site = ();
        let pending = before_acquire(CLASS, acquire_site);
        if let Some(guard) = self.inner.try_lock() {
            after_acquire(pending);
            Some(TrackedSpinGuard { guard: Some(guard) })
        } else {
            #[cfg(rustos_boot_image)]
            enable_preemption();
            None
        }
    }
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
        drop(self.guard.take());
        release(CLASS);
        #[cfg(rustos_boot_image)]
        enable_preemption();
    }
}

/// Returns the BSP task-preemption nesting depth.
///
/// Interrupt handlers remain available while this is non-zero; only an
/// explicit task scheduler handoff is forbidden. The scheduler checks this
/// before every software reschedule entry.
#[inline]
pub fn preemption_depth() -> usize {
    #[cfg(rustos_boot_image)]
    {
        PREEMPT_DISABLE_DEPTH.load(Ordering::Acquire)
    }
    #[cfg(not(rustos_boot_image))]
    {
        0
    }
}

#[inline]
pub fn preemption_disabled() -> bool {
    preemption_depth() != 0
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
        let owner = CURRENT_TASK_OWNER.load(Ordering::Acquire);
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
            release(self.class);
            enable_preemption();
        }
    }
}

#[cfg(rustos_boot_image)]
fn disable_preemption() {
    let previous = PREEMPT_DISABLE_DEPTH.fetch_add(1, Ordering::AcqRel);
    assert!(
        previous < MAX_HELD_LOCK_DEPTH,
        "raw-spin preemption depth exceeded bound"
    );
}

#[cfg(rustos_boot_image)]
fn enable_preemption() {
    let previous = PREEMPT_DISABLE_DEPTH.fetch_sub(1, Ordering::AcqRel);
    assert!(previous != 0, "raw-spin preemption depth underflow");
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
    let class_index = validate_class(class);
    record_irq_usage(class_index, acquire_site);
    if irq_context_depth() == 0 {
        let owner = CURRENT_TASK_OWNER.load(Ordering::Acquire);
        if owner != 0 {
            with_task_stack(owner, false, |stack| {
                for held in &stack.classes[..stack.len] {
                    let held_index = usize::from(*held);
                    DEPENDENCIES[held_index].fetch_or(1_u64 << class_index, Ordering::SeqCst);
                    assert!(
                        !dependency_reaches(class_index, held_index),
                        "sleepable-to-raw lock-class dependency cycle {} -> {} acquire={}:{}",
                        held,
                        class,
                        acquire_site.file(),
                        acquire_site.line(),
                    );
                }
            });
        }
    }
    with_current_stack(|stack| {
        assert!(
            !stack.classes[..stack.len].contains(&class),
            "recursive lock-class acquisition class={}",
            class
        );
        for held in &stack.classes[..stack.len] {
            let held_index = usize::from(*held);
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
        }
    });
    PendingAcquire { class }
}

#[cfg(rustos_boot_image)]
fn record_irq_usage(class: usize, acquire_site: &'static Location<'static>) {
    let bit = 1_u64 << class;
    if irq_context_depth() != 0 {
        let unsafe_classes = IRQ_UNSAFE_CLASSES.load(Ordering::SeqCst);
        assert!(
            unsafe_classes & bit == 0,
            "IRQ-unsafe lock class acquired in interrupt context class={} acquire={}:{}",
            class,
            acquire_site.file(),
            acquire_site.line(),
        );
        IRQ_SAFE_CLASSES.fetch_or(bit, Ordering::SeqCst);
        assert!(
            !class_reaches_any(class, unsafe_classes),
            "IRQ-safe lock class reaches an IRQ-unsafe class class={}",
            class
        );
    } else if x86_64::instructions::interrupts::are_enabled() {
        let safe_classes = IRQ_SAFE_CLASSES.load(Ordering::SeqCst);
        assert!(
            safe_classes & bit == 0,
            "IRQ-safe lock class acquired with interrupts enabled class={} acquire={}:{}",
            class,
            acquire_site.file(),
            acquire_site.line(),
        );
        IRQ_UNSAFE_CLASSES.fetch_or(bit, Ordering::SeqCst);
        assert!(
            !any_class_reaches(safe_classes, class),
            "IRQ-unsafe lock class is reachable from an IRQ-safe class class={}",
            class
        );
    }
}

#[cfg(rustos_boot_image)]
fn irq_dependency_conflicts(held: usize, acquired: usize) -> bool {
    let safe_classes = IRQ_SAFE_CLASSES.load(Ordering::SeqCst);
    let unsafe_classes = IRQ_UNSAFE_CLASSES.load(Ordering::SeqCst);
    (safe_classes & (1_u64 << held) != 0 || any_class_reaches(safe_classes, held))
        && (unsafe_classes & (1_u64 << acquired) != 0
            || class_reaches_any(acquired, unsafe_classes))
}

#[cfg(rustos_boot_image)]
fn class_reaches_any(class: usize, targets: u64) -> bool {
    targets != 0
        && (0..MAX_LOCK_CLASSES)
            .any(|target| targets & (1_u64 << target) != 0 && dependency_reaches(class, target))
}

#[cfg(rustos_boot_image)]
fn any_class_reaches(classes: u64, target: usize) -> bool {
    classes != 0
        && (0..MAX_LOCK_CLASSES)
            .any(|class| classes & (1_u64 << class) != 0 && dependency_reaches(class, target))
}

#[cfg(not(rustos_boot_image))]
fn before_acquire(class: u8, _acquire_site: ()) -> PendingAcquire {
    let _ = validate_class(class);
    PendingAcquire {}
}

#[cfg(rustos_boot_image)]
fn after_acquire(pending: PendingAcquire) {
    with_current_stack(|stack| {
        assert!(
            stack.len < stack.classes.len(),
            "lock-class nesting exceeds {}",
            MAX_HELD_LOCK_DEPTH
        );
        stack.classes[stack.len] = pending.class;
        stack.len += 1;
    });
}

#[cfg(not(rustos_boot_image))]
fn after_acquire(_pending: PendingAcquire) {}

#[cfg(rustos_boot_image)]
fn release(class: u8) {
    with_current_stack(|stack| {
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
fn dependency_reaches(start: usize, target: usize) -> bool {
    graph_reaches(start, target, |node| {
        DEPENDENCIES[node].load(Ordering::SeqCst)
    })
}

#[cfg(any(rustos_boot_image, test))]
fn graph_reaches(start: usize, target: usize, mut edges: impl FnMut(usize) -> u64) -> bool {
    let mut frontier = 1_u64 << start;
    let mut visited = 0_u64;
    while frontier != 0 {
        let node = frontier.trailing_zeros() as usize;
        let bit = 1_u64 << node;
        frontier &= !bit;
        if node == target {
            return true;
        }
        if visited & bit != 0 {
            continue;
        }
        visited |= bit;
        frontier |= edges(node) & !visited;
    }
    false
}

#[cfg(rustos_boot_image)]
fn with_current_stack<R>(f: impl FnOnce(&mut HeldLockStack) -> R) -> R {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let cpu = current_cpu_index();
        // SAFETY: one CPU owns its stack and interrupts are disabled for the
        // complete mutation. RustOS does not migrate a running kernel frame.
        f(unsafe { &mut *HELD_STACKS.0[cpu].get() })
    })
}

#[cfg(rustos_boot_image)]
fn with_task_stack<R>(
    owner: u64,
    create: bool,
    f: impl FnOnce(&mut HeldLockStack) -> R,
) -> Option<R> {
    if let Some(entry) = TASK_HELD_STACKS
        .iter()
        .find(|entry| entry.owner.load(Ordering::Acquire) == owner)
    {
        // SAFETY: a task cannot execute concurrently on two CPUs, and its
        // stack remains registered until the last held class is released.
        return Some(f(unsafe { &mut *entry.stack.get() }));
    }
    if !create {
        return None;
    }
    let entry = TASK_HELD_STACKS.iter().find(|entry| {
        entry
            .owner
            .compare_exchange(0, owner, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    })?;
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
    let entry = TASK_HELD_STACKS
        .iter()
        .find(|entry| entry.owner.load(Ordering::Acquire) == owner)
        .expect("sleepable lock-class owner disappeared");
    // SAFETY: the task owns this entry and has just observed an empty stack.
    assert_eq!(unsafe { (*entry.stack.get()).len }, 0);
    entry.owner.store(0, Ordering::Release);
}

#[cfg(rustos_boot_image)]
fn current_cpu_index() -> usize {
    // The scheduler and boot contract are BSP-only. Avoid a serializing CPUID
    // on every lock operation; enabling another CPU is forbidden until the
    // scheduler, per-CPU stacks, and this index are upgraded together.
    0
}

#[cfg(test)]
mod tests {
    use super::graph_reaches;

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
}
