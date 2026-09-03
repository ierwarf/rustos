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
//!   wrap/reuse, W+X publication, physical address, or exception-time wait.
//! - **Evidence:** Focused unit tests plus the `pager-vma-publication-*` formal
//!   and implementation mutations.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::pager::{
    PAGER_MAX_REGION_GROWTH_PER_PROTECT, PAGER_MAX_REGION_GROWTH_PER_UNMAP,
    PAGER_MAX_VMAS_PER_PROCESS, PAGER_PAGE_BYTES, PagerEndpointCapabilityWire,
    PagerObjectIdentityWire, PagerRangeEdit, PagerRegionEdit, PagerVmRegionWire, VM_ACCESS_EXECUTE,
    VM_ACCESS_KNOWN, VM_ACCESS_READ, VM_ACCESS_WRITE, VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE,
    VM_SHARING_PRIVATE, VM_SHARING_SHARED, apply_region_edit,
};

use super::process_table::MAX_PROCESS_OBJECTS;
use super::process_table::{ProcessHandle, ProcessIdentity};

/// Ring0's per-process pager VMA table, taken from the shared ABI.
///
/// This used to be a private `64` with no declared relationship to pagerd's
/// region table, so the two capacities could drift apart silently and nothing
/// said what the safe relation between them was. `PAGER_MIN_FULLY_TRACKED_PROCESSES`
/// in the ABI is that relation, and it is only meaningful if both replicas
/// read the same constant.
const MAX_PAGER_VMAS_PER_PROCESS: usize = PAGER_MAX_VMAS_PER_PROCESS;
const MAX_PUBLICATION_SEQUENCE: u64 = u64::MAX - 3;

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
            vma_generation: AtomicU64::new(0),
            process_handle: AtomicU64::new(0),
            process_generation: AtomicU64::new(0),
            mm_generation: AtomicU64::new(0),
            fault_endpoint_slot: AtomicU64::new(0),
            fault_endpoint_generation: AtomicU64::new(0),
            fault_endpoint_rights: AtomicU64::new(0),
        }
    }

    /// Cheap address filter for the linear scan.
    ///
    /// The scan rejects almost every slot, and the full `snapshot` it used to
    /// call for each one reads twenty-one atomics to build a region it then
    /// throws away. This reads the three fields an *address* rejection can be
    /// decided from.
    ///
    /// It filters on address extent and nothing else, deliberately. An earlier
    /// version also compared the process and MM generations here, which looks
    /// like a free win and is not: duplicating an authority check means the
    /// duplicate keeps rejecting when the real one is broken, so the
    /// registered mutant for `identityExact` survived. A filter that can mask
    /// a security check is no longer a filter. Every authority decision stays
    /// in the single validated path below.
    /// Cheap extent-overlap filter, the range counterpart of [`may_cover`].
    ///
    /// Same contract: address extent only, never authority, conservative when
    /// a writer holds the slot. Its job is to size an exact allocation before
    /// the expensive full snapshots run.
    fn may_overlap(&self, start: u64, end: u64) -> bool {
        // ORDERING: Acquire observes the writer's even commit before the
        // Relaxed payload reads below, exactly as `snapshot` does.
        if self.sequence.load(Ordering::Acquire) & 1 != 0 {
            return true;
        }
        let slot_start = self.start.load(Ordering::Relaxed);
        let slot_end = self.end.load(Ordering::Relaxed);
        slot_start != 0 && start < slot_end && slot_start < end
    }

    fn may_cover(&self, address: u64) -> bool {
        // ORDERING: Acquire observes the writer's even commit before the
        // Relaxed payload reads below, exactly as `snapshot` does.
        let before = self.sequence.load(Ordering::Acquire);
        if before & 1 != 0 {
            // A writer holds this slot. Let the full snapshot decide.
            return true;
        }
        let start = self.start.load(Ordering::Relaxed);
        let end = self.end.load(Ordering::Relaxed);
        start != 0 && address >= start && address < end
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
                reserved0: 0,
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
    reason = "array initializer for fixed all-atomic VMA publications"
)]
const EMPTY_PAGER_VMA: PublishedPagerVma = PublishedPagerVma::empty();

static PAGER_VMAS: [PublishedPagerVma; MAX_PROCESS_OBJECTS * MAX_PAGER_VMAS_PER_PROCESS] =
    [EMPTY_PAGER_VMA; MAX_PROCESS_OBJECTS * MAX_PAGER_VMAS_PER_PROCESS];
