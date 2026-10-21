//! Shipping acquire half of a tracked raw guard, split from `lockdep.rs`.
//!
//! - **Owner:** the acquiring CPU, with IRQs masked for the guard's life.
//! - **Evidence:** release re-proves the CPU and held-class order
//!   (`guard_release.rs`); full validation stays in `lock_with`.

use super::*;

impl<T: ?Sized, const CLASS: u8> TrackedSpinLock<T, CLASS> {
    /// The shipping acquire for a caller whose IRQs are masked.
    ///
    /// With IF clear no handler can interleave with the bookkeeping, so the
    /// acquire needs only what release and the preemption assertions consume:
    /// the CPU-private depth, the held-class stack, and the spin itself. The
    /// lock-order graph, IRQ-classification, recursion scan and per-guard
    /// identity capture are validation, not enforcement; they run in host,
    /// test and hardening builds (`lock_with`), as in other kernels' lockdep.
    #[cfg(rustos_boot_image)]
    #[track_caller]
    #[inline]
    pub(super) fn lock_lean(&self) -> TrackedSpinGuard<'_, T, CLASS> {
        let acquire_site = Location::caller();
        let cpu = current_cpu_index();
        work_budget::charge_acquire_for_class::<CLASS>(cpu);
        // ORDERING: CPU-private depth; IRQs are masked for the guard's life.
        let previous = cpu_counter::add_usize(&CPU_LOCK_STATE[cpu].preempt_disable_depth, 1);
        assert!(
            previous < MAX_HELD_LOCK_DEPTH,
            "raw-spin preemption depth exceeded bound"
        );
        with_current_stack_on(cpu, |stack| {
            assert!(
                stack.len < stack.classes.len(),
                "lock-class nesting exceeds {}",
                MAX_HELD_LOCK_DEPTH
            );
            stack.classes[stack.len] = CLASS;
            stack.len += 1;
        });
        let mut wait_start_tsc = 0_u64;
        let mut spins = 0usize;
        let guard = loop {
            if (spins == 0 || !self.inner.is_locked())
                && let Some(guard) = self.inner.try_lock()
            {
                break guard;
            }
            if spins == 0 {
                wait_start_tsc = read_tsc();
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
        };
        TrackedSpinGuard {
            guard: Some(guard),
            owner_cpu: cpu,
            owner_apic_id: 0,
            acquire_file: "",
            acquire_line: 0,
            acquire_preemption_depth: 0,
            irqs_masked: true,
            lean: true,
        }
    }

    /// The shipping acquire for a caller whose IRQs are enabled.
    ///
    /// Only the CPU-private bookkeeping (index, depth, held-class push) needs
    /// interrupt exclusion, so a `cli`/`sti` pair brackets it and the spin runs
    /// with IRQs as the caller left them. Release masks the matching pop.
    #[cfg(all(rustos_boot_image, not(rustos_lock_phase_profile)))]
    #[track_caller]
    #[inline]
    pub(super) fn lock_lean_irq(&self) -> TrackedSpinGuard<'_, T, CLASS> {
        let acquire_site = Location::caller();
        x86_64::instructions::interrupts::disable();
        let cpu = current_cpu_index();
        work_budget::charge_acquire_for_class::<CLASS>(cpu);
        // ORDERING: CPU-private depth; IRQs are masked for this window.
        let previous = cpu_counter::add_usize(&CPU_LOCK_STATE[cpu].preempt_disable_depth, 1);
        assert!(
            previous < MAX_HELD_LOCK_DEPTH,
            "raw-spin preemption depth exceeded bound"
        );
        with_current_stack_on(cpu, |stack| {
            assert!(
                stack.len < stack.classes.len(),
                "lock-class nesting exceeds {}",
                MAX_HELD_LOCK_DEPTH
            );
            stack.classes[stack.len] = CLASS;
            stack.len += 1;
        });
        x86_64::instructions::interrupts::enable();
        let guard = self.spin_lean(acquire_site);
        TrackedSpinGuard {
            guard: Some(guard),
            owner_cpu: cpu,
            owner_apic_id: 0,
            acquire_file: "",
            acquire_line: 0,
            acquire_preemption_depth: 0,
            irqs_masked: false,
            lean: true,
        }
    }

    #[cfg(rustos_boot_image)]
    #[inline]
    fn spin_lean(&self, acquire_site: &'static Location<'static>) -> MutexGuard<'_, T> {
        let mut wait_start_tsc = 0_u64;
        let mut spins = 0usize;
        loop {
            if (spins == 0 || !self.inner.is_locked())
                && let Some(guard) = self.inner.try_lock()
            {
                break guard;
            }
            if spins == 0 {
                wait_start_tsc = read_tsc();
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
    }
}
