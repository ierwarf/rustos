//! Transactional user address-space and page-table ownership.
//!
//! - **Owner:** `kernel-mm` owns PTE mutation; `syscalld` owns mapping policy
//!   and `kernel-ps` owns process-generation lifetime.
//! - **Boundary:** User ranges, protections, backing frames, and replacement
//!   plans are untrusted until complete-span admission.
//! - **Lifecycle:** Reserve/validate the full plan, install atomically, retain
//!   backing, then unmap/protect/reclaim exactly once.
//! - **Concurrency:** Callers serialize process address-space mutation and hold
//!   an exact process generation; no service call occurs under page-table
//!   mutation state.
//! - **Failure:** Overflow, alias, W+X, partial span, and allocation failure
//!   leave the prior mapping unchanged.
//! - **Forbidden:** No destructive `MAP_FIXED` pre-cleanup, partial protection,
//!   guest pointer as frame authority, or hidden identity mapping.
//! - **Evidence:** `memory-map` and `user-memory-access`.
use alloc::vec::Vec;
use core::ptr;

use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::instructions::{interrupts, tlb};
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{PageTable, PageTableFlags};

use kernel_hal::api::arch::tlb::{AddressSpaceMutationGuard, begin_address_space_mutation};

mod atomic_user;
mod frame_release;
mod pager_fault_mapping;
mod retirement;
mod rollback;
mod user_copy;
use frame_release::{
    FRAME_BATCH_CHUNK, free_frame_buffer_tail, free_removed_frames_exact,
    free_retired_frames_logged, free_rollback_frames_exact,
};
use pager_fault_mapping::{
    COW_USER_LEAF, ROOT_OWNED_USER_LEAF, ROOT_OWNED_USER_TABLE, publish_user_table_entry,
    read_user_table_entry, without_root_owned_user_leaf_tag,
};
pub use pager_fault_mapping::{
    CowWriteResult, ensure_current_fault_tables_at, map_current_prepared_pager_fault_frame_at,
    resolve_current_cow_write_at,
};
use rollback::{rollback_external_user_pages, rollback_user_pages};
pub use user_copy::{ValidatedUserRead, ValidatedUserWrite};

use crate::memory::{kernel_vm, phys};

const ENTRIES_PER_TABLE: usize = 512;
const PAGE_4KIB: usize = 4096;
const PAGE_4KIB_U64: u64 = PAGE_4KIB as u64;
const USER_PML4_INDEX: usize = 1;
pub const USER_SPACE_BASE: u64 = (USER_PML4_INDEX as u64) << 39;
pub const USER_SPACE_END_EXCLUSIVE: u64 = ((USER_PML4_INDEX + 1) as u64) << 39;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PagerFaultLeaf {
    virtual_address: u64,
    physical_address: u64,
}

/// The tagged frames a retirement walk reconciles with physical descriptors.
///
/// Every address-space-owned data leaf has both a PTE tag and an exact
/// `(root, virtual_address)` descriptor. Every dynamically created user table,
/// from either normal-time mapping or fault entry, has the directory tag and a
/// root-owned descriptor-list entry.
#[derive(Debug)]
pub(crate) struct PagerFaultOwnership {
    leaves: Vec<PagerFaultLeaf>,
    tables: Vec<u64>,
}

