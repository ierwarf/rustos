//! Lock-free publication of pager-owned virtual-memory regions.
//!
//! - **Owner:** `kernel-ps` stamps process/MM/VMA generations; pagerd owns
//!   backing and fault policy carried by each published region.
//! - **Boundary:** Syscall-time writers publish under one bounded raw lock;
//!   exception-time readers use only atomics and exact live process identity.
//! - **Lifecycle:** Validate template, reject overlap, stamp and publish, then
//!   revoke the exact generation before unmap, exec, exit, or pager restart.
//! - **Concurrency:** Each slot is an all-atomic sequence publication. Readers
//!   perform at most two attempts and never acquire `ProcessStateLock`.
//! - **Failure:** Malformed, overlapping, stale, unstable, exhausted, or
//!   unauthorized observations fail closed without blocking the fault path.
//! - **Forbidden:** No plain-data seqlock race, PID-only authority, generation
//!   wrap/reuse, unauthorized W+X publication, physical address, or exception-time wait.
//! - **Evidence:** Focused unit tests plus the `pager-vma-publication-*` formal
//!   and implementation mutations.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::pager::{
    PAGER_MAX_REGION_GROWTH_PER_PROTECT, PAGER_MAX_REGION_GROWTH_PER_UNMAP,
    PAGER_MAX_VMAS_PER_PROCESS, PAGER_PAGE_BYTES, PagerEndpointCapabilityWire,
    PagerObjectIdentityWire, PagerRangeEdit, PagerRegionEdit, PagerVmRegionWire, VM_ACCESS_EXECUTE,
    VM_ACCESS_KNOWN, VM_ACCESS_READ, VM_ACCESS_WRITE, VM_COMMIT_COMMITTED, VM_COMMIT_RESERVED,
    VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE, VM_SHARING_PRIVATE, VM_SHARING_SHARED,
    apply_region_edit, vm_prot_is_rwx,
};
use kernel_mm::api::address_space::ExecMemoryPolicy;

use super::slot_dir::AtomicDir;
use super::process_table::{ProcessHandle, ProcessIdentity};

mod fork_hold;
mod rewrite_epoch;
pub(super) use fork_hold::with_fork_held_regions;
use rewrite_epoch::{RangeRewrite, read_rewrite_epoch};

/// Hard per-process directory ceiling for pager VMAs, taken from the shared ABI.
///
/// Matches Linux `vm.max_map_count` (`65536`). Each process's VMA slots grow
/// lazily in `VMA_SLOT_CHUNK`-slot chunks up to this directory ceiling
/// (narrowed by Ring3 policy `ceiling`), indexed by a lock-free-readable
/// per-process B-tree for O(log N) fault lookup and O(log N + K) range edits.
/// `PAGER_MIN_FULLY_TRACKED_PROCESSES` in the ABI remains the cross-process
/// relation with pagerd.
const MAX_PAGER_VMAS_PER_PROCESS: usize = PAGER_MAX_VMAS_PER_PROCESS;
const VMA_SLOT_CHUNK: usize = 128;
const VMA_SLOT_DIR: usize = 512;
const _: () = assert!(
    VMA_SLOT_CHUNK * VMA_SLOT_DIR == MAX_PAGER_VMAS_PER_PROCESS,
    "chunked per-process VMA directory must match the ABI per-process ceiling"
);
const MAX_PUBLICATION_SEQUENCE: u64 = u64::MAX - 3;

/// The exact process and MM incarnation a caller captured before it asked
/// for a range edit. Scalar PIDs identify nothing across `exec`; an edit that
/// names an expectation is refused as `Stale` rather than landing in whatever
/// image now owns the PID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedMm {
    pub process_generation: u32,
    pub mm_generation: u32,
}

impl ExpectedMm {
    pub(super) fn check(
        expected: Option<Self>,
        identity: ProcessIdentity,
    ) -> Result<(), PagerVmaError> {
        match expected {
            Some(expected)
                if expected.process_generation != identity.process_generation()
                    || expected.mm_generation != identity.mm_generation() =>
            {
                Err(PagerVmaError::Stale)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PagerVmaError {
    Malformed,
    Overlap,
    Pressure,
    Stale,
    Unstable,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PagerVmaSnapshot {
    pub task_id: u64,
    pub process_id: u64,
    pub region: PagerVmRegionWire,
}

/// How long a withdrawing writer waits for exception-time installers to drop
/// their permits before it fails closed.
///
/// Sized for host vCPU preemption, not for the installer's own work, which is
/// a handful of instructions. Exceeding it means an installer was descheduled
/// for longer than any ordinary host scheduling quantum, and the withdrawal
/// must not proceed on the assumption that it is finished.
#[cfg(rustos_boot_image)]
const INSTALLER_DRAIN_TIMEOUT_NS: u64 = 250_000_000;

struct PublishedPagerVma {
    sequence: AtomicU64,
    /// Number of exception-time installers that observed this exact published
    /// VMA and may still touch one of its prepared leaves.  A writer withdraws
    /// the publication before changing PTEs, then drains this count; the
    /// installer itself never takes the writer or process-state lock.
    fault_installers: AtomicU64,
    start: AtomicU64,
    end: AtomicU64,
    object_type: AtomicU64,
    object_rights: AtomicU64,
    backing_service: AtomicU64,
    object_slot: AtomicU64,
    object_generation: AtomicU64,
    pager_epoch: AtomicU64,
    backing_generation: AtomicU64,
    object_offset: AtomicU64,
    prot: AtomicU64,
    sharing: AtomicU64,
    commit_state: AtomicU64,
    vma_generation: AtomicU64,
    process_handle: AtomicU64,
    process_generation: AtomicU64,
    mm_generation: AtomicU64,
    fault_endpoint_slot: AtomicU64,
    fault_endpoint_generation: AtomicU64,
    fault_endpoint_rights: AtomicU64,
}

impl PublishedPagerVma {
    const fn empty() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            fault_installers: AtomicU64::new(0),
            start: AtomicU64::new(0),
            end: AtomicU64::new(0),
            object_type: AtomicU64::new(0),
            object_rights: AtomicU64::new(0),
            backing_service: AtomicU64::new(0),
            object_slot: AtomicU64::new(0),
            object_generation: AtomicU64::new(0),
            pager_epoch: AtomicU64::new(0),
            backing_generation: AtomicU64::new(0),
            object_offset: AtomicU64::new(0),
            prot: AtomicU64::new(0),
            sharing: AtomicU64::new(0),
            commit_state: AtomicU64::new(0),
            vma_generation: AtomicU64::new(0),
            process_handle: AtomicU64::new(0),
            process_generation: AtomicU64::new(0),
            mm_generation: AtomicU64::new(0),
            fault_endpoint_slot: AtomicU64::new(0),
            fault_endpoint_generation: AtomicU64::new(0),
            fault_endpoint_rights: AtomicU64::new(0),
        }
    }

    /// An extent prefilter may reject an address only from one unchanged, even
    /// publication. Otherwise let the full snapshot decide; filtering from
    /// stale bounds can turn an in-flight valid first touch into `Stale`.
    fn stable_extent(&self) -> Option<(u64, u64)> {
        // ORDERING: the final Acquire rechecks the writer's odd invalidation
        // or next even Release after both Relaxed bounds reads. The first
        // Acquire alone cannot authorize an early negative result.
        let before = self.sequence.load(Ordering::Acquire);
        if before & 1 != 0 {
            return None;
        }
        let start = self.start.load(Ordering::Relaxed);
        let end = self.end.load(Ordering::Relaxed);
        // ORDERING: this Acquire must see the same even publication sampled
        // before the bounds, or the filter cannot discard the slot.
        let after = self.sequence.load(Ordering::Acquire);
        Self::stable_extent_from_samples(before, after, start, end)
    }

    #[inline]
    fn stable_extent_from_samples(
        before: u64,
        after: u64,
        start: u64,
        end: u64,
    ) -> Option<(u64, u64)> {
        (before == after && before & 1 == 0).then_some((start, end))
    }

    #[cfg(test)]
    fn may_overlap(&self, start: u64, end: u64) -> bool {
        self.stable_extent().is_none_or(|(slot_start, slot_end)| {
            slot_start != 0 && start < slot_end && slot_start < end
        })
    }

    #[cfg(test)]
    fn may_overlap_or_touch(&self, start: u64, end: u64) -> bool {
        self.stable_extent().is_none_or(|(slot_start, slot_end)| {
            slot_start != 0 && start <= slot_end && slot_start <= end
        })
    }

    fn may_cover(&self, address: u64) -> bool {
        self.stable_extent()
            .is_none_or(|(start, end)| start != 0 && address >= start && address < end)
    }

    fn snapshot(&self) -> Result<Option<PagerVmRegionWire>, PagerVmaError> {
        for _ in 0..2 {
            // ORDERING: this acquire observes the writer's final even Release
            // commit before any Relaxed payload field is read; an odd value
            // means no field in this attempt may become a fault authority.
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                continue;
            }
            let start = self.start.load(Ordering::Relaxed);
            let region = PagerVmRegionWire {
                start,
                end: self.end.load(Ordering::Relaxed),
                object: PagerObjectIdentityWire {
                    object_type: self.object_type.load(Ordering::Relaxed) as u16,
                    reserved0: 0,
                    rights: self.object_rights.load(Ordering::Relaxed) as u32,
                    backing_service: self.backing_service.load(Ordering::Relaxed),
                    slot: self.object_slot.load(Ordering::Relaxed),
                    generation: self.object_generation.load(Ordering::Relaxed),
                    pager_epoch: self.pager_epoch.load(Ordering::Relaxed),
                    backing_generation: self.backing_generation.load(Ordering::Relaxed),
                },
                object_offset: self.object_offset.load(Ordering::Relaxed),
                prot: self.prot.load(Ordering::Relaxed) as u32,
                sharing: self.sharing.load(Ordering::Relaxed) as u16,
                commit_state: self.commit_state.load(Ordering::Relaxed) as u16,
                vma_generation: self.vma_generation.load(Ordering::Relaxed),
                process_handle: self.process_handle.load(Ordering::Relaxed),
                process_generation: self.process_generation.load(Ordering::Relaxed),
                mm_generation: self.mm_generation.load(Ordering::Relaxed),
                fault_endpoint: PagerEndpointCapabilityWire {
                    slot: self.fault_endpoint_slot.load(Ordering::Relaxed),
                    generation: self.fault_endpoint_generation.load(Ordering::Relaxed),
                    rights: self.fault_endpoint_rights.load(Ordering::Relaxed),
                },
                reserved1: [0; 2],
            };
            // ORDERING: this acquire pairs with either the writer's odd
            // invalidation or final even commit. Equality proves no writer
            // published or revoked the Relaxed payload during this snapshot.
            let after = self.sequence.load(Ordering::Acquire);
            if before == after {
                return Ok((start != 0).then_some(region));
            }
        }
        Err(PagerVmaError::Unstable)
    }

    fn publish(&self, region: Option<PagerVmRegionWire>) -> Result<(), PagerVmaError> {
        let before = self.sequence.load(Ordering::Relaxed);
        if before & 1 != 0 || before > MAX_PUBLICATION_SEQUENCE {
            return Err(PagerVmaError::Pressure);
        }
        // ORDERING: Release publishes the odd invalidation before the payload
        // changes, so a concurrent reader abandons its snapshot rather than
        // accepting a mixture of old and new VMA fields.
        self.sequence.store(before + 1, Ordering::Release);
        // The store above is followed by a load of a **different** location -
        // the installer count - while an installer does the mirror image:
        // register, then load this sequence. Release/Acquire orders neither
        // pair against the other, and x86 TSO permits exactly this StoreLoad
        // reordering, so without a full barrier a writer can read zero
        // installers while an installer reads a stale even sequence and takes
        // a permit. Both then own the same prepared leaf, and the writer
        // reclaims a frame an exception-time CAS is still installing into.
        // `loom-proof-kernel` enumerates that interleaving in one iteration,
        // and `formal/litmus/x86_64/pager_fault_install_permit.litmus` shows
        // its mutant reaching the forbidden state without this fence.
        // ORDERING: SeqCst fence, the store-buffer barrier that orders this
        // writer's odd-sequence store before its load of the installer count.
        core::sync::atomic::fence(Ordering::SeqCst);
        if region.is_none() && !self.drain_fault_installers() {
            // Payload remains intact; restore its prior stable sequence so
            // callers fail closed without publishing a half-withdrawn VMA.
            // ORDERING: Release restores the previously stable payload
            // before readers may accept its even sequence again.
            self.sequence.store(before, Ordering::Release);
            return Err(PagerVmaError::Unstable);
        }
        let region = region.unwrap_or_default();
        self.start.store(region.start, Ordering::Relaxed);
        self.end.store(region.end, Ordering::Relaxed);
        self.object_type
            .store(u64::from(region.object.object_type), Ordering::Relaxed);
        self.object_rights
            .store(u64::from(region.object.rights), Ordering::Relaxed);
        self.backing_service
            .store(region.object.backing_service, Ordering::Relaxed);
        self.object_slot
            .store(region.object.slot, Ordering::Relaxed);
        self.object_generation
            .store(region.object.generation, Ordering::Relaxed);
        self.pager_epoch
            .store(region.object.pager_epoch, Ordering::Relaxed);
        self.backing_generation
            .store(region.object.backing_generation, Ordering::Relaxed);
        self.object_offset
            .store(region.object_offset, Ordering::Relaxed);
        self.prot.store(u64::from(region.prot), Ordering::Relaxed);
        self.sharing
            .store(u64::from(region.sharing), Ordering::Relaxed);
        self.commit_state
            .store(u64::from(region.commit_state), Ordering::Relaxed);
        self.vma_generation
            .store(region.vma_generation, Ordering::Relaxed);
        self.process_handle
            .store(region.process_handle, Ordering::Relaxed);
        self.process_generation
            .store(region.process_generation, Ordering::Relaxed);
        self.mm_generation
            .store(region.mm_generation, Ordering::Relaxed);
        self.fault_endpoint_slot
            .store(region.fault_endpoint.slot, Ordering::Relaxed);
        self.fault_endpoint_generation
            .store(region.fault_endpoint.generation, Ordering::Relaxed);
        self.fault_endpoint_rights
            .store(region.fault_endpoint.rights, Ordering::Relaxed);
        // ORDERING: the final even Release commits every preceding Relaxed
        // payload store; a reader that acquires this exact sequence may admit
        // the VMA only after its matching second sequence observation.
        self.sequence.store(before + 2, Ordering::Release);
        Ok(())
    }

    /// Waits for every installer that observed the now-odd publication to drop
    /// its permit. `false` means one did not, and withdrawal must not commit.
    ///
    /// The bound is wall-clock, not a spin count, and that distinction is the
    /// whole contract. An installer holds its permit across a lock-free frame
    /// reservation and one leaf CAS with interrupts clear: it cannot block,
    /// allocate, or be preempted by this guest, so in *guest* time the drain is
    /// always short. What it can lose is its physical CPU - a KVM host may
    /// deschedule that vCPU mid-permit for milliseconds. A fixed spin count
    /// measures the wrong clock and turns ordinary host scheduling into a
    /// spurious `munmap`/`mprotect` failure, which is exactly the shape that
    /// only appears under a loaded multi-vCPU guest.
    fn drain_fault_installers(&self) -> bool {
        // ORDERING: Acquire observes every installer Drop's Release decrement
        // before this writer changes any PTE topology. The fence in `publish`
        // is what keeps this load from being reordered before the odd-sequence
        // store that must precede it.
        if self.fault_installers.load(Ordering::Acquire) == 0 {
            return true;
        }
        #[cfg(rustos_boot_image)]
        {
            let mut deadline = kernel_hal::api::arch::clock::SpinDeadline::start();
            loop {
                // ORDERING: same Acquire/Release drain contract as the fast
                // path; a drained count means every installer's leaf CAS is
                // already visible to this writer.
                if self.fault_installers.load(Ordering::Acquire) == 0 {
                    return true;
                }
                if deadline.elapsed_nanos() >= INSTALLER_DRAIN_TIMEOUT_NS {
                    return false;
                }
                core::hint::spin_loop();
            }
        }
        #[cfg(not(rustos_boot_image))]
        {
            // Host tests have no monotonic clock and no concurrent installer
            // that outlives its caller, so a fixed bound is exact here.
            for _ in 0..1_000_000 {
                // ORDERING: as above; the host-test bound differs, the drain
                // contract does not.
                if self.fault_installers.load(Ordering::Acquire) == 0 {
                    return true;
                }
                core::hint::spin_loop();
            }
            false
        }
    }

    /// Holds this exact live publication at an odd sequence without clearing
    /// its payload. Fault readers therefore return `Unstable` and restart;
    /// unlike a stable empty publication, the hold can never turn a concurrent
    /// fork-time protection fault into an unmapped-address verdict.
    fn begin_fork_hold(&self, expected: PagerVmRegionWire) -> Result<u64, PagerVmaError> {
        // ORDERING: acquire pairs with the prior even publication before the
        // expected payload is validated and converted into a writer hold.
        let before = self.sequence.load(Ordering::Acquire);
        if before == 0
            || before & 1 != 0
            || before > MAX_PUBLICATION_SEQUENCE - 2
            || self.snapshot()? != Some(expected)
        {
            return Err(PagerVmaError::Stale);
        }
        // ORDERING: release publishes the odd hold; the SeqCst fence closes the
        // writer/installer store-buffering cycle before installer drain.
        self.sequence.store(before + 1, Ordering::Release);
        core::sync::atomic::fence(Ordering::SeqCst);
        if !self.drain_fault_installers() {
            // ORDERING: release withdraws the failed odd hold so later readers
            // may again observe the unchanged payload at its prior generation.
            self.sequence.store(before, Ordering::Release);
            return Err(PagerVmaError::Unstable);
        }
        Ok(before)
    }

    fn finish_fork_hold(&self, before: u64) {
        // ORDERING: acquire proves this exact odd hold still owns the slot.
        assert_eq!(self.sequence.load(Ordering::Acquire), before + 1);
        // ORDERING: the payload was never changed; release of the next even
        // generation publishes completed parent downgrade and TLB ACK.
        self.sequence.store(before + 2, Ordering::Release);
    }

    fn try_acquire_fault_install(
        &'static self,
        expected: PagerVmRegionWire,
    ) -> Result<PagerFaultInstallPermit, PagerVmaError> {
        // ORDERING: Acquire pairs with the publisher's final Release commit
        // before this prospective installer reads an exact VMA snapshot.
        let before = self.sequence.load(Ordering::Acquire);
        if before == 0 || before & 1 != 0 || self.snapshot()? != Some(expected) {
            return Err(PagerVmaError::Stale);
        }
        // ORDERING: AcqRel makes this installer visible to a withdrawing
        // writer before the second sequence check closes the publication race.
        self.fault_installers.fetch_add(1, Ordering::AcqRel);
        // The installer half of the store-buffer barrier described in
        // `publish`. Without it this registration and the sequence load below
        // may be reordered, and a writer whose own pair was reordered would
        // see no installer while this installer sees no withdrawal - each
        // concluding the other is absent.
        // ORDERING: SeqCst fence, ordering this registration before the
        // sequence re-read that decides whether the permit is issued.
        core::sync::atomic::fence(Ordering::SeqCst);
        // ORDERING: Acquire observes a writer's odd invalidation whenever that
        // writer did not observe this installer's count.
        let after = self.sequence.load(Ordering::Acquire);
        if before == after && after & 1 == 0 && self.snapshot()? == Some(expected) {
            return Ok(PagerFaultInstallPermit { slot: self });
        }
        // ORDERING: Release lets a withdrawing writer observe that this
        // rejected installer will never access a leaf.
        self.fault_installers.fetch_sub(1, Ordering::Release);
        Err(PagerVmaError::Stale)
    }
}

/// Exact publication permit for a single IRQ-off prepared-leaf CAS.
///
/// The permit is deliberately neither cloneable nor transferable.  Dropping it
/// is the linearization point that lets a withdrawing VMA writer proceed to
/// ordinary locked PTE mutation and TLB reclamation.
pub struct PagerFaultInstallPermit {
    slot: &'static PublishedPagerVma,
}

impl Drop for PagerFaultInstallPermit {
    fn drop(&mut self) {
        // ORDERING: Release pairs with the withdrawing writer's Acquire drain
        // before it mutates or reclaims the prepared leaf.
        self.slot.fault_installers.fetch_sub(1, Ordering::Release);
    }
}

#[expect(
    clippy::declare_interior_mutable_const,
    reason = "initializer for lazily allocated all-atomic VMA slot chunks"
)]
const EMPTY_PAGER_VMA: PublishedPagerVma = PublishedPagerVma::empty();

