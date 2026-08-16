//! Per-CPU preemption-depth accounting for tracked raw guards.
//!
//! - **Owner:** this module owns the disable/enable depth and the pending
//!   reservation that a raw acquisition holds between its admission check and
//!   its held-stack publication.
//! - **Boundary:** every depth here is CPU-local. A caller that supplies a
//!   logical index must already hold interrupts masked or preemption disabled,
//!   so the index cannot change under it.
//! - **Lifecycle:** reserve pending on acquire, convert pending to held after
//!   the lock word is taken, decrement on release.
//! - **Concurrency:** the depth, the pending count, and the held-class stack
//!   must stay in correspondence; every transition asserts that they do.
//! - **Failure:** a mismatch, an underflow, or a depth past the bound is an
//!   invariant panic, never a silent correction.
//! - **Forbidden:** no scheduler dispatch and no migration while a depth is
//!   nonzero.
//! - **Evidence:** `scheduler-cpu-ownership`, `cpu-online-lifecycle`.
//!
//! Split out of `lockdep.rs` when that file crossed its size threshold. The
//! `_on` forms exist because the release path derived the same logical index
//! six times at roughly two hundred cycles apiece; taking it as an argument
//! removes the repeat without weakening any assertion.

use core::sync::atomic::Ordering;

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreemptionSnapshot {
    pub logical_cpu: usize,
    pub apic_id: u32,
    pub depth: usize,
    pub pending_depth: usize,
    pub held_depth: usize,
    pub top_class: Option<u8>,
}

#[inline]
#[cfg(any(rustos_boot_image, test))]
pub(super) const fn preemption_units_match(depth: usize, held_depth: usize, pending_depth: usize) -> bool {
    match held_depth.checked_add(pending_depth) {
        Some(expected) => depth == expected,
        None => false,
    }
}

#[inline]
#[cfg(any(rustos_boot_image, test))]
pub(super) const fn preemption_release_is_admissible(
    depth: usize,
    held_depth: usize,
    pending_depth: usize,
) -> bool {
    match held_depth.checked_add(pending_depth) {
        Some(units) => match units.checked_add(1) {
            Some(expected) => depth == expected,
            None => false,
        },
        None => false,
    }
}

/// Returns the current CPU's task-preemption nesting depth.
///
/// Interrupt handlers remain available while this is non-zero; only an
/// explicit task scheduler handoff is forbidden. The scheduler checks this
/// before every software reschedule entry.
#[inline]
pub fn preemption_depth() -> usize {
    #[cfg(rustos_boot_image)]
    {
        preemption_snapshot().depth
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

/// Take one same-CPU, IRQ-atomic snapshot of scheduler-preemption ownership.
pub fn preemption_snapshot() -> PreemptionSnapshot {
    #[cfg(rustos_boot_image)]
    {
        return x86_64::instructions::interrupts::without_interrupts(|| {
            let logical_cpu = current_cpu_index();
            let apic_id = current_apic_id();
            // ORDERING: Acquire observes completed guard/pending transitions
            // before a scheduler gate consumes this coherent snapshot.
            let depth = PREEMPT_DISABLE_DEPTH[logical_cpu].load(Ordering::Acquire);
            let pending_depth = PREEMPT_PENDING_DEPTH[logical_cpu].load(Ordering::Relaxed);
            let held_depth = held_spin_lock_depth();
            let top_class = current_lock_class();
            assert!(
                preemption_units_match(depth, held_depth, pending_depth),
                "raw-spin preemption snapshot mismatch cpu={} apic={:#x} depth={} held_depth={} pending_depth={} top_class={:?}",
                logical_cpu,
                apic_id,
                depth,
                held_depth,
                pending_depth,
                top_class
            );
            PreemptionSnapshot {
                logical_cpu,
                apic_id,
                depth,
                pending_depth,
                held_depth,
                top_class,
            }
        });
    }
    #[cfg(not(rustos_boot_image))]
    {
        PreemptionSnapshot {
            logical_cpu: 0,
            apic_id: 0,
            depth: 0,
            pending_depth: 0,
            held_depth: 0,
            top_class: None,
        }
    }
}

#[track_caller]
/// Disables preemption and returns the logical index it derived.
///
/// Preemption stays disabled for the guard's whole lifetime, so the index
/// cannot change afterwards -- that is the same invariant
/// `guard_release_is_admissible` checks on the way out. Returning it lets the
/// rest of the acquire reuse the answer instead of deriving it four more times.
#[cfg(rustos_boot_image)]
pub(super) fn disable_preemption() -> usize {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let cpu = current_cpu_index();
        // ORDERING: Acquire observes the last same-CPU guard release before
        // checking the held plus pending ownership correspondence.
        let depth = PREEMPT_DISABLE_DEPTH[cpu].load(Ordering::Acquire);
        let held_depth = with_current_stack_on(cpu, |stack| stack.len);
        let pending_depth = PREEMPT_PENDING_DEPTH[cpu].load(Ordering::Relaxed);
        assert!(
            preemption_units_match(depth, held_depth, pending_depth),
            "raw-spin preemption acquire mismatch cpu={} apic={:#x} depth={} held_depth={} pending_depth={} top_class={:?}",
            cpu,
            hardware_apic_id(),
            depth,
            held_depth,
            pending_depth,
            with_current_stack_on(cpu, |stack| stack
                .len
                .checked_sub(1)
                .map(|index| stack.classes[index]))
        );
        // ORDERING: AcqRel publishes guard entry before protected raw state can
        // be observed and serializes depth with the matching decrement.
        let previous = PREEMPT_DISABLE_DEPTH[cpu].fetch_add(1, Ordering::AcqRel);
        assert!(
            previous < MAX_HELD_LOCK_DEPTH,
            "raw-spin preemption depth exceeded bound"
        );
        PREEMPT_PENDING_DEPTH[cpu].fetch_add(1, Ordering::Relaxed);
        cpu
    })
}

