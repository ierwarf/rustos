//! Release half of a tracked raw guard, split from `lockdep.rs` for size.
//!
//! - **Owner:** the guard's acquiring CPU; release re-proves that CPU, the
//!   preemption unit it took, and its held-class stack before unlocking.
//! - **Evidence:** the same release invariants and panics as before the split.

use super::*;

impl<T: ?Sized, const CLASS: u8> Drop for TrackedSpinGuard<'_, T, CLASS> {
    #[inline(always)]
    fn drop(&mut self) {
        #[cfg(rustos_boot_image)]
        if self.lean {
            let cpu = self.owner_cpu;
            // Preemption stays disabled for the guard's life, so the owner CPU
            // is still this CPU; the release-on-another-CPU proof lives in the
            // host, test and phase-profile validation builds.
            drop(self.guard.take());
            if self.irqs_masked {
                // A masked acquire is already inside its masked scope.
                Self::pop_lean(cpu);
            } else {
                // An IF-set acquire pairs with a masked pop.
                self.pop_lean_masked_by_cli(cpu);
            }
            return;
        }
        self.drop_full();
    }
}

impl<T: ?Sized, const CLASS: u8> TrackedSpinGuard<'_, T, CLASS> {
    #[cfg(rustos_boot_image)]
    #[inline(always)]
    fn pop_lean(cpu: usize) {
        with_current_stack_on(cpu, |stack| {
            let len = stack.len;
            // The class check and the zeroing store are validation for host,
            // test and hardening builds: shipping images keep the length (it is
            // enforcement state) and the preemption depth.
            #[cfg(any(not(rustos_boot_image), rustos_lock_phase_profile))]
            {
                assert!(len != 0, "lock-class release without acquisition");
                let top = stack.classes[len - 1];
                assert_eq!(
                    top, CLASS,
                    "lock-class release order violation held={} released={}",
                    top, CLASS
                );
                stack.classes[len - 1] = 0;
            }
            stack.len = len - 1;
        });
        let observed = cpu_counter::dec(&CPU_LOCK_STATE[cpu].preempt_disable_depth);
        #[cfg(any(not(rustos_boot_image), rustos_lock_phase_profile))]
        assert!(observed != 0, "raw-spin preemption depth underflow");
        let _ = observed;
    }

    #[cfg(rustos_boot_image)]
    #[inline(never)]
    fn pop_lean_masked_by_cli(&self, cpu: usize) {
        let irq_enabled = x86_64::instructions::interrupts::are_enabled();
        preemption::masked(irq_enabled, || Self::pop_lean(cpu));
    }

    #[inline(never)]
    fn drop_full(&mut self) {
        let profile_entry = lock_profile::now();
        #[cfg(rustos_boot_image)]
        {
            // A `lock_masked` guard is released inside the same masked scope.
            let irq_enabled = !self.irqs_masked && x86_64::instructions::interrupts::are_enabled();
            preemption::masked(irq_enabled, || {
                let profile_masked = lock_profile::now();
                // Interrupts are masked for this whole block, so the logical
                // index cannot change inside it. Deriving it once and passing
                // it down is what makes the release cheap: the identity step
                // used to derive the same answer six times, and the actual
                // lock-word handoff below is thirty-six cycles.
                let release_cpu = current_cpu_index();
                // An index-authoritative owner needs no identity lookup: after
                // publication the identity is a pure function of the index.
                let release_apic_id = if self.owner_apic_id == cpu_identity::INDEX_AUTHORITATIVE {
                    cpu_identity::INDEX_AUTHORITATIVE
                } else {
                    cpu_identity::apic_id_for_index(release_cpu)
                };
                // ORDERING: Acquire observes completed guard/pending
                // transitions before the release admissibility comparison.
                let depth = CPU_LOCK_STATE[release_cpu].preempt_disable_depth.load(Ordering::Acquire);
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
                        self.acquire_site.map_or("", |site| site.file()),
                        self.acquire_site.map_or(0, |site| site.line()),
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
