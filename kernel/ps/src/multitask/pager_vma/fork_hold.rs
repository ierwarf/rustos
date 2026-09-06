//! Linux fork VMA publication hold.
//!
//! - **Owner:** `kernel-ps` owns the whole-process VMA snapshot and hold.
//! - **Boundary:** Only the exact process/MM generation may enter the hold.
//! - **Lifecycle:** Snapshot, publish every slot odd, drain installers, run the
//!   address-space callback, then restore every original even sequence.
//! - **Concurrency:** The per-process writer serializes the set and establishes
//!   process state -> VMA writer -> installer drain -> TLB -> descriptor order.
//! - **Failure:** Partial hold acquisition unwinds in reverse and calls no body.
//! - **Forbidden:** The callback cannot recursively acquire a VMA writer.
//! - **Evidence:** `fork_hold_makes_fault_readers_retry_without_publishing_a_hole`.

use super::*;

/// Holds every live region at an odd publication sequence while `f` runs.
///
/// `lookup` answers "which VMA covers this address"; fork needs the whole set,
/// because what a child inherits is the reservation, not the pages the parent
/// happened to have touched. The writer lock makes the set one reading rather
/// than a scan racing an `mmap` in another thread of the same process.
///
/// The input is bounded by the fixed per-process slot count. Residue left by a
/// previous occupant is skipped by the same identity comparison `lookup_slot`
/// uses, so it is never inherited.
pub(in crate::multitask) fn with_fork_held_regions<R>(
    handle: ProcessHandle,
    identity: ProcessIdentity,
    f: impl FnOnce(&[PagerVmRegionWire]) -> R,
) -> Result<R, PagerVmaError> {
    if handle.generation() != identity.process_generation() {
        return Err(PagerVmaError::Stale);
    }
    let process = handle.object_identity().ok_or(PagerVmaError::Stale)?;
    let _writer = writer_lock(handle).ok_or(PagerVmaError::Stale)?.lock();
    let slots = process_slots(handle).ok_or(PagerVmaError::Stale)?;
    let mut regions = Vec::new();
    regions
        .try_reserve_exact(MAX_PAGER_VMAS_PER_PROCESS)
        .map_err(|_| PagerVmaError::Pressure)?;
    let mut slot_indices = Vec::new();
    slot_indices
        .try_reserve_exact(MAX_PAGER_VMAS_PER_PROCESS)
        .map_err(|_| PagerVmaError::Pressure)?;
    for (slot_index, slot) in slots.iter().enumerate() {
        let Some(region) = slot.snapshot()? else {
            continue;
        };
        if region_matches_identity(region, process.slot(), identity) {
            regions.push(region);
            slot_indices.push(slot_index);
        }
    }

    let mut held = Vec::new();
    held.try_reserve_exact(slot_indices.len())
        .map_err(|_| PagerVmaError::Pressure)?;
    for (position, slot_index) in slot_indices.iter().copied().enumerate() {
        match slots[slot_index].begin_fork_hold(regions[position]) {
            Ok(before) => held.push((slot_index, before)),
            Err(error) => {
                for (held_index, before) in held.iter().rev().copied() {
                    slots[held_index].finish_fork_hold(before);
                }
                return Err(error);
            }
        }
    }
    let result = f(&regions);
    for (slot_index, before) in held.iter().rev().copied() {
        slots[slot_index].finish_fork_hold(before);
    }
    Ok(result)
}
