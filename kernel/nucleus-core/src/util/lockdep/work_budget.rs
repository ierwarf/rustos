//! Declared ceilings on how many times a scope may take a lock class.
//!
//! - **Owner:** this module owns the per-CPU acquisition counters and the scope
//!   declaration. The counts are charged by `lockdep`'s own acquire paths,
//!   which already hold the CPU index and the class.
//! - **Boundary:** a ceiling asserts something about this kernel's own code.
//!   No caller-supplied value reaches it, so exceeding one is a kernel defect
//!   and never a rejected request.
//! - **Lifecycle:** declare a ceiling, do the work, drop the guard; the drop
//!   compares the delta against the ceiling.
//! - **Concurrency:** the counters are CPU-private and read-modify-written
//!   without an atomic RMW. The guard records the CPU and the running task and
//!   declines to judge when either changed, so preemption, migration, and a
//!   blocking scope cannot manufacture a failure.
//! - **Failure:** exceeding a declared ceiling panics, naming the class, the
//!   ceiling, the count, and the declaring site.
//! - **Forbidden:** never declare a *lock-class* ceiling on a class an interrupt
//!   handler can take. `LockClass::Scheduler` is the clear example: the timer
//!   dispatch takes it, so an unrelated handler firing inside a budgeted scope
//!   would land in this CPU's counter. Sleepable classes qualify by
//!   construction -- `record_sleepable_acquire` already fails an acquisition
//!   from interrupt context -- and a raw class qualifies only with a stated
//!   argument that no handler reaches it. The identity ceiling carries no such
//!   restriction, because `IrqContextGuard` restores the derivation count a
//!   handler found on entry; that is not a courtesy but the fix for the first
//!   run of it, which charged an acquisition with six derivations it never
//!   made.
//! - **Evidence:** `docs/benchmarks/README.md`.
//!
//! # Why cost gets an assertion
//!
//! Correctness invariants in this kernel panic. Cost invariants did not, and
//! the difference showed. A synchronous receive bound the caller's address
//! space eight times to write four words. It produced exactly the right bytes,
//! so no assertion in the kernel had anything to object to, and the defect
//! survived until a benchmark priced it at 12,500 cycles per IPC call -- 11% of
//! a round trip. A benchmark finds that after it ships and only if somebody
//! runs it. A declared ceiling finds it in whichever test first exceeds it,
//! with the site that declared the ceiling in the panic message.
//!
//! The ceiling is deliberately the *exact* count the path is designed to
//! perform, not a generous bound. A bound loose enough never to fire is a bound
//! that would not have caught the eight.

use core::panic::Location;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::{LockClass, MAX_LOCK_CLASSES, MAX_TRACKED_CPUS, current_cpu_index};

/// Per-CPU, per-class acquisition counts. Monotonic and free-running; a scope
/// reads the delta across its own lifetime, so wrapping is harmless.
static ACQUIRES: [[AtomicU32; MAX_LOCK_CLASSES]; MAX_TRACKED_CPUS] =
    [const { [const { AtomicU32::new(0) }; MAX_LOCK_CLASSES] }; MAX_TRACKED_CPUS];

/// Records one acquisition of `class_index` on `cpu`.
///
/// Called from the acquire paths, which already derived both, so this is one
/// bounds-checked index and one increment.
#[inline]
pub(crate) fn charge_acquire(cpu: usize, class_index: usize) {
    let Some(counter) = ACQUIRES
        .get(cpu)
        .and_then(|classes| classes.get(class_index))
    else {
        return;
    };
    // The counter is CPU-private, so this needs no atomic read-modify-write.
    // An interrupt landing between the load and the store loses one count,
    // which can only understate a scope; it can never fail one that behaved.
    counter.store(
        counter.load(Ordering::Relaxed).wrapping_add(1),
        Ordering::Relaxed,
    );
}