type VmaChunkBox = alloc::boxed::Box<[PublishedPagerVma]>;

fn build_default_vma_chunk() -> Option<VmaChunkBox> {
    AtomicDir::<PublishedPagerVma, VMA_SLOT_CHUNK, VMA_SLOT_DIR>::build_chunk(|| EMPTY_PAGER_VMA)
}

/// All-atomic B-tree indexing non-overlapping VMA intervals sorted by `start`.
///
/// - Nodes live in a grow-only `AtomicDir`, so they never move or free and
///   exception-time readers never experience use-after-free or plain-data
///   seqlock races.
/// - Writers mutate nodes only while holding the per-process writer spinlock
///   AND an active `RangeRewrite` (odd `REWRITE_EPOCHS`).
/// - Exception-time readers traverse under `read_rewrite_epoch`: every atomic
///   read is bounds-checked and step-bounded (`BTREE_MAX_WALK_STEPS`), so a
///   concurrent writer can never cause a panic or loop and is caught as
///   `PagerVmaError::Unstable` by the enclosing epoch check.
mod btree {
    use super::*;

    pub(super) const BTREE_DEGREE: usize = 16;
    pub(super) const BTREE_MIN_KEYS: usize = BTREE_DEGREE - 1; // 15
    pub(super) const BTREE_MAX_KEYS: usize = 2 * BTREE_DEGREE - 1; // 31
    pub(super) const BTREE_MAX_CHILDREN: usize = 2 * BTREE_DEGREE; // 32
    pub(super) const BTREE_NODE_CHUNK: usize = 32;
    pub(super) const BTREE_NODE_DIR: usize = 256;
    pub(super) const BTREE_MAX_NODES: usize = BTREE_NODE_CHUNK * BTREE_NODE_DIR; // 8192
    pub(super) const BTREE_NO_NODE: u32 = u32::MAX;
    /// Mathematical max height for 65,536 keys at degree 16 is 5; 8 gives
    /// headroom while bounding torn concurrent walks to O(1) steps.
    pub(super) const BTREE_MAX_WALK_STEPS: usize = 8;

    const _: () = assert!(
        BTREE_MAX_NODES >= 2 + (MAX_PAGER_VMAS_PER_PROCESS / BTREE_MIN_KEYS) + BTREE_MAX_WALK_STEPS,
        "B-tree node directory must cover worst-case minimum fill at full VMA ceiling"
    );

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub(super) struct BTreeEntry {
        pub start: u64,
        pub end: u64,
        pub slot: u32,
    }

    pub(super) struct BTreeNode {
        /// Low 16 bits: key count `0..=BTREE_MAX_KEYS`. Bit 16: `1` if leaf.
        header: AtomicU32,
        starts: [AtomicU64; BTREE_MAX_KEYS],
        ends: [AtomicU64; BTREE_MAX_KEYS],
        slots: [AtomicU32; BTREE_MAX_KEYS],
        children: [AtomicU32; BTREE_MAX_CHILDREN],
    }

    impl BTreeNode {
        const fn empty() -> Self {
            Self {
                header: AtomicU32::new(1 << 16),
                starts: [const { AtomicU64::new(0) }; BTREE_MAX_KEYS],
                ends: [const { AtomicU64::new(0) }; BTREE_MAX_KEYS],
                slots: [const { AtomicU32::new(0) }; BTREE_MAX_KEYS],
                children: [const { AtomicU32::new(BTREE_NO_NODE) }; BTREE_MAX_CHILDREN],
            }
        }

        #[inline]
        const fn decode_header(header: u32) -> (usize, bool) {
            ((header & 0xFFFF) as usize, (header >> 16) != 0)
        }

        #[inline]
        const fn encode_header(len: usize, is_leaf: bool) -> u32 {
            (len as u32) | ((is_leaf as u32) << 16)
        }

        #[inline]
        fn len(&self) -> usize {
            Self::decode_header(self.header.load(Ordering::Relaxed)).0
        }

        #[inline]
        fn is_leaf(&self) -> bool {
            Self::decode_header(self.header.load(Ordering::Relaxed)).1
        }

        #[inline]
        fn set_header(&self, len: usize, is_leaf: bool) {
            debug_assert!(len <= BTREE_MAX_KEYS);
            self.header
                .store(Self::encode_header(len, is_leaf), Ordering::Relaxed);
        }

        #[inline]
        fn key(&self, idx: usize) -> BTreeEntry {
            BTreeEntry {
                start: self.starts[idx].load(Ordering::Relaxed),
                end: self.ends[idx].load(Ordering::Relaxed),
                slot: self.slots[idx].load(Ordering::Relaxed),
            }
        }

        #[inline]
        fn set_key(&self, idx: usize, entry: BTreeEntry) {
            self.starts[idx].store(entry.start, Ordering::Relaxed);
            self.ends[idx].store(entry.end, Ordering::Relaxed);
            self.slots[idx].store(entry.slot, Ordering::Relaxed);
        }

        #[inline]
        fn child(&self, idx: usize) -> u32 {
            self.children[idx].load(Ordering::Relaxed)
        }

        #[inline]
        fn set_child(&self, idx: usize, child: u32) {
            self.children[idx].store(child, Ordering::Relaxed);
        }

        /// First index in `0..len` with `starts[idx] >= target`.
        #[inline]
        fn lower_bound_start(&self, target: u64, len: usize) -> usize {
            let mut lo = 0_usize;
            let mut hi = len;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if self.starts[mid].load(Ordering::Relaxed) < target {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        }

        /// First index in `0..len` with `ends[idx] > target` (or `>= target` if `inclusive`).
        #[inline]
        fn lower_bound_end(&self, target: u64, len: usize, inclusive: bool) -> usize {
            let mut lo = 0_usize;
            let mut hi = len;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let end = self.ends[mid].load(Ordering::Relaxed);
                let skip = if inclusive {
                    end < target
                } else {
                    end <= target
                };
                if skip {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        }
    }

    #[expect(
        clippy::declare_interior_mutable_const,
        reason = "initializer for lazily allocated all-atomic B-tree node chunks"
    )]
    const EMPTY_BTREE_NODE: BTreeNode = BTreeNode::empty();

    pub(super) struct VmaIndex {
        root: AtomicU32,
        nodes: AtomicDir<BTreeNode, BTREE_NODE_CHUNK, BTREE_NODE_DIR>,
    }

    impl VmaIndex {
        pub(super) const fn empty() -> Self {
            Self {
                root: AtomicU32::new(BTREE_NO_NODE),
                nodes: AtomicDir::new(),
            }
        }

        #[inline]
        fn node(&self, idx: u32) -> &BTreeNode {
            &self.nodes[idx as usize]
        }

