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
mod eof_guard;
pub use eof_guard::current_user_leaf_is_eof_guard;
mod execution_lifetime;
mod frame_release;
mod immutable;
pub use immutable::ImmutableBacking;
mod pager_fault_mapping;
mod range_query;
mod retirement;
pub use execution_lifetime::{
    ActivationPin, ExecutionPin, RetirementOwner, drain_retired_address_spaces,
};
mod rollback;
mod user_copy;
use frame_release::{
    FRAME_BATCH_CHUNK, RetiredFrameBatcher, free_frame_buffer_tail, free_removed_frames_exact,
    free_rollback_frames_exact,
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
pub use user_copy::{UserTranslationMemo, ValidatedUserRead, ValidatedUserWrite};

use crate::memory::{kernel_vm, phys};

const ENTRIES_PER_TABLE: usize = 512;
const PAGE_4KIB: usize = 4096;
const PAGE_4KIB_U64: u64 = PAGE_4KIB as u64;
const USER_PML4_INDEX: usize = 1;
pub const USER_SPACE_BASE: u64 = (USER_PML4_INDEX as u64) << 39;
pub const USER_SPACE_END_EXCLUSIVE: u64 = ((USER_PML4_INDEX + 1) as u64) << 39;
/// A retained leaf whose bytes are inaccessible to userspace (PROT_NONE).
/// Bit 10 is software-defined at leaf level; its directory-level meaning is
/// independently ROOT_OWNED_USER_TABLE. Keep PRESENT for ownership/reclaim,
/// but clear USER_ACCESSIBLE so hardware and checked user-copy both deny it.
pub const USER_INACCESSIBLE_LEAF: PageTableFlags = PageTableFlags::BIT_10;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PagerFaultLeaf {
    virtual_address: u64,
    physical_address: u64,
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

pub(crate) fn data_claim_result(claim: phys::DataClaim) -> Result<(), AddressSpaceError> {
    match claim {
        phys::DataClaim::Claimed => Ok(()),
        phys::DataClaim::Quota => Err(AddressSpaceError::OutOfFrames),
        phys::DataClaim::Invalid => Err(AddressSpaceError::InvalidFrameOwnership),
    }
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
    /// The parent's PTE flags before the fork downgrade (restored on failure).
    parent_flags: PageTableFlags,
}

#[derive(Clone, Copy)]
struct PendingSharedLeaf {
    slot: u16,
    parent_flags: PageTableFlags,
    child_flags: PageTableFlags,
}

impl PendingSharedLeaf {
    const EMPTY: Self = Self {
        slot: 0,
        parent_flags: PageTableFlags::empty(),
        child_flags: PageTableFlags::empty(),
    };
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
    execution: Option<execution_lifetime::Owner>,
    immutable: Vec<immutable::ImmutableMapping>,
}

impl ProcessAddressSpace {
    #[inline(never)]
    pub fn new() -> Result<Self, AddressSpaceError> {
        let pml4_phys = phys::alloc_frame_user().ok_or(AddressSpaceError::OutOfFrames)?;
        phys::register_lazy_table_root(pml4_phys.as_u64());
        phys::register_data_leaf_root(pml4_phys.as_u64());
        // SAFETY: `pml4_phys` is a frame just returned by `alloc_frame_user`: exclusively
        // owned, 4 KiB aligned, direct-mapped, and not yet published in any root, so
        // this `&mut` cannot alias another reference.
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
            execution: Some(execution_lifetime::Owner::new(pml4_phys.as_u64(), true)),
            immutable: Vec::new(),
        })
    }

    pub fn root_phys(&self) -> PhysAddr {
        PhysAddr::new(self.pml4_frame_phys)
    }

    /// Keep mapping-accounting custody until this execution root is reclaimed.
    pub fn set_mapping_meter(&mut self, meter: crate::memory::kmem_charge::SharedMeter) {
        self.execution.as_mut().expect("live MM owner").set_mapping_meter(meter);
    }

    /// Bind both immutable backing custody and resident ledger atomically for
    /// an unpublished, quiescent root. A refused bind retains no reservation.
    pub fn bind_job_charge(&mut self, group: usize) -> bool {
        let owner = self.execution.as_mut().expect("live MM owner");
        if !owner.bind_backings(group) { return false; }
        if !phys::bind_root_charge(self.pml4_frame_phys, group) {
            owner.unbind_backings();
            return false;
        }
        true
    }

    #[cfg(all(debug_assertions, not(rustos_boot_image)))]
    pub fn bind_job_charge_for_tests(&mut self, group: usize) -> bool {
        self.execution.as_mut().expect("test MM owner").bind_backings(group)
    }

    /// Retains the complete owned root/PTE/frame ledger, not mutable mappings.
    pub fn pin_execution(&self) -> Option<ExecutionPin> {
        self.execution.as_ref()?.pin()
    }
    pub fn into_retirement_owner(mut self) -> RetirementOwner {
        self.immutable.clear();
        RetirementOwner {
            owner: self
                .execution
                .take()
                .expect("MM retirement owner transferred twice"),
        }
    }

    /// Irreversible: captured execution refs survive, new activation refs refuse.
    /// Refuses without closing while claim/architecture/RSP activation custody
    /// is outstanding. The cold caller must cancel/retry before image effects.
    pub fn seal_execution(&self) -> bool {
        self.execution
            .as_ref()
            .expect("MM lacks root owner")
            .try_seal_activation()
    }

    #[cfg(not(rustos_boot_image))]
    pub fn modeled_root_for_host(root: u64) -> Self {
        Self {
            pml4_frame_phys: root,
            next_user_addr: USER_SPACE_BASE,
            execution: Some(execution_lifetime::Owner::new(root, false)),
            immutable: Vec::new(),
        }
    }

    #[cfg(any(test, debug_assertions))]
    pub fn empty_for_tests() -> Self {
        Self {
            pml4_frame_phys: 0,
            next_user_addr: USER_SPACE_BASE,
            execution: Some(execution_lifetime::Owner::new(0, false)),
            immutable: Vec::new(),
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
        // Only page-table pages are recorded here; the leaves themselves are
        // read straight from each 512-entry page below, so the walk neither
        // allocates per leaf nor re-translates an address it already holds.
        let (leaf_pages, leaf_count) = self.owned_leaf_table_pages()?;
        let mut cloned_leaves = Vec::new();
        cloned_leaves
            .try_reserve_exact(leaf_count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        // The child mapper acquires its own mutation stripe. Clone immutable
        // aliases before taking the parent stripe to avoid nested class-38
        // locks; the caller's fork hold still excludes parent VMA mutation.
        self.clone_immutable_mappings_into(&mut cloned)?;
        let mutation = begin_address_space_mutation(self.root_phys());
        let parent_root = self.pml4_frame_phys;
        let child_root = cloned.pml4_frame_phys;
        let mut cow_cursor = RegionCursor::new(cow_ranges);
        let mut eager_cursor = RegionCursor::new(eager_private_ranges);
        // Runs of leaves usually share one flag word, so the normalization of
        // the child's COW flags is computed once per distinct input.
        let mut shared_flags_memo: Option<(PageTableFlags, PageTableFlags)> = None;

        let mut pending_leaves = [PendingSharedLeaf::EMPTY; phys::SHARED_ALIAS_RUN_MAX];
        let mut pending_requests = [phys::SharedAliasRequest::EMPTY; phys::SHARED_ALIAS_RUN_MAX];
        let mut pending_tickets = [phys::SharedAliasTicket::EMPTY; phys::SHARED_ALIAS_RUN_MAX];
        let mut pending_len = 0_usize;
        // Leaves shared copy-on-write vs copied eagerly, for the per-fork
        // `fork-clone-leaves` milestone: a fork that pays for many eager
        // copies shows up in the log instead of as unexplained memory growth.
        let mut shared_leaves = 0_u64;
        let mut eager_leaves = 0_u64;

        macro_rules! fail {
            ($error:expr) => {{
                let error = $error;
                rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                drop(mutation);
                return Err(error);
            }};
        }

        macro_rules! flush_shared_run {
            ($child_table:expr) => {{
                if pending_len != 0 {
                    let count = pending_len;
                    pending_len = 0;
                    let needed = cloned_leaves.len().saturating_add(count);
                    if needed > cloned_leaves.capacity()
                        && cloned_leaves.try_reserve(count).is_err()
                    {
                        fail!(AddressSpaceError::OutOfFrames);
                    }
                    if let Err(error) = phys::prepare_shared_alias_run(
                        parent_root,
                        child_root,
                        &pending_requests[..count],
                        &mut pending_tickets[..count],
                    ) {
                        let mapped = match error {
                            phys::SharedAliasRunError::OutOfFrames => {
                                AddressSpaceError::OutOfFrames
                            }
                            phys::SharedAliasRunError::InvalidFrameOwnership => {
                                AddressSpaceError::InvalidFrameOwnership
                            }
                        };
                        fail!(mapped);
                    }
                    // SAFETY: `child_table` is the child's own PT, created or
                    // found by `ensure_leaf_table` under the same mutation
                    // stripe; the child root is unpublished and never loaded,
                    // so no CPU can hold a translation that needs a flush.
                    let installed = {
                        let table = unsafe { kernel_vm::phys_to_table_mut($child_table) };
                        let mut written = 0_usize;
                        while written < count {
                            let slot = pending_leaves[written].slot as usize;
                            let entry = &mut table[slot];
                            if !entry.is_unused() {
                                for undo in &pending_leaves[..written] {
                                    table[undo.slot as usize].set_unused();
                                }
                                break;
                            }
                            entry.set_addr(
                                PhysAddr::new(pending_requests[written].frame_phys),
                                pending_leaves[written].child_flags,
                            );
                            written += 1;
                        }
                        written == count
                    };
                    if !installed {
                        phys::cancel_shared_alias_run(&pending_tickets[..count]);
                        fail!(AddressSpaceError::AlreadyMapped);
                    }
                    phys::publish_shared_alias_run(&pending_tickets[..count]);
                    for i in 0..count {
                        cloned_leaves.push(ClonedLeaf {
                            virtual_address: VirtAddr::new(pending_requests[i].new_va),
                            child_frame_phys: pending_requests[i].frame_phys,
                            shared_alias: Some(pending_tickets[i]),
                            parent_flags: pending_leaves[i].parent_flags,
                        });
                    }
                }
            }};
        }

        for page in &leaf_pages {
            // One directory walk per page-table page for the child; the parent
            // page is addressed directly through the direct map.
            let child_table = match cloned.ensure_leaf_table(page.base_virtual_address) {
                Ok(table) => table,
                Err(error) => fail!(error),
            };
            for slot in 0..512_usize {
                // SAFETY: `page.table_phys` is a present, non-huge parent PT
                // recorded under the caller's fork hold; the parent mutation
                // stripe and `&mut self` exclude other normal-time writers and
                // tables are freed only at retirement. The reference is
                // dropped before any call that could re-derive this table.
                let (src_phys, flags) = {
                    let entry = &unsafe { kernel_vm::phys_to_table_ref(page.table_phys) }[slot];
                    if entry.is_unused() || !entry.flags().contains(ROOT_OWNED_USER_LEAF) {
                        continue;
                    }
                    if !entry.flags().contains(PageTableFlags::PRESENT) {
                        fail!(AddressSpaceError::InvalidFrameOwnership);
                    }
                    (entry.addr(), entry.flags())
                };
                let virt_address = page.base_virtual_address | ((slot as u64) << 12);
                let virt = VirtAddr::new(virt_address);

                let eager_private = eager_cursor.contains(virt_address);
                let shared_mode = if eager_private {
                    None
                } else if flags.contains(COW_USER_LEAF) {
                    Some(phys::SharedAliasMode::InheritShared)
                } else if cow_cursor.contains(virt_address) {
                    Some(phys::SharedAliasMode::AnonymousFork)
                } else {
                    None
                };

                if shared_mode.is_some() {
                    shared_leaves += 1;
                } else if !eager_private {
                    eager_leaves += 1;
                }
                if let Some(mode) = shared_mode {
                    let child_flags = match shared_flags_memo {
                        Some((input, output)) if input == flags => output,
                        _ => match normalize_user_page_flags(flags) {
                            Ok(normalized) => {
                                let output = normalized
                                    .difference(PageTableFlags::WRITABLE)
                                    .union(ROOT_OWNED_USER_LEAF | COW_USER_LEAF);
                                shared_flags_memo = Some((flags, output));
                                output
                            }
                            Err(error) => fail!(error),
                        },
                    };
                    pending_leaves[pending_len] = PendingSharedLeaf {
                        slot: slot as u16,
                        parent_flags: flags,
                        child_flags,
                    };
                    pending_requests[pending_len] = phys::SharedAliasRequest {
                        owner_va: virt_address,
                        new_va: virt_address,
                        frame_phys: src_phys.as_u64(),
                        mode,
                    };
                    pending_len += 1;
                    if pending_len == phys::SHARED_ALIAS_RUN_MAX {
                        flush_shared_run!(child_table);
                    }
                    continue;
                }

                flush_shared_run!(child_table);
                if cloned_leaves.len() == cloned_leaves.capacity()
                    && cloned_leaves.try_reserve(1).is_err()
                {
                    fail!(AddressSpaceError::OutOfFrames);
                }
                if flags.contains(COW_USER_LEAF)
                    && phys::data_leaf_cow_identity(parent_root, virt_address, src_phys.as_u64())
                        .is_none()
                {
                    fail!(AddressSpaceError::InvalidFrameOwnership);
                }

                if !phys::data_leaf_is_owned(parent_root, virt_address, src_phys.as_u64()) {
                    fail!(AddressSpaceError::InvalidFrameOwnership);
                }
                let Some(dst_phys) = phys::alloc_frame_user() else {
                    fail!(AddressSpaceError::OutOfFrames);
                };
                if let Err(error) = data_claim_result(phys::claim_data_leaf(
                    child_root,
                    virt_address,
                    dst_phys.as_u64(),
                )) {
                    phys::free_frame(dst_phys);
                    fail!(error);
                }
                // SAFETY: `src_phys` is the parent's leaf, read from its PTE
                // under the mutation stripe and ownership-checked above;
                // `dst_phys` is a fresh exclusive frame from `alloc_frame_user`.
                // Both are direct-mapped, 4 KiB, and distinct frames, so the
                // two PAGE_4KIB ranges cannot overlap.
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
                        phys::cancel_data_leaf(child_root, virt_address, dst_phys.as_u64());
                        phys::free_frame(dst_phys);
                        fail!(error);
                    }
                };
                if let Err(error) = cloned.map_user_page(virt, dst_phys, child_flags) {
                    phys::cancel_data_leaf(child_root, virt_address, dst_phys.as_u64());
                    phys::free_frame(dst_phys);
                    fail!(error);
                }
                phys::publish_data_leaf(child_root, virt_address, dst_phys.as_u64());
                cloned_leaves.push(ClonedLeaf {
                    virtual_address: virt,
                    child_frame_phys: dst_phys.as_u64(),
                    shared_alias: None,
                    parent_flags: flags,
                });
            }
            flush_shared_run!(child_table);
        }

        // Pass 2: write-protect the parent. `cloned_leaves` is in ascending
        // address order, so each run inside one page-table page costs a single
        // directory walk. The flag word is re-read from the live PTE (not the
        // pass-1 snapshot) so hardware accessed/dirty updates are preserved.
        let mut done = 0_usize;
        while done < cloned_leaves.len() {
            let table_key = cloned_leaves[done].virtual_address.as_u64() >> 21;
            let run_end = done
                + cloned_leaves[done..]
                    .iter()
                    .take_while(|leaf| leaf.virtual_address.as_u64() >> 21 == table_key)
                    .count();
            let first = cloned_leaves[done].virtual_address;
            let mut failure = None;
            match self.leaf_pt_mut(first) {
                Err(error) => failure = Some(error),
                Ok(table) => {
                    for index in done..run_end {
                        let leaf = &mut cloned_leaves[index];
                        if leaf.shared_alias.is_none() {
                            continue;
                        }
                        let entry = &mut table[p1_index(leaf.virtual_address)];
                        if entry.is_unused() {
                            failure = Some(AddressSpaceError::NotMapped);
                            break;
                        }
                        if entry.addr().as_u64() != leaf.child_frame_phys {
                            failure = Some(AddressSpaceError::InvalidFrameOwnership);
                            break;
                        }
                        let old_flags = entry.flags();
                        let cow_flags = old_flags
                            .difference(PageTableFlags::WRITABLE)
                            .union(ROOT_OWNED_USER_LEAF | COW_USER_LEAF);
                        entry.set_addr(entry.addr(), cow_flags);
                        leaf.parent_flags = old_flags;
                        // Count the entry as downgraded only after the write.
                        done = index + 1;
                    }
                }
            }
            if let Some(error) = failure {
                restore_parent_flags(self, &cloned_leaves[..done]);
                rollback_cloned_leaves(&mut cloned, &cloned_leaves);
                drop(mutation);
                return Err(error);
            }
            done = run_end;
        }

        // Drop performs the one parent-root shootdown and waits for every CPU
        // that could retain a writable translation before the child can later
        // become runnable.
        drop(mutation);
        crate::debug::record_milestone(
            crate::debug::LogCategory::Memory,
            "fork-clone-leaves",
            shared_leaves,
            eager_leaves,
        );
        Ok(cloned)
    }

    /// Returns the child-side leaf page table covering `base_virtual_address`,
    /// creating the directory chain through the unified claim/CAS protocol.
    fn ensure_leaf_table(
        &mut self,
        base_virtual_address: u64,
    ) -> Result<PhysAddr, AddressSpaceError> {
        let virt = VirtAddr::new(base_virtual_address);
        validate_user_page_range(virt, 1)?;
        let root = self.root_phys();
        let root_phys = root.as_u64();
        let pdpt = ensure_next_table(root_phys, root, p4_index(virt))?;
        let pd = ensure_next_table(root_phys, pdpt, p3_index(virt))?;
        ensure_next_table(root_phys, pd, p2_index(virt))
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
                // Admit the whole batch before drawing frames: a refusal then
                // costs no allocation and unwinds like any out-of-frames case.
                if !phys::root_admits(self.pml4_frame_phys, want as u64) {
                    rollback_user_pages(self, &mapped_pages, mutation);
                    return Err(AddressSpaceError::OutOfFrames);
                }
                frame_buffer_len = phys::alloc_frames_batch_user(&mut frame_buffer[..want]);
                frame_buffer_pos = 0;
                if frame_buffer_len == 0 {
                    rollback_user_pages(self, &mapped_pages, mutation);
                    return Err(AddressSpaceError::OutOfFrames);
                }
            }
            let frame_phys = frame_buffer[frame_buffer_pos];
            frame_buffer_pos += 1;

            // SAFETY: `frame_phys` was just taken from the batch allocator, so it is an
            // exclusively owned, direct-mapped 4 KiB frame not yet claimed or mapped;
            // zeroing it before `map_user_page` publishes it leaks nothing to userspace.
            unsafe {
                ptr::write_bytes(higher_half_ptr(frame_phys), 0, PAGE_4KIB);
            }

            let virtual_address = virt.as_u64();
            if let Err(error) = data_claim_result(phys::claim_data_leaf(self.pml4_frame_phys, virtual_address, frame_phys.as_u64())) {
                phys::free_frame(frame_phys);
                free_frame_buffer_tail(&frame_buffer[frame_buffer_pos..frame_buffer_len]);
                rollback_user_pages(self, &mapped_pages, mutation);
                return Err(error);
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
        // The frames are external (a lease, memfd or device aperture), so each
        // must be an encodable leaf target before any PTE changes: an unaligned
        // address makes `set_addr` panic and a too-wide one is a reserved-bit
        // fault the first time the process touches the page.
        let addressable_limit = super::cache_attributes::max_physical_address();
        if frames
            .iter()
            .any(|&frame| !external_frame_is_encodable(frame, addressable_limit))
        {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
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
        // Removed pages are a prefix; a failure stops the loop and the prefix is
        // still released through the reclaim barrier before the error returns.
        let mut removed = 0usize;
        let mut failure = None;
        for (page_index, frame_phys) in frames.iter().copied().enumerate() {
            let virtual_address = page_addr(start, page_index)
                .expect("validated unmap range changed while removing leaves")
                .as_u64();
            match self.unmap_user_page(VirtAddr::new(virtual_address)) {
                Some(unmapped) => {
                    debug_assert_eq!(unmapped.as_u64(), frame_phys);
                    removed += 1;
                }
                None => {
                    failure = Some(AddressSpaceError::NotMapped);
                    break;
                }
            }
        }
        let _flushed_mutation = mutation.flush_for_reclaim();

        for (page_index, frame_phys) in frames[..removed].iter().copied().enumerate() {
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
        match failure {
            Some(error) => Err(error),
            None => Ok(page_count),
        }
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
        if page_count == 0 {
            return Ok(());
        }

        if !self.all_user_pages_present(start, page_count) {
            return Err(AddressSpaceError::NotMapped);
        }

        // Private immutable aliases must become owned copies before a write
        // upgrade. Complete-span validation above precedes all allocation/PTEs.
        if page_flags.contains(PageTableFlags::WRITABLE) {
            self.privatize_immutable_pages(start, page_count)?;
        }
        let _mutation = begin_address_space_mutation(self.root_phys());
        let is_active = Cr3::read().0.start_address() == self.root_phys();
        let mut done = 0_usize;
        while done < page_count {
            let chunk_start = page_addr(start, done)?;
            let p1 = p1_index(chunk_start);
            let chunk = (512 - p1).min(page_count - done);
            let pt = self.leaf_pt_mut(chunk_start)?;
            for offset in 0..chunk {
                let entry = &mut pt[p1 + offset];
                if entry.is_unused() {
                    return Err(AddressSpaceError::NotMapped);
                }
                // A page past the end of its file stays a guard: protection
                // changes apply to the file's pages, never reopen the hole.
                if eof_guard::is_guard_leaf(entry.addr().as_u64(), entry.flags()) {
                    continue;
                }
                let updated = preserve_4k_leaf_pat(entry.flags(), page_flags);
                entry.set_addr(entry.addr(), updated);
                if is_active {
                    tlb::flush(VirtAddr::new(
                        chunk_start.as_u64() + (offset as u64) * PAGE_4KIB_U64,
                    ));
                }
            }
            done += chunk;
        }

        Ok(())
    }

    pub fn ensure_user_region_mapped(
        &self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<(), AddressSpaceError> {
        match self.first_unmapped_user_page(start, page_count)? {
            Some(_) => Err(AddressSpaceError::NotMapped),
            None => Ok(()),
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

        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB page-table frame in the direct map. Tables are freed
        // only at address-space retirement, which cannot overlap this `&self` borrow;
        // a racing exception-time installer only CASes unused -> table, never frees.
        let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
        let pdpt_entry = &pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB page-table frame in the direct map. Tables are freed
        // only at address-space retirement, which cannot overlap this `&self` borrow;
        // a racing exception-time installer only CASes unused -> table, never frees.
        let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
        let pd_entry = &pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB page-table frame in the direct map. Tables are freed
        // only at address-space retirement, which cannot overlap this `&self` borrow;
        // a racing exception-time installer only CASes unused -> table, never frees.
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
        // SAFETY: `root_phys` is this address space's own PML4 frame, allocated in `new`
        // and live until retirement, so the direct-map alias is a valid 4 KiB table.
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
        // SAFETY: `pt_phys` came from `ensure_next_table`, which returned the PT frame this
        // walk already published (or found) under the mutation guard; `&mut self`
        // excludes other normal-time writers, so the direct-map `&mut` is the sole one.
        let pt = unsafe { kernel_vm::phys_to_table_mut(pt_phys) };

        let entry = &mut pt[p1_index(virt)];
        if !entry.is_unused() {
            return Err(AddressSpaceError::AlreadyMapped);
        }

        entry.set_addr(phys, flags);
        self.flush_if_active(virt);
        Ok(())
    }

    fn all_user_pages_present(&self, start: VirtAddr, page_count: usize) -> bool {
        let mut done = 0_usize;
        while done < page_count {
            let Ok(chunk_start) = page_addr(start, done) else {
                return false;
            };
            let p1 = p1_index(chunk_start);
            let chunk = (512 - p1).min(page_count - done);
            let Some(pt) = self.leaf_pt_ref(chunk_start) else {
                return false;
            };
            if (p1..p1 + chunk).any(|idx| pt[idx].is_unused()) {
                return false;
            }
            done += chunk;
        }
        true
    }

    fn leaf_pt_ref(&self, virt: VirtAddr) -> Option<&PageTable> {
        if !is_user_addr(virt.as_u64()) {
            return None;
        }
        let root = self.root_table_ref();
        let pml4_entry = &root[p4_index(virt)];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }
        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB page-table frame in the direct map. Tables are freed
        // only at address-space retirement, which cannot overlap this `&self` borrow;
        // a racing exception-time installer only CASes unused -> table, never frees.
        let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
        let pdpt_entry = &pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }
        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB page-table frame in the direct map. Tables are freed
        // only at address-space retirement, which cannot overlap this `&self` borrow;
        // a racing exception-time installer only CASes unused -> table, never frees.
        let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
        let pd_entry = &pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }
        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB page-table frame in the direct map. Tables are freed
        // only at address-space retirement, which cannot overlap this `&self` borrow;
        // a racing exception-time installer only CASes unused -> table, never frees.
        Some(unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) })
    }

    fn leaf_pt_mut(&mut self, virt: VirtAddr) -> Result<&mut PageTable, AddressSpaceError> {
        validate_user_page_range(virt, 1)?;
        // SAFETY: `root_phys` is this address space's own PML4 frame, allocated in `new`
        // and live until retirement; `&mut self` plus the mutation guard exclude other
        // normal-time writers (the exception-time installer only CASes empty entries).
        let root = unsafe { kernel_vm::phys_to_table_mut(self.root_phys()) };
        let pml4_entry = &mut root[p4_index(virt)];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }
        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB table frame in the direct map. `&mut self` plus the
        // address-space mutation guard exclude other normal-time writers; the only
        // concurrent writer is the exception-time installer's unused -> table CAS.
        let pdpt = unsafe { kernel_vm::phys_to_table_mut(pml4_entry.addr()) };
        let pdpt_entry = &mut pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }
        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB table frame in the direct map. `&mut self` plus the
        // address-space mutation guard exclude other normal-time writers; the only
        // concurrent writer is the exception-time installer's unused -> table CAS.
        let pd = unsafe { kernel_vm::phys_to_table_mut(pdpt_entry.addr()) };
        let pd_entry = &mut pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }
        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB table frame in the direct map. `&mut self` plus the
        // address-space mutation guard exclude other normal-time writers; the only
        // concurrent writer is the exception-time installer's unused -> table CAS.
        Ok(unsafe { kernel_vm::phys_to_table_mut(pd_entry.addr()) })
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
        let p1 = p1_index(virt);
        let pt = self.leaf_pt_mut(virt)?;
        let pt_entry = &mut pt[p1];
        if pt_entry.is_unused() {
            return Err(AddressSpaceError::NotMapped);
        }
        let updated = preserve_4k_leaf_pat(pt_entry.flags(), flags);
        pt_entry.set_addr(pt_entry.addr(), updated);
        self.flush_if_active(virt);
        Ok(())
    }

    fn set_user_page_flags_exact_without_flush(
        &mut self,
        virt: VirtAddr,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        let p1 = p1_index(virt);
        let pt = self.leaf_pt_mut(virt)?;
        let pt_entry = &mut pt[p1];
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

        // SAFETY: `root_phys` is this address space's own PML4 frame, allocated in `new`
        // and live until retirement; `&mut self` plus the mutation guard exclude other
        // normal-time writers (the exception-time installer only CASes empty entries).
        let root = unsafe { kernel_vm::phys_to_table_mut(self.root_phys()) };
        let pml4_entry = &mut root[p4_index(virt)];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB table frame in the direct map. `&mut self` plus the
        // address-space mutation guard exclude other normal-time writers; the only
        // concurrent writer is the exception-time installer's unused -> table CAS.
        let pdpt = unsafe { kernel_vm::phys_to_table_mut(pml4_entry.addr()) };
        let pdpt_entry = &mut pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB table frame in the direct map. `&mut self` plus the
        // address-space mutation guard exclude other normal-time writers; the only
        // concurrent writer is the exception-time installer's unused -> table CAS.
        let pd = unsafe { kernel_vm::phys_to_table_mut(pdpt_entry.addr()) };
        let pd_entry = &mut pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB table frame in the direct map. `&mut self` plus the
        // address-space mutation guard exclude other normal-time writers; the only
        // concurrent writer is the exception-time installer's unused -> table CAS.
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

/// Membership test over sorted, disjoint regions for queries issued in
/// ascending address order; each query resumes where the last one stopped, so
/// a whole fork walk is linear in `regions + leaves` instead of a binary
/// search per leaf.
struct RegionCursor<'a> {
    regions: &'a [UserRegion],
    index: usize,
}

