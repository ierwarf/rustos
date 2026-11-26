//! Contiguous anonymous ownership claims and their preallocated split tables.
//!
//! A huge leaf still owns one ordinary descriptor per 4 KiB frame. The run
//! operations amortize charge and census atomics without inventing compound
//! frame ownership. The head's otherwise-unused exclusive backing word holds
//! a root-owned page table, making demotion independent of memory pressure.

use super::*;
use crate::memory::phys::ANON_HUGE_PAGE_COUNT;

impl FrameDescriptorLedger {
    fn exclusive_run_records(
        &self,
        root_phys: u64,
        va: u64,
        first_frame: u64,
        page_count: usize,
    ) -> Option<&[FrameDescriptorRecord]> {
        if page_count == 0 || page_count > ANON_HUGE_PAGE_COUNT
            || !valid_mapping(root_phys, va, first_frame)
            || !first_frame.is_multiple_of(PAGE_SIZE)
        {
            return None;
        }
        let last_offset = (page_count as u64 - 1) * PAGE_SIZE;
        va.checked_add(last_offset)?;
        let last_frame = first_frame.checked_add(last_offset)?;
        if (first_frame..=last_frame).contains(&root_phys) {
            return None;
        }
        let first_index = usize::try_from(first_frame / PAGE_SIZE).ok()?;
        let end = first_index.checked_add(page_count)?;
        let frame_count = self.frame_count.load(DESCRIPTOR_ACQUIRE);
        let records = self.records.load(DESCRIPTOR_ACQUIRE);
        if records.is_null() || end > frame_count {
            return None;
        }
        // SAFETY: bootstrap permanently reserves the descriptor array; the
        // entire aligned physical run is inside it and its fields are atomic.
        Some(unsafe { core::slice::from_raw_parts(records.add(first_index), page_count) })
    }

    pub(super) fn claim_exclusive_data_run(
        &self,
        root_phys: u64,
        va: u64,
        first_frame: u64,
        page_count: usize,
    ) -> DataClaim {
        let Some(root) = self.verify_root(root_phys) else {
            return DataClaim::Invalid;
        };
        let Some(frames) = self.exclusive_run_records(root_phys, va, first_frame, page_count) else {
            return DataClaim::Invalid;
        };
        if !self.try_charge_root(root_phys, page_count as u64) {
            return DataClaim::Quota;
        }
        for (index, frame) in frames.iter().enumerate() {
            if frame.role.compare_exchange(
                ROLE_FREE,
                ROLE_EXCLUSIVE_DATA_CLAIMED,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE,
            ).is_err() {
                // Only the successfully claimed prefix belongs to this run.
                // Its cancellation must not individually refund the one run
                // charge, nor touch the conflicting descriptor.
                for undo in 0..index {
                    let offset = undo as u64 * PAGE_SIZE;
                    self.cancel_exclusive_data(root_phys, va + offset, first_frame + offset, false);
                }
                self.uncharge_root(root, page_count as u64);
                return DataClaim::Invalid;
            }
            frame.owner_or_table_head.store(root_phys, Ordering::Relaxed);
            frame.next_or_virtual_address.store(va + index as u64 * PAGE_SIZE, Ordering::Relaxed);
            debug_assert_eq!(frame.shared_backing_identity.load(Ordering::Relaxed), 0);
        }
        DataClaim::Claimed
    }