        /// Ensures enough B-tree node chunks exist so subsequent mutations up
        /// to `peak_live` keys are infallible inside `RangeRewrite`.
        pub(super) fn ensure_node_headroom(
            &self,
            writer: &mut VmaWriterState,
            peak_live: usize,
        ) -> Result<(), PagerVmaError> {
            let needed_for_peak = if peak_live == 0 {
                0
            } else {
                2 + (peak_live / BTREE_MIN_KEYS) + BTREE_MAX_WALK_STEPS
            };
            let used = writer.nodes_initialized.saturating_sub(writer.free_nodes.len());
            let target = needed_for_peak
                .max(used.saturating_add(BTREE_MAX_WALK_STEPS + 2))
                .min(BTREE_MAX_NODES);
            if writer.nodes_initialized >= target {
                return Ok(());
            }
            let extra_chunks = (target - writer.nodes_initialized).div_ceil(BTREE_NODE_CHUNK);
            let base_chunk = writer.nodes_initialized / BTREE_NODE_CHUNK;
            if base_chunk.checked_add(extra_chunks).is_none_or(|end| end > BTREE_NODE_DIR) {
                return Err(PagerVmaError::Pressure);
            }
            writer
                .free_nodes
                .try_reserve(extra_chunks * BTREE_NODE_CHUNK)
                .map_err(|_| PagerVmaError::Pressure)?;
            let mut built = Vec::new();
            built
                .try_reserve_exact(extra_chunks)
                .map_err(|_| PagerVmaError::Pressure)?;
            for _ in 0..extra_chunks {
                let chunk = AtomicDir::<BTreeNode, BTREE_NODE_CHUNK, BTREE_NODE_DIR>::build_chunk(
                    || EMPTY_BTREE_NODE,
                )
                .ok_or(PagerVmaError::Pressure)?;
                built.push(chunk);
            }
            for (offset, chunk) in built.into_iter().enumerate() {
                if self.nodes.publish_chunk(base_chunk + offset, chunk).is_err() {
                    return Err(PagerVmaError::Pressure);
                }
            }
            let first = writer.nodes_initialized;
            let count = extra_chunks * BTREE_NODE_CHUNK;
            for node_idx in (first..first + count).rev() {
                writer.free_nodes.push(node_idx as u32);
            }
            writer.nodes_initialized += count;
            Ok(())
        }

        /// Lock-free O(log N) lookup of the slot index whose interval covers
        /// `address`. Must be enclosed in `read_rewrite_epoch`.
        pub(super) fn find_containing(&self, address: u64) -> Result<Option<usize>, PagerVmaError> {
            // ORDERING: Acquire pairs with the writer's Release commit on
            // `REWRITE_EPOCHS` (checked by `read_rewrite_epoch`) before
            // traversing any B-tree node reachable from `root`.
            let mut node_idx = self.root.load(Ordering::Acquire);
            if node_idx == BTREE_NO_NODE {
                return Ok(None);
            }
            for _ in 0..BTREE_MAX_WALK_STEPS {
                let Some(node) = self.nodes.get(node_idx as usize) else {
                    return Err(PagerVmaError::Unstable);
                };
                // ORDERING: Acquire observes the node header published prior
                // to the enclosing rewrite epoch's Release before reading
                // Relaxed key/child arrays; a concurrent mutation is rejected
                // by `read_rewrite_epoch`.
                let (len, is_leaf) = BTreeNode::decode_header(node.header.load(Ordering::Acquire));
                if len > BTREE_MAX_KEYS {
                    return Err(PagerVmaError::Unstable);
                }
                let mut lo = 0_usize;
                let mut hi = len;
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if node.starts[mid].load(Ordering::Relaxed) <= address {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                if lo > 0 {
                    let end = node.ends[lo - 1].load(Ordering::Relaxed);
                    if address < end {
                        return Ok(Some(node.slots[lo - 1].load(Ordering::Relaxed) as usize));
                    }
                }
                if is_leaf {
                    return Ok(None);
                }
                let child = node.children[lo].load(Ordering::Relaxed);
                if child == BTREE_NO_NODE {
                    return Err(PagerVmaError::Unstable);
                }
                node_idx = child;
            }
            Err(PagerVmaError::Unstable)
        }

        /// Writer-locked O(log N) lookup for an exact `start` address.
        pub(super) fn find_exact(&self, start: u64) -> Option<BTreeEntry> {
            let mut node_idx = self.root.load(Ordering::Relaxed);
            while node_idx != BTREE_NO_NODE {
                let node = self.node(node_idx);
                let len = node.len();
                let pos = node.lower_bound_start(start, len);
                if pos < len && node.starts[pos].load(Ordering::Relaxed) == start {
                    return Some(node.key(pos));
                }
                if node.is_leaf() {
                    return None;
                }
                node_idx = node.child(pos);
            }
            None
        }

        /// Writer-locked O(log N) lookup of the minimum-`start` entry whose
        /// `end > query_start`. Because intervals are non-overlapping, `end`
        /// is strictly increasing in in-order traversal, so a single root-to-
        /// leaf descent finds the unique earliest candidate.
        pub(super) fn first_ending_after(&self, query_start: u64) -> Option<BTreeEntry> {
            let mut best = None;
            let mut node_idx = self.root.load(Ordering::Relaxed);
            while node_idx != BTREE_NO_NODE {
                let node = self.node(node_idx);
                let len = node.len();
                let pos = node.lower_bound_end(query_start, len, false);
                if pos < len {
                    best = Some(node.key(pos));
                }
                if node.is_leaf() {
                    break;
                }
                node_idx = node.child(pos);
            }
            best
        }

        /// Writer-locked O(log N + K) in-order collection of all entries
        /// intersecting `[start, end)` (`inclusive == false`) or intersecting
        /// or touching `[start, end]` (`inclusive == true`).
        pub(super) fn collect_range(
            &self,
            start: u64,
            end: u64,
            inclusive: bool,
            out: &mut Vec<BTreeEntry>,
        ) -> Result<(), PagerVmaError> {
            let root = self.root.load(Ordering::Relaxed);
            if root != BTREE_NO_NODE {
                self.collect_range_node(root, start, end, inclusive, out)?;
            }
            Ok(())
        }

        fn collect_range_node(
            &self,
            node_idx: u32,
            start: u64,
            end: u64,
            inclusive: bool,
            out: &mut Vec<BTreeEntry>,
        ) -> Result<(), PagerVmaError> {
            let node = self.node(node_idx);
            let len = node.len();
            let is_leaf = node.is_leaf();
            let first = node.lower_bound_end(start, len, inclusive);
            for i in first..len {
                if !is_leaf {
                    self.collect_range_node(node.child(i), start, end, inclusive, out)?;
                }
                let entry = node.key(i);
                let past = if inclusive {
                    entry.start > end
                } else {
                    entry.start >= end
                };
                if past {
                    return Ok(());
                }
                out.try_reserve(1).map_err(|_| PagerVmaError::Pressure)?;
                out.push(entry);
            }
            if !is_leaf {
                self.collect_range_node(node.child(len), start, end, inclusive, out)?;
            }
            Ok(())
        }

        /// Writer-locked O(L) in-order collection of every live entry.
        pub(super) fn collect_all(&self, out: &mut Vec<BTreeEntry>) -> Result<(), PagerVmaError> {
            let root = self.root.load(Ordering::Relaxed);
            if root != BTREE_NO_NODE {
                self.collect_all_node(root, out)?;
            }
            Ok(())
        }

        fn collect_all_node(
            &self,
            node_idx: u32,
            out: &mut Vec<BTreeEntry>,
        ) -> Result<(), PagerVmaError> {
            let node = self.node(node_idx);
            let len = node.len();
            let is_leaf = node.is_leaf();
            for i in 0..len {
                if !is_leaf {
                    self.collect_all_node(node.child(i), out)?;
                }
                out.try_reserve(1).map_err(|_| PagerVmaError::Pressure)?;
                out.push(node.key(i));
            }
            if !is_leaf {
                self.collect_all_node(node.child(len), out)?;
            }
            Ok(())
        }

        /// Writer-locked O(log N) top-down B-tree insert (infallible once
        /// `ensure_node_headroom` has run).
        pub(super) fn insert(&self, writer: &mut VmaWriterState, entry: BTreeEntry) {
            let root_idx = self.root.load(Ordering::Relaxed);
            if root_idx == BTREE_NO_NODE {
                let new_root = writer.pop_free_node();
                let node = self.node(new_root);
                node.set_key(0, entry);
                node.set_child(0, BTREE_NO_NODE);
                node.set_header(1, true);
                self.root.store(new_root, Ordering::Relaxed);
                return;
            }
            if self.node(root_idx).len() == BTREE_MAX_KEYS {
                let new_root = writer.pop_free_node();
                let root_node = self.node(new_root);
                root_node.set_child(0, root_idx);
                root_node.set_header(0, false);
                self.root.store(new_root, Ordering::Relaxed);
                self.split_child(writer, new_root, 0);
                self.insert_non_full(writer, new_root, entry);
            } else {
                self.insert_non_full(writer, root_idx, entry);
            }
        }

        fn split_child(&self, writer: &mut VmaWriterState, parent_idx: u32, child_pos: usize) {
            let parent = self.node(parent_idx);
            let parent_len = parent.len();
            let child_idx = parent.child(child_pos);
            let child = self.node(child_idx);
            let child_is_leaf = child.is_leaf();
            let right_idx = writer.pop_free_node();
            let right = self.node(right_idx);

            let mid = BTREE_MIN_KEYS;
            for j in 0..BTREE_MIN_KEYS {
                right.set_key(j, child.key(mid + 1 + j));
            }
            if !child_is_leaf {
                for j in 0..=BTREE_MIN_KEYS {
                    right.set_child(j, child.child(mid + 1 + j));
                    child.set_child(mid + 1 + j, BTREE_NO_NODE);
                }
            }
            right.set_header(BTREE_MIN_KEYS, child_is_leaf);
            let median = child.key(mid);
            child.set_header(BTREE_MIN_KEYS, child_is_leaf);

            for j in (child_pos + 1..=parent_len).rev() {
                parent.set_child(j + 1, parent.child(j));
            }
            parent.set_child(child_pos + 1, right_idx);
            for j in (child_pos..parent_len).rev() {
                parent.set_key(j + 1, parent.key(j));
            }
            parent.set_key(child_pos, median);
            parent.set_header(parent_len + 1, false);
        }

        fn insert_non_full(
            &self,
            writer: &mut VmaWriterState,
            mut node_idx: u32,
            entry: BTreeEntry,
        ) {
            loop {
                let node = self.node(node_idx);
                let len = node.len();
                let is_leaf = node.is_leaf();
                let mut pos = node.lower_bound_start(entry.start, len);
                if is_leaf {
                    for j in (pos..len).rev() {
                        node.set_key(j + 1, node.key(j));
                    }
                    node.set_key(pos, entry);
                    node.set_header(len + 1, true);
                    return;
                }
                let child_idx = node.child(pos);
                if self.node(child_idx).len() == BTREE_MAX_KEYS {
                    self.split_child(writer, node_idx, pos);
                    if entry.start > node.key(pos).start {
                        pos += 1;
                    }
                }
                node_idx = node.child(pos);
            }
        }

        /// Writer-locked O(log N) top-down B-tree deletion with borrow/merge
        /// rebalancing so every non-root node keeps at least `BTREE_MIN_KEYS`.
        pub(super) fn remove(&self, writer: &mut VmaWriterState, start: u64) -> Option<BTreeEntry> {
            let root_idx = self.root.load(Ordering::Relaxed);
            if root_idx == BTREE_NO_NODE {
                return None;
            }
            let removed = self.remove_from_node(writer, root_idx, start);
            let root_node = self.node(root_idx);
            if root_node.len() == 0 {
                if root_node.is_leaf() {
                    self.root.store(BTREE_NO_NODE, Ordering::Relaxed);
                } else {
                    self.root.store(root_node.child(0), Ordering::Relaxed);
                }
                writer.push_free_node(root_idx);
            }
            removed
        }

        fn remove_from_node(
            &self,
            writer: &mut VmaWriterState,
            node_idx: u32,
            start: u64,
        ) -> Option<BTreeEntry> {
            let node = self.node(node_idx);
            let len = node.len();
            let is_leaf = node.is_leaf();
            let mut idx = node.lower_bound_start(start, len);

            if idx < len && node.starts[idx].load(Ordering::Relaxed) == start {
                let target = node.key(idx);
                if is_leaf {
                    for j in idx + 1..len {
                        node.set_key(j - 1, node.key(j));
                    }
                    node.set_header(len - 1, true);
                    return Some(target);
                }
                let left_idx = node.child(idx);
                let right_idx = node.child(idx + 1);
                if self.node(left_idx).len() >= BTREE_DEGREE {
                    let pred = self.max_key(left_idx);
                    node.set_key(idx, pred);
                    let _ = self.remove_from_node(writer, left_idx, pred.start);
                    return Some(target);
                }
                if self.node(right_idx).len() >= BTREE_DEGREE {
                    let succ = self.min_key(right_idx);
                    node.set_key(idx, succ);
                    let _ = self.remove_from_node(writer, right_idx, succ.start);
                    return Some(target);
                }
                self.merge_children(writer, node_idx, idx);
                let _ = self.remove_from_node(writer, left_idx, start);
                return Some(target);
            }

            if is_leaf {
                return None;
            }

            if self.node(node.child(idx)).len() < BTREE_DEGREE {
                if idx > 0 && self.node(node.child(idx - 1)).len() >= BTREE_DEGREE {
                    self.borrow_from_prev(node_idx, idx);
                } else if idx < len && self.node(node.child(idx + 1)).len() >= BTREE_DEGREE {
                    self.borrow_from_next(node_idx, idx);
                } else if idx < len {
                    self.merge_children(writer, node_idx, idx);
                } else {
                    self.merge_children(writer, node_idx, idx - 1);
                    idx -= 1;
                }
            }
            self.remove_from_node(writer, node.child(idx), start)
        }

        fn min_key(&self, mut node_idx: u32) -> BTreeEntry {
            loop {
                let node = self.node(node_idx);
                if node.is_leaf() {
                    return node.key(0);
                }
                node_idx = node.child(0);
            }
        }

        fn max_key(&self, mut node_idx: u32) -> BTreeEntry {
            loop {
                let node = self.node(node_idx);
                let len = node.len();
                if node.is_leaf() {
                    return node.key(len - 1);
                }
                node_idx = node.child(len);
            }
        }

        fn borrow_from_prev(&self, parent_idx: u32, idx: usize) {
            let parent = self.node(parent_idx);
            let child = self.node(parent.child(idx));
            let sibling = self.node(parent.child(idx - 1));
            let child_len = child.len();
            let child_is_leaf = child.is_leaf();
            let sib_len = sibling.len();
            let sib_is_leaf = sibling.is_leaf();

            for j in (0..child_len).rev() {
                child.set_key(j + 1, child.key(j));
            }
            if !child_is_leaf {
                for j in (0..=child_len).rev() {
                    child.set_child(j + 1, child.child(j));
                }
                child.set_child(0, sibling.child(sib_len));
                sibling.set_child(sib_len, BTREE_NO_NODE);
            }
            child.set_key(0, parent.key(idx - 1));
            parent.set_key(idx - 1, sibling.key(sib_len - 1));
            child.set_header(child_len + 1, child_is_leaf);
            sibling.set_header(sib_len - 1, sib_is_leaf);
        }

        fn borrow_from_next(&self, parent_idx: u32, idx: usize) {
            let parent = self.node(parent_idx);
            let child = self.node(parent.child(idx));
            let sibling = self.node(parent.child(idx + 1));
            let child_len = child.len();
            let child_is_leaf = child.is_leaf();
            let sib_len = sibling.len();
            let sib_is_leaf = sibling.is_leaf();

            child.set_key(child_len, parent.key(idx));
            if !child_is_leaf {
                child.set_child(child_len + 1, sibling.child(0));
            }
            parent.set_key(idx, sibling.key(0));
            for j in 1..sib_len {
                sibling.set_key(j - 1, sibling.key(j));
            }
            if !child_is_leaf {
                for j in 1..=sib_len {
                    sibling.set_child(j - 1, sibling.child(j));
                }
                sibling.set_child(sib_len, BTREE_NO_NODE);
            }
            child.set_header(child_len + 1, child_is_leaf);
            sibling.set_header(sib_len - 1, sib_is_leaf);
        }

        fn merge_children(&self, writer: &mut VmaWriterState, parent_idx: u32, idx: usize) {
            let parent = self.node(parent_idx);
            let parent_len = parent.len();
            let left_idx = parent.child(idx);
            let right_idx = parent.child(idx + 1);
            let left = self.node(left_idx);
            let right = self.node(right_idx);
            let left_len = left.len();
            let left_is_leaf = left.is_leaf();
            let right_len = right.len();

            left.set_key(left_len, parent.key(idx));
            for j in 0..right_len {
                left.set_key(left_len + 1 + j, right.key(j));
            }
            if !left_is_leaf {
                for j in 0..=right_len {
                    left.set_child(left_len + 1 + j, right.child(j));
                }
            }
            left.set_header(left_len + 1 + right_len, left_is_leaf);

            for j in idx + 1..parent_len {
                parent.set_key(j - 1, parent.key(j));
            }
            for j in idx + 2..=parent_len {
                parent.set_child(j - 1, parent.child(j));
            }
            parent.set_child(parent_len, BTREE_NO_NODE);
            parent.set_header(parent_len - 1, false);
            writer.push_free_node(right_idx);
        }

        #[cfg(test)]
        pub(super) fn verify_invariants(&self) -> (usize, usize) {
            let root = self.root.load(Ordering::Relaxed);
            if root == BTREE_NO_NODE {
                return (0, 0);
            }
            let mut count = 0;
            let mut prev_end = 0_u64;
            let height = self.verify_node(root, true, &mut prev_end, &mut count);
            (count, height)
        }

        #[cfg(test)]
        fn verify_node(
            &self,
            node_idx: u32,
            is_root: bool,
            prev_end: &mut u64,
            count: &mut usize,
        ) -> usize {
            let node = self.node(node_idx);
            let len = node.len();
            let is_leaf = node.is_leaf();
            assert!(len <= BTREE_MAX_KEYS);
            if is_root {
                assert!(len >= 1);
            } else {
                assert!(len >= BTREE_MIN_KEYS);
            }
            let mut child_height = None;
            for i in 0..len {
                if !is_leaf {
                    let h = self.verify_node(node.child(i), false, prev_end, count);
                    if let Some(expected) = child_height {
                        assert_eq!(h, expected, "B-tree leaves must be at equal depth");
                    } else {
                        child_height = Some(h);
                    }
                }
                let entry = node.key(i);
                assert!(entry.start < entry.end);
                assert!(*prev_end <= entry.start, "B-tree keys must be strictly non-overlapping");
                *prev_end = entry.end;
                *count += 1;
            }
            if !is_leaf {
                let h = self.verify_node(node.child(len), false, prev_end, count);
                if let Some(expected) = child_height {
                    assert_eq!(h, expected, "B-tree leaves must be at equal depth");
                }
                h + 1
            } else {
                1
            }
        }
    }
}

use btree::{BTreeEntry, VmaIndex};

/// One process's chunked publication slots plus lazily-allocated O(log N) B-tree index.
///
/// Starts with zero allocated slot chunks and a null index pointer; normal-context
/// writers append `VMA_SLOT_CHUNK`-slot chunks and index nodes on demand up to
/// the effective policy/directory ceiling. Neither slot chunks nor index nodes
/// are ever moved or freed, so exception-time readers traverse the B-tree and
/// borrow `&'static PublishedPagerVma` without allocating or locking.
struct ProcessVmaTable {
    slots: AtomicDir<PublishedPagerVma, VMA_SLOT_CHUNK, VMA_SLOT_DIR>,
    index: AtomicPtr<VmaIndex>,
}

impl ProcessVmaTable {
    const fn empty() -> Self {
        Self {
            slots: AtomicDir::new(),
            index: AtomicPtr::new(ptr::null_mut()),
        }
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.slots.contiguous_chunks() * VMA_SLOT_CHUNK
    }

