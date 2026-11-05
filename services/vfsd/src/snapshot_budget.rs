//! Ring3 admission for materialized executable snapshots, independent of caches.
//!
//! The reservation covers final memfd frames, conservative frame-vector growth,
//! transfer scratch and owned mapping metadata. It lives through a pending
//! handle reply, including asynchronous results; cancellation/error releases it.
//! The small sealed-image cache has its own existing explicit budget. This is
//! service-owned staging admission, not a claim of complete kernel kmem billing.
use core::sync::atomic::{AtomicUsize, Ordering};

pub const SNAPSHOT_WINDOW_BYTES: usize = 256 * 1024;
pub const SNAPSHOT_MAP_BYTES: usize = 2 * 1024 * 1024;
pub const SNAPSHOT_IN_FLIGHT_BYTES: usize = 640 * 1024 * 1024;

pub fn snapshot_reservation_bytes(file_bytes: u64) -> Option<usize> {
    let bytes = usize::try_from(file_bytes).ok()?;
    let pages = bytes.checked_add(4095)? / 4096;
    pages.checked_mul(4096 + 16)?
        .checked_add(SNAPSHOT_WINDOW_BYTES)?
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
        assert!(budget.reserve(usize::MAX).is_none());
        assert_eq!(budget.used(), 0);
    }
}