impl PagerFaultOwnership {
    const fn empty() -> Self {
        Self {
            leaves: Vec::new(),
            tables: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressSpaceError {
    ZeroSizedAllocation,
    AddressOverflow,
    AddressOutOfRange,
    AddressNotPageAligned,
    AlreadyMapped,
    NotMapped,
    ProtectionViolation,
    HugePageConflict,
    OutOfFrames,
    InvalidFrameOwnership,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserRegion {
    pub start: VirtAddr,
    pub page_count: usize,
}

#[derive(Clone, Copy)]
struct ClonedLeaf {
    virtual_address: VirtAddr,
    child_frame_phys: u64,
    shared_alias: Option<phys::SharedAliasTicket>,
}

impl UserRegion {
    pub fn len_bytes(&self) -> usize {
        self.page_count.saturating_mul(PAGE_4KIB)
    }

    pub fn end(&self) -> VirtAddr {
        VirtAddr::new(self.start.as_u64() + self.len_bytes() as u64)
    }
}

#[derive(Debug)]
pub struct ProcessAddressSpace {
    pml4_frame_phys: u64,
    next_user_addr: u64,
}

impl ProcessAddressSpace {
    #[inline(never)]
    pub fn new() -> Result<Self, AddressSpaceError> {
        let pml4_phys = phys::alloc_frame().ok_or(AddressSpaceError::OutOfFrames)?;
        phys::register_lazy_table_root(pml4_phys.as_u64());
        phys::register_data_leaf_root(pml4_phys.as_u64());
        let root = unsafe { kernel_vm::phys_to_table_mut(pml4_phys) };
        root.zero();

        interrupts::without_interrupts(|| {
            let kernel_pml4 = kernel_vm::KERNEL_PML4.lock();
            for index in 0..ENTRIES_PER_TABLE {
                let src = &kernel_pml4.pml4[index];
                let dst = &mut root[index];
                if src.is_unused() {
                    dst.set_unused();
                } else {
                    dst.set_addr(src.addr(), src.flags());
                }
            }
        });

        Ok(Self {
            pml4_frame_phys: pml4_phys.as_u64(),
            next_user_addr: USER_SPACE_BASE,
        })
    }

    pub fn root_phys(&self) -> PhysAddr {
        PhysAddr::new(self.pml4_frame_phys)
    }

    #[cfg(any(test, debug_assertions))]
    pub fn empty_for_tests() -> Self {
        Self {
            pml4_frame_phys: 0,
            next_user_addr: USER_SPACE_BASE,
        }
    }

    /// Clones resident leaves, sharing the ranges admitted for anonymous fork
    /// COW and eagerly copying every excluded mapping class.
    ///
    /// The caller holds the parent's VMA publication in an odd-sequence fork
    /// hold, so no new fault installer can enter while the parent PTEs are
    /// downgraded. Existing private-file COW leaves retain their descriptor
    /// kind and backing identity. `eager_private_ranges` names writable pages
    /// that the kernel must mutate before child publication, such as libc's
    /// `CLONE_CHILD_SETTID` word; those pages are copied and made writable
    /// instead of being shared. The child root is unpublished. One mutation
    /// guard batches all parent write protection into one exact shootdown.
    pub fn clone_user_space_cow(
        &mut self,
        cow_ranges: &[UserRegion],
        eager_private_ranges: &[UserRegion],
    ) -> Result<Self, AddressSpaceError> {
        let mut cloned = Self::new()?;
        cloned.next_user_addr = self.next_user_addr;
        let ownership = self.pager_fault_ownership()?;
        let mut cloned_leaves = Vec::new();
        cloned_leaves
            .try_reserve_exact(ownership.leaves.len())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mut downgraded = Vec::new();
        downgraded
            .try_reserve_exact(ownership.leaves.len())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mutation = begin_address_space_mutation(self.root_phys());

        for leaf in ownership.leaves {
            let virt = VirtAddr::new(leaf.virtual_address);
            let Some((src_phys, flags)) = self.translate_user_with_flags(virt) else {
                rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                drop(mutation);
                return Err(AddressSpaceError::NotMapped);
            };
            if src_phys.as_u64() != leaf.physical_address
                || !phys::data_leaf_is_owned(
                    self.pml4_frame_phys,
                    leaf.virtual_address,
                    leaf.physical_address,
                )
            {
                rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                drop(mutation);
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }

            let eager_private = user_regions_contain(eager_private_ranges, virt);
            let inherited_cow = if flags.contains(COW_USER_LEAF) {
                match phys::data_leaf_cow_identity(
                    self.pml4_frame_phys,
                    leaf.virtual_address,
                    leaf.physical_address,
                ) {
                    Some(identity) => Some(identity),
                    None => {
                        rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                        drop(mutation);
                        return Err(AddressSpaceError::InvalidFrameOwnership);
                    }
                }
            } else {
                None
            };
            let shared_identity = if eager_private {
                None
            } else {
                inherited_cow.or_else(|| {
                    user_regions_contain(cow_ranges, virt)
                        .then_some((phys::CowFrameKind::AnonymousFork, 0))
                })
            };

            if let Some((kind, backing_identity)) = shared_identity {
                let Some(ticket) = phys::prepare_shared_alias(
                    self.pml4_frame_phys,
                    leaf.virtual_address,
                    cloned.pml4_frame_phys,
                    leaf.virtual_address,
                    leaf.physical_address,
                    kind,
                    backing_identity,
                ) else {
                    rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                    drop(mutation);
                    return Err(AddressSpaceError::OutOfFrames);
                };
                if let Err(error) =
                    cloned.map_shared_cow_leaf_without_mutation(virt, leaf.physical_address, flags)
                {
                    phys::cancel_shared_alias(ticket);
                    rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                    drop(mutation);
                    return Err(error);
                }
                phys::publish_shared_alias(ticket);
                cloned_leaves.push(ClonedLeaf {
                    virtual_address: virt,
                    child_frame_phys: leaf.physical_address,
                    shared_alias: Some(ticket),
                });
                continue;
            }

            let Some(dst_phys) = phys::alloc_frame() else {
                rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                drop(mutation);
                return Err(AddressSpaceError::OutOfFrames);
            };
            let child_root = cloned.pml4_frame_phys;
            if !phys::claim_data_leaf(child_root, leaf.virtual_address, dst_phys.as_u64()) {
                phys::free_frame(dst_phys);
                rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                drop(mutation);
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
            unsafe {
                ptr::copy_nonoverlapping(
                    higher_half_ptr(src_phys),
                    higher_half_ptr(dst_phys),
                    PAGE_4KIB,
                );
            }
            let inherited_flags = without_root_owned_user_leaf_tag(flags);
            let child_source_flags = if eager_private {
                inherited_flags
                    .difference(COW_USER_LEAF)
                    .union(PageTableFlags::WRITABLE)
            } else {
                inherited_flags
            };
            let child_flags = match normalize_user_page_flags(child_source_flags) {
                Ok(flags) => flags | ROOT_OWNED_USER_LEAF,
                Err(error) => {
                    phys::cancel_data_leaf(child_root, leaf.virtual_address, dst_phys.as_u64());
                    phys::free_frame(dst_phys);
                    rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                    drop(mutation);
                    return Err(error);
                }
            };
            if let Err(error) = cloned.map_user_page(virt, dst_phys, child_flags) {
                phys::cancel_data_leaf(child_root, leaf.virtual_address, dst_phys.as_u64());
                phys::free_frame(dst_phys);
                rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                drop(mutation);
                return Err(error);
            }
            phys::publish_data_leaf(child_root, leaf.virtual_address, dst_phys.as_u64());
            cloned_leaves.push(ClonedLeaf {
                virtual_address: virt,
                child_frame_phys: dst_phys.as_u64(),
                shared_alias: None,
            });
        }

        for leaf in cloned_leaves
            .iter()
            .filter(|leaf| leaf.shared_alias.is_some())
        {
            let old_flags = match self.user_page_flags(leaf.virtual_address) {
                Ok(flags) => flags,
                Err(error) => {
                    restore_parent_flags(self, &downgraded);
                    rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                    drop(mutation);
                    return Err(error);
                }
            };
            let cow_flags = old_flags
                .difference(PageTableFlags::WRITABLE)
                .union(ROOT_OWNED_USER_LEAF | COW_USER_LEAF);
            if let Err(error) =
                self.set_user_page_flags_exact_without_flush(leaf.virtual_address, cow_flags)
            {
                restore_parent_flags(self, &downgraded);
                rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                drop(mutation);
                return Err(error);
            }
            downgraded.push((leaf.virtual_address, old_flags));
        }

        // Drop performs the one parent-root shootdown and waits for every CPU
        // that could retain a writable translation before the child can later
        // become runnable.
        drop(mutation);
        Ok(cloned)
    }

    pub fn alloc_user_bytes(
        &mut self,
        byte_len: usize,
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        let page_count = byte_len_to_page_count(byte_len)?;
        self.alloc_user_pages(page_count, flags)
    }

    pub fn alloc_user_pages(
        &mut self,
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }

        let start = align_up(self.next_user_addr, PAGE_4KIB_U64)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let region = self.map_zeroed_user_pages_at(VirtAddr::new(start), page_count, flags)?;
        self.next_user_addr = region.end().as_u64();
        Ok(region)
    }

    pub fn map_zeroed_user_bytes_at(
        &mut self,
        start: VirtAddr,
        byte_len: usize,
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        let page_count = byte_len_to_page_count(byte_len)?;
        self.map_zeroed_user_pages_at(start, page_count, flags)
    }

    pub fn map_zeroed_user_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        validate_user_page_range(start, page_count)?;

        let page_flags = normalize_user_page_flags(flags)? | ROOT_OWNED_USER_LEAF;
        let mut mapped_pages = Vec::new();
        mapped_pages
            .try_reserve_exact(page_count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mutation = begin_address_space_mutation(self.root_phys());

        let mut frame_buffer = [PhysAddr::new(0); FRAME_BATCH_CHUNK];
        let mut frame_buffer_len = 0usize;
        let mut frame_buffer_pos = 0usize;

        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            if self.translate_user(virt).is_some() {
                free_frame_buffer_tail(&frame_buffer[frame_buffer_pos..frame_buffer_len]);
                rollback_user_pages(self, &mapped_pages, mutation);
                return Err(AddressSpaceError::AlreadyMapped);
            }

            if frame_buffer_pos == frame_buffer_len {
                let want = (page_count - page_index).min(FRAME_BATCH_CHUNK);
                frame_buffer_len = phys::alloc_frames_batch(&mut frame_buffer[..want]);
                frame_buffer_pos = 0;
                if frame_buffer_len == 0 {
                    rollback_user_pages(self, &mapped_pages, mutation);
                    return Err(AddressSpaceError::OutOfFrames);
                }
            }
            let frame_phys = frame_buffer[frame_buffer_pos];
            frame_buffer_pos += 1;

            unsafe {
                ptr::write_bytes(higher_half_ptr(frame_phys), 0, PAGE_4KIB);
            }

            let virtual_address = virt.as_u64();
            if !phys::claim_data_leaf(self.pml4_frame_phys, virtual_address, frame_phys.as_u64()) {
                phys::free_frame(frame_phys);
                free_frame_buffer_tail(&frame_buffer[frame_buffer_pos..frame_buffer_len]);
                rollback_user_pages(self, &mapped_pages, mutation);
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
            if let Err(err) = self.map_user_page(virt, frame_phys, page_flags) {
                phys::cancel_data_leaf(self.pml4_frame_phys, virtual_address, frame_phys.as_u64());
                phys::free_frame(frame_phys);
                free_frame_buffer_tail(&frame_buffer[frame_buffer_pos..frame_buffer_len]);
                rollback_user_pages(self, &mapped_pages, mutation);
                return Err(err);
            }
            phys::publish_data_leaf(self.pml4_frame_phys, virtual_address, frame_phys.as_u64());
            mapped_pages.push((virt, frame_phys.as_u64()));
        }

        Ok(UserRegion { start, page_count })
    }

    pub fn map_existing_user_pages_at(
        &mut self,
        start: VirtAddr,
        frames: &[u64],
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        self.map_existing_user_pages_at_with_leaf_flags(
            start,
            frames,
            flags,
            PageTableFlags::empty(),
        )
    }

    /// Map page-aligned pre-owned frames with the x86 4-KiB PAT selector.
    /// The caller must ensure every alias of these external frames uses the
    /// same write-combine memory type.
    pub fn map_existing_user_pages_at_write_combine(
        &mut self,
        start: VirtAddr,
        frames: &[u64],
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        self.map_existing_user_pages_at_with_leaf_flags(
            start,
            frames,
            flags,
            PageTableFlags::HUGE_PAGE,
        )
    }

    fn map_existing_user_pages_at_with_leaf_flags(
        &mut self,
        start: VirtAddr,
        frames: &[u64],
        flags: PageTableFlags,
        leaf_flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        if frames.is_empty() {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }

        validate_user_page_range(start, frames.len())?;
        let page_flags = normalize_user_page_flags(flags)? | leaf_flags;
        let mut mapped_pages = Vec::new();
        mapped_pages
            .try_reserve_exact(frames.len())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mutation = begin_address_space_mutation(self.root_phys());

        for (page_index, frame_phys) in frames.iter().copied().enumerate() {
            let virt = page_addr(start, page_index)?;
            if self.translate_user(virt).is_some() {
                rollback_external_user_pages(self, &mapped_pages, mutation);
                return Err(AddressSpaceError::AlreadyMapped);
            }
            if let Err(err) = self.map_user_page(virt, PhysAddr::new(frame_phys), page_flags) {
                rollback_external_user_pages(self, &mapped_pages, mutation);
                return Err(err);
            }
            mapped_pages.push(virt);
        }

        Ok(UserRegion {
            start,
            page_count: frames.len(),
        })
    }

    pub fn unmap_user_bytes(
        &mut self,
        start: VirtAddr,
        byte_len: usize,
    ) -> Result<usize, AddressSpaceError> {
        let page_count = byte_len_to_page_count(byte_len)?;
        self.unmap_user_pages_at(start, page_count)
    }

    pub fn unmap_user_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<usize, AddressSpaceError> {
        let frames = plan_user_page_unmap(start, page_count, |virt| {
            let phys = self
                .translate_user(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            if phys.as_u64() % PAGE_4KIB_U64 != 0
                || !phys::data_leaf_is_owned(self.pml4_frame_phys, virt.as_u64(), phys.as_u64())
            {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
            Ok(phys)
        })?;
        let mut reusable_frames = Vec::new();
        reusable_frames
            .try_reserve_exact(frames.len())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;

        let mutation = begin_address_space_mutation(self.root_phys());
        for (page_index, frame_phys) in frames.iter().copied().enumerate() {
            let virtual_address = page_addr(start, page_index)
                .expect("validated unmap range changed while removing leaves")
                .as_u64();
            let unmapped = self
                .unmap_user_page(VirtAddr::new(virtual_address))
                .ok_or(AddressSpaceError::NotMapped)?;
            debug_assert_eq!(unmapped.as_u64(), frame_phys);
        }
        let _flushed_mutation = mutation.flush_for_reclaim();

        for (page_index, frame_phys) in frames.iter().copied().enumerate() {
            let virtual_address = page_addr(start, page_index)
                .expect("validated unmap range changed while releasing descriptors")
                .as_u64();
            match phys::release_data_leaf(self.pml4_frame_phys, virtual_address, frame_phys) {
                Some(phys::DataLeafRelease::FrameReusable) => {
                    reusable_frames.push(frame_phys);
                }
                Some(phys::DataLeafRelease::FrameRetained) => {}
                None => panic!("unmapped data leaf lost exact descriptor ownership"),
            }
        }
        free_removed_frames_exact(&reusable_frames);
        Ok(page_count)
    }

    pub fn unmap_user_pages_without_free_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<usize, AddressSpaceError> {
        validate_user_page_range(start, page_count)?;
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            let phys = self
                .translate_user(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            if !phys.as_u64().is_multiple_of(PAGE_4KIB_U64) {
                return Err(AddressSpaceError::NotMapped);
            }
        }

        let _mutation = begin_address_space_mutation(self.root_phys());
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            self.unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
        }
        Ok(page_count)
    }

    pub fn protect_user_bytes(
        &mut self,
        start: VirtAddr,
        byte_len: usize,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        let page_count = byte_len_to_page_count(byte_len)?;
        self.protect_user_pages_at(start, page_count, flags)
    }

    pub fn protect_user_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, page_count)?;
        let page_flags = normalize_user_page_flags(flags)?;

        if !validate_protection_span(page_count, |page_index| {
            page_addr(start, page_index).ok().is_some_and(|virt| {
                matches!(
                    self.lookup_user_page_state(virt),
                    UserPageLookup::Present { .. }
                )
            })
        }) {
            return Err(AddressSpaceError::NotMapped);
        }

        let _mutation = begin_address_space_mutation(self.root_phys());
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            self.protect_user_page(virt, page_flags)?;
        }

        Ok(())
    }

    pub fn ensure_user_region_mapped(
        &self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, page_count)?;
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            if self.translate_user(virt).is_none() {
                return Err(AddressSpaceError::NotMapped);
            }
        }
        Ok(())
    }