    fn get(&self, index: usize) -> Option<&PublishedPagerVma> {
        self.slots.get(index)
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &PublishedPagerVma> + '_ {
        self.slots.iter_contiguous()
    }

    #[inline]
    fn index(&self) -> Option<&VmaIndex> {
        // ORDERING: Acquire pairs with the Release CAS in `ensure_index`, so
        // the initialized `VmaIndex` is visible before its pointer is read.
        let ptr = self.index.load(Ordering::Acquire);
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `ptr` comes from a leaked 1-element boxed slice of `VmaIndex`
        // published via `ensure_index` and never freed or moved.
        Some(unsafe { &*ptr })
    }

    fn ensure_index(&self) -> Result<&VmaIndex, PagerVmaError> {
        if let Some(index) = self.index() {
            return Ok(index);
        }
        let mut storage = Vec::new();
        storage
            .try_reserve_exact(1)
            .map_err(|_| PagerVmaError::Pressure)?;
        storage.push(VmaIndex::empty());
        let raw = Box::into_raw(storage.into_boxed_slice()).cast::<VmaIndex>();
        // ORDERING: AcqRel publishes the initialized `VmaIndex` to all lock-free readers.
        match self
            .index
            .compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                // SAFETY: `raw` is now leaked permanently in `self.index`.
                Ok(unsafe { &*raw })
            }
            Err(existing) => {
                // SAFETY: `raw` lost the CAS so nothing else references it.
                unsafe {
                    drop(Box::from_raw(ptr::slice_from_raw_parts_mut(raw, 1)));
                }
                // SAFETY: `existing` was published by the winning CAS and is never freed.
                Ok(unsafe { &*existing })
            }
        }
    }

    /// Allocates and appends `count` slot chunks atomically with respect to
    /// OOM and directory bounds. On failure, zero chunks are published.
    fn append_chunks_with(
        &self,
        count: usize,
        mut build: impl FnMut() -> Option<VmaChunkBox>,
    ) -> Result<usize, PagerVmaError> {
        let base_chunk = self.slots.contiguous_chunks();
        if count == 0 {
            return Ok(base_chunk * VMA_SLOT_CHUNK);
        }
        let Some(end_chunk) = base_chunk.checked_add(count) else {
            return Err(PagerVmaError::Pressure);
        };
        if end_chunk > VMA_SLOT_DIR {
            return Err(PagerVmaError::Pressure);
        }
        let mut built = Vec::new();
        built
            .try_reserve_exact(count)
            .map_err(|_| PagerVmaError::Pressure)?;
        for _ in 0..count {
            let chunk = build().ok_or(PagerVmaError::Pressure)?;
            built.push(chunk);
        }
        for (offset, chunk) in built.into_iter().enumerate() {
            if self.slots.publish_chunk(base_chunk + offset, chunk).is_err() {
                return Err(PagerVmaError::Pressure);
            }
        }
        Ok(base_chunk * VMA_SLOT_CHUNK)
    }
}

impl core::ops::Index<usize> for ProcessVmaTable {
    type Output = PublishedPagerVma;
    fn index(&self, index: usize) -> &Self::Output {
        &self.slots[index]
    }
}

/// Writer-private book-keeping guarded by each process's `PAGER_VMA_WRITERS` lock.
///
/// Keeping owner identity, live count, and free-lists for both VMA slots and
/// B-tree nodes here lets every writer perform O(1) slot/node allocation and
/// O(1) incarnation checks without scanning unused slots or taking unsafe
/// interior-mutable shortcuts.
struct VmaWriterState {
    owner_process_slot: u64,
    owner_process_generation: u64,
    owner_mm_generation: u64,
    live_count: usize,
    free_slots: Vec<u32>,
    free_nodes: Vec<u32>,
    nodes_initialized: usize,
}

impl VmaWriterState {
    const fn empty() -> Self {
        Self {
            owner_process_slot: 0,
            owner_process_generation: 0,
            owner_mm_generation: 0,
            live_count: 0,
            free_slots: Vec::new(),
            free_nodes: Vec::new(),
            nodes_initialized: 0,
        }
    }

    #[inline]
    fn matches_owner(&self, process_slot: u64, identity: ProcessIdentity) -> bool {
        self.owner_process_slot == process_slot
            && self.owner_process_generation == u64::from(identity.process_generation())
            && self.owner_mm_generation == u64::from(identity.mm_generation())
    }

    #[inline]
    fn set_owner(&mut self, process_slot: u64, identity: ProcessIdentity) {
        self.owner_process_slot = process_slot;
        self.owner_process_generation = u64::from(identity.process_generation());
        self.owner_mm_generation = u64::from(identity.mm_generation());
    }

    #[inline]
    fn pop_free_node(&mut self) -> u32 {
        self.free_nodes
            .pop()
            .expect("B-tree node headroom pre-reserved before RangeRewrite")
    }

    #[inline]
    fn push_free_node(&mut self, node_idx: u32) {
        self.free_nodes.push(node_idx);
    }

    fn ensure_free_slots(
        &mut self,
        slots: &ProcessVmaTable,
        needed: usize,
        build: fn() -> Option<VmaChunkBox>,
    ) -> Result<(), PagerVmaError> {
        if self.free_slots.len() >= needed {
            return Ok(());
        }
        let missing = needed - self.free_slots.len();
        let needed_chunks = missing.div_ceil(VMA_SLOT_CHUNK);
        let new_count = needed_chunks * VMA_SLOT_CHUNK;
        self.free_slots
            .try_reserve(new_count)
            .map_err(|_| PagerVmaError::Pressure)?;
        let first_new_slot = slots.append_chunks_with(needed_chunks, build)?;
        for slot_idx in (first_new_slot..first_new_slot + new_count).rev() {
            self.free_slots.push(slot_idx as u32);
        }
        Ok(())
    }
}

static PAGER_VMAS: AtomicDir<ProcessVmaTable> = AtomicDir::new();
type PagerVmaWriterLock =
    TrackedSpinLock<VmaWriterState, { LockClass::PagerVmaPublication as u8 }>;
/// One publication writer lock **per process**, not one for the system.
///
/// Every `mmap`, `munmap`, and `mprotect` that touches a pager VMA takes this.
/// A single global lock therefore serialized every address-space edit in the
/// machine against every other, on a path that is already hot at 8 vCPUs - and
/// worse, a withdrawal holds it across the installer drain, which is bounded
/// in wall-clock time rather than instructions. One process stalling an
/// installer would stall unrelated processes' `mmap`.
///
/// Per-process is sound because the publication tables are already disjoint:
/// `process_slots` hands each process its own table, and the only shared state
/// a writer touches is `NEXT_ANON_OBJECT_SLOT`, which is a standalone atomic.
#[expect(
    clippy::declare_interior_mutable_const,
    reason = "array initializer for the fixed per-process writer lock table"
)]
const EMPTY_PAGER_VMA_WRITER: PagerVmaWriterLock = TrackedSpinLock::new(VmaWriterState::empty());
static PAGER_VMA_WRITERS: AtomicDir<PagerVmaWriterLock> = AtomicDir::new();

/// Reclaims every published VMA belonging to a previous process or MM
/// incarnation under the caller's writer lock.
///
/// Visits only the `live_count` entries recorded in the B-tree (never scanning
/// unused chunks). If an installer drain fails mid-reclamation, every already-
/// cleared slot has also been removed from the B-tree and returned to
/// `free_slots`, preserving exact consistency.
fn reclaim_stale_incarnation(
    handle: ProcessHandle,
    slots: &ProcessVmaTable,
    writer: &mut VmaWriterState,
    process_slot: u64,
    identity: ProcessIdentity,
) -> Result<(), PagerVmaError> {
    if writer.live_count == 0 {
        writer.set_owner(process_slot, identity);
        return Ok(());
    }
    if writer.matches_owner(process_slot, identity) {
        return Ok(());
    }
    let Some(index) = slots.index() else {
        writer.live_count = 0;
        writer.set_owner(process_slot, identity);
        return Ok(());
    };
    let mut stale = Vec::new();
    stale
        .try_reserve_exact(writer.live_count)
        .map_err(|_| PagerVmaError::Pressure)?;
    index.collect_all(&mut stale)?;
    writer
        .free_slots
        .try_reserve(stale.len())
        .map_err(|_| PagerVmaError::Pressure)?;
    let _reclaim_rewrite = RangeRewrite::begin(handle)?;
    for entry in stale {
        let slot = &slots[entry.slot as usize];
        // A reaped process slot or completed exec may leave an old generation
        // in this table. The writer lock and installer drain make reclamation
        // exact before this slot can be reused by the new live identity.
        slot.publish(None)?;
        let _ = index.remove(writer, entry.start);
        writer.free_slots.push(entry.slot);
        writer.live_count -= 1;
    }
    writer.set_owner(process_slot, identity);
    Ok(())
}

