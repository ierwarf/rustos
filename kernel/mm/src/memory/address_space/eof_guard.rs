//! End-of-file guard leaves for external (memfd-backed) shared mappings.
//!
//! - **Owner:** `kernel-mm` owns the leaf encoding and the PTE mutation;
//!   `kernel-ps` owns which pages of a mapping are inside the backing object
//!   (`user/memfd.rs`) and calls here with that exact frame list.
//! - **Boundary:** a *guard leaf* is a present, `USER_INACCESSIBLE_LEAF`,
//!   no-execute leaf whose frame is the single process-wide guard frame. It
//!   marks "this page belongs to a mapping but lies beyond the end of its
//!   file". Hardware raises a protection fault on any user access, checked
//!   user copies refuse it, and `translate_user` still reports the page as
//!   occupied, so overlap checks and `munmap` keep treating the range as one
//!   mapping.
//! - **Lifecycle:** install at map time → rewritten in either direction by
//!   [`ProcessAddressSpace::reconcile_external_user_pages`] when the file
//!   shrinks (real leaf → guard leaf, TLB shootdown completes before return)
//!   or grows (guard leaf → real leaf) → removed by `munmap` as an external
//!   segment like any memfd leaf. The guard frame is allocated once and never
//!   freed, never written and never exposed to a process.
//! - **Concurrency:** callers hold the process-state lock that serializes this
//!   address space; the fault path only *reads* the leaf, lock-free, through
//!   [`current_user_leaf_is_eof_guard`]. A guard leaf is never created or
//!   removed from exception context.
//! - **Failure:** a frame list that cannot be encoded, or a page whose
//!   existing leaf is address-space-owned, fails before any leaf changes for
//!   that page; a leaf already rewritten stays valid (each page is either the
//!   real frame or the guard).
//! - **Forbidden:** treating a guard leaf as data (copying, ledger claims,
//!   `mprotect` widening) or freeing the guard frame.
//! - **Evidence:** `memory-map`, `tlb-shootdown-lifecycle`.
use core::sync::atomic::{AtomicU64, Ordering};

use super::*;

/// Physical address of the one shared guard frame, or zero before first use.
static EOF_GUARD_FRAME: AtomicU64 = AtomicU64::new(0);

/// Flags of every guard leaf: present, inaccessible to ring 3, never executable.
fn guard_leaf_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | USER_INACCESSIBLE_LEAF | PageTableFlags::NO_EXECUTE
}

/// Returns the guard frame, allocating and zeroing it on first use. `None`
/// means the allocator refused; callers fail the mapping, never fall back to
/// an accessible page.
pub(crate) fn eof_guard_frame() -> Option<u64> {
    // ORDERING: Acquire pairs with the Release publication below so the zeroed
    // frame contents are visible to any CPU that reads the address.
    let existing = EOF_GUARD_FRAME.load(Ordering::Acquire);
    if existing != 0 {
        return Some(existing);
    }
    let frame = phys::alloc_frame_user()?;
    // SAFETY: a freshly allocated, exclusively owned, direct-mapped 4 KiB frame.
    unsafe {
        ptr::write_bytes(higher_half_ptr(frame) as *mut u8, 0, PAGE_4KIB);
    }
    match EOF_GUARD_FRAME.compare_exchange(
        0,
        frame.as_u64(),
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => Some(frame.as_u64()),
        Err(winner) => {
            // Lost the publication race; our frame was never published.
            phys::free_frame(frame);
            Some(winner)
        }
    }
}

/// Whether `flags`/`frame` describe a guard leaf. A leaf is a guard only if it
/// names the guard frame, so a `PROT_NONE` page of real file data never
/// matches.
pub(crate) fn is_guard_leaf(frame: u64, flags: PageTableFlags) -> bool {
    let guard = EOF_GUARD_FRAME.load(Ordering::Acquire);
    guard != 0
        && frame == guard
        && flags.contains(PageTableFlags::PRESENT | USER_INACCESSIBLE_LEAF)
        && !flags.contains(PageTableFlags::USER_ACCESSIBLE)
}

/// Exception-time, lock-free: whether the current address space maps `virt`
/// with a guard leaf. The page fault handler uses it to report a bus error
/// (the access was beyond the end of a mapped file) instead of a segmentation
/// fault. Reads only; a concurrent rewrite is observed as either leaf.
pub fn current_user_leaf_is_eof_guard(virt: u64) -> bool {
    if EOF_GUARD_FRAME.load(Ordering::Acquire) == 0 || !is_user_addr(virt) {
        return false;
    }
    let virt = VirtAddr::new(virt & !(PAGE_4KIB_U64 - 1));
    let root_phys = kernel_vm::current_root_phys();
    // SAFETY: the current root is live on this CPU for the whole exception;
    // tables are only freed at address-space retirement.
    let root = unsafe { kernel_vm::phys_to_table_ref(root_phys) };
    let pml4 = &root[p4_index(virt)];
    if pml4.is_unused() || pml4.flags().contains(PageTableFlags::HUGE_PAGE) {
        return false;
    }
    // SAFETY: present non-huge entry names a live user table frame.
    let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4.addr()) };
    let pdpt_entry = &pdpt[p3_index(virt)];
    if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return false;
    }
    // SAFETY: as above.
    let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
    let pd_entry = &pd[p2_index(virt)];
    if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return false;
    }
    // SAFETY: as above.
    let pt = unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) };
    let leaf = &pt[p1_index(virt)];
    !leaf.is_unused() && is_guard_leaf(leaf.addr().as_u64(), leaf.flags())
}

