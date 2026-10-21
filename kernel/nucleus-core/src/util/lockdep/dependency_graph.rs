//! Lock-class dependency graph and interrupt-context classification.
//!
//! - **Owner:** this module owns the global lock-class edge set, the IRQ-safe
//!   and IRQ-unsafe classifications, and every reachability query over them.
//! - **Boundary:** callers supply validated dense class indexes only; no raw
//!   lock, task, or CPU identity crosses this boundary.
//! - **Lifecycle:** edges and classifications are monotonic for the boot. An
//!   edge is admitted exactly once, after its cycle and conflict checks pass,
//!   and is never retracted.
//! - **Concurrency:** validation is a property of the edge set, not of each
//!   acquisition. Steady-state acquisitions take an acquire load of the
//!   validated-edge cache; only a genuinely new edge or classification pays the
//!   globally ordered publication and reachability search. Re-running that
//!   search per acquisition made this shared matrix the dominant cost of every
//!   nested-lock critical section and scaled it with CPU count.
//! - **Failure:** a cycle, a recursive class, or an IRQ-safety conflict is an
//!   immediate invariant panic; there is no degraded or best-effort mode.
//! - **Forbidden:** no edge retraction, no unvalidated fast path, and no
//!   publication of an edge whose checks have not passed.
//! - **Evidence:** `scheduler-cpu-ownership` and the lockdep unit witnesses.

use super::{MAX_LOCK_CLASSES, irq_context_depth_on};
use core::panic::Location;
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(rustos_boot_image)]
pub(super) static DEPENDENCIES: [AtomicU64; MAX_LOCK_CLASSES] =
    [const { AtomicU64::new(0) }; MAX_LOCK_CLASSES];
// Dependency edges whose cycle and IRQ-conflict checks have already passed.
//
// Lock-order validation is a safety property of the *edge set*, not of each
// acquisition. Republishing an already-known edge with a globally ordered
// read-modify-write and re-running the reachability search on every
// acquisition made this shared class matrix the dominant cost of every
// critical section that takes a nested lock, and that cost grows with CPU
// count because each publication invalidates the matrix on every other CPU.
// The serialized dispatch path takes several nested locks per turn, so it paid
// that cost on every scheduling decision on every CPU.
//
// This mirrors Linux's lockdep, which records each (held, acquired) pair once
// and afterwards performs a lookup: a cycle can only be created by an edge
// that is itself new, and every new edge is still searched before it is
// admitted. An edge is published here only after its checks pass.
#[cfg(rustos_boot_image)]
pub(super) static VALIDATED_RAW_EDGES: [AtomicU64; MAX_LOCK_CLASSES] =
    [const { AtomicU64::new(0) }; MAX_LOCK_CLASSES];
#[cfg(rustos_boot_image)]
pub(super) static VALIDATED_TASK_EDGES: [AtomicU64; MAX_LOCK_CLASSES] =
    [const { AtomicU64::new(0) }; MAX_LOCK_CLASSES];

/// Reports whether this exact dependency edge has already been admitted.
#[cfg(rustos_boot_image)]
pub(super) fn edge_already_validated(
    validated: &[AtomicU64; MAX_LOCK_CLASSES],
    held_index: usize,
    class_index: usize,
) -> bool {
    // ORDERING: Acquire observes the complete validation performed by whichever
    // CPU first admitted this edge before its publication below.
    validated[held_index].load(Ordering::Acquire) & (1_u64 << class_index) != 0
}

/// Admits this exact dependency edge after its checks have passed.
#[cfg(rustos_boot_image)]
pub(super) fn publish_validated_edge(
    validated: &[AtomicU64; MAX_LOCK_CLASSES],
    held_index: usize,
    class_index: usize,
) {
    // ORDERING: Release publishes the edge only after the cycle and IRQ
    // conflict assertions above have accepted it.
    validated[held_index].fetch_or(1_u64 << class_index, Ordering::Release);
}

#[cfg(rustos_boot_image)]
static IRQ_SAFE_CLASSES: AtomicU64 = AtomicU64::new(0);
#[cfg(rustos_boot_image)]
static IRQ_UNSAFE_CLASSES: AtomicU64 = AtomicU64::new(0);