    pub fn debug_dump_user_range_state(&self, start: VirtAddr, page_count: usize, reason: &str) {
        crate::debug::println!(
            "address space range dump: reason={} start={:#x} pages={}",
            reason,
            start.as_u64(),
            page_count,
        );
        let capped = page_count.min(8);
        for page_index in 0..capped {
            let Ok(virt) = page_addr(start, page_index) else {
                break;
            };
            self.debug_dump_user_page_state(virt, reason);
        }
        if page_count > capped {
            crate::debug::println!(
                "address space range dump: truncated remaining_pages={}",
                page_count - capped
            );
        }
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
    pub fn debug_dump_user_page_state(&self, virt: VirtAddr, reason: &str) {
        match self.lookup_user_page_state(virt) {
            UserPageLookup::NotUser => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=not-user",
                reason,
                virt.as_u64(),
            ),
            UserPageLookup::MissingPml4 => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=missing-pml4",
                reason,
                virt.as_u64(),
            ),
            UserPageLookup::MissingPdpt => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=missing-pdpt",
                reason,
                virt.as_u64(),
            ),
            UserPageLookup::MissingPd => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=missing-pd",
                reason,
                virt.as_u64(),
            ),
            UserPageLookup::MissingPt => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=missing-pt",
                reason,
                virt.as_u64(),
            ),
            UserPageLookup::Present { phys, flags } => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=present phys={:#x} flags={:?}",
                reason,
                virt.as_u64(),
                phys.as_u64(),
                flags,
            ),
        }
    }

    pub fn translate_user(&self, virt: VirtAddr) -> Option<PhysAddr> {
        if !is_user_addr(virt.as_u64()) {
            return None;
        }

        let (phys, _) = self.translate_user_with_flags(virt)?;
        Some(phys)
    }

    pub fn translate_user_with_flags(&self, virt: VirtAddr) -> Option<(PhysAddr, PageTableFlags)> {
        if !is_user_addr(virt.as_u64()) {
            return None;
        }

        let p4 = p4_index(virt);
        let root = self.root_table_ref();
        let pml4_entry = &root[p4];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
        let pdpt_entry = &pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
        let pd_entry = &pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pt = unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) };
        let pt_entry = &pt[p1_index(virt)];
        if pt_entry.is_unused() {
            return None;
        }

        let offset = virt.as_u64() & (PAGE_4KIB_U64 - 1);
        Some((
            PhysAddr::new(pt_entry.addr().as_u64() + offset),
            pt_entry.flags(),
        ))
    }

    fn root_table_ref(&self) -> &'static PageTable {
        unsafe { kernel_vm::phys_to_table_ref(self.root_phys()) }
    }

    fn flush_if_active(&self, virt: VirtAddr) {
        let (current_frame, _) = Cr3::read();
        if current_frame.start_address() == self.root_phys() {
            tlb::flush(virt);
        }
    }

    fn map_user_page(
        &mut self,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(virt, 1)?;

        let pml4_phys = self.root_phys();
        let root_phys = pml4_phys.as_u64();
        let pdpt_phys = ensure_next_table(root_phys, pml4_phys, p4_index(virt))?;
        let pd_phys = ensure_next_table(root_phys, pdpt_phys, p3_index(virt))?;
        let pt_phys = ensure_next_table(root_phys, pd_phys, p2_index(virt))?;
        let pt = unsafe { kernel_vm::phys_to_table_mut(pt_phys) };

        let entry = &mut pt[p1_index(virt)];
        if !entry.is_unused() {
            return Err(AddressSpaceError::AlreadyMapped);
        }

        entry.set_addr(phys, flags);
        self.flush_if_active(virt);
        Ok(())
    }

    fn protect_user_page(
        &mut self,
        virt: VirtAddr,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        // Bit 7 is PS in a directory entry, but PAT in a 4-KiB leaf PTE.
        // Preserve it across mprotect so a write-combine external mapping
        // cannot silently become write-back and create conflicting aliases.
        // Software ownership and COW tags are likewise lifecycle evidence,
        // not protection requested by mprotect, and may never be detached.
        let existing = self.user_page_flags(virt)?;
        self.set_user_page_flags_exact_without_flush(virt, preserve_4k_leaf_pat(existing, flags))?;
        self.flush_if_active(virt);
        Ok(())
    }

    fn user_page_flags(&self, virt: VirtAddr) -> Result<PageTableFlags, AddressSpaceError> {
        self.translate_user_with_flags(virt)
            .map(|(_, flags)| flags)
            .ok_or(AddressSpaceError::NotMapped)
    }

    fn set_user_page_flags_exact_without_flush(
        &mut self,
        virt: VirtAddr,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(virt, 1)?;
        let root = unsafe { kernel_vm::phys_to_table_mut(self.root_phys()) };
        let pml4_entry = &mut root[p4_index(virt)];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }
        let pdpt = unsafe { kernel_vm::phys_to_table_mut(pml4_entry.addr()) };
        let pdpt_entry = &mut pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }
        let pd = unsafe { kernel_vm::phys_to_table_mut(pdpt_entry.addr()) };
        let pd_entry = &mut pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }
        let pt = unsafe { kernel_vm::phys_to_table_mut(pd_entry.addr()) };
        let pt_entry = &mut pt[p1_index(virt)];
        if pt_entry.is_unused() {
            return Err(AddressSpaceError::NotMapped);
        }
        pt_entry.set_addr(pt_entry.addr(), flags);
        Ok(())
    }

    fn unmap_user_page(&mut self, virt: VirtAddr) -> Option<PhysAddr> {
        if !is_user_addr(virt.as_u64()) {
            return None;
        }

        let root = unsafe { kernel_vm::phys_to_table_mut(self.root_phys()) };
        let pml4_entry = &mut root[p4_index(virt)];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pdpt = unsafe { kernel_vm::phys_to_table_mut(pml4_entry.addr()) };
        let pdpt_entry = &mut pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pd = unsafe { kernel_vm::phys_to_table_mut(pdpt_entry.addr()) };
        let pd_entry = &mut pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pt = unsafe { kernel_vm::phys_to_table_mut(pd_entry.addr()) };
        let pt_entry = &mut pt[p1_index(virt)];
        if pt_entry.is_unused() {
            return None;
        }

        let frame_phys = pt_entry.addr();
        pt_entry.set_unused();
        self.flush_if_active(virt);
        Some(frame_phys)
    }
}

