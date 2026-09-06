//! Ring3-owned anonymous-mapping policy, published as a coherent snapshot and read locally.
//!
//! - **Owner:** `pagerd` owns anonymous arbitration policy; `kernel-compat`
//!   owns only the publication slot and the enforcement that reads it.
//! - **Boundary:** A publication is untrusted until `is_canonical` accepts it,
//!   and only a process holding the pagerd service endpoint may publish.
//! - **Lifecycle:** ring0 applies its compiled-in default until pagerd
//!   publishes; later pressure-driven publications replace the whole snapshot.
//! - **Concurrency:** writers publish through an odd/even sequence. Normal
//!   readers retry; the single IRQ-off reader makes two attempts and falls
//!   back to the compiled-in run length when a writer is active.
//! - **Failure:** malformed policy and bounded writer contention are refused;
//!   the prior stable snapshot remains authoritative.
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
//! decision without putting a round trip back on the fault. It follows
//! Zircon's pressure split: mutable system state is recomputed out of line and
//! the fault path consumes a local coherent snapshot.
//!
//! # Why a sequence, rather than a commit bit
//!
//! Memory-pressure levels and the thresholds derived from them change while
//! processes run. A one-shot commit would make those policy fields immutable
//! precisely when pressure needs to narrow them. The sequence makes the whole
//! wire value one publication: readers never combine fields from two pressure
//! epochs. The IRQ-off fault-around reader is best effort, so an unstable read
//! changes only that fault's speculative run length and safely uses the
//! default; readers that make admission decisions retry in normal context.

use core::sync::atomic::{AtomicU64, Ordering};

use rustos_user_abi::pager::{
    PAGER_FAULT_ABI_VERSION, PAGER_FAULT_RUN_PAGES_MAX, PAGER_MAX_VMAS_PER_PROCESS,
    PAGER_MAX_WIRED_SERVICES, PagerAnonymousPolicyWire,
};
use rustos_user_abi::syscall::{
    IPC_SERVICE_LINUX_SYSCALLD, IPC_SERVICE_PAGERD, IPC_SERVICE_ROOTD, IPC_SERVICE_STORAGED,
    IPC_SERVICE_VFSD,
};

/// Even sequences are stable snapshots; odd sequences have a writer.
const UNPUBLISHED: u64 = 0;
const MAX_PUBLICATION_SEQUENCE: u64 = u64::MAX - 2;
const POLICY_WRITE_ATTEMPTS: usize = 128;

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
    sequence: AtomicU64,
    fault_run_pages: AtomicU64,
    process_vma_ceiling: AtomicU64,
    demand_enabled: AtomicU64,
    wired_services: [AtomicU64; PAGER_MAX_WIRED_SERVICES],
}

impl PublishedAnonymousPolicy {
    const fn empty() -> Self {
        Self {
            sequence: AtomicU64::new(UNPUBLISHED),
            fault_run_pages: AtomicU64::new(0),
            process_vma_ceiling: AtomicU64::new(0),
            demand_enabled: AtomicU64::new(0),
            wired_services: [const { AtomicU64::new(0) }; PAGER_MAX_WIRED_SERVICES],
        }
    }

