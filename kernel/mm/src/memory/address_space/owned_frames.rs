//! Batched physical-frame settlement for address-space transactions.
//!
//! - **Owner:** `kernel-mm` owns exact process-frame ledger settlement.
//! - **Boundary:** callers provide only frames already removed from the private
//!   address-space ledger after the required page-table mutation and shootdown.
//! - **Lifecycle:** collect exact owned identities → return bounded batches →
//!   fail closed on any allocator ownership rejection.
//! - **Concurrency:** each bounded allocator call owns one IRQ-off allocator
//!   critical section; diagnostics run only after that section ends.
//! - **Failure:** rollback and committed unmap reject invalid or duplicate
//!   frame return as fatal ownership corruption; synthetic teardown is the
//!   explicitly best-effort diagnostic boundary.
//! - **Forbidden:** no scalar per-page allocator locking for a settled batch,
//!   no fabricated success, and no frame return before translation revocation.
//! - **Evidence:** `physical-frame-lifecycle`,
//!   `process-lifecycle-transaction`, and
//!   `large_unmap_settles_owned_frames_in_bounded_allocator_lock_batches`.

use alloc::vec::Vec;
use x86_64::PhysAddr;

use super::{AddressSpaceError, PAGE_4KIB_U64};
use crate::memory::phys;

pub(super) const FRAME_BATCH_CHUNK: usize = 64;

pub(super) fn free_owned_frames_exact(frames: &[u64]) {
    settle_owned_frame_chunks(frames, |batch| {
        let mut failures =
            [(PhysAddr::new(0), phys::FreeFrameError::AlreadyFree); FRAME_BATCH_CHUNK];
        phys::try_free_frames_batch(batch, &mut failures)
    });
}

fn settle_owned_frame_chunks(frames: &[u64], mut free_batch: impl FnMut(&[PhysAddr]) -> usize) {
    let mut batch = [PhysAddr::new(0); FRAME_BATCH_CHUNK];
    for chunk in frames.chunks(FRAME_BATCH_CHUNK) {
        for (destination, frame) in batch.iter_mut().zip(chunk) {
            *destination = PhysAddr::new(*frame);
        }
        let failed = free_batch(&batch[..chunk.len()]);
        assert_eq!(failed, 0, "unmap rejected an exactly owned frame batch");
    }
}

/// Frees a process's owned frames under `PHYS_ALLOCATOR` in chunks instead of
/// one lock acquisition per frame, discarding every failure. Synthetic address
/// spaces have no privileged root to attribute a log line to.
pub(super) fn free_owned_frames_silently(frames: impl Iterator<Item = u64>) {
    let mut batch = [PhysAddr::new(0); FRAME_BATCH_CHUNK];
    let mut failures = [(PhysAddr::new(0), phys::FreeFrameError::AlreadyFree); FRAME_BATCH_CHUNK];
    let mut batch_len = 0;
    for frame_phys in frames {
        if frame_phys == 0 || frame_phys % PAGE_4KIB_U64 != 0 {
            continue;
        }
        batch[batch_len] = PhysAddr::new(frame_phys);
        batch_len += 1;
        if batch_len == FRAME_BATCH_CHUNK {
            phys::try_free_frames_batch(&batch[..batch_len], &mut failures);
            batch_len = 0;
        }
    }
    if batch_len != 0 {
        phys::try_free_frames_batch(&batch[..batch_len], &mut failures);
    }
}