fn user_regions_contain(regions: &[UserRegion], address: VirtAddr) -> bool {
    let index = regions.partition_point(|region| region.start <= address);
    index != 0 && address < regions[index - 1].end()
}

fn restore_parent_flags(
    space: &mut ProcessAddressSpace,
    downgraded: &[(VirtAddr, PageTableFlags)],
) {
    for (virt, flags) in downgraded.iter().rev().copied() {
        space
            .set_user_page_flags_exact_without_flush(virt, flags)
            .expect("fork COW rollback lost a parent leaf");
    }
}

fn rollback_cloned_leaves(space: &mut ProcessAddressSpace, leaves: &[ClonedLeaf]) {
    for leaf in leaves.iter().rev().copied() {
        let unmapped = space
            .unmap_user_page(leaf.virtual_address)
            .expect("fork COW rollback lost a child leaf");
        assert_eq!(unmapped.as_u64(), leaf.child_frame_phys);
        if let Some(ticket) = leaf.shared_alias {
            assert!(
                phys::rollback_shared_alias(ticket),
                "fork COW rollback lost a shared alias"
            );
        } else {
            assert_eq!(
                phys::release_data_leaf(
                    space.pml4_frame_phys,
                    leaf.virtual_address.as_u64(),
                    leaf.child_frame_phys,
                ),
                Some(phys::DataLeafRelease::FrameReusable),
                "fork COW rollback lost an exclusive child leaf"
            );
            phys::free_frame(PhysAddr::new(leaf.child_frame_phys));
        }
    }
}