/// Publishes the VMA tables and writer locks covering `index` before that
/// process slot can exist.
pub(super) fn ensure_slot_storage(index: usize) -> bool {
    PAGER_VMAS.ensure(index, ProcessVmaTable::empty)
        && PAGER_VMA_WRITERS.ensure(index, || EMPTY_PAGER_VMA_WRITER)
        && rewrite_epoch::ensure_slot_storage(index)
}

#[cfg(test)]
pub(super) fn slot_storage_exists_for_tests(handle: ProcessHandle) -> bool {
    process_slots(handle).is_some() && writer_lock(handle).is_some()
        && rewrite_epoch::REWRITE_EPOCHS.get(handle.index()).is_some()
}

/// The writer lock guarding one process's publication slots.
fn writer_lock(handle: ProcessHandle) -> Option<&'static PagerVmaWriterLock> {
    PAGER_VMA_WRITERS.get(handle.index())
}

fn process_slots(handle: ProcessHandle) -> Option<&'static ProcessVmaTable> {
    PAGER_VMAS.get(handle.index())
}

fn template_is_canonical(region: PagerVmRegionWire) -> bool {
    region.start != 0
        && region.start < region.end
        && region.start.is_multiple_of(PAGER_PAGE_BYTES)
        && region.end.is_multiple_of(PAGER_PAGE_BYTES)
        && region.object_offset.is_multiple_of(PAGER_PAGE_BYTES)
        && region.object.has_authority()
        // `PROT_NONE` is still an authoritative reservation and may also be a
        // committed guard mapping. Access is denied later by the exact VMA
        // protection check; rejecting it here loses the reservation during
        // fork and recreates a split address-space authority.
        && region.prot & !rustos_user_abi::pager::VM_PROT_KNOWN == 0
        && region.prot & !region.object.rights == 0
        && ((!vm_prot_is_rwx(region.prot) && !vm_prot_is_rwx(region.object.rights))
            || region.is_owned_private_anonymous())
        && (region.sharing == VM_SHARING_PRIVATE || region.sharing == VM_SHARING_SHARED)
        && (region.commit_state == VM_COMMIT_RESERVED || region.commit_state == VM_COMMIT_COMMITTED)
        && region.reserved1 == [0; 2]
        && region.vma_generation == 0
        && region.process_handle == 0
        && region.process_generation == 0
        && region.mm_generation == 0
        && region.fault_endpoint.has_authority()
}

fn stamped_region(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    mut template: PagerVmRegionWire,
    vma_generation: u64,
) -> Result<PagerVmRegionWire, PagerVmaError> {
    if !template_is_canonical(template) || vma_generation == 0 {
        return Err(PagerVmaError::Malformed);
    }
    let process = handle.object_identity().ok_or(PagerVmaError::Malformed)?;
    if process.generation() != u64::from(identity.process_generation()) {
        return Err(PagerVmaError::Stale);
    }
    template.vma_generation = vma_generation;
    template.process_handle = process.slot();
    template.process_generation = process.generation();
    template.mm_generation = u64::from(identity.mm_generation());
    template
        .is_canonical()
        .then_some(template)
        .ok_or(PagerVmaError::Malformed)
}

fn regions_mergeable(left: PagerVmRegionWire, right: PagerVmRegionWire) -> bool {
    left.end == right.start
        && left
            .object_offset
            .checked_add(left.end - left.start)
            .is_some_and(|next| next == right.object_offset)
        && left.object == right.object
        && left.prot == right.prot
        && left.sharing == right.sharing
        && left.commit_state == right.commit_state
        && left.vma_generation == right.vma_generation
        && left.process_handle == right.process_handle
        && left.process_generation == right.process_generation
        && left.mm_generation == right.mm_generation
        && left.fault_endpoint == right.fault_endpoint
        && left.reserved1 == right.reserved1
}

/// Coalesces fragments from one publication generation in place.
///
/// Merging is deliberately identity-preserving: independently published VMAs
/// are not silently restamped. Range edits therefore cannot leave permanent
/// slot fragmentation when protection or commit state returns to its original
/// value.
fn merge_adjacent_regions(regions: &mut Vec<PagerVmRegionWire>) {
    let mut write = 0_usize;
    for read in 0..regions.len() {
        let region = regions[read];
        if write != 0 && regions_mergeable(regions[write - 1], region) {
            regions[write - 1].end = region.end;
        } else {
            regions[write] = region;
            write += 1;
        }
    }
    regions.truncate(write);
}

/// Publishes one region, refusing beyond `ceiling` live regions.
///
/// `ceiling` is policy and arrives from the caller; this table only enforces
/// it and clamps to `MAX_PAGER_VMAS_PER_PROCESS` (the directory ceiling).
/// When all currently allocated chunks are full and `live < ceiling`, one
/// additional chunk is allocated before publishing; on policy limit or OOM,
/// existing mappings are left untouched.
pub(super) fn publish(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    template: PagerVmRegionWire,
    ceiling: usize,
) -> Result<PagerVmRegionWire, PagerVmaError> {
    publish_for_policy(handle, identity, template, ceiling, ExecMemoryPolicy::StrictWx)
}

pub(super) fn publish_for_policy(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    template: PagerVmRegionWire,
    ceiling: usize,
    policy: ExecMemoryPolicy,
) -> Result<PagerVmRegionWire, PagerVmaError> {
    if (vm_prot_is_rwx(template.prot) || vm_prot_is_rwx(template.object.rights))
        && (policy != ExecMemoryPolicy::AllowRwx || !template.is_owned_private_anonymous()) {
        return Err(PagerVmaError::Denied);
    }
    publish_with_builder(handle, identity, template, ceiling, build_default_vma_chunk)
}

fn publish_with_builder(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    template: PagerVmRegionWire,
    ceiling: usize,
    build: fn() -> Option<VmaChunkBox>,
) -> Result<PagerVmRegionWire, PagerVmaError> {
    if handle.generation() != identity.process_generation() {
        return Err(PagerVmaError::Stale);
    }
    let ceiling = ceiling.min(MAX_PAGER_VMAS_PER_PROCESS);
    if ceiling == 0 {
        return Err(PagerVmaError::Pressure);
    }
    let mut writer = writer_lock(handle).ok_or(PagerVmaError::Stale)?.lock();
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    let process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    reclaim_stale_incarnation(handle, slots, &mut writer, process.slot(), identity)?;

    if let Some(index) = slots.index()
        && let Some(existing) = index.first_ending_after(template.start)
    {
        if template.start < existing.end && existing.start < template.end {
            return Err(PagerVmaError::Overlap);
        }
    }
    if writer.live_count >= ceiling {
        return Err(PagerVmaError::Pressure);
    }
    writer.ensure_free_slots(slots, 1, build)?;
    let index = slots.ensure_index()?;
    let next_live = writer.live_count + 1;
    index.ensure_node_headroom(&mut writer, next_live)?;

    let slot_index = *writer.free_slots.last().ok_or(PagerVmaError::Pressure)? as usize;
    let slot = slots.get(slot_index).ok_or(PagerVmaError::Pressure)?;
    let sequence = slot.sequence.load(Ordering::Relaxed);
    let generation = sequence
        .checked_div(2)
        .and_then(|value| value.checked_add(1))
        .ok_or(PagerVmaError::Pressure)?;
    let region = stamped_region(handle, identity, template, generation)?;

    let _publish_rewrite = RangeRewrite::begin(handle)?;
    slot.publish(Some(region))?;
    index.insert(
        &mut writer,
        BTreeEntry {
            start: region.start,
            end: region.end,
            slot: slot_index as u32,
        },
    );
    let _ = writer.free_slots.pop();
    writer.live_count += 1;
    Ok(region)
}

/// Whether a published region still belongs to this exact process incarnation.
///
/// One definition and three callers, deliberately. A second copy of this
/// comparison is not a cheap guard: a duplicate keeps rejecting when the real
/// one is broken, which is exactly how the registered `identityExact` mutant
/// survived a duplicated pre-filter once already. A filter may narrow by
/// address extent; it may never restate an authority decision.
///
/// This function is the registered implementation-mutation anchor
/// `pager-vma-publication-identity-bypass`.
fn region_matches_identity(
    region: PagerVmRegionWire,
    process_slot: u64,
    identity: ProcessIdentity,
) -> bool {
    region.process_handle == process_slot
        && region.process_generation == u64::from(identity.process_generation())
        && region.mm_generation == u64::from(identity.mm_generation())
}

/// Decides whether `access` is permitted by this region's protection.
///
/// Split out so the decision keeps its own statement. That statement is the
/// registered implementation-mutation anchor
/// `pager-vma-publication-permission-bypass`, whose witness proves a
/// permission escalation fails closed; inlining it into a caller that also
/// returns a slot index would silently retire that witness.
fn admit_access(
    region: PagerVmRegionWire,
    access: u16,
) -> Result<PagerVmRegionWire, PagerVmaError> {
    if !region.is_committed() {
        return Err(PagerVmaError::Denied);
    }
    let allowed = (access == VM_ACCESS_READ && region.prot & VM_PROT_READ != 0)
        || (access == VM_ACCESS_WRITE && region.prot & VM_PROT_WRITE != 0)
        || (access == VM_ACCESS_EXECUTE && region.prot & VM_PROT_EXECUTE != 0);
    return allowed.then_some(region).ok_or(PagerVmaError::Denied);
}

pub(super) fn lookup(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    address: u64,
    access: u16,
) -> Result<PagerVmRegionWire, PagerVmaError> {
    lookup_slot(handle, identity, address, access).map(|(_, region)| region)
}

/// Finds the publication covering `address`, and reports *which slot* holds it.
///
/// Uses the per-process lock-free-readable B-tree inside `read_rewrite_epoch`
/// so fault lookup is O(log N) in the live VMA count (at most 5 node hops at
/// 65,536 VMAs) and touches only the single candidate slot's atomic payload.
/// Returning the slot index lets `acquire_fault_install` go straight to that
/// exact slot for its permit.
pub(super) fn lookup_slot(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    address: u64,
    access: u16,
) -> Result<(usize, PagerVmRegionWire), PagerVmaError> {
    if access == 0 || access & !VM_ACCESS_KNOWN != 0 || access.count_ones() != 1 {
        return Err(PagerVmaError::Malformed);
    }
    let process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    read_rewrite_epoch(handle, || {
        let Some(index_tree) = slots.index() else {
            return Err(PagerVmaError::Stale);
        };
        let Some(index) = index_tree.find_containing(address)? else {
            return Err(PagerVmaError::Stale);
        };
        let slot = slots.get(index).ok_or(PagerVmaError::Unstable)?;
        if !slot.may_cover(address) {
            return Err(PagerVmaError::Stale);
        }
        let Some(region) = slot.snapshot()? else {
            return Err(PagerVmaError::Stale);
        };
        // B-tree index and extent filtering are not authority. The candidate
        // still needs the exact process/MM, VMA, protection and commit checks.
        if !region_matches_identity(region, process.slot(), identity) {
            return Err(PagerVmaError::Stale);
        }
        if region.contains(address) {
            return admit_access(region, access).map(|region| (index, region));
        }
        Err(PagerVmaError::Stale)
    })
}

/// Acquires one exact VMA publication permit for an IRQ-off prepared-leaf
/// install.  This validates the same generation/object/offset tuple carried by
/// the fault request, but intentionally does not enter `ProcessStateLock`.
///
/// The two failure classes are not interchangeable, and the caller must not
/// collapse them:
///
/// - [`PagerVmaError::Unstable`] means the range *is* published but a writer
///   is mid-edit, so no permit can be issued this instant. The faulting
///   instruction must be restarted, not killed.
/// - Every other error means this fault carries no authority for this address,
///   and the thread is retired.
///
/// Returning `Stale` for contention is what makes a concurrent `munmap` or
/// `mprotect` anywhere in the same process able to SIGSEGV an unrelated,
/// perfectly valid first touch.
pub(super) fn acquire_fault_install(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    request: rustos_user_abi::pager::PagerFaultRequestWire,
) -> Result<(PagerFaultInstallPermit, PagerVmRegionWire), PagerVmaError> {
    if handle.generation() != identity.process_generation() {
        return Err(PagerVmaError::Stale);
    }
    let (slot_index, region) =
        lookup_slot(handle, identity, request.virtual_address, request.access)?;
    let delta = request
        .virtual_address
        .checked_sub(region.start)
        .ok_or(PagerVmaError::Stale)?;
    if region.vma_generation != request.vma_generation
        || region.mm_generation != request.mm_generation
        || region.object != request.object
        || region
            .object_offset
            .checked_add(delta)
            .ok_or(PagerVmaError::Stale)?
            != request.object_offset
    {
        return Err(PagerVmaError::Stale);
    }
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    // Exactly one slot, the one the scan above already identified. Sweeping
    // every slot here re-read the whole table for a permit whose owner was
    // already known.
    let slot = slots.get(slot_index).ok_or(PagerVmaError::Stale)?;
    slot.try_acquire_fault_install(region)
        // `lookup_slot` already proved this exact region published in this
        // exact slot, so a refusal means a writer changed the publication
        // under us between the two. That is contention, not absent authority.
        .map_err(|_| PagerVmaError::Unstable)
        .map(|permit| (permit, region))
}