#[cfg(rustos_boot_image)]
#[track_caller]
pub(super) fn enable_preemption(class: u8) {
    enable_preemption_on(current_cpu_index(), class);
}

/// `enable_preemption` for a caller that already derived its logical index
/// under interrupt masking. Every assertion below is unchanged.
#[cfg(rustos_boot_image)]
pub(super) fn enable_preemption_on(cpu: usize, class: u8) {
    // The architectural identity is only rendered in the failure messages
    // below. Deriving it eagerly cost a `CPUID` VM exit on every lock release.
    let held_depth = with_current_stack_on(cpu, |stack| stack.len);
    let pending_depth = PREEMPT_PENDING_DEPTH[cpu].load(Ordering::Relaxed);
    // ORDERING: AcqRel publishes every protected write before the final depth
    // decrement; Acquire failure ordering reports the exact observed depth.
    let previous =
        PREEMPT_DISABLE_DEPTH[cpu].fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| {
            depth.checked_sub(1)
        });
    match previous {
        Ok(observed) if preemption_release_is_admissible(observed, held_depth, pending_depth) => {}
        Ok(observed) => panic!(
            "raw-spin preemption release mismatch class={} cpu={} apic={:#x} observed={} expected={} irq_depth={} held_depth={} pending_depth={} outer_class={:?}",
            class,
            cpu,
            hardware_apic_id(),
            observed,
            held_depth
                .checked_add(pending_depth)
                .and_then(|units| units.checked_add(1))
                .unwrap_or(usize::MAX),
            irq_context_depth(),
            held_depth,
            pending_depth,
            with_current_stack_on(cpu, |stack| stack
                .len
                .checked_sub(1)
                .map(|index| stack.classes[index]))
        ),
        Err(observed) => panic!(
            "raw-spin preemption depth underflow class={} cpu={} apic={:#x} observed={} irq_depth={} held_depth={} outer_class={:?}",
            class,
            cpu,
            hardware_apic_id(),
            observed,
            irq_context_depth(),
            held_depth,
            current_lock_class()
        ),
    }
}

#[cfg(rustos_boot_image)]
pub(super) fn cancel_pending_acquire() {
    let cpu = current_cpu_index();
    let previous = PREEMPT_PENDING_DEPTH[cpu].fetch_sub(1, Ordering::Relaxed);
    assert!(previous != 0, "raw-spin pending-acquire depth underflow");
}

#[cfg(rustos_boot_image)]
pub(super) fn cancel_pending_acquire_and_enable(class: u8) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        cancel_pending_acquire();
        enable_preemption(class);
    });
}