type PagerVmaWriterLock = TrackedSpinLock<(), { LockClass::PagerVmaPublication as u8 }>;
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
/// `process_slots` hands each process its own slice, and the only shared state
/// a writer touches is `NEXT_ANON_OBJECT_SLOT`, which is a standalone atomic.
#[expect(
    clippy::declare_interior_mutable_const,
    reason = "array initializer for the fixed per-process writer lock table"
)]
const EMPTY_PAGER_VMA_WRITER: PagerVmaWriterLock = TrackedSpinLock::new(());
static PAGER_VMA_WRITERS: [PagerVmaWriterLock; MAX_PROCESS_OBJECTS] =
    [EMPTY_PAGER_VMA_WRITER; MAX_PROCESS_OBJECTS];

/// The writer lock guarding one process's publication slots.
fn writer_lock(handle: ProcessHandle) -> Option<&'static PagerVmaWriterLock> {
    PAGER_VMA_WRITERS.get(handle.index())
}

fn process_slots(handle: ProcessHandle) -> Option<&'static [PublishedPagerVma]> {
    let start = handle.index().checked_mul(MAX_PAGER_VMAS_PER_PROCESS)?;
    PAGER_VMAS.get(start..start.checked_add(MAX_PAGER_VMAS_PER_PROCESS)?)
}

