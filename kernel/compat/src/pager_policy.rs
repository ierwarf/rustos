//! Ring3-owned anonymous-mapping policy, published once and read locally.
//!
//! - **Owner:** `pagerd` owns anonymous arbitration policy; `kernel-compat`
//!   owns only the publication slot and the enforcement that reads it.
//! - **Boundary:** A publication is untrusted until `is_canonical` accepts it,
//!   and only a process holding the pagerd service endpoint may publish.
//! - **Lifecycle:** ring0 applies its compiled-in default until pagerd
//!   publishes exactly once; the publication is immutable afterwards.
//! - **Concurrency:** one acquire load on the commit word, then relaxed reads
//!   of fields that can no longer change. Safe from the IRQ-off fault path
//!   because it neither locks nor allocates.
//! - **Failure:** a malformed policy is refused and the default stands; a
//!   second publication is refused rather than mutating a live policy.
//! - **Forbidden:** no policy widening past the fixed table sizes, no
//!   publication from a process that does not own the pager endpoint, and no
//!   ring0 default that differs from the constants it replaced.
//! - **Evidence:** the focused tests below and `memory-map`.
//!
//! # Why publication rather than consultation
//!
//! These decisions were ring0 constants because the only transport to ring3
//! was a synchronous call on the fault path, which measured 5.7 ms p99 on
//! `mmap`. That retired the *transport*, not the ownership. Reading a
//! published policy costs what reading a constant costs, so ring3 can own the
//! decision without putting a round trip back on the fault. It is Zircon's
//! split - userspace sets policy, the kernel enforces it.
//!
//! # Why one-shot
//!
//! Immutability after commit is what lets the IRQ-off reader take the policy
//! with a single acquire and no seqlock: there is no window in which a reader
//! can observe half of one policy and half of another. A pager sets this
//! during startup, before the processes it governs exist, exactly as a Zircon
//! job policy is set before the job runs rather than mutated under load.

use core::sync::atomic::{AtomicU64, Ordering};

use rustos_user_abi::pager::{
    PagerAnonymousPolicyWire, PAGER_FAULT_ABI_VERSION, PAGER_FAULT_RUN_PAGES_MAX,
    PAGER_MAX_VMAS_PER_PROCESS, PAGER_MAX_WIRED_SERVICES,
};
use rustos_user_abi::syscall::{
    IPC_SERVICE_LINUX_SYSCALLD, IPC_SERVICE_PAGERD, IPC_SERVICE_ROOTD, IPC_SERVICE_STORAGED,
    IPC_SERVICE_VFSD,
};

/// Publication states of the commit word.
const UNPUBLISHED: u64 = 0;
const CLAIMED: u64 = 1;
const COMMITTED: u64 = 2;

/// What ring0 applies until a pager publishes.
///
/// This must stay byte-for-byte the behaviour of the constants it replaced, so
/// that introducing the publication slot changes nothing on its own and the
/// only observable change is a pager choosing to publish something else.
pub(crate) fn default_policy() -> PagerAnonymousPolicyWire {
    let mut wired_services = [0_u64; PAGER_MAX_WIRED_SERVICES];
    // The five services the whole system starts through. Ring0 answers
    // anonymous faults itself now, so the recursive-fault cycle this list was
    // built for cannot form; it is a conservative hold on boot-time memory
    // behaviour, which is exactly the kind of judgement a pager should own.
    wired_services[0] = IPC_SERVICE_PAGERD;
    wired_services[1] = IPC_SERVICE_ROOTD;
    wired_services[2] = IPC_SERVICE_VFSD;
    wired_services[3] = IPC_SERVICE_STORAGED;
    wired_services[4] = IPC_SERVICE_LINUX_SYSCALLD;
    PagerAnonymousPolicyWire {
        version: PAGER_FAULT_ABI_VERSION,
        reserved0: 0,
        fault_run_pages: PAGER_FAULT_RUN_PAGES_MAX,
        process_vma_ceiling: PAGER_MAX_VMAS_PER_PROCESS as u32,
        demand_enabled: 1,
        wired_services,
        reserved1: [0; 2],
    }
}

struct PublishedAnonymousPolicy {
    commit: AtomicU64,
    fault_run_pages: AtomicU64,
    process_vma_ceiling: AtomicU64,
    demand_enabled: AtomicU64,
    wired_services: [AtomicU64; PAGER_MAX_WIRED_SERVICES],
}

impl PublishedAnonymousPolicy {
    const fn empty() -> Self {
        Self {
            commit: AtomicU64::new(UNPUBLISHED),
            fault_run_pages: AtomicU64::new(0),
            process_vma_ceiling: AtomicU64::new(0),
            demand_enabled: AtomicU64::new(0),
            wired_services: [const { AtomicU64::new(0) }; PAGER_MAX_WIRED_SERVICES],
        }
    }