/// Revalidates every authority carried by a dispatched request immediately
/// before a pager reply may mutate its address space.
pub fn validate_fault_request(
    request: rustos_user_abi::pager::PagerFaultRequestWire,
) -> Result<PagerVmaSnapshot, PagerVmaError> {
    let index = usize::try_from(
        request
            .process_handle
            .checked_sub(1)
            .ok_or(PagerVmaError::Stale)?,
    )
    .map_err(|_| PagerVmaError::Stale)?;
    let generation = u32::try_from(request.process_generation).map_err(|_| PagerVmaError::Stale)?;
    let handle = ProcessHandle::new(index, generation);
    let identity =
        super::process_table::mm_owner_identity(handle).ok_or(PagerVmaError::Stale)?;
    let region = lookup(handle, identity, request.virtual_address, request.access)?;
    let delta = request
        .virtual_address
        .checked_sub(region.start)
        .ok_or(PagerVmaError::Stale)?;
    if region.vma_generation != request.vma_generation
        || region.mm_generation != request.mm_generation
        || region.object != request.object
        || region
            .object_offset
            .checked_add(delta)
            .ok_or(PagerVmaError::Stale)?
            != request.object_offset
    {
        return Err(PagerVmaError::Stale);
    }
    Ok(PagerVmaSnapshot {
        task_id: request.task_id,
        process_id: identity.process_id(),
        region,
    })
}

/// Executes one address-space mutation while process, MM, VMA, object, and
/// access authority are all revalidated under the exact process-state lock.
pub fn with_validated_fault_address_space<R>(
    request: rustos_user_abi::pager::PagerFaultRequestWire,
    f: impl FnOnce(
        u64,
        crate::memory::paging::UserRegion,
        &mut crate::memory::paging::ProcessAddressSpace,
    ) -> R,
) -> Result<R, PagerVmaError> {
    let index = usize::try_from(
        request
            .process_handle
            .checked_sub(1)
            .ok_or(PagerVmaError::Stale)?,
    )
    .map_err(|_| PagerVmaError::Stale)?;
    let generation = u32::try_from(request.process_generation).map_err(|_| PagerVmaError::Stale)?;
    let handle = ProcessHandle::new(index, generation);
    let expected =
        super::process_table::mm_owner_identity(handle).ok_or(PagerVmaError::Stale)?;
    let process = super::process_table::retain_process(handle).ok_or(PagerVmaError::Stale)?;
    process
        .with_exact_mm_owner_state_mut(expected, |process_id, state| {
            let region = lookup(handle, expected, request.virtual_address, request.access)?;
            let delta = request
                .virtual_address
                .checked_sub(region.start)
                .ok_or(PagerVmaError::Stale)?;
            if region.vma_generation != request.vma_generation
                || region.mm_generation != request.mm_generation
                || region.object != request.object
                || region
                    .object_offset
                    .checked_add(delta)
                    .ok_or(PagerVmaError::Stale)?
                    != request.object_offset
            {
                return Err(PagerVmaError::Stale);
            }
            // The exact span the validated VMA covers. Mapping helpers take it
            // so a leaf can never be installed past the VMA the fault was
            // validated against, whatever run length a caller computed.
            let page_count = region
                .end
                .checked_sub(region.start)
                .and_then(|bytes| usize::try_from(bytes / PAGER_PAGE_BYTES).ok())
                .ok_or(PagerVmaError::Stale)?;
            let extent = crate::memory::paging::UserRegion {
                start: x86_64::VirtAddr::new(region.start),
                page_count,
            };
            Ok(f(process_id, extent, state.address_space_mut()))
        })
        .ok_or(PagerVmaError::Stale)?
}

/// Rewrites one fully pager-managed range while preserving only attenuated
/// authority. The original publications are withdrawn before `mutate` changes
/// any PTE, so exception-time readers fail closed throughout the transaction.
#[expect(
    clippy::too_many_arguments,
    reason = "internal range-edit core parameterized by policy ceiling and chunk builder"
)]
fn rewrite_range<F>(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    start: u64,
    end: u64,
    replacement_prot: Option<u32>,
    replacement_commit_state: Option<u16>,
    ceiling: usize,
    build: fn() -> Option<VmaChunkBox>,
    mutate: F,
) -> Result<bool, PagerVmaError>
where F: FnOnce() -> Result<(), PagerVmaError>,
{
    rewrite_range_for_policy(handle, identity, start, end, replacement_prot, replacement_commit_state, ceiling, build, ExecMemoryPolicy::StrictWx, mutate)
}

#[expect(clippy::too_many_arguments, reason = "exact range and policy admission")]
fn rewrite_range_for_policy<F>(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    start: u64,
    end: u64,
    replacement_prot: Option<u32>,
    replacement_commit_state: Option<u16>,
    ceiling: usize,
    build: fn() -> Option<VmaChunkBox>,
    policy: ExecMemoryPolicy,
    mutate: F,
) -> Result<bool, PagerVmaError>
where
    F: FnOnce() -> Result<(), PagerVmaError>,
{
    if start == 0
        || start >= end
        || !start.is_multiple_of(PAGER_PAGE_BYTES)
        || !end.is_multiple_of(PAGER_PAGE_BYTES)
    {
        return Err(PagerVmaError::Malformed);
    }
    if replacement_prot.is_some_and(|prot| {
        prot & !rustos_user_abi::pager::VM_PROT_KNOWN != 0
            || (vm_prot_is_rwx(prot) && policy != ExecMemoryPolicy::AllowRwx)
    }) {
        return Err(PagerVmaError::Denied);
    }
    if replacement_commit_state
        .is_some_and(|state| state != VM_COMMIT_RESERVED && state != VM_COMMIT_COMMITTED)
    {
        return Err(PagerVmaError::Malformed);
    }
    if handle.generation() != identity.process_generation() {
        return Err(PagerVmaError::Stale);
    }
    let ceiling = ceiling.min(MAX_PAGER_VMAS_PER_PROCESS);

    let mut writer = writer_lock(handle).ok_or(PagerVmaError::Stale)?.lock();
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    let live_process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    reclaim_stale_incarnation(handle, slots, &mut writer, live_process.slot(), identity)?;
    if writer.live_count == 0 {
        return Ok(false);
    }
    let Some(btree_index) = slots.index() else {
        return Ok(false);
    };
    // O(log N + K) in-order query of overlapping and adjacent (merge-candidate)
    // regions. Entries arrive sorted by `start`, so no sort is required.
    let mut candidates: Vec<BTreeEntry> = Vec::new();
    btree_index.collect_range(start, end, true, &mut candidates)?;
    // Adjacent regions are merge candidates, not authority for the requested
    // interval. Repeating an unmap of an existing hole must not withdraw its
    // neighbors or report a stale process/MM incarnation.
    if !candidates
        .iter()
        .any(|entry| start < entry.end && entry.start < end)
    {
        return Ok(false);
    }
    let mut overlapping: Vec<(usize, PagerVmRegionWire)> = Vec::new();
    overlapping
        .try_reserve_exact(candidates.len())
        .map_err(|_| PagerVmaError::Pressure)?;
    for entry in candidates {
        let slot_index = entry.slot as usize;
        let Some(region) = slots[slot_index].snapshot()? else {
            return Err(PagerVmaError::Stale);
        };
        overlapping.push((slot_index, region));
    }
    let live_total = writer.live_count;
    let overlapping_len = overlapping.len();
    let removing = replacement_prot.is_none() && replacement_commit_state.is_none();

    let process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    let mut cursor = start;
    let mut rewritten: Vec<PagerVmRegionWire> = Vec::new();
    let maximum_growth = if replacement_prot.is_none() && replacement_commit_state.is_none() {
        PAGER_MAX_REGION_GROWTH_PER_UNMAP
    } else {
        PAGER_MAX_REGION_GROWTH_PER_PROTECT
    };
    rewritten
        .try_reserve_exact(
            overlapping_len
                .checked_add(maximum_growth)
                .ok_or(PagerVmaError::Pressure)?,
        )
        .map_err(|_| PagerVmaError::Pressure)?;
    for (_, region) in overlapping.iter().copied() {
        if !region_matches_identity(region, process.slot(), identity) {
            return Err(PagerVmaError::Stale);
        }
        let intersects = start < region.end && region.start < end;
        if !intersects {
            rewritten.push(region);
            continue;
        }
        if !removing && region.start > cursor {
            return Err(PagerVmaError::Stale);
        }
        // The split/trim/remove rule is the shared ABI one. pagerd applies the
        // same call to its own replica of this region, so the two tables
        // cannot disagree about what an edit leaves behind - which is exactly
        // what happened while each side derived its own remainders.
        // A commit/decommit edit preserves protection. Supplying the region's
        // existing protection to the shared splitter distinguishes this from
        // unmap while keeping the geometry rule identical for both ABIs.
        let edit = PagerRangeEdit {
            start,
            end,
            replacement_prot: replacement_prot
                .or_else(|| replacement_commit_state.map(|_| region.prot)),
        };
        let mut push = |mut fragment: PagerVmRegionWire| {
            if let Some(commit_state) = replacement_commit_state
                && fragment.start >= start
                && fragment.end <= end
            {
                fragment.commit_state = commit_state;
            }
            rewritten.push(fragment);
        };
        match apply_region_edit(region, edit) {
            PagerRegionEdit::Untouched(_) => return Err(PagerVmaError::Stale),
            PagerRegionEdit::Removed => {}
            PagerRegionEdit::Replaced(only) => push(only),
            PagerRegionEdit::Split { left, right } => {
                push(left);
                push(right);
            }
            PagerRegionEdit::ProtectedSplit {
                left,
                middle,
                right,
            } => {
                push(left);
                push(middle);
                push(right);
            }
            PagerRegionEdit::Denied => return Err(PagerVmaError::Denied),
            PagerRegionEdit::Malformed => return Err(PagerVmaError::Malformed),
        }
        cursor = cursor.max(end.min(region.end));
    }
    merge_adjacent_regions(&mut rewritten);
    let rewritten_len = rewritten.len();
    debug_assert!(
        rewritten_len <= overlapping_len + maximum_growth,
        "one range edit may add at most one interior split's fragments"
    );
    if !removing && cursor < end {
        return Err(PagerVmaError::Stale);
    }
    let new_live = live_total
        .checked_sub(overlapping_len)
        .and_then(|rest| rest.checked_add(rewritten_len))
        .ok_or(PagerVmaError::Pressure)?;
    if rewritten_len > overlapping_len && new_live > ceiling {
        return Err(PagerVmaError::Pressure);
    }
    let extra_needed = rewritten_len.saturating_sub(overlapping_len);
    if extra_needed > 0 {
        writer.ensure_free_slots(slots, extra_needed, build)?;
    }
    let freed_on_shrink = overlapping_len.saturating_sub(rewritten_len);
    if freed_on_shrink > 0 {
        writer
            .free_slots
            .try_reserve(freed_on_shrink)
            .map_err(|_| PagerVmaError::Pressure)?;
    }
    btree_index.ensure_node_headroom(&mut writer, live_total.max(new_live))?;

    let free_len = writer.free_slots.len();
    let mut targets: Vec<usize> = Vec::new();
    targets
        .try_reserve_exact(rewritten_len)
        .map_err(|_| PagerVmaError::Pressure)?;
    for idx in 0..rewritten_len {
        let target_slot = if idx < overlapping_len {
            overlapping[idx].0
        } else {
            writer.free_slots[free_len - 1 - (idx - overlapping_len)] as usize
        };
        let sequence = slots[target_slot].sequence.load(Ordering::Relaxed);
        if sequence & 1 != 0 || sequence > MAX_PUBLICATION_SEQUENCE - 2 {
            return Err(PagerVmaError::Pressure);
        }
        targets.push(target_slot);
    }

    // A stable empty per-slot publication during withdrawal is NOT a real
    // unmap. Retain the whole-set odd interval through mutate and all rollback
    // or replacement publications so fault-time misses are restartable.
    let _rewrite = RangeRewrite::begin(handle)?;

    // Withdrawal is all-or-nothing across the overlapping slots.
    //
    // `publish(None)` can fail after this writer has already withdrawn an
    // earlier slot, because withdrawal drains exception-time installers and
    // that drain can time out. Propagating straight out of the loop would
    // leave those earlier regions withdrawn and never restored: their pages
    // stay mapped in the address space but no longer fault, and no later
    // `munmap` can name them. Restore what this attempt withdrew before
    // reporting the failure, so a refused edit changes nothing at all.
    for (position, (slot_index, _)) in overlapping.iter().copied().enumerate() {
        let Err(error) = slots[slot_index].publish(None) else {
            continue;
        };
        for (restore_index, region) in overlapping[..position].iter().copied() {
            // Republication cannot drain, and its sequence headroom was
            // preflighted above, so this restore does not fail for a reason
            // the withdrawal just created.
            let _ = slots[restore_index].publish(Some(region));
        }
        return Err(error);
    }
    if let Err(error) = mutate() {
        for (slot_index, region) in overlapping.iter().copied() {
            let _ = slots[slot_index].publish(Some(region));
        }
        return Err(error);
    }
    for index in 0..rewritten_len {
        slots[targets[index]].publish(Some(rewritten[index]))?;
    }
    // Update the O(log N) B-tree index and free-slot list only after `mutate`
    // and every replacement publication succeeded (still inside `_rewrite`).
    for (_, old_region) in overlapping.iter().copied() {
        let _ = btree_index.remove(&mut writer, old_region.start);
    }
    for _ in 0..extra_needed {
        let _ = writer.free_slots.pop();
    }
    for (slot_index, _) in overlapping[rewritten_len.min(overlapping_len)..]
        .iter()
        .copied()
    {
        writer.free_slots.push(slot_index as u32);
    }
    for idx in 0..rewritten_len {
        btree_index.insert(
            &mut writer,
            BTreeEntry {
                start: rewritten[idx].start,
                end: rewritten[idx].end,
                slot: targets[idx] as u32,
            },
        );
    }
    writer.live_count = new_live;
    Ok(true)
}

pub(super) fn rewrite_attenuated_range<F>(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    start: u64,
    end: u64,
    replacement_prot: Option<u32>,
    mutate: F,
) -> Result<bool, PagerVmaError>
where
    F: FnOnce() -> Result<(), PagerVmaError>,
{
    rewrite_range(
        handle,
        identity,
        start,
        end,
        replacement_prot,
        None,
        MAX_PAGER_VMAS_PER_PROCESS,
        build_default_vma_chunk,
        mutate,
    )
}

