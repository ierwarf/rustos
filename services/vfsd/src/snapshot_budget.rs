//! Ring3 admission for materialized executable snapshots, independent of caches.
//!
//! The reservation covers final memfd frames, conservative frame-vector growth,
//! the pipeline's window ring and owned mapping metadata. It lives through a pending
//! handle reply, including asynchronous results; cancellation/error releases it.
//! The small sealed-image cache has its own existing explicit budget. This is
//! service-owned staging admission, not a claim of complete kernel kmem billing.
use core::sync::atomic::{AtomicUsize, Ordering};
use crate::snapshot_pipeline::RingGeometry;
use rustos_user_abi::syscall::EXECUTABLE_IMAGE_TRANSFER_WINDOW_BYTES;

/// vfsd's own snapshot read/write window. It is larger than the shared
/// `EXECUTABLE_IMAGE_TRANSFER_WINDOW_BYTES` (loaderd's ELF read window and the
/// early-system broker ceiling) so one volume read is big enough to be split
/// across the parallel storaged lanes (`bulk_plan::PARALLEL_MIN_BYTES`).
pub const SNAPSHOT_WINDOW_BYTES: usize = 4 * 1024 * 1024;
/// Windows in the snapshot pipeline's ring: one being read, one being written
/// or hashed, and one slack so a stage that finishes early never idles the
/// reader. The ring is allocated once per snapshot and billed in full.
pub const SNAPSHOT_RING_WINDOWS: usize = 3;
const _: () = assert!(SNAPSHOT_WINDOW_BYTES >= EXECUTABLE_IMAGE_TRANSFER_WINDOW_BYTES);
pub const SNAPSHOT_MAP_BYTES: usize = 2 * 1024 * 1024;
pub const SNAPSHOT_IN_FLIGHT_BYTES: usize = 640 * 1024 * 1024;

/// The pipeline's ring geometry for a snapshot of `file_bytes`; `None` for an
/// empty file, which the pipeline rejects.
pub fn snapshot_ring_geometry(file_bytes: u64) -> Option<RingGeometry> {
    RingGeometry::new(file_bytes, SNAPSHOT_WINDOW_BYTES, SNAPSHOT_RING_WINDOWS)
}

pub fn snapshot_reservation_bytes(file_bytes: u64) -> Option<usize> {
    let bytes = usize::try_from(file_bytes).ok()?;
    let pages = bytes.checked_add(4095)? / 4096;
    let ring = match snapshot_ring_geometry(file_bytes) {
        Some(geometry) => geometry.buffer_bytes()?,
        None => 0,
    };
    pages.checked_mul(4096 + 16)?
        .checked_add(ring)?
        .checked_add(SNAPSHOT_MAP_BYTES)?
        .checked_add(128 * 1024)
}

pub struct SnapshotBudget {
    used: AtomicUsize,
    limit: usize,
}

impl SnapshotBudget {
    pub const fn new(limit: usize) -> Self { Self { used: AtomicUsize::new(0), limit } }
    pub fn used(&self) -> usize { self.used.load(Ordering::Acquire) }
    pub fn reserve(&self, bytes: usize) -> Option<SnapshotReservation<'_>> {
        if bytes == 0 { return None; }
        self.used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(bytes).filter(|total| *total <= self.limit)
        }).ok()?;
        Some(SnapshotReservation { budget: self, bytes })
    }
}

pub struct SnapshotReservation<'a> { budget: &'a SnapshotBudget, bytes: usize }
impl Drop for SnapshotReservation<'_> {
    fn drop(&mut self) {
        let prior = self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(prior >= self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_budget_reserves_final_frames_and_refunds_every_exit() {
        let budget = SnapshotBudget::new(SNAPSHOT_IN_FLIGHT_BYTES);
        let cost = snapshot_reservation_bytes(306_789_704).unwrap();
        assert!(cost > 306_789_704);
        let a = budget.reserve(cost).unwrap();
        let b = budget.reserve(cost).unwrap();
        assert!(budget.reserve(cost).is_none());
        drop(a);
        assert_eq!(budget.used(), cost);
        drop(b);
        assert_eq!(budget.used(), 0);
        assert!(snapshot_reservation_bytes(u64::MAX).is_none());
        // The ring is billed in full: three windows for a large image, only
        // the file itself for a tiny one, nothing for an empty one.
        let overhead = snapshot_reservation_bytes(0).unwrap();
        let large = snapshot_reservation_bytes(306_789_704).unwrap();
        assert_eq!(large - 74_900 * (4096 + 16) - overhead,
            SNAPSHOT_RING_WINDOWS * SNAPSHOT_WINDOW_BYTES);
        assert_eq!(snapshot_reservation_bytes(100).unwrap() - (4096 + 16) - overhead, 100);
        assert!(budget.reserve(usize::MAX).is_none());
        assert_eq!(budget.used(), 0);
    }
}
