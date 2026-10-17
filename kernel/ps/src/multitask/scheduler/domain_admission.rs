//! Owner: cold cross-domain utilization and descriptor allocation arbitration.
//! Boundary: userspace policy is validated, never chosen here; not CPU funding.
//! Lifecycle: captured admission retains the domain until context publication.
//! Concurrency: registry → context48 → domain49; numeric class IDs are not rank.
//! Failure: invalid/quota/full/exhausted admission refuses before replacement.
//! Forbidden: no allocation, resource leaves, hardware or external callbacks.
//! Evidence: source/native witnesses; not complete creation or hardware proof.
use super::{
    scheduling_context::{self, SchedulingContextPolicy},
    scheduling_runtime,
};
use core::marker::PhantomData;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
static REGISTRY: TrackedSpinLock<(), { LockClass::SchedulerDomainAdmission as u8 }> =
    TrackedSpinLock::new(());
/// Borrowed only within the cold registry turn. It is not a CPU dispatch grant.
pub(super) struct Turn(PhantomData<*mut ()>);
pub(super) fn with<R>(f: impl FnOnce(&Turn) -> R) -> R {
    // Registry arbitration is a bounded raw transaction, not a CPU turn.
    // Constructors already retain an original dispatch owner; acquiring another
    // exclusive unit here would panic only in production. The same registry
    // callback runs natively; only the privileged local IRQ mask is modeled.
    let protected = || {
        let _guard = REGISTRY.lock();
        f(&Turn(PhantomData))
    };
    #[cfg(rustos_boot_image)]
    {
        x86_64::instructions::interrupts::without_interrupts(protected)
    }
    #[cfg(not(rustos_boot_image))]
    {
        protected()
    }
}
/// Not Copy: selection must retain a member through the sleep/creation gap.
pub(super) struct Admission {
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
}
impl Admission {
    pub(super) fn slot(&self) -> usize {
        self.slot
    }
    pub(super) fn generation(&self) -> u32 {
        self.generation
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        scheduling_runtime::release_domain_member(self.slot, self.generation, self.policy);
    }
}
fn quota(policy: SchedulingContextPolicy) -> bool {
    if policy.criticality != 2 {
        return true;
    }
    let Some(candidate) = scheduling_context::deadline_utilization_ppm(policy) else {
        return false;
    };
    for cpu in 0..u64::BITS {
        let bit = 1_u64 << cpu;
        if policy.cpu_mask & bit == 0 {
            continue;
        }
        let mut total = candidate;
        for slot in 0..scheduling_context::MAX_SCHEDULING_DOMAINS {
            let Some(domain) = scheduling_runtime::domain_descriptor(slot) else {
                continue;
            };
            let admitted = domain.policy();
            if admitted.domain == policy.domain
                || admitted.criticality != 2
                || admitted.cpu_mask & bit == 0
            {
                continue;
            }
            let Some(sum) = scheduling_context::deadline_utilization_ppm(admitted)
                .and_then(|n| total.checked_add(n))
            else {
                return false;
            };
            total = sum;
        }
        if total > scheduling_context::DEADLINE_UTILIZATION_LIMIT_PPM {
            return false;
        }
    }
    true
}
pub(super) fn admit(policy: SchedulingContextPolicy) -> Option<Admission> {
    let (slot, generation) = with(|turn| {
        if !quota(policy) {
            return None;
        }
        let mut vacant = None;
        for slot in 0..scheduling_context::MAX_SCHEDULING_DOMAINS {
            match scheduling_runtime::domain_descriptor(slot) {
                Some(domain) if domain.domain() == policy.domain => {
                    let generation = if domain.policy() == policy {
                        domain.generation()
                    } else {
                        scheduling_runtime::replace_domain_owned(
                            turn,
                            slot,
                            domain.policy(),
                            domain.generation(),
                            policy,
                        )?
                    };
                    scheduling_runtime::retain_domain_member(slot, generation, policy)?;
                    return Some((slot, generation));
                }
                None if vacant.is_none() => vacant = Some(slot),
                _ => {}
            }
        }
        let slot = vacant?;
        let generation = scheduling_runtime::initialize_domain_owned(turn, slot, policy)?;
        scheduling_runtime::retain_domain_member(slot, generation, policy)?;
        Some((slot, generation))
    })?;
    // Construct/destruct a cross-owner grant outside the registry/row leaves.
    Some(Admission {
        slot,
        generation,
        policy,
    })
}
#[cfg(test)]
pub(super) mod tests;