pub(super) fn rewrite_attenuated_range_with_ceiling<F>(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    start: u64,
    end: u64,
    replacement_prot: Option<u32>,
    ceiling: usize,
    mutate: F,
) -> Result<bool, PagerVmaError>
where
    F: FnOnce() -> Result<(), PagerVmaError>,
{
    rewrite_range(
        handle,
        identity,
        start,
        end,
        replacement_prot,
        None,
        ceiling,
        build_default_vma_chunk,
        mutate,
    )
}

/// Narrows protection over one exact range while enforcing `ceiling` on any
/// resulting VMA split.
///
/// Returning the stamped `(process_handle, process_generation)` lets the caller
/// publish the same narrowing to the pager under the identity ring0 actually
/// edited, rather than re-deriving one that could disagree - the same reason
/// [`unmap_for_process`] returns it.
pub fn protect_for_process_with_ceiling(
    process_id: u64,
    expected: Option<ExpectedMm>,
    start: u64,
    end: u64,
    prot: u32,
    page_flags: x86_64::structures::paging::PageTableFlags,
    ceiling: usize,
) -> Result<Option<(u64, u64)>, PagerVmaError> {
    let retained =
        super::process_table::retain_process_by_pid(process_id).ok_or(PagerVmaError::Stale)?;
    let identity = retained.live_identity().ok_or(PagerVmaError::Stale)?;
    ExpectedMm::check(expected, identity)?;
    let (vma_handle, vma_identity) = retained.vma_owner().ok_or(PagerVmaError::Stale)?;
    let page_count = end
        .checked_sub(start)
        .and_then(|bytes| usize::try_from(bytes / PAGER_PAGE_BYTES).ok())
        .ok_or(PagerVmaError::Malformed)?;
    let process = vma_handle
        .object_identity()
        .ok_or(PagerVmaError::Stale)?;
    let rewritten = retained.with_state_mut(|_, state| {
        if super::process_table::published_live_process_identity(retained.handle()) != Some(identity)
            || super::process_table::published_mm_owner_identity(vma_handle) != Some(vma_identity) {
            return Err(PagerVmaError::Unstable);
        }
        let policy = state.address_space().exec_memory_policy();
        if !policy.permits(prot & VM_PROT_WRITE != 0, prot & VM_PROT_EXECUTE != 0) {
            return Err(PagerVmaError::Denied);
        }
        rewrite_range_for_policy(
            vma_handle,
            vma_identity,
            start,
            end,
            Some(prot),
            None,
            ceiling,
            build_default_vma_chunk,
            policy,
            || {
                state
                    .address_space_mut()
                    .protect_present_prepared_pager_fault_pages_at(
                        x86_64::VirtAddr::new(start),
                        page_count,
                        page_flags,
                    )
                    .map(|_| ())
                    .map_err(|_| PagerVmaError::Stale)
            },
        )
    })?;
    Ok(rewritten.then(|| (process.slot(), process.generation())))
}

/// Cleans up pager-owned leaves after a failed mapping publication.
pub fn unmap_for_process(
    process_id: u64,
    expected: Option<ExpectedMm>,
    start: u64,
    end: u64,
) -> Result<Option<(u64, u64)>, PagerVmaError> {
    let pages = end
        .checked_sub(start)
        .and_then(|bytes| usize::try_from(bytes / PAGER_PAGE_BYTES).ok())
        .ok_or(PagerVmaError::Malformed)?;
    unmap_for_process_inner(
        process_id,
        expected,
        start,
        end,
        false,
        |state| {
            state
                .address_space_mut()
                .unmap_present_prepared_pager_fault_pages_at(x86_64::VirtAddr::new(start), pages)
                .map(|_| ())
                .map_err(|_| PagerVmaError::Stale)
        },
        |_| {},
    )
}

/// Unmaps pager publications and caller-owned PTEs under one process-state
/// lock. `mutate` must preflight the ENTIRE sparse/mixed range before removing
/// any leaf, preserve borrowed backing through TLB ACK, and never yield/IPC.
/// `release` drops mapping records only after the VMA writer/epoch has ended,
/// still under the same process-state lock (no mmap may fill a hole between).
/// The returned identity names pager publication work, not whether PTE work ran.
pub fn unmap_for_process_with(
    process_id: u64,
    expected: Option<ExpectedMm>,
    start: u64,
    end: u64,
    mutate: impl FnOnce(&mut super::UserProcessState) -> Result<(), PagerVmaError>,
    release: impl FnOnce(&mut super::UserProcessState),
) -> Result<Option<(u64, u64)>, PagerVmaError> {
    unmap_for_process_inner(process_id, expected, start, end, true, mutate, release)
}

/// Unmaps pager publications and caller-owned PTEs while enforcing `ceiling` on
/// any interior VMA split.
pub fn unmap_for_process_with_ceiling(
    process_id: u64,
    expected: Option<ExpectedMm>,
    start: u64,
    end: u64,
    ceiling: usize,
    mutate: impl FnOnce(&mut super::UserProcessState) -> Result<(), PagerVmaError>,
    release: impl FnOnce(&mut super::UserProcessState),
) -> Result<Option<(u64, u64)>, PagerVmaError> {
    unmap_for_process_inner_with_ceiling(
        process_id,
        expected,
        start,
        end,
        true,
        Some(ceiling),
        mutate,
        release,
    )
}

fn unmap_for_process_inner(
    process_id: u64,
    expected: Option<ExpectedMm>,
    start: u64,
    end: u64,
    mutate_without_publication: bool,
    mutate: impl FnOnce(&mut super::UserProcessState) -> Result<(), PagerVmaError>,
    release: impl FnOnce(&mut super::UserProcessState),
) -> Result<Option<(u64, u64)>, PagerVmaError> {
    unmap_for_process_inner_with_ceiling(
        process_id,
        expected,
        start,
        end,
        mutate_without_publication,
        None,
        mutate,
        release,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "shared unmap transaction core supporting both default and explicit policy ceiling"
)]
fn unmap_for_process_inner_with_ceiling(
    process_id: u64,
    expected: Option<ExpectedMm>,
    start: u64,
    end: u64,
    mutate_without_publication: bool,
    ceiling: Option<usize>,
    mutate: impl FnOnce(&mut super::UserProcessState) -> Result<(), PagerVmaError>,
    release: impl FnOnce(&mut super::UserProcessState),
) -> Result<Option<(u64, u64)>, PagerVmaError> {
    let retained =
        super::process_table::retain_process_by_pid(process_id).ok_or(PagerVmaError::Stale)?;
    let identity = retained.live_identity().ok_or(PagerVmaError::Stale)?;
    ExpectedMm::check(expected, identity)?;
    let (vma_handle, vma_identity) = retained.vma_owner().ok_or(PagerVmaError::Stale)?;
    let process = vma_handle
        .object_identity()
        .ok_or(PagerVmaError::Stale)?;
    let unmapped = retained.with_state_mut(|_, state| {
        // The pre-lock identity cannot authorize a new image that exec may
        // have committed while this caller waited for ProcessState. Never
        // take the lower-ranked process table from this leaf on a miss.
        if super::process_table::published_live_process_identity(retained.handle())
            != Some(identity)
            || super::process_table::published_mm_owner_identity(vma_handle)
                != Some(vma_identity)
        {
            return Err(PagerVmaError::Unstable);
        }
        let mut mutate = Some(mutate);
        let rewritten = match ceiling {
            None => rewrite_attenuated_range(vma_handle, vma_identity, start, end, None, || {
                mutate.take().expect("unmap mutation consumed once")(state)
            })?,
            Some(ceiling) => rewrite_attenuated_range_with_ceiling(
                vma_handle,
                vma_identity,
                start,
                end,
                None,
                ceiling,
                || mutate.take().expect("unmap mutation consumed once")(state),
            )?,
        };
        // No intersecting pager VMA is still a valid sparse/legacy unmap.
        if mutate_without_publication {
            if let Some(mutate) = mutate {
                mutate(state)?;
            }
        }
        release(state);
        Ok::<_, PagerVmaError>(rewritten)
    })?;
    Ok(unmapped.then(|| (process.slot(), process.generation())))
}

/// Commits or decommits one fully reserved range without changing ownership.
///
/// A reserved range remains the VMA authority but refuses every access.
/// Decommit first removes any resident leaves; commit publishes only the state
/// transition and lets later faults populate backing. This is the primitive
/// required by Windows `MEM_RESERVE`/`MEM_COMMIT` and is also usable by future
/// Linux decommit policy without creating another allocation ledger.
pub fn set_commit_state_for_process(
    process_id: u64,
    start: u64,
    end: u64,
    commit_state: u16,
) -> Result<Option<(u64, u64)>, PagerVmaError> {
    let retained =
        super::process_table::retain_process_by_pid(process_id).ok_or(PagerVmaError::Stale)?;
    let _identity = retained.live_identity().ok_or(PagerVmaError::Stale)?;
    let (vma_handle, vma_identity) = retained.vma_owner().ok_or(PagerVmaError::Stale)?;
    let page_count = end
        .checked_sub(start)
        .and_then(|bytes| usize::try_from(bytes / PAGER_PAGE_BYTES).ok())
        .ok_or(PagerVmaError::Malformed)?;
    let process = vma_handle
        .object_identity()
        .ok_or(PagerVmaError::Stale)?;
    let rewritten = retained.with_state_mut(|_, state| {
        rewrite_range(
            vma_handle,
            vma_identity,
            start,
            end,
            None,
            Some(commit_state),
            MAX_PAGER_VMAS_PER_PROCESS,
            build_default_vma_chunk,
            || {
                if commit_state != VM_COMMIT_RESERVED {
                    return Ok(());
                }
                state
                    .address_space_mut()
                    .unmap_present_prepared_pager_fault_pages_at(
                        x86_64::VirtAddr::new(start),
                        page_count,
                    )
                    .map(|_| ())
                    .map_err(|_| PagerVmaError::Stale)
            },
        )
    })?;
    Ok(rewritten.then(|| (process.slot(), process.generation())))
}

/// Relocates `[old_start, old_end)` to `new_start` while enforcing `ceiling`
/// on any resulting VMA growth, and (through `mutate`, run under the same
/// locks) its page-table leaves and descriptor claims. Returns how many VMAs
/// were moved.
///
/// **Invariant: concurrent threads see the old mapping or fault into the new
/// one, never a copy.** Lock order is process state -> per-process VMA writer ->
/// installer drain -> address-space mutation (TLB) -> frame descriptor. Within
/// the writer, the whole VMA set is held at an odd rewrite epoch, so an
/// exception-time lookup returns `Unstable` and restarts rather than reading a
/// mixture. The overlapping publications are withdrawn (draining installers)
/// *before* `mutate` runs, so no installer holds a prepared leaf of the old
/// range while it moves; the new publications appear only after `mutate`
/// finished its shootdown, so the first access through the new address finds
/// both leaf and VMA. A store racing through a stale translation lands in the
/// frame that is the new mapping, so no store is lost.
///
/// The destination must hold no VMA; the source may contain holes (reserved
/// ranges that have no publication, image or legacy leaves are `mutate`'s
/// business). Nothing changes on any `Err`.
pub fn move_for_process_with_ceiling(
    process_id: u64,
    expected: Option<ExpectedMm>,
    old_start: u64,
    old_end: u64,
    new_start: u64,
    ceiling: usize,
    mutate: impl FnOnce(&mut super::UserProcessState) -> Result<(), PagerVmaError>,
) -> Result<usize, PagerVmaError> {
    let retained =
        super::process_table::retain_process_by_pid(process_id).ok_or(PagerVmaError::Stale)?;
    let identity = retained.live_identity().ok_or(PagerVmaError::Stale)?;
    ExpectedMm::check(expected, identity)?;
    let (vma_handle, vma_identity) = retained.vma_owner().ok_or(PagerVmaError::Stale)?;
    retained.with_state_mut(|_, state| {
        if super::process_table::published_live_process_identity(retained.handle())
            != Some(identity)
            || super::process_table::published_mm_owner_identity(vma_handle)
                != Some(vma_identity)
        {
            return Err(PagerVmaError::Unstable);
        }
        let mut mutate = Some(mutate);
        relocate_range_with(
            vma_handle,
            vma_identity,
            old_start,
            old_end,
            new_start,
            ceiling,
            build_default_vma_chunk,
            || mutate.take().expect("relocation mutation consumed once")(state),
        )
    })
}

/// The geometry half of a relocation: which publications survive where.
///
/// Pure so that the split/translate rule is testable without a process.
/// Returns `(remainders, moved)`: the pieces left outside `[old_start,
/// old_end)` at their old addresses (same publication generation), and the
/// clipped pieces already translated to the destination (generation unstamped).
fn plan_relocation(
    overlapping: &[PagerVmRegionWire],
    old_start: u64,
    old_end: u64,
    new_start: u64,
) -> Result<(Vec<PagerVmRegionWire>, Vec<PagerVmRegionWire>), PagerVmaError> {
    let mut remainders = Vec::new();
    let mut moved = Vec::new();
    remainders
        .try_reserve_exact(overlapping.len().saturating_add(2))
        .map_err(|_| PagerVmaError::Pressure)?;
    moved
        .try_reserve_exact(overlapping.len())
        .map_err(|_| PagerVmaError::Pressure)?;
    for region in overlapping.iter().copied() {
        if !(old_start < region.end && region.start < old_end) {
            return Err(PagerVmaError::Stale);
        }
        let edit = PagerRangeEdit {
            start: old_start,
            end: old_end,
            replacement_prot: None,
        };
        match apply_region_edit(region, edit) {
            PagerRegionEdit::Removed => {}
            PagerRegionEdit::Replaced(only) => remainders.push(only),
            PagerRegionEdit::Split { left, right } => {
                remainders.push(left);
                remainders.push(right);
            }
            PagerRegionEdit::Untouched(_)
            | PagerRegionEdit::ProtectedSplit { .. }
            | PagerRegionEdit::Denied => return Err(PagerVmaError::Stale),
            PagerRegionEdit::Malformed => return Err(PagerVmaError::Malformed),
        }
        let clipped_start = region.start.max(old_start);
        let clipped_end = region.end.min(old_end);
        let mut piece = region;
        piece.object_offset = region
            .object_offset
            .checked_add(clipped_start - region.start)
            .ok_or(PagerVmaError::Malformed)?;
        let (translated_start, translated_end) = if new_start >= old_start {
            let delta = new_start - old_start;
            (
                clipped_start.checked_add(delta),
                clipped_end.checked_add(delta),
            )
        } else {
            let delta = old_start - new_start;
            (
                clipped_start.checked_sub(delta),
                clipped_end.checked_sub(delta),
            )
        };
        piece.start = translated_start.ok_or(PagerVmaError::Malformed)?;
        piece.end = translated_end.ok_or(PagerVmaError::Malformed)?;
        piece.vma_generation = 0;
        piece.process_handle = 0;
        piece.process_generation = 0;
        piece.mm_generation = 0;
        moved.push(piece);
    }
    Ok((remainders, moved))
}

