//! Anonymous 2 MiB leaves with ordinary 4 KiB frame ownership.
//!
//! - **Owner:** `kernel-mm` owns PDE huge-leaf encoding, 512-frame ownership
//!   claims, and pre-deposited split tables.
//! - **Boundary:** Operates only on pagerd-authorized 2 MiB private anonymous
//!   VMAs pinned under the fault-install permit or VMA mutation guard.
//! - **Lifecycle:** Allocates/claims 512 zeroed frames + split table, installs
//!   via single absent-to-present CAS, demotes via deposited table, frees both
//!   on retirement.
//! - **Concurrency:** Concurrent installers race on absent PDE CAS (losers
//!   return claims); demotion completes TLB shootdown before subpage changes.
//! - **Failure:** CAS loss or decode/deposit mismatch fails closed and returns
//!   every unpublished frame claim without partial PTE state.
//! - **Forbidden:** Compound non-4-KiB frame descriptors, demotion allocation,
//!   COW huge leaves, or missing shootdown before split-table mutation.
//! - **Evidence:** `PageTableMapTransaction` and `kernel-mm` huge-leaf tests.

use core::sync::atomic::Ordering;

use super::pager_fault_mapping::{read_user_directory_entry, with_user_table_entry_atomic};
use super::*;

pub const ANON_HUGE_BYTES: u64 = 2 * 1024 * 1024;
const HUGE_PAGES: usize = phys::ANON_HUGE_PAGE_COUNT;
const HUGE_ADDR_MASK: u64 = 0x000f_ffff_ffe0_0000;
const HUGE_PAT: u64 = 1 << 12;
const AD_BITS: u64 = PageTableFlags::ACCESSED.bits() | PageTableFlags::DIRTY.bits();

#[derive(Clone, Copy)]
pub(super) struct HugeLeaf {
    pub(super) head: u64,
    /// Equivalent 4 KiB flags; bit 7 is PAT here, never PS.
    pub(super) page_flags: PageTableFlags,
}