impl<'a> RegionCursor<'a> {
    fn new(regions: &'a [UserRegion]) -> Self {
        Self { regions, index: 0 }
    }

    fn contains(&mut self, address: u64) -> bool {
        while self
            .regions
            .get(self.index)
            .is_some_and(|region| region.end().as_u64() <= address)
        {
            self.index += 1;
        }
        self.regions
            .get(self.index)
            .is_some_and(|region| region.start.as_u64() <= address)
    }
}

/// Re-installs the pre-fork flags of every parent leaf that pass 2 already
/// downgraded (`leaves` is exactly that prefix).
fn restore_parent_flags(space: &mut ProcessAddressSpace, leaves: &[ClonedLeaf]) {
    for leaf in leaves
        .iter()
        .rev()
        .filter(|leaf| leaf.shared_alias.is_some())
    {
        space
            .set_user_page_flags_exact_without_flush(leaf.virtual_address, leaf.parent_flags)
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

    let Some(table_phys) = phys::alloc_frame_user() else {
        return Err(AddressSpaceError::OutOfFrames);
    };
    // SAFETY: `table_phys` is a fresh exclusive frame from `alloc_frame_user`, direct-
    // mapped and not yet linked into any table, so zeroing it as a `PageTable`
    // cannot alias another reference and precedes the publishing CAS below.
    unsafe {
        kernel_vm::phys_to_table_mut(table_phys).zero();
    }
    // Every dynamically published user table, regardless of whether a normal
    // mapper or an IRQ-off fault won the directory CAS, has one root-owned
    // descriptor. The claim precedes publication so a reachable table never
    // exists outside that ledger.
    match phys::claim_lazy_table_record(root_phys, table_phys.as_u64()) {
        phys::TableClaim::Claimed => {}
        // The Job's page-table budget refuses like a frame-allocation failure.
        phys::TableClaim::Quota => {
            let _ = phys::try_free_frame(table_phys);
            return Err(AddressSpaceError::OutOfFrames);
        }
        phys::TableClaim::Invalid => {
            let _ = phys::try_free_frame(table_phys);
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
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

#[cfg(test)]
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

/// Whether `frame` is a page-aligned frame wholly below the CPU's physical
/// address ceiling (`limit` is exclusive).
fn external_frame_is_encodable(frame: u64, limit: u64) -> bool {
    frame.is_multiple_of(PAGE_4KIB_U64)
        && frame
            .checked_add(PAGE_4KIB_U64)
            .is_some_and(|end| end <= limit)
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

    if flags.contains(USER_INACCESSIBLE_LEAF) {
        if flags.contains(PageTableFlags::WRITABLE) || !flags.contains(PageTableFlags::NO_EXECUTE) {
            return Err(AddressSpaceError::ProtectionViolation);
        }
        Ok((flags | PageTableFlags::PRESENT).difference(PageTableFlags::USER_ACCESSIBLE))
    } else {
        Ok(flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE)
    }
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
    Present,
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

        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB page-table frame in the direct map. Tables are freed
        // only at address-space retirement, which cannot overlap this `&self` borrow;
        // a racing exception-time installer only CASes unused -> table, never frees.
        let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
        let pdpt_entry = &pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return UserPageLookup::MissingPdpt;
        }

        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB page-table frame in the direct map. Tables are freed
        // only at address-space retirement, which cannot overlap this `&self` borrow;
        // a racing exception-time installer only CASes unused -> table, never frees.
        let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
        let pd_entry = &pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return UserPageLookup::MissingPd;
        }

        // SAFETY: the entry above was checked present and non-huge, so `addr()` names a
        // kernel-allocated 4 KiB page-table frame in the direct map. Tables are freed
        // only at address-space retirement, which cannot overlap this `&self` borrow;
        // a racing exception-time installer only CASes unused -> table, never frees.
        let pt = unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) };
        let pt_entry = &pt[p1_index(virt)];
        if pt_entry.is_unused() {
            return UserPageLookup::MissingPt;
        }

        UserPageLookup::Present
    }
}