impl ProcessAddressSpace {
    /// Maps `page_count` pages at `start`: the first `frames.len()` to the
    /// given external frames with `flags`, the rest as guard leaves. Every page
    /// must be vacant; a refusal leaves nothing mapped.
    pub fn map_external_user_pages_with_eof_guard(
        &mut self,
        start: VirtAddr,
        frames: &[u64],
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, page_count)?;
        if frames.len() > page_count {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        for index in 0..page_count {
            if self.translate_user(page_addr(start, index)?).is_some() {
                return Err(AddressSpaceError::AlreadyMapped);
            }
        }
        self.install_external_pages(start, frames, page_count, flags, true)
            .map(|_| ())
    }

    /// Rewrites the leaves of `[start, start + page_count)` so page `i` maps
    /// `frames[i]` when it exists and the guard leaf otherwise. Pages that
    /// already match are untouched. Returns how many leaves changed. Every
    /// removed or replaced leaf is shot down on all CPUs before this returns,
    /// which is what lets the caller release truncated backing afterwards.
    pub fn reconcile_external_user_pages(
        &mut self,
        start: VirtAddr,
        frames: &[u64],
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<usize, AddressSpaceError> {
        validate_user_page_range(start, page_count)?;
        if frames.len() > page_count {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        self.install_external_pages(start, frames, page_count, flags, false)
    }

    fn install_external_pages(
        &mut self,
        start: VirtAddr,
        frames: &[u64],
        page_count: usize,
        flags: PageTableFlags,
        fresh: bool,
    ) -> Result<usize, AddressSpaceError> {
        let addressable_limit = crate::memory::cache_attributes::max_physical_address();
        if frames
            .iter()
            .any(|&frame| !external_frame_is_encodable(frame, addressable_limit))
        {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        let real_flags = normalize_user_page_flags(flags)?;
        let guard_frame = if frames.len() < page_count {
            Some(eof_guard_frame().ok_or(AddressSpaceError::OutOfFrames)?)
        } else {
            EOF_GUARD_FRAME
                .load(Ordering::Acquire)
                .ne(&0)
                .then(|| EOF_GUARD_FRAME.load(Ordering::Acquire))
        };
        let guard_flags = normalize_user_page_flags(guard_leaf_flags())?;
        let mut touched: Vec<VirtAddr> = Vec::new();
        touched
            .try_reserve_exact(page_count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mutation = begin_address_space_mutation(self.root_phys());
        let mut changed = 0usize;
        let mut failure = None;
        for index in 0..page_count {
            let virt = page_addr(start, index)?;
            let (desired, desired_flags) = match frames.get(index) {
                Some(&frame) => (frame, real_flags),
                None => (
                    guard_frame.expect("guard frame exists when a page is past the frames"),
                    guard_flags,
                ),
            };
            match self.translate_user_with_flags(virt) {
                Some((current, current_flags)) => {
                    if fresh {
                        failure = Some(AddressSpaceError::AlreadyMapped);
                        break;
                    }
                    if current_flags.contains(ROOT_OWNED_USER_LEAF) {
                        failure = Some(AddressSpaceError::InvalidFrameOwnership);
                        break;
                    }
                    if current.as_u64() == desired {
                        continue;
                    }
                    if self.unmap_user_page(virt).is_none() {
                        failure = Some(AddressSpaceError::NotMapped);
                        break;
                    }
                }
                None => {}
            }
            if let Err(error) = self.map_user_page(virt, PhysAddr::new(desired), desired_flags) {
                failure = Some(error);
                break;
            }
            touched.push(virt);
            changed += 1;
        }
        if let Some(error) = failure {
            if fresh {
                rollback_external_user_pages(self, &touched, mutation);
            } else {
                let _flushed = mutation.flush_for_reclaim();
            }
            return Err(error);
        }
        let _flushed = mutation.flush_for_reclaim();
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_guard_frame_with_inaccessible_flags_is_a_guard_leaf() {
        EOF_GUARD_FRAME.store(0x7000, Ordering::Release);
        let guard = guard_leaf_flags();
        assert!(is_guard_leaf(0x7000, guard));
        // PROT_NONE of real file data shares the flags but not the frame.
        assert!(!is_guard_leaf(0x8000, guard));
        // An accessible leaf on the guard frame is not a guard.
        assert!(!is_guard_leaf(
            0x7000,
            PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE
        ));
        EOF_GUARD_FRAME.store(0, Ordering::Release);
        assert!(!is_guard_leaf(0x7000, guard));
    }

    #[test]
    fn guard_leaf_flags_survive_user_flag_normalization() {
        let flags = normalize_user_page_flags(guard_leaf_flags()).unwrap();
        assert!(flags.contains(PageTableFlags::PRESENT | USER_INACCESSIBLE_LEAF));
        assert!(!flags.contains(PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE));
        assert!(flags.contains(PageTableFlags::NO_EXECUTE));
    }
}