/// Decode only the anonymous leaf encoding owned by this module. Directory
/// frames must never be interpreted as data and unsupported huge leaves fail
/// closed rather than supplying a forged frame identity to a lifetime walk.
pub(super) fn decode_huge_leaf(raw: u64) -> Result<HugeLeaf, AddressSpaceError> {
    let flags = PageTableFlags::from_bits_truncate(raw);
    if !flags.contains(PageTableFlags::PRESENT | PageTableFlags::HUGE_PAGE | ROOT_OWNED_USER_LEAF)
        || flags.intersects(COW_USER_LEAF | PageTableFlags::GLOBAL)
        || raw & 0x001f_e000 != 0
        || raw & HUGE_ADDR_MASK == 0
        || (flags.contains(PageTableFlags::WRITABLE) && !flags.contains(PageTableFlags::NO_EXECUTE))
    {
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    // A huge leaf is initially user accessible. Demotion also accepts a future
    // retained PROT_NONE leaf, but never an unexplained supervisor mapping.
    if !flags.contains(PageTableFlags::USER_ACCESSIBLE)
        && !flags.contains(USER_INACCESSIBLE_LEAF)
    {
        return Err(AddressSpaceError::ProtectionViolation);
    }
    let mut page_flags = flags.difference(PageTableFlags::HUGE_PAGE);
    if raw & HUGE_PAT != 0 {
        page_flags |= PageTableFlags::HUGE_PAGE;
    }
    Ok(HugeLeaf { head: raw & HUGE_ADDR_MASK, page_flags })
}

pub(super) fn huge_leaf_at(root: PhysAddr, virt: VirtAddr) -> Result<Option<HugeLeaf>, AddressSpaceError> {
    let Some(pdpt) = read_user_table_entry(root, p4_index(virt))? else { return Ok(None); };
    let Some(pd) = read_user_table_entry(pdpt, p3_index(virt))? else { return Ok(None); };
    let raw = read_user_directory_entry(pd, p2_index(virt));
    if raw & PageTableFlags::HUGE_PAGE.bits() == 0 {
        return Ok(None);
    }
    decode_huge_leaf(raw).map(Some)
}

/// Attempts a true 2 MiB anonymous PDE after its two upper directories exist.
/// The caller holds the exact VMA install permit covering the whole block and
/// enforces pagerd's huge-page policy. No existing table/leaf is replaced.
/// `Some(511)` installed a new block; `Some(0)` observed a competing matching
/// block. `None` leaves the ordinary 64 KiB/4 KiB path available.
pub fn try_map_current_anon_huge_at(
    start: VirtAddr,
    flags: PageTableFlags,
    mut supply: impl FnMut() -> Option<u64>,
    mut release: impl FnMut(u64),
) -> Option<u64> {
    if !start.as_u64().is_multiple_of(ANON_HUGE_BYTES) {
        return None;
    }
    validate_user_page_range(start, HUGE_PAGES).ok()?;
    let flags = normalize_user_page_flags(flags).ok()? | ROOT_OWNED_USER_LEAF;
    if !flags.contains(PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE)
        || flags.intersects(COW_USER_LEAF | USER_INACCESSIBLE_LEAF | PageTableFlags::GLOBAL)
    {
        return None;
    }
    let root = kernel_vm::current_root_phys();
    let root_phys = root.as_u64();
    let base = start.as_u64();
    let pdpt = read_user_table_entry(root, p4_index(start)).ok()??;
    let pd = read_user_table_entry(pdpt, p3_index(start)).ok()??;
    let matches = |raw| {
        decode_huge_leaf(raw).ok().is_some_and(|leaf| {
            leaf.page_flags.difference(PageTableFlags::ACCESSED | PageTableFlags::DIRTY) == flags
        })
    };
    let observed = read_user_directory_entry(pd, p2_index(start));
    if observed != 0 {
        return matches(observed).then_some(0);
    }
    if !phys::root_admits(root_phys, HUGE_PAGES as u64) {
        return None;
    }

    // Reserve the future split table up front. It remains unreachable by the
    // hardware while deposited; its existing frame-descriptor record carries
    // custody without an exception-time allocation of metadata.
    let table = supply()?;
    if table == 0 { return None; }
    if !table.is_multiple_of(PAGE_4KIB_U64) {
        release(table);
        return None;
    }
    if phys::claim_lazy_table_record(root_phys, table) != phys::TableClaim::Claimed {
        release(table);
        return None;
    }
    let Some(head) = phys::try_alloc_anon_huge_frame() else {
        phys::cancel_lazy_table_record(root_phys, table);
        release(table);
        return None;
    };
    if phys::claim_data_leaf_run(root_phys, base, head.as_u64(), HUGE_PAGES) != phys::DataClaim::Claimed {
        free_huge_run(head.as_u64());
        phys::cancel_lazy_table_record(root_phys, table);
        release(table);
        return None;
    }
    assert!(phys::deposit_anon_huge_table(root_phys, base, head.as_u64(), table));
    // SAFETY: this aligned run is exclusively allocated and descriptor-claimed;
    // no user translation reaches it before the publishing CAS below.
    unsafe { ptr::write_bytes(higher_half_ptr(head), 0, ANON_HUGE_BYTES as usize); }
    let desired = head.as_u64() | flags.bits() | PageTableFlags::HUGE_PAGE.bits();
    let installed = with_user_table_entry_atomic(pd, p2_index(start), |entry| {
        // ORDERING: release publishes the zeroed data and deposit identity;
        // acquire on failure observes a competing directory/huge publication.
        entry.compare_exchange(0, desired, Ordering::AcqRel, Ordering::Acquire)
    });
    match installed {
        Ok(_) => {
            // The VMA permit excludes teardown until both ledgers are live.
            phys::publish_lazy_table_record(root_phys, table);
            phys::publish_data_leaf_run(root_phys, base, head.as_u64(), HUGE_PAGES);
            Some(HUGE_PAGES as u64 - 1)
        }
        Err(winner) => {
            assert_eq!(phys::take_anon_huge_table(root_phys, base, head.as_u64()), Some(table));
            phys::cancel_data_leaf_run(root_phys, base, head.as_u64(), HUGE_PAGES);
            free_huge_run(head.as_u64());
            phys::cancel_lazy_table_record(root_phys, table);
            release(table);
            matches(winner).then_some(0)
        }
    }
}

fn free_huge_run(head: u64) {
    // Small bounded stack batches also preserve the allocator's per-frame
    // ownership validation; no 512-entry failure buffer lives on the IRQ stack.
    let mut frames = [PhysAddr::new(0); FRAME_BATCH_CHUNK];
    let mut failures = [(PhysAddr::new(0), phys::FreeFrameError::AlreadyFree); FRAME_BATCH_CHUNK];
    let mut done = 0;
    while done < HUGE_PAGES {
        let count = (HUGE_PAGES - done).min(FRAME_BATCH_CHUNK);
        for (offset, frame) in frames[..count].iter_mut().enumerate() {
            *frame = PhysAddr::new(head + (done + offset) as u64 * PAGE_4KIB_U64);
        }
        assert_eq!(phys::try_free_frames_batch(&frames[..count], &mut failures), 0);
        done += count;
    }
}

fn prepare_split_table(pt: &mut PageTable, leaf: HugeLeaf) {
    // A CPU can still set huge A/D until the shootdown. Conservatively retain
    // both for every child; the final flush ends the mixed-size interval.
    let flags = leaf.page_flags | PageTableFlags::ACCESSED | PageTableFlags::DIRTY;
    for (offset, entry) in pt.iter_mut().enumerate() {
        entry.set_addr(PhysAddr::new(leaf.head + offset as u64 * PAGE_4KIB_U64), flags);
    }
}

impl ProcessAddressSpace {
    /// Restore ordinary PTE topology before a 4 KiB-granular mutation. The
    /// caller holds VMA writer/fork authority, excluding fault publication for
    /// these blocks. Deposits make the operation allocation-free even at OOM.
    pub(super) fn demote_user_huge_pages(&self, start: VirtAddr, page_count: usize) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, page_count)?;
        let end = start.as_u64() + page_count as u64 * PAGE_4KIB_U64;
        let root = self.root_phys();
        let mut cursor = start.as_u64() & !(ANON_HUGE_BYTES - 1);
        // Acquire lazily: ordinary 4 KiB ranges add no TLB lock or shootdown.
        let mut mutation: Option<AddressSpaceMutationGuard> = None;
        while cursor < end {
            let virt = VirtAddr::new(cursor);
            let Some(pdpt) = read_user_table_entry(root, p4_index(virt))? else {
                cursor = ((cursor >> 39) + 1) << 39;
                continue;
            };
            let Some(pd) = read_user_table_entry(pdpt, p3_index(virt))? else {
                cursor = ((cursor >> 30) + 1) << 30;
                continue;
            };
            let index = p2_index(virt);
            let raw = read_user_directory_entry(pd, index);
            if raw & PageTableFlags::HUGE_PAGE.bits() != 0 {
                if mutation.is_none() {
                    mutation = Some(begin_address_space_mutation(root));
                }
                // Re-read under the protocol; only hardware A/D bits can
                // change while the caller holds this VMA's writer authority.
                let raw = read_user_directory_entry(pd, index);
                let leaf = decode_huge_leaf(raw)?;
                let table = phys::anon_huge_table(self.pml4_frame_phys, cursor, leaf.head)
                    .ok_or(AddressSpaceError::InvalidFrameOwnership)?;
                // Preserve A/D conservatively: a CPU may still use the huge
                // translation until the final shootdown, including a write
                // racing the PDE replacement. Marking every child A/D avoids
                // losing that hardware state without guessing a subpage.
                // SAFETY: the exact head descriptor holds this root-owned
                // deposited table, which has never been hardware-reachable.
                let pt = unsafe { kernel_vm::phys_to_table_mut(PhysAddr::new(table)) };
                prepare_split_table(pt, leaf);
                let desired = table | (user_table_flags() | ROOT_OWNED_USER_TABLE).bits();
                with_user_table_entry_atomic(pd, index, |entry| {
                    let mut current = raw;
                    loop {
                        assert_eq!(current & !AD_BITS, raw & !AD_BITS, "huge split lost VMA ownership");
                        // ORDERING: release publishes all 512 initialized PTEs;
                        // acquire refreshes hardware A/D updates on CAS retry.
                        match entry.compare_exchange_weak(current, desired, Ordering::AcqRel, Ordering::Acquire) {
                            Ok(_) => break,
                            Err(observed) => current = observed,
                        }
                    }
                });
                assert_eq!(phys::take_anon_huge_table(self.pml4_frame_phys, cursor, leaf.head), Some(table));
            }
            cursor += ANON_HUGE_BYTES;
        }
        // Includes the entire old huge translation on every relevant CPU/PCID.
        // No caller may change a subpage or release a frame before this ends.
        drop(mutation);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn huge(head: u64) -> u64 {
        head | (PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE
            | PageTableFlags::HUGE_PAGE | ROOT_OWNED_USER_LEAF).bits()
    }

    #[test]
    fn huge_decode_preserves_head_and_converts_pat_without_offsetting_backing() {
        let raw = huge(0x200000) | HUGE_PAT;
        let leaf = decode_huge_leaf(raw).unwrap();
        assert_eq!(leaf.head, 0x200000);
        assert!(leaf.page_flags.contains(PageTableFlags::HUGE_PAGE));
        assert!(!decode_huge_leaf(huge(0x200000)).unwrap().page_flags.contains(PageTableFlags::HUGE_PAGE));
    }

    #[test]
    fn split_preserves_all_512_frame_identities_permissions_and_pat() {
        for pat in [0, HUGE_PAT] {
            let leaf = decode_huge_leaf(huge(0x400000) | pat).unwrap();
            let mut table = PageTable::new();
            prepare_split_table(&mut table, leaf);
            for (index, entry) in table.iter().enumerate() {
                assert_eq!(entry.addr().as_u64(), 0x400000 + index as u64 * PAGE_4KIB_U64);
                assert!(entry.flags().contains(PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE
                    | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE | ROOT_OWNED_USER_LEAF
                    | PageTableFlags::ACCESSED | PageTableFlags::DIRTY));
                assert_eq!(entry.flags().contains(PageTableFlags::HUGE_PAGE), pat != 0);
                assert!(!entry.flags().intersects(COW_USER_LEAF | USER_INACCESSIBLE_LEAF));
            }
            assert_eq!(table[511].addr().as_u64() + PAGE_4KIB_U64, 0x600000);
        }
    }

    #[test]
    fn huge_decode_rejects_nonowned_misaligned_global_and_cow_leaves() {
        for raw in [
            huge(0), huge(0x200000) | (1 << 13),
            huge(0x200000) & !ROOT_OWNED_USER_LEAF.bits(),
            huge(0x200000) | COW_USER_LEAF.bits(),
            huge(0x200000) | PageTableFlags::GLOBAL.bits(),
            huge(0x200000) & !PageTableFlags::PRESENT.bits(),
            huge(0x200000) & !PageTableFlags::NO_EXECUTE.bits(),
        ] {
            assert!(decode_huge_leaf(raw).is_err());
        }
    }
}