    fn publish(&self, policy: PagerAnonymousPolicyWire) -> bool {
        // ORDERING: AcqRel claims the one-shot slot before any field is
        // written; a loser observes the claim and refuses rather than
        // interleaving its fields with the winner's.
        if self
            .commit
            .compare_exchange(UNPUBLISHED, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.fault_run_pages
            .store(u64::from(policy.fault_run_pages), Ordering::Relaxed);
        self.process_vma_ceiling
            .store(u64::from(policy.process_vma_ceiling), Ordering::Relaxed);
        self.demand_enabled
            .store(u64::from(policy.demand_enabled), Ordering::Relaxed);
        for (slot, service_id) in self
            .wired_services
            .iter()
            .zip(policy.wired_services.iter().copied())
        {
            slot.store(service_id, Ordering::Relaxed);
        }
        // ORDERING: release commits every field above before a reader may
        // observe `COMMITTED` and take them. Nothing writes them again, so the
        // reader needs no second observation to prove they are stable.
        self.commit.store(COMMITTED, Ordering::Release);
        true
    }

    fn read(&self) -> Option<PagerAnonymousPolicyWire> {
        // ORDERING: acquire pairs with the release commit above, so every
        // relaxed field read below observes the published value.
        if self.commit.load(Ordering::Acquire) != COMMITTED {
            return None;
        }
        let mut wired_services = [0_u64; PAGER_MAX_WIRED_SERVICES];
        for (destination, slot) in wired_services.iter_mut().zip(self.wired_services.iter()) {
            *destination = slot.load(Ordering::Relaxed);
        }
        Some(PagerAnonymousPolicyWire {
            version: PAGER_FAULT_ABI_VERSION,
            reserved0: 0,
            fault_run_pages: self.fault_run_pages.load(Ordering::Relaxed) as u32,
            process_vma_ceiling: self.process_vma_ceiling.load(Ordering::Relaxed) as u32,
            demand_enabled: self.demand_enabled.load(Ordering::Relaxed) as u32,
            wired_services,
            reserved1: [0; 2],
        })
    }
}

static ANONYMOUS_POLICY: PublishedAnonymousPolicy = PublishedAnonymousPolicy::empty();

/// The policy in force: what a pager published, or ring0's default.
///
/// Callable from the IRQ-off fault path: one acquire load and, at most, a
/// handful of relaxed loads of fields that can no longer change.
pub(crate) fn anonymous_policy() -> PagerAnonymousPolicyWire {
    ANONYMOUS_POLICY.read().unwrap_or_else(default_policy)
}

/// Records a pager's policy. `false` means refused; the prior policy stands.
pub(crate) fn publish_anonymous_policy(policy: PagerAnonymousPolicyWire) -> bool {
    policy.is_canonical() && ANONYMOUS_POLICY.publish(policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring0_default_is_exactly_the_constants_it_replaced() {
        let policy = default_policy();
        assert!(policy.is_canonical());
        assert_eq!(policy.fault_run_pages, PAGER_FAULT_RUN_PAGES_MAX);
        assert_eq!(
            policy.process_vma_ceiling as usize,
            PAGER_MAX_VMAS_PER_PROCESS
        );
        assert_eq!(policy.demand_enabled, 1);
        for service_id in [
            IPC_SERVICE_PAGERD,
            IPC_SERVICE_ROOTD,
            IPC_SERVICE_VFSD,
            IPC_SERVICE_STORAGED,
            IPC_SERVICE_LINUX_SYSCALLD,
        ] {
            assert!(policy.keeps_service_wired(service_id), "{service_id}");
        }
        assert!(!policy.keeps_service_wired(0));
    }

    #[test]
    fn a_policy_that_widens_a_fixed_table_or_hides_a_wired_service_is_refused() {
        let mut wide = default_policy();
        wide.process_vma_ceiling = PAGER_MAX_VMAS_PER_PROCESS as u32 + 1;
        assert!(!wide.is_canonical());

        let mut long_run = default_policy();
        long_run.fault_run_pages = PAGER_FAULT_RUN_PAGES_MAX + 1;
        assert!(!long_run.is_canonical());

        let mut zero_run = default_policy();
        zero_run.fault_run_pages = 0;
        assert!(!zero_run.is_canonical());

        // Ring0 tiles a region with blocks aligned to their own size, so a
        // non-power-of-two run could put its tail in a page table the fault
        // never published.
        let mut unaligned_run = default_policy();
        unaligned_run.fault_run_pages = 5;
        assert!(!unaligned_run.is_canonical());

        // A gap would let a truncated read drop a wired service and admit it
        // to demand paging.
        let mut gapped = default_policy();
        gapped.wired_services[1] = 0;
        assert!(!gapped.is_canonical());

        let mut unknown_toggle = default_policy();
        unknown_toggle.demand_enabled = 2;
        assert!(!unknown_toggle.is_canonical());
    }

    #[test]
    fn publication_is_one_shot_and_a_reader_sees_all_of_it_or_none() {
        let slot = PublishedAnonymousPolicy::empty();
        assert_eq!(slot.read(), None);

        let mut published = default_policy();
        published.fault_run_pages = 4;
        published.demand_enabled = 0;
        assert!(slot.publish(published));
        assert_eq!(slot.read(), Some(published));

        // A second publication cannot mutate a live policy.
        let mut second = default_policy();
        second.fault_run_pages = 1;
        assert!(!slot.publish(second));
        assert_eq!(slot.read(), Some(published));
    }
}