    pub(super) fn publish_exclusive_data_run(
        &self,
        root_phys: u64,
        va: u64,
        first_frame: u64,
        page_count: usize,
    ) {
        let root = self.verify_root(root_phys).expect("missing data-leaf run root");
        let frames = self.exclusive_run_records(root_phys, va, first_frame, page_count)
            .expect("invalid data-leaf publication run");
        root.data_leaf_count_or_map_head.fetch_add(page_count as u64, DESCRIPTOR_ACQ_REL);
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame.owner_or_table_head.load(Ordering::Relaxed), root_phys);
            assert_eq!(frame.next_or_virtual_address.load(Ordering::Relaxed), va + index as u64 * PAGE_SIZE);
            assert_eq!(frame.role.compare_exchange(
                ROLE_EXCLUSIVE_DATA_CLAIMED,
                ROLE_EXCLUSIVE_DATA_LIVE,
                DESCRIPTOR_RELEASE,
                DESCRIPTOR_ACQUIRE,
            ), Ok(ROLE_EXCLUSIVE_DATA_CLAIMED), "data-leaf run lost its descriptor claim");
        }
    }

    pub(super) fn cancel_exclusive_data_run(
        &self,
        root_phys: u64,
        va: u64,
        first_frame: u64,
        page_count: usize,
    ) {
        let root = self.verify_root(root_phys).expect("missing cancelled data-leaf run root");
        self.exclusive_run_records(root_phys, va, first_frame, page_count)
            .expect("invalid cancelled data-leaf run");
        for index in 0..page_count {
            let offset = index as u64 * PAGE_SIZE;
            self.cancel_exclusive_data(root_phys, va + offset, first_frame + offset, false);
        }
        self.uncharge_root(root, page_count as u64);
    }

    fn anon_huge_head(&self, root_phys: u64, va: u64, first_frame: u64) -> Option<&FrameDescriptorRecord> {
        let huge_bytes = PAGE_SIZE * ANON_HUGE_PAGE_COUNT as u64;
        if !va.is_multiple_of(huge_bytes) || !first_frame.is_multiple_of(huge_bytes)
            || !valid_mapping(root_phys, va, first_frame)
        {
            return None;
        }
        self.verify_root(root_phys)?;
        let frame = self.record(first_frame)?;
        if !matches!(frame.role.load(DESCRIPTOR_ACQUIRE), ROLE_EXCLUSIVE_DATA_CLAIMED | ROLE_EXCLUSIVE_DATA_LIVE)
            || frame.owner_or_table_head.load(Ordering::Relaxed) != root_phys
            || frame.next_or_virtual_address.load(Ordering::Relaxed) != va
        {
            return None;
        }
        Some(frame)
    }

    /// Caller holds the exact VMA install permit or root mutation ownership.
    /// The table descriptor is claimed before the PDE CAS; the winning caller
    /// publishes it into the lazy-table list before releasing its permit.
    /// A loser detaches this association and cancels the unpublished claims.
    pub(super) fn deposit_anon_huge_table(&self, root_phys: u64, va: u64, first_frame: u64, table_phys: u64) -> bool {
        let Some(frame) = self.anon_huge_head(root_phys, va, first_frame) else { return false; };
        if !self.table_is_owned(root_phys, table_phys) {
            return false;
        }
        frame.shared_backing_identity.compare_exchange(
            0, table_phys, DESCRIPTOR_RELEASE, DESCRIPTOR_ACQUIRE,
        ).is_ok()
    }

    /// Lookup does not detach the table. The caller pins the root and holds
    /// the same read/mutation ownership used to inspect its huge PDE.
    pub(super) fn anon_huge_table(&self, root_phys: u64, va: u64, first_frame: u64) -> Option<u64> {
        let frame = self.anon_huge_head(root_phys, va, first_frame)?;
        let table = frame.shared_backing_identity.load(DESCRIPTOR_ACQUIRE);
        self.table_is_owned(root_phys, table).then_some(table)
    }

    /// Detaches only the head association. The table remains root-owned and
    /// is either installed as a PT or reclaimed with the root's table list.
    pub(super) fn take_anon_huge_table(&self, root_phys: u64, va: u64, first_frame: u64) -> Option<u64> {
        let frame = self.anon_huge_head(root_phys, va, first_frame)?;
        let table = frame.shared_backing_identity.load(DESCRIPTOR_ACQUIRE);
        if !self.table_is_owned(root_phys, table) {
            return None;
        }
        frame.shared_backing_identity.compare_exchange(
            table, 0, DESCRIPTOR_ACQ_REL, DESCRIPTOR_ACQUIRE,
        ).ok().map(|_| table)
    }
}