fn ensure_next_table(
    root_phys: u64,
    parent_phys: PhysAddr,
    index: usize,
) -> Result<PhysAddr, AddressSpaceError> {
    if let Some(child) = read_user_table_entry(parent_phys, index)? {
        return Ok(child);
    }

    let Some(table_phys) = phys::alloc_frame() else {
        return Err(AddressSpaceError::OutOfFrames);
    };
    unsafe {
        kernel_vm::phys_to_table_mut(table_phys).zero();
    }
    // Every dynamically published user table, regardless of whether a normal
    // mapper or an IRQ-off fault won the directory CAS, has one root-owned
    // descriptor. The claim precedes publication so a reachable table never
    // exists outside that ledger.
    if !phys::claim_lazy_table_record(root_phys, table_phys.as_u64()) {
        let _ = phys::try_free_frame(table_phys);
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }

    // The mutation guard excludes every other normal-time writer, but not the
    // exception-time installer, which publishes tables for a pager VMA that can
    // share this 2 MiB block. The CAS is what makes the two agree on one table.
    match publish_user_table_entry(
        parent_phys,
        index,
        table_phys,
        user_table_flags() | ROOT_OWNED_USER_TABLE,
    ) {
        Ok((child, true)) => {
            phys::publish_lazy_table_record(root_phys, table_phys.as_u64());
            Ok(child)
        }
        Ok((child, false)) => {
            phys::cancel_lazy_table_record(root_phys, table_phys.as_u64());
            let _ = phys::try_free_frame(table_phys);
            Ok(child)
        }
        Err(err) => {
            phys::cancel_lazy_table_record(root_phys, table_phys.as_u64());
            let _ = phys::try_free_frame(table_phys);
            Err(err)
        }
    }
}