#[cfg(test)]
fn relocate_range<F>(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    old_start: u64,
    old_end: u64,
    new_start: u64,
    mutate: F,
) -> Result<usize, PagerVmaError>
where
    F: FnOnce() -> Result<(), PagerVmaError>,
{
    relocate_range_with(
        handle,
        identity,
        old_start,
        old_end,
        new_start,
        MAX_PAGER_VMAS_PER_PROCESS,
        build_default_vma_chunk,
        mutate,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal relocation core parameterized by policy ceiling and chunk builder"
)]
fn relocate_range_with<F>(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    old_start: u64,
    old_end: u64,
    new_start: u64,
    ceiling: usize,
    build: fn() -> Option<VmaChunkBox>,
    mutate: F,
) -> Result<usize, PagerVmaError>
where
    F: FnOnce() -> Result<(), PagerVmaError>,
{
    let aligned = |value: u64| value.is_multiple_of(PAGER_PAGE_BYTES);
    if old_start == 0
        || new_start == 0
        || old_start >= old_end
        || !aligned(old_start)
        || !aligned(old_end)
        || !aligned(new_start)
    {
        return Err(PagerVmaError::Malformed);
    }
    let new_end = new_start
        .checked_add(old_end - old_start)
        .ok_or(PagerVmaError::Malformed)?;
    if new_start < old_end && old_start < new_end {
        return Err(PagerVmaError::Overlap);
    }
    if handle.generation() != identity.process_generation() {
        return Err(PagerVmaError::Stale);
    }
    let ceiling = ceiling.min(MAX_PAGER_VMAS_PER_PROCESS);

    let mut writer = writer_lock(handle).ok_or(PagerVmaError::Stale)?.lock();
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    let live_process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    reclaim_stale_incarnation(handle, slots, &mut writer, live_process.slot(), identity)?;

    // O(log N) destination overlap check and O(log N + K) source range query.
    let mut source_entries: Vec<BTreeEntry> = Vec::new();
    if let Some(index) = slots.index() {
        if let Some(dest) = index.first_ending_after(new_start)
            && new_start < dest.end
            && dest.start < new_end
        {
            return Err(PagerVmaError::Overlap);
        }
        index.collect_range(old_start, old_end, false, &mut source_entries)?;
    }
    let mut overlapping: Vec<(usize, PagerVmRegionWire)> = Vec::new();
    overlapping
        .try_reserve_exact(source_entries.len())
        .map_err(|_| PagerVmaError::Pressure)?;
    let mut regions: Vec<PagerVmRegionWire> = Vec::new();
    regions
        .try_reserve_exact(source_entries.len())
        .map_err(|_| PagerVmaError::Pressure)?;
    for entry in source_entries {
        let slot_index = entry.slot as usize;
        let Some(region) = slots[slot_index].snapshot()? else {
            return Err(PagerVmaError::Stale);
        };
        overlapping.push((slot_index, region));
        regions.push(region);
    }
    let live_total = writer.live_count;
    let (remainders, moved) = plan_relocation(&regions, old_start, old_end, new_start)?;
    let publications = remainders.len() + moved.len();
    let new_live = live_total
        .checked_sub(overlapping.len())
        .and_then(|rest| rest.checked_add(publications))
        .ok_or(PagerVmaError::Pressure)?;
    if publications > overlapping.len() && new_live > ceiling {
        return Err(PagerVmaError::Pressure);
    }
    let extra_needed = publications.saturating_sub(overlapping.len());
    if extra_needed > 0 {
        writer.ensure_free_slots(slots, extra_needed, build)?;
    }
    let freed_on_shrink = overlapping.len().saturating_sub(publications);
    if freed_on_shrink > 0 {
        writer
            .free_slots
            .try_reserve(freed_on_shrink)
            .map_err(|_| PagerVmaError::Pressure)?;
    }
    if publications > 0 {
        let index = slots.ensure_index()?;
        index.ensure_node_headroom(&mut writer, live_total.max(new_live))?;
    }

    let free_len = writer.free_slots.len();
    let mut targets: Vec<usize> = Vec::new();
    targets
        .try_reserve_exact(publications)
        .map_err(|_| PagerVmaError::Pressure)?;
    for idx in 0..publications {
        let slot_index = if idx < overlapping.len() {
            overlapping[idx].0
        } else {
            writer.free_slots[free_len - 1 - (idx - overlapping.len())] as usize
        };
        let sequence = slots[slot_index].sequence.load(Ordering::Relaxed);
        if sequence & 1 != 0 || sequence > MAX_PUBLICATION_SEQUENCE - 2 {
            return Err(PagerVmaError::Pressure);
        }
        targets.push(slot_index);
    }
    // Stamp the moved pieces up front so a refusal cannot leave a half-built
    // publication set; each takes the generation its own slot will commit.
    let mut stamped_moved: Vec<PagerVmRegionWire> = Vec::new();
    stamped_moved
        .try_reserve_exact(moved.len())
        .map_err(|_| PagerVmaError::Pressure)?;
    for (offset, piece) in moved.iter().copied().enumerate() {
        let slot = &slots[targets[remainders.len() + offset]];
        let generation = slot
            .sequence
            .load(Ordering::Relaxed)
            .checked_div(2)
            .and_then(|value| value.checked_add(1))
            .ok_or(PagerVmaError::Pressure)?;
        stamped_moved.push(stamped_region(handle, identity, piece, generation)?);
    }

    // Whole-set odd interval: fault-time misses are restartable until every
    // publication below (or the rollback) is committed.
    let _move_rewrite = RangeRewrite::begin(handle)?;
    for (position, (slot_index, _)) in overlapping.iter().copied().enumerate() {
        let Err(error) = slots[slot_index].publish(None) else {
            continue;
        };
        for (restore_index, region) in overlapping[..position].iter().copied() {
            let _ = slots[restore_index].publish(Some(region));
        }
        return Err(error);
    }
    if let Err(error) = mutate() {
        for (slot_index, region) in overlapping.iter().copied() {
            let _ = slots[slot_index].publish(Some(region));
        }
        return Err(error);
    }
    for (idx, region) in remainders.iter().copied().enumerate() {
        slots[targets[idx]].publish(Some(region))?;
    }
    for (offset, region) in stamped_moved.iter().copied().enumerate() {
        slots[targets[remainders.len() + offset]].publish(Some(region))?;
    }
    if let Some(index) = slots.index() {
        for (_, old_region) in overlapping.iter().copied() {
            let _ = index.remove(&mut writer, old_region.start);
        }
        for _ in 0..extra_needed {
            let _ = writer.free_slots.pop();
        }
        for (slot_index, _) in overlapping[publications.min(overlapping.len())..]
            .iter()
            .copied()
        {
            writer.free_slots.push(slot_index as u32);
        }
        for (idx, region) in remainders.iter().copied().enumerate() {
            index.insert(
                &mut writer,
                BTreeEntry {
                    start: region.start,
                    end: region.end,
                    slot: targets[idx] as u32,
                },
            );
        }
        for (offset, region) in stamped_moved.iter().copied().enumerate() {
            index.insert(
                &mut writer,
                BTreeEntry {
                    start: region.start,
                    end: region.end,
                    slot: targets[remainders.len() + offset] as u32,
                },
            );
        }
        writer.live_count = new_live;
    }
    Ok(stamped_moved.len())
}

/// Reports the first occupied extent intersecting a page-aligned query.
/// This is advisory geometry, not placement or mapping authority. Ring3 picks
/// addresses; a later installer must revalidate overlap under its own custody.
/// Includes demand/PROT_NONE reservations even when they have no leaf PTEs.
pub fn first_mapping_conflict(
    process_id: u64,
    expected: Option<ExpectedMm>,
    start: u64,
    end: u64,
) -> Result<Option<(u64, u64)>, PagerVmaError> {
    use crate::memory::paging::{USER_SPACE_BASE, USER_SPACE_END_EXCLUSIVE};
    if start < USER_SPACE_BASE || start >= end || end > USER_SPACE_END_EXCLUSIVE
        || !start.is_multiple_of(PAGER_PAGE_BYTES) || !end.is_multiple_of(PAGER_PAGE_BYTES)
    {
        return Err(PagerVmaError::Malformed);
    }
    let retained = super::process_table::retain_process_by_pid(process_id)
        .ok_or(PagerVmaError::Stale)?;
    let identity = retained.live_identity().ok_or(PagerVmaError::Stale)?;
    ExpectedMm::check(expected, identity)?;
    let (vma_handle, vma_identity) = retained.vma_owner().ok_or(PagerVmaError::Stale)?;
    let process = vma_handle.object_identity().ok_or(PagerVmaError::Stale)?;
    retained.with_exact_visible_state_mut(identity, |_, state| {
        let writer = writer_lock(vma_handle).ok_or(PagerVmaError::Stale)?.lock();
        let slots = process_slots(vma_handle).ok_or(PagerVmaError::Stale)?;
        let mut first: Option<(u64, u64)> = None;
        if writer.live_count > 0
            && writer.matches_owner(process.slot(), vma_identity)
            && let Some(index) = slots.index()
            && let Some(candidate) = index.first_ending_after(start)
            && candidate.start < end
        {
            if let Some(region) = slots[candidate.slot as usize].snapshot()?
                && region.start < end
                && start < region.end
                && region_matches_identity(region, process.slot(), vma_identity)
            {
                first = Some((region.start, region.end));
            }
        }
        if first.is_some_and(|range| range.0 <= start) {
            return Ok(first);
        }
        // Process custody excludes destructive PTE edits. Demand faults may
        // only add leaves within the reservations already checked above.
        let scan_end = first.map_or(end, |range| range.0.min(end));
        let count = usize::try_from((scan_end - start) / PAGER_PAGE_BYTES)
            .map_err(|_| PagerVmaError::Malformed)?;
        let space = state.address_space();
        let mapped = space.first_mapped_user_page(x86_64::VirtAddr::new(start), count)
            .map_err(|_| PagerVmaError::Stale)?;
        if let Some(mapped) = mapped {
            // Skip a dense image/data extent in one sparse walk, rather than
            // forcing a service to issue one native query per occupied page.
            let count = usize::try_from((USER_SPACE_END_EXCLUSIVE - mapped.as_u64()) / PAGER_PAGE_BYTES)
                .map_err(|_| PagerVmaError::Malformed)?;
            let gap = space.first_unmapped_user_page(mapped, count)
                .map_err(|_| PagerVmaError::Stale)?;
            return Ok(Some((mapped.as_u64(), gap.map_or(USER_SPACE_END_EXCLUSIVE, |v| v.as_u64()))));
        }
        Ok(first)
    }).ok_or(PagerVmaError::Stale)?
}

/// Returns every region this process has published as one stable reading.
pub(super) fn snapshot_regions(
    handle: ProcessHandle,
    identity: ProcessIdentity,
) -> Result<Vec<PagerVmRegionWire>, PagerVmaError> {
    if handle.generation() != identity.process_generation() {
        return Err(PagerVmaError::Stale);
    }
    let process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    let writer = writer_lock(handle).ok_or(PagerVmaError::Stale)?.lock();
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    if writer.live_count == 0 || !writer.matches_owner(process.slot(), identity) {
        return Ok(Vec::new());
    }
    let Some(index) = slots.index() else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(writer.live_count)
        .map_err(|_| PagerVmaError::Pressure)?;
    index.collect_all(&mut entries)?;
    let mut regions = Vec::new();
    regions
        .try_reserve_exact(entries.len())
        .map_err(|_| PagerVmaError::Pressure)?;
    for entry in entries {
        let Some(region) = slots[entry.slot as usize].snapshot()? else {
            continue;
        };
        if !region_matches_identity(region, process.slot(), identity) {
            continue;
        }
        regions.push(region);
    }
    Ok(regions)
}

pub(super) fn revoke(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    start: u64,
    vma_generation: u64,
) -> Result<PagerVmRegionWire, PagerVmaError> {
    let process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    let mut writer = writer_lock(handle).ok_or(PagerVmaError::Stale)?.lock();
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    if writer.live_count == 0 || !writer.matches_owner(process.slot(), identity) {
        return Err(PagerVmaError::Stale);
    }
    let Some(index) = slots.index() else {
        return Err(PagerVmaError::Stale);
    };
    let Some(entry) = index.find_exact(start) else {
        return Err(PagerVmaError::Stale);
    };
    let slot = &slots[entry.slot as usize];
    let Some(region) = slot.snapshot()? else {
        return Err(PagerVmaError::Stale);
    };
    // Through the single identity definition like every other authority
    // decision here. This also adds the exact process-slot comparison the
    // open-coded version omitted, so residue left by a previous occupant
    // of these slots can no longer be revoked through this handle.
    if region.start == start
        && region.vma_generation == vma_generation
        && region_matches_identity(region, process.slot(), identity)
    {
        writer
            .free_slots
            .try_reserve(1)
            .map_err(|_| PagerVmaError::Pressure)?;
        let _revoke_rewrite = RangeRewrite::begin(handle)?;
        slot.publish(None)?;
        let _ = index.remove(&mut writer, start);
        writer.free_slots.push(entry.slot);
        writer.live_count -= 1;
        return Ok(region);
    }
    Err(PagerVmaError::Stale)
}

#[cfg(test)]
#[path = "pager_vma/tests.rs"]
mod tests;