/// Frees process-owned frames in bounded chunks and reports every rejected
/// frame only after the allocator lock and IRQ-off region have ended.
pub(super) fn free_owned_frames_logged(pml4_frame_phys: u64, frames: impl Iterator<Item = u64>) {
    let mut batch = [PhysAddr::new(0); FRAME_BATCH_CHUNK];
    let mut failures = [(PhysAddr::new(0), phys::FreeFrameError::AlreadyFree); FRAME_BATCH_CHUNK];
    let mut batch_len = 0;
    for frame_phys in frames {
        if frame_phys == 0 || frame_phys % PAGE_4KIB_U64 != 0 {
            crate::debug::println!(
                "process address space: skipping invalid owned frame root={:#x} frame={:#x}",
                pml4_frame_phys,
                frame_phys,
            );
            continue;
        }
        batch[batch_len] = PhysAddr::new(frame_phys);
        batch_len += 1;
        if batch_len == FRAME_BATCH_CHUNK {
            report_free_failures(pml4_frame_phys, &batch, batch_len, &mut failures);
            batch_len = 0;
        }
    }
    if batch_len != 0 {
        report_free_failures(pml4_frame_phys, &batch, batch_len, &mut failures);
    }
}

fn report_free_failures(
    pml4_frame_phys: u64,
    batch: &[PhysAddr; FRAME_BATCH_CHUNK],
    batch_len: usize,
    failures: &mut [(PhysAddr, phys::FreeFrameError); FRAME_BATCH_CHUNK],
) {
    let failed = phys::try_free_frames_batch(&batch[..batch_len], failures);
    for &(failed_phys, err) in &failures[..failed.min(failures.len())] {
        crate::debug::println!(
            "process address space: frame cleanup rejected root={:#x} frame={:#x} err={:?}",
            pml4_frame_phys,
            failed_phys.as_u64(),
            err,
        );
    }
}

pub(super) fn free_frame_buffer_tail(frames: &[PhysAddr]) {
    free_rollback_frames_exact(frames.iter().map(|frame| frame.as_u64()));
}

/// Returns frames owned by a failed unpublished map transaction in bounded
/// chunks. Any rejection is an ownership invariant failure.
pub(super) fn free_rollback_frames_exact(frames: impl Iterator<Item = u64>) {
    let mut batch = [PhysAddr::new(0); FRAME_BATCH_CHUNK];
    let mut failures = [(PhysAddr::new(0), phys::FreeFrameError::AlreadyFree); FRAME_BATCH_CHUNK];
    let mut batch_len = 0;
    for frame_phys in frames {
        batch[batch_len] = PhysAddr::new(frame_phys);
        batch_len += 1;
        if batch_len == FRAME_BATCH_CHUNK {
            let failed = phys::try_free_frames_batch_rollback(&batch[..batch_len], &mut failures);
            assert_eq!(failed, 0, "map rollback rejected an owned frame batch");
            batch_len = 0;
        }
    }
    if batch_len != 0 {
        let failed = phys::try_free_frames_batch_rollback(&batch[..batch_len], &mut failures);
        assert_eq!(failed, 0, "map rollback rejected an owned frame batch");
    }
}

pub(super) fn remove_owned_frame(
    owned_frames: &mut Vec<u64>,
    frame_phys: u64,
) -> Result<(), AddressSpaceError> {
    let Some(position) = owned_frames.iter().position(|owned| *owned == frame_phys) else {
        return Err(AddressSpaceError::NotMapped);
    };
    owned_frames.swap_remove(position);
    Ok(())
}

pub(super) fn track_owned_frame(
    owned_frames: &mut Vec<u64>,
    frame_phys: u64,
) -> Result<(), AddressSpaceError> {
    if frame_phys == 0 || !frame_phys.is_multiple_of(PAGE_4KIB_U64) {
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    if owned_frames.contains(&frame_phys) {
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    owned_frames.push(frame_phys);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_unmap_settles_owned_frames_in_bounded_allocator_lock_batches() {
        let frames: Vec<u64> = (1..=129).map(|index| index * PAGE_4KIB_U64).collect();
        let mut batch_lengths = Vec::new();
        settle_owned_frame_chunks(&frames, |batch| {
            batch_lengths.push(batch.len());
            0
        });
        assert_eq!(batch_lengths, [64, 64, 1]);
    }
}