fn template_is_canonical(region: PagerVmRegionWire) -> bool {
    region.start != 0
        && region.start < region.end
        && region.start.is_multiple_of(PAGER_PAGE_BYTES)
        && region.end.is_multiple_of(PAGER_PAGE_BYTES)
        && region.object_offset.is_multiple_of(PAGER_PAGE_BYTES)
        && region.object.has_authority()
        && region.prot != 0
        && region.prot & !rustos_user_abi::pager::VM_PROT_KNOWN == 0
        && region.prot & !region.object.rights == 0
        && !(region.prot & VM_PROT_WRITE != 0 && region.prot & VM_PROT_EXECUTE != 0)
        && (region.sharing == VM_SHARING_PRIVATE || region.sharing == VM_SHARING_SHARED)
        && region.reserved0 == 0
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

pub(super) fn publish(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    template: PagerVmRegionWire,
) -> Result<PagerVmRegionWire, PagerVmaError> {
    if handle.generation() != identity.process_generation() {
        return Err(PagerVmaError::Stale);
    }
    let _writer = writer_lock(handle).ok_or(PagerVmaError::Stale)?.lock();
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    for existing in slots {
        if let Some(existing) = existing.snapshot()? {
            if template.start < existing.end && existing.start < template.end {
                return Err(PagerVmaError::Overlap);
            }
        }
    }
    let slot = slots
        .iter()
        .find(|slot| matches!(slot.snapshot(), Ok(None)))
        .ok_or(PagerVmaError::Pressure)?;
    let sequence = slot.sequence.load(Ordering::Relaxed);
    let generation = sequence
        .checked_div(2)
        .and_then(|value| value.checked_add(1))
        .ok_or(PagerVmaError::Pressure)?;
    let region = stamped_region(handle, identity, template, generation)?;
    slot.publish(Some(region))?;
    Ok(region)
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
/// The slot index is the point. A fault used to run this scan, then run it
/// again to re-find the same slot for its permit, then a third time to clip
/// its fault-around run - three linear sweeps of a 64-slot table whose every
/// probe read twenty-one atomics. Returning the index lets the permit go
/// straight to the one slot that matters.
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
    for (index, slot) in slots.iter().enumerate() {
        if !slot.may_cover(address) {
            continue;
        }
        let Some(region) = slot.snapshot()? else {
            continue;
        };
        // Written in full rather than against the locals above on purpose:
        // this exact expression is the registered implementation-mutation
        // anchor `pager-vma-publication-identity-bypass`. The locals exist for
        // `may_cover`, which runs per slot; this runs once per candidate, so
        // spelling it out costs nothing and keeps the witness anchored.
        if region.process_handle != process.slot()
            || region.process_generation != u64::from(identity.process_generation())
            || region.mm_generation != u64::from(identity.mm_generation())
        {
            continue;
        }
        if region.contains(address) {
            return admit_access(region, access).map(|region| (index, region));
        }
    }
    Err(PagerVmaError::Stale)
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
        super::process_table::live_process_identity(handle).ok_or(PagerVmaError::Stale)?;
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
    f: impl FnOnce(u64, &mut crate::memory::paging::ProcessAddressSpace) -> R,
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
        super::process_table::live_process_identity(handle).ok_or(PagerVmaError::Stale)?;
    let process = super::process_table::retain_process(handle).ok_or(PagerVmaError::Stale)?;
    process
        .with_exact_visible_state_mut(expected, |process_id, state| {
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
            Ok(f(process_id, state.address_space_mut()))
        })
        .ok_or(PagerVmaError::Stale)?
}

/// Rewrites one fully pager-managed range while preserving only attenuated
/// authority. The original publications are withdrawn before `mutate` changes
/// any PTE, so exception-time readers fail closed throughout the transaction.
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
    if start == 0
        || start >= end
        || !start.is_multiple_of(PAGER_PAGE_BYTES)
        || !end.is_multiple_of(PAGER_PAGE_BYTES)
    {
        return Err(PagerVmaError::Malformed);
    }
    if replacement_prot.is_some_and(|prot| {
        prot & !rustos_user_abi::pager::VM_PROT_KNOWN != 0
            || (prot & VM_PROT_WRITE != 0 && prot & VM_PROT_EXECUTE != 0)
    }) {
        return Err(PagerVmaError::Denied);
    }
    if handle.generation() != identity.process_generation() {
        return Err(PagerVmaError::Stale);
    }

    // Sized exactly, on the heap, because these buffers scale with the VMA
    // table and the syscall stack does not.
    //
    // They used to be `[PagerVmRegionWire; MAX_PAGER_VMAS_PER_PROCESS]`-shaped
    // arrays: ~22 KiB of a 64 KiB syscall stack at 64 slots, and ~44 KiB at
    // 128 - which is what made the per-process VMA table unable to grow, and
    // that table filling is what turned an `mprotect(PROT_NONE)` guard page
    // into `ENOMEM`. The cheap extent filter below sizes one exact allocation
    // instead, so the common edit - one `mprotect` inside one region -
    // reserves a single entry rather than the whole table.
    let _writer = writer_lock(handle).ok_or(PagerVmaError::Stale)?.lock();
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    let candidates = slots
        .iter()
        .filter(|slot| slot.may_overlap(start, end))
        .count();
    if candidates == 0 {
        return Ok(false);
    }
    let mut overlapping: Vec<(usize, PagerVmRegionWire)> = Vec::new();
    overlapping
        .try_reserve_exact(candidates)
        .map_err(|_| PagerVmaError::Pressure)?;
    let mut empty_slots: Vec<usize> = Vec::new();
    empty_slots
        .try_reserve_exact(slots.len())
        .map_err(|_| PagerVmaError::Pressure)?;
    for (slot_index, slot) in slots.iter().enumerate() {
        match slot.snapshot()? {
            Some(region) if start < region.end && region.start < end => {
                overlapping.push((slot_index, region));
            }
            Some(_) => {}
            None => empty_slots.push(slot_index),
        }
    }
    let overlapping_len = overlapping.len();
    let empty_len = empty_slots.len();
    if overlapping_len == 0 {
        return Ok(false);
    }
    overlapping.sort_unstable_by_key(|(_, region)| region.start);

    let process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    let edit = PagerRangeEdit {
        start,
        end,
        replacement_prot,
    };
    let mut cursor = start;
    let mut rewritten: Vec<PagerVmRegionWire> = Vec::new();
    rewritten
        .try_reserve_exact(
            overlapping_len
                .checked_add(PAGER_MAX_REGION_GROWTH_PER_PROTECT)
                .ok_or(PagerVmaError::Pressure)?,
        )
        .map_err(|_| PagerVmaError::Pressure)?;
    for (_, region) in overlapping.iter().copied() {
        if region.process_handle != process.slot()
            || region.process_generation != u64::from(identity.process_generation())
            || region.mm_generation != u64::from(identity.mm_generation())
            || region.start > cursor
        {
            return Err(PagerVmaError::Stale);
        }
        // The split/trim/remove rule is the shared ABI one. pagerd applies the
        // same call to its own replica of this region, so the two tables
        // cannot disagree about what an edit leaves behind - which is exactly
        // what happened while each side derived its own remainders.
        let mut push = |fragment| rewritten.push(fragment);
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
    let rewritten_len = rewritten.len();
    debug_assert!(
        rewritten_len <= overlapping_len + PAGER_MAX_REGION_GROWTH_PER_PROTECT,
        "one range edit may add at most one interior split's fragments"
    );
    if cursor < end || rewritten_len > overlapping_len + empty_len {
        return Err(if cursor < end {
            PagerVmaError::Stale
        } else {
            PagerVmaError::Pressure
        });
    }

    let mut targets: Vec<usize> = Vec::new();
    targets
        .try_reserve_exact(rewritten_len)
        .map_err(|_| PagerVmaError::Pressure)?;
    for index in 0..rewritten_len {
        targets.push(if index < overlapping_len {
            overlapping[index].0
        } else {
            empty_slots[index - overlapping_len]
        });
        let sequence = slots[targets[index]].sequence.load(Ordering::Relaxed);
        if sequence & 1 != 0 || sequence > MAX_PUBLICATION_SEQUENCE - 2 {
            return Err(PagerVmaError::Pressure);
        }
    }

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
    Ok(true)
}

/// Narrows protection over one exact range and reports the identity whose
/// regions were rewritten.
///
/// Returning the stamped `(process_handle, process_generation)` lets the caller
/// publish the same narrowing to the pager under the identity ring0 actually
/// edited, rather than re-deriving one that could disagree - the same reason
/// [`unmap_for_process`] returns it.
pub fn protect_for_process(
    process_id: u64,
    start: u64,
    end: u64,
    prot: u32,
    page_flags: x86_64::structures::paging::PageTableFlags,
) -> Result<Option<(u64, u64)>, PagerVmaError> {
    let retained =
        super::process_table::retain_process_by_pid(process_id).ok_or(PagerVmaError::Stale)?;
    let identity = retained.live_identity().ok_or(PagerVmaError::Stale)?;
    let page_count =
        usize::try_from((end - start) / PAGER_PAGE_BYTES).map_err(|_| PagerVmaError::Malformed)?;
    let process = retained
        .handle()
        .object_identity()
        .ok_or(PagerVmaError::Stale)?;
    let rewritten = retained.with_state_mut(|_, state| {
        rewrite_attenuated_range(retained.handle(), identity, start, end, Some(prot), || {
            state
                .address_space_mut()
                .protect_present_prepared_pager_fault_pages_at(
                    x86_64::VirtAddr::new(start),
                    page_count,
                    page_flags,
                )
                .map(|_| ())
                .map_err(|_| PagerVmaError::Stale)
        })
    })?;
    Ok(rewritten.then(|| (process.slot(), process.generation())))
}

/// Unmaps one exact range and reports the identity whose slot was released.
///
/// Returning the stamped `(process_handle, process_generation)` lets the caller
/// name to the pager exactly what ring0 released, instead of re-deriving an
/// identity that could disagree with the publication.
pub fn unmap_for_process(
    process_id: u64,
    start: u64,
    end: u64,
) -> Result<Option<(u64, u64)>, PagerVmaError> {
    let retained =
        super::process_table::retain_process_by_pid(process_id).ok_or(PagerVmaError::Stale)?;
    let identity = retained.live_identity().ok_or(PagerVmaError::Stale)?;
    let page_count =
        usize::try_from((end - start) / PAGER_PAGE_BYTES).map_err(|_| PagerVmaError::Malformed)?;
    let process = retained
        .handle()
        .object_identity()
        .ok_or(PagerVmaError::Stale)?;
    let unmapped = retained.with_state_mut(|_, state| {
        rewrite_attenuated_range(retained.handle(), identity, start, end, None, || {
            state
                .address_space_mut()
                .unmap_present_prepared_pager_fault_pages_at(
                    x86_64::VirtAddr::new(start),
                    page_count,
                )
                .map(|_| ())
                .map_err(|_| PagerVmaError::Stale)
        })
    })?;
    Ok(unmapped.then(|| (process.slot(), process.generation())))
}

pub(super) fn revoke(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    start: u64,
    vma_generation: u64,
) -> Result<PagerVmRegionWire, PagerVmaError> {
    let _writer = writer_lock(handle).ok_or(PagerVmaError::Stale)?.lock();
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    for slot in slots {
        let Some(region) = slot.snapshot()? else {
            continue;
        };
        if region.start == start
            && region.vma_generation == vma_generation
            && region.process_generation == u64::from(identity.process_generation())
            && region.mm_generation == u64::from(identity.mm_generation())
        {
            slot.publish(None)?;
            return Ok(region);
        }
    }
    Err(PagerVmaError::Stale)
}

#[cfg(test)]
#[path = "pager_vma/tests.rs"]
mod tests;
