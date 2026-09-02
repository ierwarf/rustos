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

struct PublishedPagerVma {
    sequence: AtomicU64,
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
        // ORDERING: publish the odd invalidation before changing payload so a
        // concurrent reader abandons its snapshot rather than accepting a
        // mixture of old and new VMA fields.
        self.sequence.store(before + 1, Ordering::Release);
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
}

#[expect(
    clippy::declare_interior_mutable_const,
    reason = "array initializer for fixed all-atomic VMA publications"
)]
const EMPTY_PAGER_VMA: PublishedPagerVma = PublishedPagerVma::empty();

static PAGER_VMAS: [PublishedPagerVma; MAX_PROCESS_OBJECTS * MAX_PAGER_VMAS_PER_PROCESS] =
    [EMPTY_PAGER_VMA; MAX_PROCESS_OBJECTS * MAX_PAGER_VMAS_PER_PROCESS];
type PagerVmaWriterLock = TrackedSpinLock<(), { LockClass::PagerVmaPublication as u8 }>;
static PAGER_VMA_WRITER: PagerVmaWriterLock = TrackedSpinLock::new(());

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
    let _writer = PAGER_VMA_WRITER.lock();
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

pub(super) fn lookup(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    address: u64,
    access: u16,
) -> Result<PagerVmRegionWire, PagerVmaError> {
    if access == 0 || access & !VM_ACCESS_KNOWN != 0 || access.count_ones() != 1 {
        return Err(PagerVmaError::Malformed);
    }
    let process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    for slot in slots {
        let Some(region) = slot.snapshot()? else {
            continue;
        };
        if region.process_handle != process.slot()
            || region.process_generation != u64::from(identity.process_generation())
            || region.mm_generation != u64::from(identity.mm_generation())
        {
            continue;
        }
        if region.contains(address) {
            let allowed = (access == VM_ACCESS_READ && region.prot & VM_PROT_READ != 0)
                || (access == VM_ACCESS_WRITE && region.prot & VM_PROT_WRITE != 0)
                || (access == VM_ACCESS_EXECUTE && region.prot & VM_PROT_EXECUTE != 0);
            return allowed.then_some(region).ok_or(PagerVmaError::Denied);
        }
    }
    Err(PagerVmaError::Stale)
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

    const MAX_REWRITTEN_REGIONS: usize = MAX_PAGER_VMAS_PER_PROCESS + 2;
    let _writer = PAGER_VMA_WRITER.lock();
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    let mut overlapping = [(usize::MAX, PagerVmRegionWire::default()); MAX_PAGER_VMAS_PER_PROCESS];
    let mut overlapping_len = 0;
    let mut empty_slots = [usize::MAX; MAX_PAGER_VMAS_PER_PROCESS];
    let mut empty_len = 0;
    for (slot_index, slot) in slots.iter().enumerate() {
        match slot.snapshot()? {
            Some(region) if start < region.end && region.start < end => {
                overlapping[overlapping_len] = (slot_index, region);
                overlapping_len += 1;
            }
            Some(_) => {}
            None => {
                empty_slots[empty_len] = slot_index;
                empty_len += 1;
            }
        }
    }
    if overlapping_len == 0 {
        return Ok(false);
    }
    overlapping[..overlapping_len].sort_unstable_by_key(|(_, region)| region.start);

    let process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    let edit = PagerRangeEdit {
        start,
        end,
        replacement_prot,
    };
    let mut cursor = start;
    let mut rewritten = [None; MAX_REWRITTEN_REGIONS];
    let mut rewritten_len = 0;
    for (_, region) in overlapping[..overlapping_len].iter().copied() {
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
        let mut push = |fragment| {
            rewritten[rewritten_len] = Some(fragment);
            rewritten_len += 1;
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

    let mut targets = [usize::MAX; MAX_REWRITTEN_REGIONS];
    for index in 0..rewritten_len {
        targets[index] = if index < overlapping_len {
            overlapping[index].0
        } else {
            empty_slots[index - overlapping_len]
        };
        let sequence = slots[targets[index]].sequence.load(Ordering::Relaxed);
        if sequence & 1 != 0 || sequence > MAX_PUBLICATION_SEQUENCE - 2 {
            return Err(PagerVmaError::Pressure);
        }
    }

    for (slot_index, _) in overlapping[..overlapping_len].iter().copied() {
        slots[slot_index].publish(None)?;
    }
    if let Err(error) = mutate() {
        for (slot_index, region) in overlapping[..overlapping_len].iter().copied() {
            slots[slot_index].publish(Some(region))?;
        }
        return Err(error);
    }
    for index in 0..rewritten_len {
        slots[targets[index]].publish(rewritten[index])?;
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
    let _writer = PAGER_VMA_WRITER.lock();
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
mod tests {
    use super::*;
    use crate::memory::paging::ProcessAddressSpace;
    use crate::user::process_state::UserProcessState;
    use rustos_user_abi::pager::{VM_OBJECT_ANONYMOUS, VM_PROT_READ, VM_PROT_WRITE};
    use std::sync::Mutex;

    /// The publication table is a process-wide bounded array whose free slots
    /// are shared by every test in this module. Two tests publishing at once
    /// can land on the same slot and make a healthy reader observe the other
    /// writer's odd invalidation as `Unstable`, so the suite serializes here.
    static TEST_GUARD: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn identity(generation: u32, mm_generation: u32) -> ProcessIdentity {
        ProcessIdentity::from_parts(41, generation, mm_generation)
    }

    fn template(start: u64) -> PagerVmRegionWire {
        PagerVmRegionWire {
            start,
            end: start + PAGER_PAGE_BYTES * 2,
            object: PagerObjectIdentityWire {
                object_type: VM_OBJECT_ANONYMOUS,
                rights: VM_PROT_READ | VM_PROT_WRITE,
                backing_service: 0,
                slot: 17,
                generation: 19,
                pager_epoch: 23,
                backing_generation: 29,
                ..PagerObjectIdentityWire::default()
            },
            object_offset: 0,
            prot: VM_PROT_READ | VM_PROT_WRITE,
            sharing: VM_SHARING_PRIVATE,
            fault_endpoint: PagerEndpointCapabilityWire {
                slot: 31,
                generation: 37,
                rights: 1,
            },
            ..PagerVmRegionWire::default()
        }
    }

    fn process_state() -> UserProcessState {
        UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            None,
            None,
            None,
            None,
            false,
            "/pager-vma-test.elf",
        )
    }

    #[test]
    fn publication_stamps_exact_process_mm_and_nonzero_vma_generation() {
        let _guard = guard();
        let handle = ProcessHandle::new(29, 43);
        let identity = identity(43, 47);
        let published = publish(handle, identity, template(0x4000)).unwrap();
        assert_eq!(published.process_handle, 30);
        assert_eq!(published.process_generation, 43);
        assert_eq!(published.mm_generation, 47);
        assert_ne!(published.vma_generation, 0);
        assert_eq!(
            lookup(handle, identity, 0x5000, VM_ACCESS_WRITE),
            Ok(published)
        );
        revoke(handle, identity, published.start, published.vma_generation).unwrap();
    }

    #[test]
    fn overlap_and_permission_escalation_fail_closed() {
        let _guard = guard();
        let handle = ProcessHandle::new(28, 53);
        let identity = identity(53, 59);
        let published = publish(handle, identity, template(0x8000)).unwrap();
        assert_eq!(
            publish(handle, identity, template(0x9000)),
            Err(PagerVmaError::Overlap)
        );
        assert_eq!(
            lookup(handle, identity, 0x8000, VM_ACCESS_EXECUTE),
            Err(PagerVmaError::Denied)
        );
        revoke(handle, identity, published.start, published.vma_generation).unwrap();
    }

    #[test]
    fn protection_attenuation_and_unmap_rewrite_before_mutation() {
        let _guard = guard();
        let handle = ProcessHandle::new(27, 61);
        let identity = identity(61, 67);
        let published = publish(handle, identity, template(0x20_000)).unwrap();
        let mut protection_mutated = false;
        assert_eq!(
            rewrite_attenuated_range(
                handle,
                identity,
                published.start,
                published.start + PAGER_PAGE_BYTES,
                Some(0),
                || {
                    protection_mutated = true;
                    Ok(())
                },
            ),
            Ok(true)
        );
        assert!(protection_mutated);
        assert_eq!(
            lookup(handle, identity, published.start, VM_ACCESS_READ),
            Err(PagerVmaError::Denied)
        );
        let right = lookup(
            handle,
            identity,
            published.start + PAGER_PAGE_BYTES,
            VM_ACCESS_WRITE,
        )
        .unwrap();
        assert_eq!(right.vma_generation, published.vma_generation);
        assert_eq!(right.object_offset, PAGER_PAGE_BYTES);
        assert_eq!(
            rewrite_attenuated_range(
                handle,
                identity,
                right.start,
                right.end,
                Some(VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE),
                || Ok(()),
            ),
            Err(PagerVmaError::Denied)
        );

        let mut unmapped = false;
        assert_eq!(
            rewrite_attenuated_range(handle, identity, right.start, right.end, None, || {
                unmapped = true;
                Ok(())
            }),
            Ok(true)
        );
        assert!(unmapped);
        assert_eq!(
            lookup(handle, identity, right.start, VM_ACCESS_READ),
            Err(PagerVmaError::Stale)
        );
        assert_eq!(
            rewrite_attenuated_range(
                handle,
                identity,
                published.start,
                published.start + PAGER_PAGE_BYTES,
                None,
                || Ok(()),
            ),
            Ok(true)
        );
    }

    #[test]
    fn per_process_vma_bound_admits_product_envelope_and_rejects_the_next_region() {
        let _guard = guard();
        let handle = ProcessHandle::new(26, 71);
        let identity = identity(71, 73);
        let mut published = [PagerVmRegionWire::default(); MAX_PAGER_VMAS_PER_PROCESS];
        for (index, slot) in published.iter_mut().enumerate() {
            let start = 0x40_000 + (index as u64) * PAGER_PAGE_BYTES * 2;
            *slot = publish(handle, identity, template(start)).unwrap();
        }
        assert_eq!(
            publish(
                handle,
                identity,
                template(0x40_000 + (MAX_PAGER_VMAS_PER_PROCESS as u64) * PAGER_PAGE_BYTES * 2),
            ),
            Err(PagerVmaError::Pressure)
        );
        for region in published {
            revoke(handle, identity, region.start, region.vma_generation).unwrap();
        }
    }

    #[test]
    fn exec_generation_change_and_revoked_region_never_match() {
        let _guard = guard();
        let handle = ProcessHandle::new(27, 61);
        let exact_identity = identity(61, 67);
        let published = publish(handle, exact_identity, template(0xc000)).unwrap();
        assert_eq!(
            lookup(handle, identity(61, 68), 0xc000, VM_ACCESS_READ),
            Err(PagerVmaError::Stale)
        );
        assert_eq!(
            revoke(
                handle,
                exact_identity,
                published.start,
                published.vma_generation + 1,
            ),
            Err(PagerVmaError::Stale)
        );
        assert_eq!(
            lookup(handle, exact_identity, 0xc000, VM_ACCESS_READ),
            Ok(published)
        );
        revoke(
            handle,
            exact_identity,
            published.start,
            published.vma_generation,
        )
        .unwrap();
        assert_eq!(
            lookup(handle, exact_identity, 0xc000, VM_ACCESS_READ),
            Err(PagerVmaError::Stale)
        );
    }

    #[test]
    fn target_process_publication_is_generation_bound_and_revocable() {
        let _guard = guard();
        let _isolation = super::super::process_table::tests::isolate_process_table();
        let handle = super::super::process_table::create_process(4_242, process_state())
            .expect("test process handle");
        let identity = super::super::process_table::live_process_identity(handle)
            .expect("test process identity");

        // This unit owns only the lock-free VMA publication protocol.  The
        // normal-time facade prepares real page-table leaves first, while this
        // test deliberately uses an empty host-test address space.
        let published = publish(handle, identity, template(0x1_0000)).expect("target publication");
        assert_eq!(
            published.process_generation,
            u64::from(identity.process_generation())
        );
        assert_eq!(published.mm_generation, u64::from(identity.mm_generation()));
        assert_eq!(
            lookup(handle, identity, 0x1_0000, VM_ACCESS_READ),
            Ok(published)
        );
        assert_eq!(
            super::super::revoke_pager_vma_for_process(
                4_242,
                published.start,
                published.vma_generation,
            ),
            Ok(published)
        );
        assert_eq!(
            lookup(handle, identity, 0x1_0000, VM_ACCESS_READ),
            Err(PagerVmaError::Stale)
        );
    }

    /// A wide template, so an edit can land strictly inside it and split.
    fn wide_template(start: u64, pages: u64) -> PagerVmRegionWire {
        PagerVmRegionWire {
            end: start + PAGER_PAGE_BYTES * pages,
            ..template(start)
        }
    }

    fn published_spans(
        handle: ProcessHandle,
        identity: ProcessIdentity,
    ) -> alloc::vec::Vec<(u64, u64, u64)> {
        let mut spans: alloc::vec::Vec<(u64, u64, u64)> = process_slots(handle)
            .unwrap()
            .iter()
            .filter_map(|slot| slot.snapshot().unwrap())
            .filter(|region| {
                region.process_generation == u64::from(identity.process_generation())
                    && region.mm_generation == u64::from(identity.mm_generation())
            })
            .map(|region| (region.start, region.end, region.object_offset))
            .collect();
        spans.sort_unstable();
        spans
    }

    fn rule_spans(
        region: PagerVmRegionWire,
        edit: PagerRangeEdit,
    ) -> alloc::vec::Vec<(u64, u64, u64)> {
        let (fragments, len) = apply_region_edit(region, edit).fragments();
        let mut spans: alloc::vec::Vec<(u64, u64, u64)> = fragments[..len]
            .iter()
            .map(|fragment| (fragment.start, fragment.end, fragment.object_offset))
            .collect();
        spans.sort_unstable();
        spans
    }

    /// Ring0's rewrite must equal the shared ABI rule exactly.
    ///
    /// This is one half of the replica binding: pagerd is tested against the
    /// same rule, so proving each side equals it proves the two sides agree.
    /// They previously derived their own remainders, and an interior `munmap`
    /// left ring0 holding two mappings that pagerd had deleted - so the next
    /// fault in a surviving remainder passed ring0's VMA check, matched no
    /// pagerd region, and killed the thread.
    #[test]
    fn ring0_rewrite_matches_the_shared_range_edit_rule() {
        let _guard = guard();
        let handle = ProcessHandle::new(25, 79);
        let identity = identity(79, 83);
        let base = 0x8_0000;
        for (edit_start, edit_end) in [
            (base, base + PAGER_PAGE_BYTES), // trim head
            (base + PAGER_PAGE_BYTES * 3, base + PAGER_PAGE_BYTES * 4), // trim tail
            (base + PAGER_PAGE_BYTES, base + PAGER_PAGE_BYTES * 2), // interior split
            (base, base + PAGER_PAGE_BYTES * 4), // full remove
        ] {
            let published = publish(handle, identity, wide_template(base, 4)).unwrap();
            assert_eq!(
                rewrite_attenuated_range(handle, identity, edit_start, edit_end, None, || Ok(())),
                Ok(true)
            );
            assert_eq!(
                published_spans(handle, identity),
                rule_spans(published, PagerRangeEdit::unmap(edit_start, edit_end)),
                "unmap {edit_start:#x}..{edit_end:#x} must equal the shared rule"
            );
            for (start, end, _) in published_spans(handle, identity) {
                assert_eq!(
                    rewrite_attenuated_range(handle, identity, start, end, None, || Ok(())),
                    Ok(true)
                );
            }
            assert!(published_spans(handle, identity).is_empty());
        }
    }

    /// An interior unmap costs exactly one extra VMA slot, and no more - the
    /// capacity claim `PAGER_MAX_REGION_GROWTH_PER_UNMAP` states and that
    /// pagerd's table is sized against.
    #[test]
    fn an_interior_unmap_costs_exactly_one_extra_vma_slot() {
        let _guard = guard();
        let handle = ProcessHandle::new(24, 89);
        let identity = identity(89, 97);
        let base = 0x9_0000;
        let published = publish(handle, identity, wide_template(base, 4)).unwrap();
        let before = published_spans(handle, identity).len();
        assert_eq!(
            rewrite_attenuated_range(
                handle,
                identity,
                base + PAGER_PAGE_BYTES,
                base + PAGER_PAGE_BYTES * 2,
                None,
                || Ok(()),
            ),
            Ok(true)
        );
        let after = published_spans(handle, identity).len();
        assert_eq!(after - before, PAGER_MAX_REGION_GROWTH_PER_UNMAP);
        assert!(after - before <= PAGER_MAX_REGION_GROWTH_PER_PROTECT);
        // Every surviving page still faults, and the removed one does not.
        assert!(lookup(handle, identity, base, VM_ACCESS_WRITE).is_ok());
        assert!(
            lookup(
                handle,
                identity,
                base + PAGER_PAGE_BYTES * 2,
                VM_ACCESS_WRITE
            )
            .is_ok()
        );
        assert_eq!(
            lookup(handle, identity, base + PAGER_PAGE_BYTES, VM_ACCESS_WRITE),
            Err(PagerVmaError::Stale)
        );
        let _ = published;
        for (start, end, _) in published_spans(handle, identity) {
            assert_eq!(
                rewrite_attenuated_range(handle, identity, start, end, None, || Ok(())),
                Ok(true)
            );
        }
    }

    /// A split with no free VMA slot must refuse before it withdraws anything.
    /// Withdrawing first and failing to republish would lose a live mapping.
    #[test]
    fn a_split_with_no_free_vma_slot_refuses_and_keeps_every_region() {
        let _guard = guard();
        let handle = ProcessHandle::new(23, 101);
        let identity = identity(101, 103);
        let base = 0xa_0000;
        let mut published = alloc::vec::Vec::new();
        for index in 0..MAX_PAGER_VMAS_PER_PROCESS as u64 {
            let start = base + index * PAGER_PAGE_BYTES * 4;
            published.push(publish(handle, identity, wide_template(start, 3)).unwrap());
        }
        let before = published_spans(handle, identity);
        assert_eq!(before.len(), MAX_PAGER_VMAS_PER_PROCESS);
        // Every slot is taken, so an interior split has nowhere to put its
        // second fragment. It must refuse with `Pressure` and leave the whole
        // table exactly as it was; the mutation closure must never run.
        let mut mutated = false;
        assert_eq!(
            rewrite_attenuated_range(
                handle,
                identity,
                base + PAGER_PAGE_BYTES,
                base + PAGER_PAGE_BYTES * 2,
                None,
                || {
                    mutated = true;
                    Ok(())
                },
            ),
            Err(PagerVmaError::Pressure)
        );
        assert!(!mutated, "no PTE may change before the split is admitted");
        assert_eq!(published_spans(handle, identity), before);
        for region in published {
            revoke(handle, identity, region.start, region.vma_generation).unwrap();
        }
    }
}