fn validate_user_page_range(start: VirtAddr, page_count: usize) -> Result<(), AddressSpaceError> {
    if page_count == 0 {
        return Err(AddressSpaceError::ZeroSizedAllocation);
    }
    if !is_page_aligned(start.as_u64()) {
        return Err(AddressSpaceError::AddressNotPageAligned);
    }
    if !is_user_addr(start.as_u64()) {
        return Err(AddressSpaceError::AddressOutOfRange);
    }

    let span = (page_count as u64)
        .checked_mul(PAGE_4KIB_U64)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    let end = start
        .as_u64()
        .checked_add(span)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    if end > USER_SPACE_END_EXCLUSIVE {
        return Err(AddressSpaceError::AddressOutOfRange);
    }

    Ok(())
}

fn validate_protection_span<F>(page_count: usize, mut page_is_present: F) -> bool
where
    F: FnMut(usize) -> bool,
{
    (0..page_count).all(&mut page_is_present)
}

fn plan_user_page_unmap(
    start: VirtAddr,
    page_count: usize,
    mut resolve_owned: impl FnMut(VirtAddr) -> Result<PhysAddr, AddressSpaceError>,
) -> Result<Vec<u64>, AddressSpaceError> {
    validate_user_page_range(start, page_count)?;
    let mut frames = Vec::new();
    frames
        .try_reserve_exact(page_count)
        .map_err(|_| AddressSpaceError::OutOfFrames)?;
    for page_index in 0..page_count {
        let virt = page_addr(start, page_index)?;
        let phys = resolve_owned(virt)?;
        frames.push(phys.as_u64());
    }
    Ok(frames)
}