/// Publish before rechecking the opposite classification. Both CPUs may have
/// observed an empty opposite set before either publication; using that earlier
/// snapshot would admit the same class as both IRQ-safe and IRQ-unsafe. In the
/// single SeqCst order, at least the later publisher must see the conflict.
/// The fresh snapshot also closes classification-versus-new-edge races.
fn publish_irq_usage(own: &AtomicU64, opposite: &AtomicU64, bit: u64) -> u64 {
    // ORDERING: the publication precedes the opposite load in the SAME global
    // order used by new dependency edges and classification reachability.
    own.fetch_or(bit, Ordering::SeqCst);
    let opposite_classes = opposite.load(Ordering::SeqCst);
    assert_eq!(opposite_classes & bit, 0,
        "lock-class concurrent IRQ-safe/IRQ-unsafe classification conflict");
    opposite_classes
}

#[cfg(rustos_boot_image)]
/// `irq_enabled` is the caller's IF, read once for the whole acquisition.
pub(super) fn record_irq_usage(
    cpu: usize,
    class: usize,
    acquire_site: &'static Location<'static>,
    irq_enabled: bool,
) {
    let bit = 1_u64 << class;
    if irq_context_depth_on(cpu) != 0 {
        // ORDERING: SeqCst observes every prior unsafe classification before
        // this IRQ-side admission and globally orders its safe publication.
        let unsafe_classes = IRQ_UNSAFE_CLASSES.load(Ordering::SeqCst);
        assert!(
            unsafe_classes & bit == 0,
            "IRQ-unsafe lock class acquired in interrupt context class={} acquire={}:{}",
            class,
            acquire_site.file(),
            acquire_site.line(),
        );
        // The admission assertion above runs on every acquisition; the global
        // publication and reachability search only need to run when this
        // classification is new. A later edge that would create the conflict is
        // still searched when that edge is first admitted.
        // ORDERING: SeqCst observes prior safe publications in the same global
        // order used by every dependency query.
        if IRQ_SAFE_CLASSES.load(Ordering::SeqCst) & bit == 0 {
            // ORDERING: SeqCst publishes IRQ-safe use before dependency queries
            // or a process-context unsafe admission can proceed.
            let unsafe_classes = publish_irq_usage(&IRQ_SAFE_CLASSES, &IRQ_UNSAFE_CLASSES, bit);
            assert!(
                !class_reaches_any(class, unsafe_classes),
                "IRQ-safe lock class reaches an IRQ-unsafe class class={}",
                class
            );
        }
    } else if irq_enabled {
        // ORDERING: SeqCst observes all IRQ-safe publications before admitting
        // an interruptible process-context acquisition.
        let safe_classes = IRQ_SAFE_CLASSES.load(Ordering::SeqCst);
        assert!(
            safe_classes & bit == 0,
            "IRQ-safe lock class acquired with interrupts enabled class={} acquire={}:{}",
            class,
            acquire_site.file(),
            acquire_site.line(),
        );
        // See the IRQ-side branch: the per-acquisition admission assertion is
        // above; only a new classification needs the global publication and
        // reachability search.
        // ORDERING: SeqCst observes prior unsafe publications in the same
        // global order used by every dependency query.
        if IRQ_UNSAFE_CLASSES.load(Ordering::SeqCst) & bit == 0 {
            // ORDERING: SeqCst publishes the unsafe classification before the
            // global reachability query and every future IRQ acquisition.
            let safe_classes = publish_irq_usage(&IRQ_UNSAFE_CLASSES, &IRQ_SAFE_CLASSES, bit);
            assert!(
                !any_class_reaches(safe_classes, class),
                "IRQ-unsafe lock class is reachable from an IRQ-safe class class={}",
                class
            );
        }
    }
}

/// Classifies `class_index` as IRQ-unsafe.
///
/// A sleepable acquisition can never run from interrupt context, so its class
/// is unconditionally IRQ-unsafe. Publication is idempotent and monotonic.
#[cfg(rustos_boot_image)]
pub(super) fn mark_class_irq_unsafe(class_index: usize) {
    // ORDERING: SeqCst places IRQ classification and dependency edges in one
    // global order observed by every cycle/conflict query.
    IRQ_UNSAFE_CLASSES.fetch_or(1_u64 << class_index, Ordering::SeqCst);
}