/// Per-site acquisition census for one selected lock class.
///
/// The per-class census answers *which* lock a workload pays for; it cannot
/// answer *who* is taking it, and that is the question a redundant acquisition
/// hides behind. The scheduler catalog was split only after its callers were
/// named, and the IPC object locks needed the same treatment.
///
/// One class at a time, because the table is a fixed side allocation and a
/// per-class-per-caller matrix is not worth its cache footprint on the acquire
/// path. `select_site_census_class` chooses it; zero disables the census
/// entirely, which is the default and costs one relaxed load per acquisition.
const SITE_CENSUS_SLOTS: usize = 32;
const _: () = assert!(SITE_CENSUS_SLOTS.is_power_of_two());

static SITE_CENSUS_CLASS: AtomicU32 = AtomicU32::new(0);
static SITE_CENSUS_CALLERS: [AtomicPtr<Location<'static>>; SITE_CENSUS_SLOTS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; SITE_CENSUS_SLOTS];
static SITE_CENSUS_COUNTS: [AtomicU64; SITE_CENSUS_SLOTS] =
    [const { AtomicU64::new(0) }; SITE_CENSUS_SLOTS];

/// The class whose acquire sites are currently counted, or zero for none.
pub fn site_census_class() -> usize {
    SITE_CENSUS_CLASS.load(Ordering::Relaxed) as usize
}

/// Selects the class whose acquire sites are counted. Zero disables it.
pub fn select_site_census_class(class_index: usize) {
    SITE_CENSUS_CLASS.store(class_index as u32, Ordering::Relaxed);
    for caller in &SITE_CENSUS_CALLERS {
        caller.store(core::ptr::null_mut(), Ordering::Relaxed);
    }
    for count in &SITE_CENSUS_COUNTS {
        count.store(0, Ordering::Relaxed);
    }
}

/// Charges one acquisition of `class_index` to `caller` when that class is the
/// selected one. A direct-mapped bucket keeps the common case one line.
#[inline]
pub(crate) fn charge_site(class_index: usize, caller: &'static Location<'static>) {
    if SITE_CENSUS_CLASS.load(Ordering::Relaxed) as usize != class_index {
        return;
    }
    let key = caller as *const Location<'static> as *mut Location<'static>;
    // Fibonacci hashing of the pointer; `Location` allocations are aligned, so
    // the low bits alone would collide across every site.
    let mixed = (key as usize as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let first = (mixed >> 32) as usize & (SITE_CENSUS_SLOTS - 1);
    for probe in 0..SITE_CENSUS_SLOTS {
        let slot = (first + probe) & (SITE_CENSUS_SLOTS - 1);
        // ORDERING: Acquire observes a claim published by whichever CPU first
        // registered this site.
        let current = SITE_CENSUS_CALLERS[slot].load(Ordering::Acquire);
        if current == key {
            SITE_CENSUS_COUNTS[slot].fetch_add(1, Ordering::Relaxed);
            return;
        }
        if current.is_null()
            // ORDERING: Release publishes the claim before its count is used.
            && SITE_CENSUS_CALLERS[slot]
                .compare_exchange(
                    core::ptr::null_mut(),
                    key,
                    Ordering::Release,
                    Ordering::Acquire,
                )
                .is_ok()
        {
            SITE_CENSUS_COUNTS[slot].fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
}

/// Takes the per-site census so far, clearing counts for the next window.
///
/// Callers must be outside every tracked lock; rendering the result takes the
/// debug sink.
pub fn take_site_census() -> [(&'static str, u32, u64); SITE_CENSUS_SLOTS] {
    let mut census = [("", 0_u32, 0_u64); SITE_CENSUS_SLOTS];
    for slot in 0..SITE_CENSUS_SLOTS {
        let count = SITE_CENSUS_COUNTS[slot].swap(0, Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        // ORDERING: Acquire pairs with the claim publication above.
        let caller = SITE_CENSUS_CALLERS[slot].load(Ordering::Acquire);
        if caller.is_null() {
            continue;
        }
        // SAFETY: `Location::caller` returns a static allocation that outlives
        // every observer of this table.
        let caller = unsafe { &*caller };
        census[slot] = (caller.file(), caller.line(), count);
    }
    census
}

/// Takes and clears every CPU's per-class acquisition counts.
///
/// A scope ceiling proves one path behaves; this answers the question a
/// ceiling cannot, which is *which* class a workload is actually paying for.
/// The scheduler catalog was found and split that way, and the lock classes
/// under the IPC round trip needed the same census rather than a reading of
/// the call graph.
///
/// Callers must be outside every tracked lock: the counters are diagnostics,
/// and rendering them takes the debug sink.
pub fn take_class_census() -> [u64; MAX_LOCK_CLASSES] {
    let mut census = [0_u64; MAX_LOCK_CLASSES];
    for classes in &ACQUIRES {
        for (class_index, counter) in classes.iter().enumerate() {
            let count = counter.swap(0, Ordering::Relaxed);
            census[class_index] = census[class_index].saturating_add(u64::from(count));
        }
    }
    census
}

/// Further derivations of this CPU's logical index inside one lock
/// acquisition, after the one the acquire path already made.
///
/// This is not an ABI limit -- nothing outside the kernel can observe how often
/// it reads CPU-local architectural state -- so it lives with the module that
/// owns the counter rather than in `rustos_user_abi::performance`.
///
/// An acquisition holds the CPU stable, by masked interrupts or disabled
/// preemption, so every step after the first derivation can be handed the
/// answer. One sleepable acquisition instead derived it five times: its own
/// wait-context assertion asked twice, and the lockdep record that followed
/// asked the same two questions again and then once more to charge the
/// acquisition. The four extra produced identical answers and cost roughly 350
/// ticks -- more than the user-memory copy the acquisition existed to
/// authorize.
///
/// Zero is the only defensible value. A path that threads its index performs no
/// further derivation by construction, and a helper that later reaches for
/// `current_cpu_index()` instead of an `_on` form returns exactly the same
/// result, so nothing but this would notice.
pub const LOCK_ACQUIRE_MAX_EXTRA_CPU_IDENTITY_DERIVATIONS: u32 = 0;

/// Per-CPU count of logical-identity derivations. Same shape and same reason as
/// [`ACQUIRES`]: monotonic, free-running, read as a delta.
static IDENTITY_DERIVATIONS: [AtomicU32; MAX_TRACKED_CPUS] =
    [const { AtomicU32::new(0) }; MAX_TRACKED_CPUS];

/// Where each CPU last derived its index.
///
/// A `&'static Location` is one pointer, so remembering the site costs one
/// relaxed store against a function that executes `RDTSCP`. Without it the
/// ceiling below reports that a scope derived once too often and leaves the
/// reader to find which of its callees did it; the first time it fired, that
/// search was the whole cost of the diagnostic.
static LAST_IDENTITY_SITE: [AtomicUsize; MAX_TRACKED_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_TRACKED_CPUS];

/// Records one derivation of this CPU's logical index.
///
/// Charged by `cpu_identity::current_cpu_index` with the index it just derived,
/// so this adds no derivation of its own.
/// Charges one derivation without naming its site.
///
/// The count is what every declared ceiling asserts on, so it is unconditional.
/// The site is not: carrying it requires `#[track_caller]` on a function called
/// about 124 times per dispatch. See `cpu_identity::current_cpu_index`.
#[inline]
pub(crate) fn charge_identity_derivation_count(cpu: usize) {
    let Some(counter) = IDENTITY_DERIVATIONS.get(cpu) else {
        return;
    };
    // The counter is CPU-private, so this needs no atomic read-modify-write.
    counter.store(
        counter.load(Ordering::Relaxed).wrapping_add(1),
        Ordering::Relaxed,
    );
}

#[cfg_attr(not(rustos_lock_phase_profile), expect(dead_code, reason = "diagnosis build only"))]
pub(crate) fn charge_identity_derivation(cpu: usize, site: &'static Location<'static>) {
    charge_identity_derivation_count(cpu);
    if let Some(slot) = LAST_IDENTITY_SITE.get(cpu) {
        slot.store(core::ptr::from_ref(site) as usize, Ordering::Relaxed);
    }
    charge_identity_site(site);
}

/// Per-site census of logical-index derivations.
///
/// The total says how many `RDTSCP` executions a workload pays for; only the
/// per-site count says which caller to give the answer to instead. Same
/// direct-mapped, self-validating shape as the lock-site census, and gated the
/// same way: it is only populated in a diagnosis build.
static IDENTITY_SITE_CALLERS: [AtomicPtr<Location<'static>>; SITE_CENSUS_SLOTS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; SITE_CENSUS_SLOTS];
static IDENTITY_SITE_COUNTS: [AtomicU64; SITE_CENSUS_SLOTS] =
    [const { AtomicU64::new(0) }; SITE_CENSUS_SLOTS];

#[inline]
fn charge_identity_site(site: &'static Location<'static>) {
    if !cfg!(rustos_lock_phase_profile) {
        return;
    }
    let key = site as *const Location<'static> as *mut Location<'static>;
    let mixed = (key as usize as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let first = (mixed >> 32) as usize & (SITE_CENSUS_SLOTS - 1);
    for probe in 0..SITE_CENSUS_SLOTS {
        let slot = (first + probe) & (SITE_CENSUS_SLOTS - 1);
        // ORDERING: Acquire observes a claim published by whichever CPU first
        // registered this site.
        let current = IDENTITY_SITE_CALLERS[slot].load(Ordering::Acquire);
        if current == key {
            IDENTITY_SITE_COUNTS[slot].fetch_add(1, Ordering::Relaxed);
            return;
        }
        if current.is_null()
            // ORDERING: Release publishes the claim before its count is used.
            && IDENTITY_SITE_CALLERS[slot]
                .compare_exchange(
                    core::ptr::null_mut(),
                    key,
                    Ordering::Release,
                    Ordering::Acquire,
                )
                .is_ok()
        {
            IDENTITY_SITE_COUNTS[slot].fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
}

/// Takes the per-site derivation census. Callers must be outside every tracked
/// lock; rendering the result takes the debug sink.
pub fn take_identity_site_census() -> [(&'static str, u32, u64); SITE_CENSUS_SLOTS] {
    let mut census = [("", 0_u32, 0_u64); SITE_CENSUS_SLOTS];
    for slot in 0..SITE_CENSUS_SLOTS {
        let count = IDENTITY_SITE_COUNTS[slot].swap(0, Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        // ORDERING: Acquire pairs with the claim publication above.
        let caller = IDENTITY_SITE_CALLERS[slot].load(Ordering::Acquire);
        if caller.is_null() {
            continue;
        }
        // SAFETY: `Location::caller` returns a static allocation that outlives
        // every observer of this table.
        let caller = unsafe { &*caller };
        census[slot] = (caller.file(), caller.line(), count);
    }
    census
}

/// The site of `cpu`'s most recent derivation, for a failing ceiling to name.
fn last_identity_site(cpu: usize) -> Option<&'static Location<'static>> {
    let raw = LAST_IDENTITY_SITE.get(cpu)?.load(Ordering::Relaxed);
    if raw == 0 {
        return None;
    }
    // SAFETY: the only value ever stored here is a `&'static Location` written
    // by `charge_identity_derivation`, and a `Location` from `Location::caller`
    // has static storage duration.
    Some(unsafe { &*(raw as *const Location<'static>) })
}

/// The current derivation count on `cpu`, for IRQ entry to snapshot.
#[cfg(rustos_boot_image)]
pub(crate) fn identity_count_on(cpu: usize) -> u32 {
    identity_count(cpu)
}

/// Puts `cpu`'s derivation count back to what IRQ entry found.
///
/// A handler's own derivations are the handler's, so an interrupted scope must
/// not be charged for them. Restoring rather than subtracting keeps nested
/// handlers exact: each restores the count its own entry observed.
#[cfg(rustos_boot_image)]
pub(crate) fn restore_identity_count_on(cpu: usize, value: u32) {
    let Some(counter) = IDENTITY_DERIVATIONS.get(cpu) else {
        return;
    };
    counter.store(value, Ordering::Relaxed);
}

/// Total logical-index derivations across every CPU since boot.
///
/// Each one executes `RDTSCP`. The count is what turns "the dispatch derives
/// its own CPU a few times" from an inference into a number; a scope ceiling
/// proves one path behaves, and this says how many the system actually pays.
pub fn total_identity_derivations() -> u64 {
    IDENTITY_DERIVATIONS
        .iter()
        .map(|counter| u64::from(counter.load(Ordering::Relaxed)))
        .sum()
}

fn identity_count(cpu: usize) -> u32 {
    IDENTITY_DERIVATIONS
        .get(cpu)
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// A scope's declared ceiling on further derivations of this CPU's index.
///
/// Unlike [`LockBudget`], neither opening nor closing this derives an index:
/// it is handed one and trusts the caller's stability contract. An instrument
/// that performed the operation it is counting would cost more than the
/// duplication it exists to find.
#[must_use = "the ceiling is checked when the budget is dropped, so it must outlive the work"]
pub struct IdentityBudget {
    ceiling: u32,
    entry_count: u32,
    cpu: usize,
    owner: u64,
    epoch: u32,
    site: &'static Location<'static>,
}

/// Declares that the calling scope derives this CPU's logical index at most
/// `ceiling` further times.
///
/// # Contract
///
/// `cpu` must be this CPU's index and interrupts must stay masked for the
/// guard's whole life. That is asserted, not documented, because the weaker
/// contract does not work: the first version of this allowed "preemption
/// disabled" and was declared on the raw-spin acquire path, which runs with
/// interrupts on. Handler derivations were excluded by `IrqContextGuard`, but
/// the context-switch commit the timer stub runs *after* that guard is dropped
/// was not, and the ceiling failed an acquisition that had behaved. A scope
/// that cannot be interrupted has no such window. The raw-spin path keeps the
/// same property through a source witness instead.
///
/// # Why zero is the usual ceiling
///
/// A path that derived the index and threaded it into its callees performs no
/// further derivation by construction. The ceiling is how that stays true: a
/// helper that later reaches for `current_cpu_index()` instead of an `_on`
/// form produces identical results and costs a hardware read, which no test
/// and no assertion would otherwise notice. One lock acquisition accumulated
/// five that way.
#[track_caller]
pub fn declare_identity_derivations_on(cpu: usize, ceiling: u32) -> IdentityBudget {
    // Host unit tests run with interrupts enabled and no kernel to mask them,
    // so the contract is checked where it means something.
    #[cfg(rustos_boot_image)]
    assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "identity budget declared on an interruptible scope at {}:{}",
        Location::caller().file(),
        Location::caller().line(),
    );
    IdentityBudget {
        ceiling,
        entry_count: identity_count(cpu),
        cpu,
        owner: running_owner(cpu),
        epoch: running_epoch(cpu),
        site: Location::caller(),
    }
}

impl IdentityBudget {
    /// Derivations charged to this scope, or `None` when this CPU stopped
    /// running the task that declared it.
    pub fn used(&self) -> Option<u32> {
        if running_owner(self.cpu) != self.owner || running_epoch(self.cpu) != self.epoch {
            return None;
        }
        Some(identity_count(self.cpu).wrapping_sub(self.entry_count))
    }
}

impl Drop for IdentityBudget {
    fn drop(&mut self) {
        // As in `LockBudget`: a counter that stopped being this scope's says
        // nothing about it. The stability contract makes migration impossible,
        // so the owner word is the only thing left that can change.
        let Some(used) = self.used() else {
            return;
        };
        let (last_file, last_line) = match last_identity_site(self.cpu) {
            Some(site) => (site.file(), site.line()),
            None => ("<unknown>", 0),
        };
        assert!(
            used <= self.ceiling,
            "cpu identity derived {} times in a scope declared for {} at {}:{}; last derivation at {}:{}",
            used,
            self.ceiling,
            self.site.file(),
            self.site.line(),
            last_file,
            last_line,
        );
    }
}

fn acquire_count(cpu: usize, class_index: usize) -> u32 {
    ACQUIRES
        .get(cpu)
        .and_then(|classes| classes.get(class_index))
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// The task the scheduler has published as running on `cpu`.
///
/// This is the only thing that tells a budget whether the counter it is about
/// to read is still its own. The owner word lives behind the same cfg as the
/// rest of lockdep; a build without it charges nothing either, so a constant
/// keeps the type checking honest in both configurations.
fn running_owner(cpu: usize) -> u64 {
    #[cfg(rustos_boot_image)]
    {
        super::CURRENT_TASK_OWNER
            .get(cpu)
            .map(|owner| owner.load(Ordering::Acquire))
            .unwrap_or(0)
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = cpu;
        0
    }
}

/// How many times `cpu` has published a running task.
///
/// The owner word alone cannot tell a scope that was never switched from one
/// that was switched away and switched back; both read the same owner at the
/// close. This does, and it is what a budget compares.
fn running_epoch(cpu: usize) -> u32 {
    #[cfg(rustos_boot_image)]
    {
        super::CURRENT_TASK_EPOCH
            .get(cpu)
            .map(|epoch| epoch.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = cpu;
        0
    }
}

/// A scope's declared ceiling on one lock class.
///
/// Dropping this with more acquisitions charged than declared panics. See the
/// module note for why the ceiling must be the exact designed count.
#[must_use = "the ceiling is checked when the budget is dropped, so it must outlive the work"]
pub struct LockBudget {
    class_index: usize,
    class: u8,
    ceiling: u32,
    entry_count: u32,
    cpu: usize,
    owner: u64,
    epoch: u32,
    site: &'static Location<'static>,
}

/// Declares that the calling scope acquires `class` at most `ceiling` times.
///
/// The ceiling is not advisory. See the module note for the classes this may
/// be used on.
#[track_caller]
pub fn declare(class: LockClass, ceiling: u32) -> LockBudget {
    let class = class as u8;
    let class_index = usize::from(class);
    let cpu = current_cpu_index();
    LockBudget {
        class_index,
        class,
        ceiling,
        entry_count: acquire_count(cpu, class_index),
        cpu,
        owner: running_owner(cpu),
        epoch: running_epoch(cpu),
        site: Location::caller(),
    }
}

impl LockBudget {
    /// How many acquisitions this scope has charged so far, or `None` when the
    /// scope moved CPU or was switched away and the counter is no longer its
    /// own.
    pub fn used(&self) -> Option<u32> {
        let cpu = current_cpu_index();
        if cpu != self.cpu || running_owner(cpu) != self.owner || running_epoch(cpu) != self.epoch {
            return None;
        }
        Some(acquire_count(cpu, self.class_index).wrapping_sub(self.entry_count))
    }
}

impl Drop for LockBudget {
    fn drop(&mut self) {
        // `used` returns `None` when this CPU is no longer running the task
        // that declared the budget. The counter then holds another task's
        // acquisitions and says nothing about this scope, so there is nothing
        // to judge. Skipping is what keeps a contended or preempted run from
        // panicking a kernel that behaved.
        let Some(used) = self.used() else {
            return;
        };
        assert!(
            used <= self.ceiling,
            "lock class {} acquired {} times in a scope declared for {} at {}:{}",
            self.class,
            used,
            self.ceiling,
            self.site.file(),
            self.site.line(),
        );
    }
}

#[cfg(test)]
mod tests {
    //! `ACQUIRES` and the identity-derivation counters are one process-global
    //! array each, and `cargo test` runs this module's cases on parallel
    //! threads against the same host CPU index. **Every case that charges a
    //! lock class must therefore own that class alone.** Two cases sharing one
    //! meant each could observe the other's charges: they passed in isolation
    //! and failed roughly one loaded run in ten, which reads as flakiness
    //! rather than as the sharing it is.
    use super::*;

    /// The ceiling is the point of the type, so the test that matters is that
    /// exceeding it fails rather than that staying inside it passes.
    #[test]
    fn a_scope_that_exceeds_its_declared_ceiling_panics() {
        let cpu = current_cpu_index();
        let class = LockClass::ProcessState;
        let class_index = usize::from(class as u8);

        let inside = std::panic::catch_unwind(|| {
            let budget = declare(class, 2);
            charge_acquire(cpu, class_index);
            charge_acquire(cpu, class_index);
            assert_eq!(budget.used(), Some(2));
            drop(budget);
        });
        assert!(inside.is_ok(), "a scope at its ceiling must not panic");

        let outside = std::panic::catch_unwind(|| {
            let budget = declare(class, 2);
            for _ in 0..3 {
                charge_acquire(cpu, class_index);
            }
            drop(budget);
        });
        assert!(outside.is_err(), "a scope past its ceiling must panic");
    }

    /// A budget must never fail a scope whose counter stopped being its own.
    /// Without this the first contended run turns a diagnostic into a crash.
    ///
    /// The class is `IpcMessage` rather than `ProcessState` for the reason in
    /// this module's test note: `ACQUIRES` is one process-global array and the
    /// suite runs in parallel, so sharing a class with
    /// `a_scope_that_exceeds_its_declared_ceiling_panics` meant each test could
    /// see the other's charges. It failed roughly one loaded run in ten while
    /// passing every run in isolation.
    #[test]
    fn a_scope_that_lost_the_cpu_or_the_task_declines_to_judge() {
        let cpu = current_cpu_index();
        let class_index = usize::from(LockClass::IpcMessage as u8);

        let mut budget = declare(LockClass::IpcMessage, 0);
        for _ in 0..8 {
            charge_acquire(cpu, class_index);
        }
        assert_eq!(budget.used(), Some(8), "an unmoved scope still counts");

        // Same CPU, different running task: the counter now mixes in work this
        // scope did not do.
        budget.owner = budget.owner.wrapping_add(1);
        assert_eq!(budget.used(), None);
        drop(budget);

        // A different CPU's counter is likewise not this scope's.
        let mut budget = declare(LockClass::IpcMessage, 0);
        charge_acquire(cpu, class_index);
        budget.cpu = cpu.wrapping_add(1) % MAX_TRACKED_CPUS;
        if budget.cpu != cpu {
            assert_eq!(budget.used(), None);
        }
        drop(budget);
    }

    /// Zero is the ceiling every acquire path declares, so the test that
    /// matters is that one further derivation fails it.
    #[test]
    fn one_extra_identity_derivation_past_the_ceiling_panics() {
        let cpu = current_cpu_index();

        let clean = std::panic::catch_unwind(|| {
            let budget = declare_identity_derivations_on(
                cpu,
                LOCK_ACQUIRE_MAX_EXTRA_CPU_IDENTITY_DERIVATIONS,
            );
            assert_eq!(budget.used(), Some(0));
            drop(budget);
        });
        assert!(
            clean.is_ok(),
            "a scope that derives nothing further must not panic"
        );

        let extra = std::panic::catch_unwind(|| {
            let budget = declare_identity_derivations_on(
                cpu,
                LOCK_ACQUIRE_MAX_EXTRA_CPU_IDENTITY_DERIVATIONS,
            );
            charge_identity_derivation(cpu, Location::caller());
            drop(budget);
        });
        assert!(
            extra.is_err(),
            "one re-derivation past the ceiling must panic"
        );
    }

    /// The instrument must not perform the operation it counts, or declaring a
    /// ceiling of zero would immediately violate it.
    #[test]
    fn declaring_and_closing_an_identity_budget_derives_nothing() {
        let cpu = current_cpu_index();
        let before = identity_count(cpu);
        let budget = declare_identity_derivations_on(cpu, 0);
        let used = budget.used();
        drop(budget);
        assert_eq!(used, Some(0));
        assert_eq!(
            identity_count(cpu),
            before,
            "the budget itself derived the index it exists to count"
        );
    }

    /// Free-running counters wrap. A scope that straddles the wrap must read
    /// its own delta, not a huge number that fails a path which behaved.
    #[test]
    fn a_counter_wrap_does_not_manufacture_a_violation() {
        let cpu = current_cpu_index();
        let class_index = usize::from(LockClass::IpcEndpoint as u8);
        ACQUIRES[cpu][class_index].store(u32::MAX - 1, Ordering::Relaxed);

        let budget = declare(LockClass::IpcEndpoint, 3);
        charge_acquire(cpu, class_index);
        charge_acquire(cpu, class_index);
        charge_acquire(cpu, class_index);
        assert_eq!(budget.used(), Some(3));
        drop(budget);
    }
}