fn byte_len_to_page_count(byte_len: usize) -> Result<usize, AddressSpaceError> {
    if byte_len == 0 {
        return Err(AddressSpaceError::ZeroSizedAllocation);
    }

    let rounded = byte_len
        .checked_add(PAGE_4KIB - 1)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    Ok(rounded / PAGE_4KIB)
}

fn page_addr(start: VirtAddr, page_index: usize) -> Result<VirtAddr, AddressSpaceError> {
    let delta = (page_index as u64)
        .checked_mul(PAGE_4KIB_U64)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    let addr = start
        .as_u64()
        .checked_add(delta)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    Ok(VirtAddr::new(addr))
}

#[derive(Clone, Copy)]
enum UserBufferAccess {
    Read,
    Write,
}

fn validate_user_page_access(
    flags: PageTableFlags,
    access: UserBufferAccess,
) -> Result<(), AddressSpaceError> {
    let required = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if !flags.contains(required) {
        return Err(AddressSpaceError::ProtectionViolation);
    }

    if matches!(access, UserBufferAccess::Write) && !flags.contains(PageTableFlags::WRITABLE) {
        return Err(AddressSpaceError::ProtectionViolation);
    }

    Ok(())
}

fn align_down(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|aligned| aligned & !(align - 1))
}

fn is_page_aligned(addr: u64) -> bool {
    addr & (PAGE_4KIB_U64 - 1) == 0
}

