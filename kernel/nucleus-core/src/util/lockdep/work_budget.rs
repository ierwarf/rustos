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
//! - **Forbidden:** never declare a ceiling on a class an interrupt handler can
//!   take. `LockClass::Scheduler` is the clear example: the timer dispatch
//!   takes it, so an unrelated handler firing inside a budgeted scope would
//!   land in this CPU's counter. Sleepable classes qualify by construction --
//!   `record_sleepable_acquire` already fails an acquisition from interrupt
//!   context -- and a raw class qualifies only with a stated argument that no
//!   handler reaches it.
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
use core::sync::atomic::{AtomicU32, Ordering};

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
        site: Location::caller(),
    }
}

impl LockBudget {
    /// How many acquisitions this scope has charged so far, or `None` when the
    /// scope moved CPU or was switched away and the counter is no longer its
    /// own.
    pub fn used(&self) -> Option<u32> {
        let cpu = current_cpu_index();
        if cpu != self.cpu || running_owner(cpu) != self.owner {
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
    #[test]
    fn a_scope_that_lost_the_cpu_or_the_task_declines_to_judge() {
        let cpu = current_cpu_index();
        let class_index = usize::from(LockClass::ProcessState as u8);

        let mut budget = declare(LockClass::ProcessState, 0);
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
        let mut budget = declare(LockClass::ProcessState, 0);
        charge_acquire(cpu, class_index);
        budget.cpu = cpu.wrapping_add(1) % MAX_TRACKED_CPUS;
        if budget.cpu != cpu {
            assert_eq!(budget.used(), None);
        }
        drop(budget);
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
