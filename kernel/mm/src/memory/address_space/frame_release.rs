//! Batched physical-frame release after address-space revocation.
//!
//! Callers provide frames whose exact descriptor role and PTE reachability
//! were already settled. Each allocator call owns one bounded IRQ-off critical
//! section; no frame is returned before translation invalidation completes.
//!
//! - **Owner:** `kernel-mm` owns post-revocation frame release.
//! - **Boundary:** callers supply only descriptor-settled physical frames.
//! - **Lifecycle:** removed PTE -> acknowledged TLB -> bounded allocator batch.
//! - **Concurrency:** each batch takes one IRQ-off allocator critical section.
//! - **Failure:** exact rollback/unmap rejection is fatal; retirement logs it.
//! - **Forbidden:** no scalar allocator locking or pre-invalidation release.
//! - **Evidence:** `physical-frame-lifecycle` and
//!   `large_unmap_settles_frames_in_bounded_allocator_lock_batches`.

use x86_64::PhysAddr;

use super::PAGE_4KIB_U64;
use crate::memory::phys;

pub(super) const FRAME_BATCH_CHUNK: usize = 64;

pub(super) fn free_removed_frames_exact(frames: &[u64]) {
    settle_frame_chunks(frames, |batch| {
        let mut failures =
            [(PhysAddr::new(0), phys::FreeFrameError::AlreadyFree); FRAME_BATCH_CHUNK];
        phys::try_free_frames_batch(batch, &mut failures)
    });
}

fn settle_frame_chunks(frames: &[u64], mut free_batch: impl FnMut(&[PhysAddr]) -> usize) {
    let mut batch = [PhysAddr::new(0); FRAME_BATCH_CHUNK];
    for chunk in frames.chunks(FRAME_BATCH_CHUNK) {
        for (destination, frame) in batch.iter_mut().zip(chunk) {
            *destination = PhysAddr::new(*frame);
        }
        let failed = free_batch(&batch[..chunk.len()]);
        assert_eq!(failed, 0, "unmap rejected an exactly owned frame batch");
    }
}

/// Frees retired frames in bounded chunks and reports allocator rejections
/// only after the allocator lock and IRQ-off region have ended.
pub(super) fn free_retired_frames_logged(pml4_frame_phys: u64, frames: impl Iterator<Item = u64>) {
    let mut batch = [PhysAddr::new(0); FRAME_BATCH_CHUNK];
    let mut failures = [(PhysAddr::new(0), phys::FreeFrameError::AlreadyFree); FRAME_BATCH_CHUNK];
    let mut batch_len = 0;
    for frame_phys in frames {
        if frame_phys == 0 || frame_phys % PAGE_4KIB_U64 != 0 {
            crate::debug::println!(
                "process address space: skipping invalid retired frame root={:#x} frame={:#x}",
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

/// Returns frames from a failed unpublished map transaction in bounded chunks.
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn large_unmap_settles_frames_in_bounded_allocator_lock_batches() {
        let frames: Vec<u64> = (1..=129).map(|index| index * PAGE_4KIB_U64).collect();
        let mut batch_lengths = Vec::new();
        settle_frame_chunks(&frames, |batch| {
            batch_lengths.push(batch.len());
            0
        });
        assert_eq!(batch_lengths, [64, 64, 1]);
    }
}