fn is_user_addr(addr: u64) -> bool {
    (USER_SPACE_BASE..USER_SPACE_END_EXCLUSIVE).contains(&addr)
}

fn p4_index(virt: VirtAddr) -> usize {
    ((virt.as_u64() >> 39) & 0x1ff) as usize
}

fn p3_index(virt: VirtAddr) -> usize {
    ((virt.as_u64() >> 30) & 0x1ff) as usize
}

fn p2_index(virt: VirtAddr) -> usize {
    ((virt.as_u64() >> 21) & 0x1ff) as usize
}

fn p1_index(virt: VirtAddr) -> usize {
    ((virt.as_u64() >> 12) & 0x1ff) as usize
}

fn user_table_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE
}

fn normalize_user_page_flags(flags: PageTableFlags) -> Result<PageTableFlags, AddressSpaceError> {
    if flags.contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::HugePageConflict);
    }
    if flags.contains(PageTableFlags::WRITABLE) && !flags.contains(PageTableFlags::NO_EXECUTE) {
        return Err(AddressSpaceError::ProtectionViolation);
    }

    Ok(flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE)
}

fn preserve_4k_leaf_pat(existing: PageTableFlags, requested: PageTableFlags) -> PageTableFlags {
    let preserved =
        requested | (existing & (PageTableFlags::HUGE_PAGE | ROOT_OWNED_USER_LEAF | COW_USER_LEAF));
    if existing.contains(COW_USER_LEAF) {
        preserved.difference(PageTableFlags::WRITABLE)
    } else {
        preserved
    }
}

enum UserPageLookup {
    NotUser,
    MissingPml4,
    MissingPdpt,
    MissingPd,
    MissingPt,
    Present {
        phys: PhysAddr,
        flags: PageTableFlags,
    },
}

impl UserPageLookup {
    /// Whether the walk found no leaf because a level of its path is absent.
    ///
    /// Every absent level is an ordinary demand state once intermediate tables
    /// are published at fault time rather than at reservation time: an
    /// untouched page has no leaf, and an untouched 2 MiB block has no page
    /// table to hold one. Treating only a missing leaf as ordinary would make
    /// `munmap` and `mprotect` fail on a range the process never touched.
    fn is_absent(&self) -> bool {
        matches!(
            self,
            Self::MissingPml4 | Self::MissingPdpt | Self::MissingPd | Self::MissingPt
        )
    }
}

impl ProcessAddressSpace {
    fn lookup_user_page_state(&self, virt: VirtAddr) -> UserPageLookup {
        if !is_user_addr(virt.as_u64()) {
            return UserPageLookup::NotUser;
        }

        let p4 = p4_index(virt);
        let root = self.root_table_ref();
        let pml4_entry = &root[p4];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return UserPageLookup::MissingPml4;
        }

        let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
        let pdpt_entry = &pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return UserPageLookup::MissingPdpt;
        }

        let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
        let pd_entry = &pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return UserPageLookup::MissingPd;
        }

        let pt = unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) };
        let pt_entry = &pt[p1_index(virt)];
        if pt_entry.is_unused() {
            return UserPageLookup::MissingPt;
        }

        let offset = virt.as_u64() & (PAGE_4KIB_U64 - 1);
        UserPageLookup::Present {
            phys: PhysAddr::new(pt_entry.addr().as_u64() + offset),
            flags: pt_entry.flags(),
        }
    }
}

fn higher_half_ptr(phys: PhysAddr) -> *mut u8 {
    kernel_vm::higher_half_addr(phys.as_u64()) as *mut u8
}

pub fn smoke_test() {
    use crate::debug;

    let mut space =
        ProcessAddressSpace::new().expect("process address space allocation must succeed");
    let region = space
        .alloc_user_bytes(8192, PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE)
        .expect("process user region allocation must succeed");
    let sample = b"proc-paging-ok";
    space
        .copy_into_user(region.start, sample)
        .expect("process user copy must succeed");

    let phys = space
        .translate_user(region.start)
        .expect("process user translation must succeed");
    let mut probe = [0_u8; 14];
    unsafe {
        ptr::copy_nonoverlapping(
            higher_half_ptr(phys) as *const u8,
            probe.as_mut_ptr(),
            probe.len(),
        );
    }

    if probe != *sample {
        panic!("process paging smoke test data mismatch");
    }

    debug::println!(
        "Process paging smoke test passed: user_va={:#x}, phys={:#x}",
        region.start.as_u64(),
        phys.as_u64()
    );
}

#[cfg(test)]
mod tests;