    fn publish(&self, policy: PagerAnonymousPolicyWire) -> bool {
        let mut stable = UNPUBLISHED;
        let mut claimed = false;
        for _ in 0..POLICY_WRITE_ATTEMPTS {
            stable = self.sequence.load(Ordering::Acquire);
            if stable & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            if stable > MAX_PUBLICATION_SEQUENCE {
                return false;
            }
            // ORDERING: AcqRel changes the stable even sequence to odd before
            // any payload field changes. A reader that saw the old even value
            // must reject it at its second sequence observation.
            if self
                .sequence
                .compare_exchange_weak(stable, stable + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                claimed = true;
                break;
            }
        }
        if !claimed {
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
        // ORDERING: Release publishes every Relaxed payload store before the
        // next stable even sequence. Readers accept the payload only when two
        // acquire observations agree on this exact sequence.
        self.sequence.store(stable + 2, Ordering::Release);
        true
    }

    fn read(&self) -> Result<Option<PagerAnonymousPolicyWire>, ()> {
        for _ in 0..2 {
            let before = self.sequence.load(Ordering::Acquire);
            if before == UNPUBLISHED {
                return Ok(None);
            }
            if before & 1 != 0 {
                continue;
            }

            let mut wired_services = [0_u64; PAGER_MAX_WIRED_SERVICES];
            for (destination, slot) in wired_services.iter_mut().zip(self.wired_services.iter()) {
                *destination = slot.load(Ordering::Relaxed);
            }
            let policy = PagerAnonymousPolicyWire {
                version: PAGER_FAULT_ABI_VERSION,
                reserved0: 0,
                fault_run_pages: self.fault_run_pages.load(Ordering::Relaxed) as u32,
                process_vma_ceiling: self.process_vma_ceiling.load(Ordering::Relaxed) as u32,
                demand_enabled: self.demand_enabled.load(Ordering::Relaxed) as u32,
                wired_services,
                reserved1: [0; 2],
            };

            // ORDERING: equality with the first Acquire proves no writer
            // invalidated or committed the payload during this attempt.
            let after = self.sequence.load(Ordering::Acquire);
            if before == after {
                return Ok(Some(policy));
            }
        }
        Err(())
    }
}

static ANONYMOUS_POLICY: PublishedAnonymousPolicy = PublishedAnonymousPolicy::empty();

/// The policy in force for normal, retryable context.
///
/// Admission decisions may not use a mixed or fallback snapshot, so they
/// retry while a bounded writer publication is in progress.
pub(crate) fn anonymous_policy() -> PagerAnonymousPolicyWire {
    loop {
        match ANONYMOUS_POLICY.read() {
            Ok(Some(policy)) => return policy,
            Ok(None) => return default_policy(),
            Err(()) => core::hint::spin_loop(),
        }
    }
}

/// The policy in force for the IRQ-off best-effort fault-around reader.
///
/// Two failed sequence attempts change only this fault's speculative run
/// length, so the compiled-in default is the safe bounded fallback.
pub(crate) fn anonymous_policy_irq_off() -> PagerAnonymousPolicyWire {
    ANONYMOUS_POLICY
        .read()
        .ok()
        .flatten()
        .unwrap_or_else(default_policy)
}

/// Replaces the policy snapshot. `false` means the prior snapshot stands.
pub(crate) fn publish_anonymous_policy(policy: PagerAnonymousPolicyWire) -> bool {
    policy.is_canonical() && ANONYMOUS_POLICY.publish(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

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
    fn every_publication_is_a_coherent_replaceable_snapshot() {
        let slot = PublishedAnonymousPolicy::empty();
        assert_eq!(slot.read(), Ok(None));

        let mut first = default_policy();
        first.fault_run_pages = 4;
        first.demand_enabled = 0;
        assert!(slot.publish(first));
        assert_eq!(slot.read(), Ok(Some(first)));

        let mut second = default_policy();
        second.fault_run_pages = 1;
        second.process_vma_ceiling = 7;
        assert!(slot.publish(second));
        assert_eq!(slot.read(), Ok(Some(second)));
    }

    #[test]
    fn concurrent_republication_never_yields_a_mixed_policy_epoch() {
        use core::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let slot = Arc::new(PublishedAnonymousPolicy::empty());
        let done = Arc::new(AtomicBool::new(false));
        let mut first = default_policy();
        first.fault_run_pages = 4;
        first.process_vma_ceiling = 11;
        first.demand_enabled = 0;
        let mut second = default_policy();
        second.fault_run_pages = 1;
        second.process_vma_ceiling = 23;
        second.demand_enabled = 1;

        let writer_slot = Arc::clone(&slot);
        let writer_done = Arc::clone(&done);
        let writer = std::thread::spawn(move || {
            for index in 0..20_000 {
                assert!(writer_slot.publish(if index & 1 == 0 { first } else { second }));
            }
            // ORDERING: release publishes completion after the last policy commit.
            writer_done.store(true, Ordering::Release);
        });

        // ORDERING: acquire keeps the reader active until all publications finish.
        while !done.load(Ordering::Acquire) {
            if let Ok(Some(observed)) = slot.read() {
                assert!(observed == first || observed == second);
            }
        }
        writer.join().unwrap();
    }

    #[test]
    fn an_irq_reader_refuses_an_in_progress_snapshot() {
        let slot = PublishedAnonymousPolicy::empty();
        slot.sequence.store(1, Ordering::Release);
        assert_eq!(slot.read(), Err(()));
    }
}