#[cfg(rustos_boot_image)]
pub(super) fn irq_dependency_conflicts(held: usize, acquired: usize) -> bool {
    // ORDERING: SeqCst takes both classifications from the single global order
    // shared with every class publication and dependency-edge insertion.
    let safe_classes = IRQ_SAFE_CLASSES.load(Ordering::SeqCst);
    let unsafe_classes = IRQ_UNSAFE_CLASSES.load(Ordering::SeqCst);
    (safe_classes & (1_u64 << held) != 0 || any_class_reaches(safe_classes, held))
        && (unsafe_classes & (1_u64 << acquired) != 0
            || class_reaches_any(acquired, unsafe_classes))
}

#[cfg(rustos_boot_image)]
pub(super) fn class_reaches_any(class: usize, targets: u64) -> bool {
    targets != 0
        && (0..MAX_LOCK_CLASSES)
            .any(|target| targets & (1_u64 << target) != 0 && dependency_reaches(class, target))
}

#[cfg(rustos_boot_image)]
pub(super) fn any_class_reaches(classes: u64, target: usize) -> bool {
    classes != 0
        && (0..MAX_LOCK_CLASSES)
            .any(|class| classes & (1_u64 << class) != 0 && dependency_reaches(class, target))
}

#[cfg(rustos_boot_image)]
pub(super) fn dependency_reaches(start: usize, target: usize) -> bool {
    graph_reaches(start, target, |node| {
        // ORDERING: SeqCst observes edges in the same total order in which
        // acquisitions publish them before asking this reachability question.
        DEPENDENCIES[node].load(Ordering::SeqCst)
    })
}

#[cfg(any(rustos_boot_image, test))]
pub(super) fn graph_reaches(
    start: usize,
    target: usize,
    mut edges: impl FnMut(usize) -> u64,
) -> bool {
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

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn stale_opposite_observations_cannot_admit_conflicting_irq_classifications() {
        for reverse in [false, true] {
            let safe = AtomicU64::new(0);
            let unsafe_ = AtomicU64::new(0);
            let bit = 1 << 42;
            // BOTH actual record_irq_usage prechecks may run before either
            // publication. Replay that exact cross-CPU interleaving, without
            // timing assumptions or changes to the global production graph.
            assert_eq!(safe.load(Ordering::SeqCst) & bit, 0);
            assert_eq!(unsafe_.load(Ordering::SeqCst) & bit, 0);
            let (first, second) = if reverse { (&unsafe_, &safe) } else { (&safe, &unsafe_) };
            assert_eq!(publish_irq_usage(first, second, bit), 0);
            assert!(std::panic::catch_unwind(|| publish_irq_usage(second, first, bit)).is_err());
        }
    }

    #[test]
    fn simultaneous_irq_classification_publishers_cannot_both_succeed() {
        let safe = AtomicU64::new(0);
        let unsafe_ = AtomicU64::new(0);
        let ready = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                assert_eq!(unsafe_.load(Ordering::SeqCst), 0);
                ready.wait();
                std::panic::catch_unwind(|| publish_irq_usage(&safe, &unsafe_, 1 << 51)).is_ok()
            });
            let b = scope.spawn(|| {
                assert_eq!(safe.load(Ordering::SeqCst), 0);
                ready.wait();
                std::panic::catch_unwind(|| publish_irq_usage(&unsafe_, &safe, 1 << 51)).is_ok()
            });
            assert!(!(a.join().unwrap() && b.join().unwrap()));
        });
    }

    #[test]
    fn new_irq_classification_returns_fresh_opposite_set_for_transitive_checks() {
        let own = AtomicU64::new(0);
        let opposite = AtomicU64::new((1 << 7) | (1 << 54));
        assert_eq!(publish_irq_usage(&own, &opposite, 1 << 42), (1 << 7) | (1 << 54));
        assert_eq!(own.load(Ordering::SeqCst), 1 << 42);
    }
}