fn higher_half_ptr(phys: PhysAddr) -> *mut u8 {
    kernel_vm::higher_half_addr(phys.as_u64()) as *mut u8
}

/// Boot-time proof that a Job's page-table budget actually binds in the real
/// allocator: the root and one directory fit, the next table is refused with
/// `OutOfFrames`, and lifting the limit lets the same mapping succeed. Uses the
/// last charge group, which no Job can hold this early; its retirement drains
/// the charge later and the job layer waits for that before reusing the slot.
fn kmem_limit_smoke() {
    use crate::memory::{charge, kmem_charge};

    const GROUP: usize = charge::CHARGE_GROUPS - 1;
    const TABLE_BYTES: u64 = 4096;
    kmem_charge::init_group(GROUP, None);
    let mut space =
        ProcessAddressSpace::new().expect("kmem smoke address space allocation must succeed");
    assert!(phys::bind_root_charge(space.root_phys().as_u64(), GROUP));
    assert_eq!(kmem_charge::usage(kmem_charge::Class::PageTable, GROUP), TABLE_BYTES);
    kmem_charge::set_limit(kmem_charge::Class::PageTable, GROUP, 2 * TABLE_BYTES);
    let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    let refused = space.alloc_user_bytes(4096, flags);
    let held = kmem_charge::usage(kmem_charge::Class::PageTable, GROUP);
    assert!(
        matches!(refused, Err(AddressSpaceError::OutOfFrames)),
        "page-table budget did not refuse the third frame: {refused:?}"
    );
    assert!(held <= 2 * TABLE_BYTES, "page-table usage {held} exceeds its limit");
    kmem_charge::set_limit(kmem_charge::Class::PageTable, GROUP, charge::UNLIMITED);
    space
        .alloc_user_bytes(4096, flags)
        .expect("mapping succeeds once the page-table budget is lifted");
    // `println!` is compiled out of release images, so the proof is a milestone
    // (refused at `held` bytes, then mapped within the lifted budget).
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Memory,
        "kmem-page-table-limit-smoke",
        held,
        kmem_charge::usage(kmem_charge::Class::PageTable, GROUP),
    );
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
    // SAFETY: boot smoke test: `phys` is the frame just translated for the mapped
    // user page, direct-mapped and valid for `probe.len()` (14 <= 4096) bytes; the
    // destination is a local array, so the ranges are disjoint.
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

    // Relocation moves the frame, its flags and its descriptor claim, copying
    // nothing; the old range must stop translating and the bytes stay put.
    let before_flags = space
        .translate_user_with_flags(region.start)
        .expect("relocation source leaf")
        .1;
    let relocated = VirtAddr::new(region.start.as_u64() + 0x1000_0000);
    let moved = space
        .move_present_user_leaves_at(region.start, relocated, region.page_count)
        .expect("process leaf relocation must succeed");
    assert_eq!(moved, region.page_count, "relocation moved every resident leaf");
    assert!(
        space.translate_user(region.start).is_none(),
        "the old range must be vacant after relocation"
    );
    assert_eq!(
        space.translate_user_with_flags(relocated),
        Some((phys, before_flags)),
        "same frame, same protection at the new address"
    );
    assert!(phys::data_leaf_is_owned(space.pml4_frame_phys, relocated.as_u64(), phys.as_u64()));
    assert!(!phys::data_leaf_is_owned(space.pml4_frame_phys, region.start.as_u64(), phys.as_u64()));
    assert_eq!(
        space.move_present_user_leaves_at(relocated, region.start, region.page_count),
        Ok(region.page_count),
        "relocation back must restore the original mapping"
    );
    assert_eq!(space.translate_user(region.start), Some(phys));

    debug::println!(
        "Process paging smoke test passed: user_va={:#x}, phys={:#x}",
        region.start.as_u64(),
        phys.as_u64()
    );
    kmem_limit_smoke();
    immutable::smoke_test();
}

#[cfg(test)]
mod tests;
