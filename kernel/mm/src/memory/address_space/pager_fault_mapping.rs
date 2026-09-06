//! Pager-fault leaf preparation and bounded PTE ownership transfer.
//!
//! - **Owner:** `kernel-mm` owns prepared leaf tables, PTE publication, and
//!   exact frame-descriptor transitions; the VMA supplies policy authority.
//! - **Boundary:** Normal-time preparation may allocate tables, while
//!   exception-time install/replacement accepts only an already-present exact
//!   leaf and pre-admitted metadata.
//! - **Lifecycle:** Claim descriptor, install PTE, publish; replacement removes
//!   the old key only after exact TLB acknowledgement.
//! - **Concurrency:** PTE CAS plus the address-space mutation guard serializes
//!   normal writers and gives exception writers a nonblocking retry boundary.
//! - **Failure:** CAS loss, stale PTE/VMA, descriptor mismatch, or contention
//!   returns every unpublished frame and leaves the live mapping unchanged.
//! - **Forbidden:** No exception-time allocation of ownership metadata,
//!   sleepable lock, W+X grant, or present non-COW replacement.
//! - **Evidence:** `CowFrameLifecycle`, `PageTableMapTransaction`, and focused
//!   PTE/COW source mutations.
use core::sync::atomic::{AtomicU64, Ordering};

use super::*;
use x86_64::structures::paging::page_table::PageTableEntry;

/// Software-owned leaf bit for every address-space-owned data frame. It is an
/// x86 available-to-software PTE bit, never consumed by hardware translation
/// or protection.
pub(crate) const ROOT_OWNED_USER_LEAF: PageTableFlags = PageTableFlags::BIT_9;

/// Software-owned leaf bit for an exact read-only COW mapping. Bit 11 is the
/// remaining x86 available-to-software PTE bit; hardware never grants access
/// from it, so the VMA plus shared-frame ledger remain the authority.
pub(crate) const COW_USER_LEAF: PageTableFlags = PageTableFlags::BIT_11;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CowWriteResult {
    Replaced,
    PromotedInPlace,
    Retry,
    NotCow,
}

/// Strips the source root's ownership tag from flags copied out of a live leaf.
///
/// A clone claims a fresh exact descriptor before installing its copy. The
/// source tag is never inherited as frame authority.
pub(crate) fn without_root_owned_user_leaf_tag(flags: PageTableFlags) -> PageTableFlags {
    flags.difference(ROOT_OWNED_USER_LEAF | COW_USER_LEAF)
}

/// Software-owned directory bit for every dynamically published user table.
///
/// Both normal-time mappers and IRQ-off anonymous faults claim the same
/// root-owned table descriptor before their parent-entry CAS. This x86
/// available-to-software bit is the independent topology witness used to
/// reconcile that descriptor list at retirement; table and data frames occupy
/// mutually exclusive roles in the same physical descriptor catalog.
pub(crate) const ROOT_OWNED_USER_TABLE: PageTableFlags = PageTableFlags::BIT_10;

/// The physical-address field of a paging-structure entry.
const TABLE_ENTRY_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// Calls `f` with the atomic representation of one intermediate user directory
/// entry.
///
/// Both installers reach a directory entry through this helper: the normal-time
/// mapping transaction under its mutation guard, and the exception-time path
/// under its fault-install permit.  They therefore contend on one atomic
/// location rather than on a plain store racing a CAS.
fn with_user_table_entry_atomic<R>(
    parent_phys: PhysAddr,
    index: usize,
    f: impl FnOnce(&AtomicU64) -> R,
) -> R {
    // SAFETY: the caller owns this root and supplies an in-range index. A
    // `PageTableEntry` is a `repr(transparent)` `u64` and naturally aligned, and
    // every writer of an intermediate entry reaches it through this helper.
    let table = unsafe { kernel_vm::phys_to_table_ref(parent_phys) };
    let raw = &table[index] as *const PageTableEntry as *mut u64;
    // SAFETY: `raw` retains the table entry's natural u64 alignment and every
    // access to this entry is atomic for the duration of this borrowed view.
    let entry = unsafe { AtomicU64::from_ptr(raw) };
    f(entry)
}

/// Decides what an observed non-zero intermediate entry authorizes.
fn classify_user_table_entry(observed: u64) -> Result<PhysAddr, AddressSpaceError> {
    let flags = PageTableFlags::from_bits_truncate(observed);
    if flags.contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::HugePageConflict);
    }
    if !flags.contains(user_table_flags()) {
        return Err(AddressSpaceError::ProtectionViolation);
    }
    Ok(PhysAddr::new(observed & TABLE_ENTRY_ADDR_MASK))
}

/// Reads one intermediate user directory entry.
///
/// `Ok(None)` means the level is absent, which is the ordinary demand state
/// once tables are published at fault time rather than at reservation time.
pub(crate) fn read_user_table_entry(
    parent_phys: PhysAddr,
    index: usize,
) -> Result<Option<PhysAddr>, AddressSpaceError> {
    with_user_table_entry_atomic(parent_phys, index, |entry| {
        // ORDERING: Acquire pairs with the installer's release CAS, so a reader
        // that observes this entry also observes the zeroed table it names.
        let observed = entry.load(Ordering::Acquire);
        if observed == 0 {
            return Ok(None);
        }
        classify_user_table_entry(observed).map(Some)
    })
}

/// Publishes `table_phys` into an absent intermediate user directory entry.
///
/// A directory entry moves from not-present to present exactly like a leaf, so
/// no shootdown is owed: the hardware caches neither a TLB entry nor a
/// paging-structure-cache entry for an absent entry, and the release CAS
/// publishes the zeroed table before the entry that names it.
///
/// `Ok((child, false))` means another installer won this entry first, so
/// `table_phys` was never published and must be returned to its supply.
pub(crate) fn publish_user_table_entry(
    parent_phys: PhysAddr,
    index: usize,
    table_phys: PhysAddr,
    flags: PageTableFlags,
) -> Result<(PhysAddr, bool), AddressSpaceError> {
    let desired = table_phys.as_u64() | flags.bits();
    with_user_table_entry_atomic(parent_phys, index, |entry| {
        // ORDERING: AcqRel releases the zeroed table before the entry naming it.
        match entry.compare_exchange(0, desired, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Ok((table_phys, true)),
            Err(observed) => classify_user_table_entry(observed).map(|child| (child, false)),
        }
    })
}

/// Resolves one intermediate entry during the retirement walk, recording the
/// child frame when the entry says the fault path published it.
fn tagged_child_of(
    entry: &PageTableEntry,
    tables: &mut Vec<u64>,
) -> Result<Option<PhysAddr>, AddressSpaceError> {
    if entry.is_unused() {
        return Ok(None);
    }
    let flags = entry.flags();
    if flags.contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::HugePageConflict);
    }
    // Nothing publishes an intermediate entry without `PRESENT`, and nothing
    // clears one to a non-zero value, so this state is corruption rather than
    // an ordinary demand state.
    if !flags.contains(PageTableFlags::PRESENT) {
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    if flags.contains(ROOT_OWNED_USER_TABLE) {
        tables.push(entry.addr().as_u64());
    }
    Ok(Some(entry.addr()))
}

/// The virtual-address bit position each paging level indexes.
const PML4_SHIFT: u32 = 39;
const PDPT_SHIFT: u32 = 30;
const PD_SHIFT: u32 = 21;

/// Pages remaining from `virt` to the next boundary of the level at `shift`.
///
/// This is what lets an absent level skip its whole span instead of one page
/// table at a time.
fn pages_to_level_boundary(virt: VirtAddr, shift: u32) -> usize {
    let span = 1_u64 << shift;
    let offset = virt.as_u64() & (span - 1);
    ((span - offset) / PAGE_4KIB_U64) as usize
}

pub fn map_current_prepared_pager_fault_frame_at(
    start: VirtAddr,
    frame_phys: u64,
    flags: PageTableFlags,
) -> Result<bool, AddressSpaceError> {
    validate_user_page_range(start, 1)?;
    if frame_phys == 0 || !frame_phys.is_multiple_of(PAGE_4KIB_U64) {
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    let page_flags = normalize_user_page_flags(flags)? | ROOT_OWNED_USER_LEAF;
    let root_phys = kernel_vm::current_root_phys().as_u64();
    let virtual_address = start.as_u64();
    if !phys::claim_data_leaf(root_phys, virtual_address, frame_phys) {
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    let desired = frame_phys | page_flags.bits();
    // ORDERING: success releases descriptor-admitted frame contents before the
    // present leaf; failure acquires any leaf published by a competing fault.
    let result = with_prepared_user_leaf_atomic(PhysAddr::new(root_phys), start, |leaf| match leaf
        .compare_exchange(0, desired, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => Ok(true),
        Err(observed) if observed & PageTableFlags::PRESENT.bits() != 0 => Ok(false),
        Err(_) => Err(AddressSpaceError::AlreadyMapped),
    });
    match result {
        Ok(true) => {
            phys::publish_data_leaf(root_phys, virtual_address, frame_phys);
            Ok(true)
        }
        other => {
            phys::cancel_data_leaf(root_phys, virtual_address, frame_phys);
            other
        }
    }
}

/// Resolves one exact present-write COW fault in the current address space.
///
/// `new_frame_phys` remains caller-owned on `PromotedInPlace`, `Retry`,
/// `NotCow`, or `Err`; only `Replaced` consumes it. A sole anonymous survivor
/// becomes writable without a redundant 4 KiB copy. Exception entry tries the
/// global TLB protocol once and restarts the instruction if another mutation
/// owns it.
pub fn resolve_current_cow_write_at(
    start: VirtAddr,
    new_frame_phys: u64,
    expected_kind: phys::CowFrameKind,
) -> Result<CowWriteResult, AddressSpaceError> {
    let root_phys = kernel_vm::current_root_phys().as_u64();
    let Some(mutation) =
        kernel_hal::api::arch::tlb::try_begin_address_space_mutation_from_exception(PhysAddr::new(
            root_phys,
        ))
    else {
        return Ok(CowWriteResult::Retry);
    };
    resolve_cow_write_under_mutation(root_phys, start, new_frame_phys, expected_kind, mutation)
}

/// Shared exact-root transaction for exception faults and retained copyout.
/// The caller holds logical VMA write authority and the matching root's TLB
/// guard. In particular, retained copyout must never derive its root from CR3.
fn resolve_cow_write_under_mutation(
    root_phys: u64,
    start: VirtAddr,
    new_frame_phys: u64,
    expected_kind: phys::CowFrameKind,
    mutation: kernel_hal::api::arch::tlb::AddressSpaceMutationGuard,
) -> Result<CowWriteResult, AddressSpaceError> {
    validate_user_page_range(start, 1)?;
    if new_frame_phys == 0 || !new_frame_phys.is_multiple_of(PAGE_4KIB_U64) {
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    let virtual_address = start.as_u64();
    let observed = with_prepared_user_leaf_atomic(PhysAddr::new(root_phys), start, |leaf| {
        // ORDERING: acquire observes the flags and frame identity published by
        // the latest leaf installer before the descriptor claim is attempted.
        Ok(leaf.load(Ordering::Acquire))
    })?;
    let observed_flags = PageTableFlags::from_bits_truncate(observed);
    if observed & TABLE_ENTRY_ADDR_MASK == 0
        || !observed_flags.contains(
            PageTableFlags::PRESENT
                | PageTableFlags::USER_ACCESSIBLE
                | ROOT_OWNED_USER_LEAF
                | COW_USER_LEAF,
        )
        || observed_flags.contains(PageTableFlags::WRITABLE)
    {
        return Ok(CowWriteResult::NotCow);
    }
    let old_frame_phys = observed & TABLE_ENTRY_ADDR_MASK;
    let Some(claim) = phys::try_claim_cow_mapping(root_phys, virtual_address, old_frame_phys)
    else {
        drop(mutation);
        return Ok(CowWriteResult::Retry);
    };
    if claim.kind() != expected_kind {
        phys::cancel_cow_mapping(claim);
        drop(mutation);
        return Ok(CowWriteResult::NotCow);
    }

    if claim.can_promote_in_place() {
        let promoted = with_prepared_user_leaf_atomic(PhysAddr::new(root_phys), start, |leaf| {
            // ORDERING: acquire validates the exact leaf protected by the COW
            // mapping claim before an in-place permission transition.
            let mut current = leaf.load(Ordering::Acquire);
            loop {
                let flags = PageTableFlags::from_bits_truncate(current);
                if current & TABLE_ENTRY_ADDR_MASK != old_frame_phys
                    || !flags.contains(ROOT_OWNED_USER_LEAF | COW_USER_LEAF)
                    || flags.contains(PageTableFlags::WRITABLE)
                {
                    return Ok(false);
                }
                let desired_flags = flags
                    .difference(COW_USER_LEAF)
                    .union(PageTableFlags::WRITABLE | ROOT_OWNED_USER_LEAF);
                let desired = old_frame_phys | desired_flags.bits();
                match leaf.compare_exchange_weak(
                    current,
                    desired,
                    // ORDERING: success publishes the writable promotion after
                    // the exclusive mapping claim; failure refreshes the leaf.
                    Ordering::AcqRel,
                    // ORDERING: acquire makes the refreshed leaf flags and
                    // identity visible before the retry predicate runs.
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(true),
                    Err(next) => current = next,
                }
            }
        });
        let promoted = match promoted {
            Ok(promoted) => promoted,
            Err(err) => {
                phys::cancel_cow_mapping(claim);
                drop(mutation);
                return Err(err);
            }
        };
        if !promoted {
            phys::cancel_cow_mapping(claim);
            drop(mutation);
            return Ok(CowWriteResult::Retry);
        }
        let _flushed = mutation.flush_for_reclaim();
        phys::promote_cow_mapping_in_place(claim);
        return Ok(CowWriteResult::PromotedInPlace);
    }

    if !phys::claim_data_leaf(root_phys, virtual_address, new_frame_phys) {
        phys::cancel_cow_mapping(claim);
        drop(mutation);
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    // SAFETY: both identities name descriptor-admitted, direct-mapped 4 KiB
    // frames. The destination is unpublished and the shared source is locked
    // against mapping removal while all of its PTEs remain read-only.
    unsafe {
        ptr::copy_nonoverlapping(
            higher_half_ptr(PhysAddr::new(old_frame_phys)),
            higher_half_ptr(PhysAddr::new(new_frame_phys)),
            PAGE_4KIB,
        );
    }

    let replaced = with_prepared_user_leaf_atomic(PhysAddr::new(root_phys), start, |leaf| {
        // ORDERING: acquire validates that the claimed old mapping still names
        // this read-only COW leaf before publishing the private replacement.
        let mut current = leaf.load(Ordering::Acquire);
        loop {
            let flags = PageTableFlags::from_bits_truncate(current);
            if current & TABLE_ENTRY_ADDR_MASK != old_frame_phys
                || !flags.contains(ROOT_OWNED_USER_LEAF | COW_USER_LEAF)
                || flags.contains(PageTableFlags::WRITABLE)
            {
                return Ok(false);
            }
            let desired_flags = flags
                .difference(COW_USER_LEAF)
                .union(PageTableFlags::WRITABLE | ROOT_OWNED_USER_LEAF);
            let desired = new_frame_phys | desired_flags.bits();
            // ORDERING: success releases the completed page copy through the
            // new present leaf; failure acquires the competing leaf mutation.
            match leaf.compare_exchange_weak(current, desired, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(true),
                Err(next) => current = next,
            }
        }
    });
    let replaced = match replaced {
        Ok(replaced) => replaced,
        Err(err) => {
            phys::cancel_cow_mapping(claim);
            phys::cancel_data_leaf(root_phys, virtual_address, new_frame_phys);
            drop(mutation);
            return Err(err);
        }
    };
    if !replaced {
        phys::cancel_cow_mapping(claim);
        phys::cancel_data_leaf(root_phys, virtual_address, new_frame_phys);
        drop(mutation);
        return Ok(CowWriteResult::Retry);
    }

    phys::publish_data_leaf(root_phys, virtual_address, new_frame_phys);
    let _flushed = mutation.flush_for_reclaim();
    if phys::commit_cow_mapping(claim) == phys::DataLeafRelease::FrameReusable {
        phys::free_frame(PhysAddr::new(old_frame_phys));
    }
    Ok(CowWriteResult::Replaced)
}

/// Publishes every intermediate table the active root needs in order to hold a
/// leaf for `start`, and reports how many this caller published.
///
/// # Exception-path contract
///
/// This runs at exception entry with interrupts disabled, no address-space
/// lock, and no TLB protocol.  Its only mutations are release CAS publications
/// of absent directory entries, and a not-present-to-present directory
/// transition owes no shootdown for the same reason a leaf does not: the
/// hardware caches neither a translation nor a paging-structure entry for an
/// absent entry, so no stale state can survive the publication.
///
/// `supply` must yield a **zeroed** 4 KiB frame; a table is published only
/// after its contents are, so a CPU that observes the entry observes an empty
/// table.  `release` takes back any frame that lost its CAS, which was never
/// reachable from any root.
pub fn ensure_current_fault_tables_at(
    start: VirtAddr,
    mut supply: impl FnMut() -> Option<u64>,
    mut release: impl FnMut(u64),
) -> Result<usize, AddressSpaceError> {
    validate_user_page_range(start, 1)?;
    let table_flags = user_table_flags() | ROOT_OWNED_USER_TABLE;
    let mut published = 0;
    let root_phys = kernel_vm::current_root_phys().as_u64();
    let mut parent = PhysAddr::new(root_phys);
    for index in [p4_index(start), p3_index(start), p2_index(start)] {
        parent = match read_user_table_entry(parent, index)? {
            Some(child) => child,
            None => {
                let Some(frame) = supply() else {
                    return Err(AddressSpaceError::OutOfFrames);
                };
                // A zero frame is not a frame, so there is nothing to give
                // back; handing it to a release path would free address zero.
                if frame == 0 {
                    return Err(AddressSpaceError::InvalidFrameOwnership);
                }
                if !frame.is_multiple_of(PAGE_4KIB_U64) {
                    release(frame);
                    return Err(AddressSpaceError::InvalidFrameOwnership);
                }
                if !crate::memory::phys::claim_lazy_table_record(root_phys, frame) {
                    release(frame);
                    return Err(AddressSpaceError::InvalidFrameOwnership);
                }
                match publish_user_table_entry(parent, index, PhysAddr::new(frame), table_flags) {
                    Ok((child, true)) => {
                        crate::memory::phys::publish_lazy_table_record(root_phys, frame);
                        published += 1;
                        child
                    }
                    Ok((child, false)) => {
                        crate::memory::phys::cancel_lazy_table_record(root_phys, frame);
                        release(frame);
                        child
                    }
                    Err(error) => {
                        crate::memory::phys::cancel_lazy_table_record(root_phys, frame);
                        release(frame);
                        return Err(error);
                    }
                }
            }
        };
    }
    Ok(published)
}

impl ProcessAddressSpace {
    /// Splits a resident COW page in this retained address space, not the
    /// executor's current CR3. The process owner must hold its state lock and
    /// prove committed private VMA write permission before calling this.
    /// Normal-context contention releases the TLB guard between attempts;
    /// the existing exception entry remains strictly try-only.
    pub fn resolve_cow_write_for_copyout(
        &self,
        start: VirtAddr,
        kind: phys::CowFrameKind,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, 1)?;
        let frame = phys::alloc_frame().ok_or(AddressSpaceError::OutOfFrames)?;
        let mut deadline = kernel_hal::api::arch::clock::SpinDeadline::start();
        loop {
            let mutation = kernel_hal::api::arch::tlb::begin_address_space_mutation(PhysAddr::new(
                self.pml4_frame_phys,
            ));
            let result = resolve_cow_write_under_mutation(
                self.pml4_frame_phys,
                start,
                frame.as_u64(),
                kind,
                mutation,
            );
            match result {
                Ok(CowWriteResult::Replaced) => return Ok(()),
                Ok(CowWriteResult::Retry) => {
                    // No mutation guard or shared-frame claim survives here.
                    // A lost descriptor owner is an invariant failure, not
                    // user EFAULT or an unbounded wait under the MM lock.
                    assert!(
                        deadline.elapsed_nanos() < 1_000_000_000,
                        "COW copyout descriptor stalled: root={:#x} va={:#x}",
                        self.pml4_frame_phys,
                        start.as_u64()
                    );
                    core::hint::spin_loop();
                }
                result => {
                    phys::free_frame(frame);
                    return match result {
                        Ok(CowWriteResult::PromotedInPlace | CowWriteResult::NotCow) => Ok(()),
                        Err(error) => Err(error),
                        _ => unreachable!(),
                    };
                }
            }
        }
    }

    pub(super) fn map_shared_cow_leaf_without_mutation(
        &mut self,
        start: VirtAddr,
        frame_phys: u64,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, 1)?;
        if frame_phys == 0 || !frame_phys.is_multiple_of(PAGE_4KIB_U64) {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        let page_flags = normalize_user_page_flags(flags)?
            .difference(PageTableFlags::WRITABLE)
            .union(ROOT_OWNED_USER_LEAF | COW_USER_LEAF);
        self.map_user_page(start, PhysAddr::new(frame_phys), page_flags)
    }

    /// Publishes the first mapping of one private file/section cache frame.
    ///
    /// The backing identity is a stable pager/page-cache identity, not a raw
    /// capability slot. The leaf starts read-only with the COW tag even when it
    /// is the sole mapping, so Linux `MAP_PRIVATE` and Windows WRITECOPY use the
    /// same first-write transition as a later alias.
    pub fn map_private_file_cow_frame_at(
        &mut self,
        start: VirtAddr,
        frame_phys: u64,
        flags: PageTableFlags,
        backing_identity: u64,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, 1)?;
        if frame_phys == 0 || !frame_phys.is_multiple_of(PAGE_4KIB_U64) || backing_identity == 0 {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        let page_flags = normalize_user_page_flags(flags)?
            .difference(PageTableFlags::WRITABLE)
            .union(ROOT_OWNED_USER_LEAF | COW_USER_LEAF);
        let root = self.root_phys();
        let root_phys = root.as_u64();
        let virtual_address = start.as_u64();
        if !phys::claim_private_file_data_leaf(
            root_phys,
            virtual_address,
            frame_phys,
            backing_identity,
        ) {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        let mutation = begin_address_space_mutation(root);
        let installed = (|| {
            let pdpt_phys = ensure_next_table(root_phys, root, p4_index(start))?;
            let pd_phys = ensure_next_table(root_phys, pdpt_phys, p3_index(start))?;
            ensure_next_table(root_phys, pd_phys, p2_index(start))?;
            with_prepared_user_leaf_mut(root, start, |entry| {
                if !entry.is_unused() {
                    return Err(AddressSpaceError::AlreadyMapped);
                }
                entry.set_addr(PhysAddr::new(frame_phys), page_flags);
                Ok(())
            })
        })();
        if let Err(error) = installed {
            phys::cancel_private_file_data_leaf(root_phys, virtual_address, frame_phys);
            drop(mutation);
            return Err(error);
        }
        phys::publish_private_file_data_leaf(root_phys, virtual_address, frame_phys);
        drop(mutation);
        Ok(())
    }

    /// Adds another exact private-file/section mapping of a shared cache frame.
    pub fn map_private_file_cow_alias_at(
        &mut self,
        start: VirtAddr,
        owner_root: u64,
        owner_va: u64,
        frame_phys: u64,
        flags: PageTableFlags,
        backing_identity: u64,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, 1)?;
        let root = self.root_phys();
        let root_phys = root.as_u64();
        let virtual_address = start.as_u64();
        let ticket = phys::prepare_shared_alias(
            owner_root,
            owner_va,
            root_phys,
            virtual_address,
            frame_phys,
            phys::CowFrameKind::PrivateFileSection,
            backing_identity,
        )
        .ok_or(AddressSpaceError::InvalidFrameOwnership)?;
        let mutation = begin_address_space_mutation(root);
        let installed = (|| {
            let pdpt_phys = ensure_next_table(root_phys, root, p4_index(start))?;
            let pd_phys = ensure_next_table(root_phys, pdpt_phys, p3_index(start))?;
            ensure_next_table(root_phys, pd_phys, p2_index(start))?;
            self.map_shared_cow_leaf_without_mutation(start, frame_phys, flags)
        })();
        if let Err(error) = installed {
            phys::cancel_shared_alias(ticket);
            drop(mutation);
            return Err(error);
        }
        phys::publish_shared_alias(ticket);
        drop(mutation);
        Ok(())
    }

    /// Highest mapped user-leaf end, derived from the page-table topology.
    ///
    /// This is not a second reservation ledger. Bootstrap uses it only to seed
    /// its cursor before the process has a published VMA set.
    pub fn highest_user_mapping_end(&self) -> Result<Option<u64>, AddressSpaceError> {
        if self.pml4_frame_phys == 0 {
            return Ok(None);
        }
        let root = self.root_phys();
        let Some(pdpt_phys) = read_user_table_entry(root, USER_PML4_INDEX)? else {
            return Ok(None);
        };
        for p3 in (0..ENTRIES_PER_TABLE).rev() {
            let Some(pd_phys) = read_user_table_entry(pdpt_phys, p3)? else {
                continue;
            };
            for p2 in (0..ENTRIES_PER_TABLE).rev() {
                let Some(pt_phys) = read_user_table_entry(pd_phys, p2)? else {
                    continue;
                };
                // SAFETY: the three parent entries resolved to present,
                // non-huge user tables owned by this inactive bootstrap state.
                let pt = unsafe { kernel_vm::phys_to_table_ref(pt_phys) };
                for p1 in (0..ENTRIES_PER_TABLE).rev() {
                    let entry = &pt[p1];
                    if entry.is_unused() {
                        continue;
                    }
                    if !entry.flags().contains(PageTableFlags::PRESENT) {
                        return Err(AddressSpaceError::InvalidFrameOwnership);
                    }
                    let start = ((USER_PML4_INDEX as u64) << 39)
                        | ((p3 as u64) << 30)
                        | ((p2 as u64) << 21)
                        | ((p1 as u64) << 12);
                    return start
                        .checked_add(PAGE_4KIB_U64)
                        .map(Some)
                        .ok_or(AddressSpaceError::AddressOverflow);
                }
            }
        }
        Ok(None)
    }

    /// Whether every page of the range is still unmapped, walking only the
    /// tables that already exist.
    ///
    /// This replaces reservation-time page-table preparation. An absent level
    /// means nothing in the span it would cover is mapped, so the scan skips
    /// that whole span: a reservation nobody has touched costs one directory
    /// read rather than one read per page, and costs no frames at all.
    ///
    /// The check is bounded by what is *resident*, not by what is reserved,
    /// which is the property reservation-time preparation could not have.
    pub fn user_range_is_unmapped(
        &self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<bool, AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }
        validate_user_page_range(start, page_count)?;
        let root = self.root_phys();
        let mut page_index = 0;
        while page_index < page_count {
            let virt = page_addr(start, page_index)?;
            let remaining = page_count - page_index;
            let Some(pdpt_phys) = read_user_table_entry(root, p4_index(virt))? else {
                page_index += pages_to_level_boundary(virt, PML4_SHIFT).min(remaining);
                continue;
            };
            let Some(pd_phys) = read_user_table_entry(pdpt_phys, p3_index(virt))? else {
                page_index += pages_to_level_boundary(virt, PDPT_SHIFT).min(remaining);
                continue;
            };
            let Some(pt_phys) = read_user_table_entry(pd_phys, p2_index(virt))? else {
                page_index += pages_to_level_boundary(virt, PD_SHIFT).min(remaining);
                continue;
            };
            // SAFETY: every level above resolved to a present non-huge user
            // table this address space owns, and the caller holds its exact
            // process state, so no writer can retire it during this read.
            let table = unsafe { kernel_vm::phys_to_table_ref(pt_phys) };
            let first_entry = p1_index(virt);
            let entries_here = (ENTRIES_PER_TABLE - first_entry).min(remaining);
            for entry in first_entry..first_entry + entries_here {
                if !table[entry].is_unused() {
                    return Ok(false);
                }
            }
            page_index += entries_here;
        }
        Ok(true)
    }

    pub fn map_prepared_pager_fault_frame_at(
        &mut self,
        start: VirtAddr,
        frame_phys: u64,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, 1)?;
        if frame_phys == 0 || !frame_phys.is_multiple_of(PAGE_4KIB_U64) {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        let page_flags = normalize_user_page_flags(flags)? | ROOT_OWNED_USER_LEAF;
        let root = self.root_phys();
        let root_phys = root.as_u64();
        let virtual_address = start.as_u64();
        if !phys::claim_data_leaf(root_phys, virtual_address, frame_phys) {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }

        let mutation = begin_address_space_mutation(root);
        let tables = (|| {
            let pdpt_phys = ensure_next_table(root_phys, root, p4_index(start))?;
            let pd_phys = ensure_next_table(root_phys, pdpt_phys, p3_index(start))?;
            ensure_next_table(root_phys, pd_phys, p2_index(start))
        })();
        if let Err(error) = tables {
            phys::cancel_data_leaf(root_phys, virtual_address, frame_phys);
            drop(mutation);
            return Err(error);
        }
        let installed = with_prepared_user_leaf_mut(root, start, |entry| {
            if !entry.is_unused() {
                return Err(AddressSpaceError::AlreadyMapped);
            }
            entry.set_addr(PhysAddr::new(frame_phys), page_flags);
            Ok(())
        });
        if let Err(error) = installed {
            phys::cancel_data_leaf(root_phys, virtual_address, frame_phys);
            drop(mutation);
            return Err(error);
        }

        phys::publish_data_leaf(root_phys, virtual_address, frame_phys);
        drop(mutation);
        Ok(())
    }

    /// Revokes and frees pager-owned leaves while retaining their prepared
    /// intermediate tables for a later mapping of the same VMA.
    pub fn unmap_prepared_pager_fault_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<usize, AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }
        validate_user_page_range(start, page_count)?;
        let range_end = pager_fault_range_end(start, page_count)?;
        let leaves = self.pager_fault_leaves_in_range(start.as_u64(), range_end, page_count)?;
        if leaves.len() != page_count {
            return Err(AddressSpaceError::NotMapped);
        }

        let mut frames = Vec::new();
        frames
            .try_reserve_exact(page_count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        for (page_index, leaf) in leaves.iter().copied().enumerate() {
            let virt = page_addr(start, page_index)?;
            if leaf.virtual_address != virt.as_u64() {
                return Err(AddressSpaceError::NotMapped);
            }
            if !phys::data_leaf_is_owned(
                self.pml4_frame_phys,
                leaf.virtual_address,
                leaf.physical_address,
            ) {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
            frames.push(leaf.physical_address);
        }
        let mutation = begin_address_space_mutation(self.root_phys());
        for leaf in leaves.iter().copied() {
            let virt = VirtAddr::new(leaf.virtual_address);
            let unmapped = self
                .unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            debug_assert_eq!(unmapped.as_u64(), leaf.physical_address);
        }
        let _flushed_mutation = mutation.flush_for_reclaim();
        frames.clear();
        for leaf in leaves.iter().copied() {
            match phys::release_data_leaf(
                self.pml4_frame_phys,
                leaf.virtual_address,
                leaf.physical_address,
            ) {
                Some(phys::DataLeafRelease::FrameReusable) => {
                    frames.push(leaf.physical_address);
                }
                Some(phys::DataLeafRelease::FrameRetained) => {}
                None => panic!("unmapped pager leaf lost exact data descriptor"),
            }
        }
        free_removed_frames_exact(&frames);
        Ok(page_count)
    }

    /// Updates protection on the present subset of a prepared pager range.
    ///
    /// Missing leaves are the normal demand state and retain no PTE rights;
    /// every present leaf must still have exact pager-ledger ownership before
    /// any mutation is published.
    pub fn protect_present_prepared_pager_fault_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<usize, AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }
        validate_user_page_range(start, page_count)?;
        let page_flags = normalize_user_page_flags(flags)?;
        let mut present = 0;
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            match self.pager_fault_leaf_at(virt.as_u64()) {
                Some(leaf) => {
                    if self.translate_user(virt).map(|phys| phys.as_u64())
                        != Some(leaf.physical_address)
                    {
                        return Err(AddressSpaceError::InvalidFrameOwnership);
                    }
                    present += 1;
                }
                None if self.lookup_user_page_state(virt).is_absent() => {}
                None => return Err(AddressSpaceError::NotMapped),
            }
        }

        let _mutation = begin_address_space_mutation(self.root_phys());
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            if self.pager_fault_leaf_at(virt.as_u64()).is_some() {
                let tagged = self
                    .translate_user_with_flags(virt)
                    .is_some_and(|(_, flags)| flags.contains(ROOT_OWNED_USER_LEAF));
                self.protect_user_page(
                    virt,
                    page_flags
                        | if tagged {
                            ROOT_OWNED_USER_LEAF
                        } else {
                            PageTableFlags::empty()
                        },
                )?;
            }
        }
        Ok(present)
    }

    /// Revokes the present subset of a prepared pager range and frees only
    /// frames owned by the exact, normal-time-reserved pager-leaf ledger.
    pub fn unmap_present_prepared_pager_fault_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<usize, AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }
        validate_user_page_range(start, page_count)?;
        let range_end = pager_fault_range_end(start, page_count)?;
        let leaves = self.pager_fault_leaves_in_range(start.as_u64(), range_end, page_count)?;
        let unmapped_count = leaves.len();
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(leaves.len())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        for leaf in leaves.iter().copied() {
            if !phys::data_leaf_is_owned(
                self.pml4_frame_phys,
                leaf.virtual_address,
                leaf.physical_address,
            ) {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
            frames.push(leaf.physical_address);
        }
        let mutation = begin_address_space_mutation(self.root_phys());
        for leaf in leaves.iter().copied() {
            let virt = VirtAddr::new(leaf.virtual_address);
            let unmapped = self
                .unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            if unmapped.as_u64() != leaf.physical_address {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
        }
        let _flushed_mutation = mutation.flush_for_reclaim();
        frames.clear();
        for leaf in leaves.iter().copied() {
            match phys::release_data_leaf(
                self.pml4_frame_phys,
                leaf.virtual_address,
                leaf.physical_address,
            ) {
                Some(phys::DataLeafRelease::FrameReusable) => {
                    frames.push(leaf.physical_address);
                }
                Some(phys::DataLeafRelease::FrameRetained) => {}
                None => panic!("unmapped pager leaf lost exact data descriptor"),
            }
        }
        free_removed_frames_exact(&frames);
        Ok(unmapped_count)
    }

    fn pager_fault_leaf_at(&self, virtual_address: u64) -> Option<PagerFaultLeaf> {
        self.irq_off_pager_fault_leaf_at(VirtAddr::new(virtual_address))
    }

    fn pager_fault_leaves_in_range(
        &self,
        start: u64,
        end: u64,
        maximum_count: usize,
    ) -> Result<Vec<PagerFaultLeaf>, AddressSpaceError> {
        let mut leaves = Vec::new();
        leaves
            .try_reserve_exact(maximum_count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let Some(pdpt_phys) = read_user_table_entry(self.root_phys(), USER_PML4_INDEX)? else {
            return Ok(leaves);
        };
        // A large sparse reservation used to perform two complete four-level
        // translations per absent page. Cache the two directory decisions that
        // change only at 1-GiB and 2-MiB boundaries, then inspect each leaf PTE
        // once. The caller serializes VMA/PTE mutation for this root.
        let mut cached_p3 = usize::MAX;
        let mut pd_phys = None;
        let mut cached_p2 = usize::MAX;
        let mut pt_phys = None;
        let mut virtual_address = start;
        while virtual_address < end {
            let virt = VirtAddr::new(virtual_address);
            let p3 = p3_index(virt);
            if p3 != cached_p3 {
                pd_phys = read_user_table_entry(pdpt_phys, p3)?;
                cached_p3 = p3;
                cached_p2 = usize::MAX;
                pt_phys = None;
            }
            let p2 = p2_index(virt);
            if p2 != cached_p2 {
                pt_phys = match pd_phys {
                    Some(pd_phys) => read_user_table_entry(pd_phys, p2)?,
                    None => None,
                };
                cached_p2 = p2;
            }
            if let Some(pt_phys) = pt_phys {
                // SAFETY: `read_user_table_entry` admitted one present,
                // non-huge user table and the serialized owner keeps it alive.
                let pt = unsafe { kernel_vm::phys_to_table_ref(pt_phys) };
                let entry = &pt[p1_index(virt)];
                if !entry.is_unused() {
                    let flags = entry.flags();
                    if !flags.contains(PageTableFlags::PRESENT) {
                        return Err(AddressSpaceError::InvalidFrameOwnership);
                    }
                    if !flags.contains(ROOT_OWNED_USER_LEAF) {
                        return Err(AddressSpaceError::NotMapped);
                    }
                    if leaves.len() == maximum_count {
                        return Err(AddressSpaceError::InvalidFrameOwnership);
                    }
                    leaves.push(PagerFaultLeaf {
                        virtual_address,
                        physical_address: entry.addr().as_u64(),
                    });
                }
            }
            virtual_address = virtual_address
                .checked_add(PAGE_4KIB_U64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
        }
        Ok(leaves)
    }

    fn irq_off_pager_fault_leaf_at(&self, virt: VirtAddr) -> Option<PagerFaultLeaf> {
        let (frame, flags) = self.translate_user_with_flags(virt)?;
        flags
            .contains(ROOT_OWNED_USER_LEAF)
            .then_some(PagerFaultLeaf {
                virtual_address: virt.as_u64(),
                physical_address: frame.as_u64(),
            })
    }

    /// Collects every frame whose page-table tag must agree with a descriptor.
    ///
    /// Every owned leaf carries `ROOT_OWNED_USER_LEAF`. Every dynamically created
    /// user table carries `ROOT_OWNED_USER_TABLE`, regardless of whether a
    /// normal mapper or a fault published it. This normal-context walk is how
    /// retirement reconciles PTE evidence with the physical descriptor catalog.
    ///
    /// A non-zero intermediate entry that is not present, or a huge entry in
    /// the user subtree, is not a reclaim failure but a structural violation:
    /// nothing in this tree can produce either, so the walk reports it rather
    /// than guessing what the topology meant.
    pub(crate) fn pager_fault_ownership(&self) -> Result<PagerFaultOwnership, AddressSpaceError> {
        let mut owned = PagerFaultOwnership::empty();
        if self.pml4_frame_phys == 0 {
            return Ok(owned);
        }
        let root = self.root_table_ref();
        let Some(pdpt_phys) = tagged_child_of(&root[USER_PML4_INDEX], &mut owned.tables)? else {
            return Ok(owned);
        };
        // SAFETY: this normal-context owner walks its own page-table hierarchy
        // while the address space is provably inactive on every CPU.  VMA
        // writers serialize destructive changes externally.
        let pdpt = unsafe { kernel_vm::phys_to_table_ref(pdpt_phys) };
        for p3 in 0..ENTRIES_PER_TABLE {
            let Some(pd_phys) = tagged_child_of(&pdpt[p3], &mut owned.tables)? else {
                continue;
            };
            // SAFETY: as above for the next present non-huge owned table.
            let pd = unsafe { kernel_vm::phys_to_table_ref(pd_phys) };
            for p2 in 0..ENTRIES_PER_TABLE {
                let Some(pt_phys) = tagged_child_of(&pd[p2], &mut owned.tables)? else {
                    continue;
                };
                // SAFETY: as above for the leaf-bearing table.
                let pt = unsafe { kernel_vm::phys_to_table_ref(pt_phys) };
                for p1 in 0..ENTRIES_PER_TABLE {
                    let entry = &pt[p1];
                    if entry.is_unused() || !entry.flags().contains(ROOT_OWNED_USER_LEAF) {
                        continue;
                    }
                    if !entry.flags().contains(PageTableFlags::PRESENT) {
                        return Err(AddressSpaceError::InvalidFrameOwnership);
                    }
                    let virtual_address = ((USER_PML4_INDEX as u64) << 39)
                        | ((p3 as u64) << 30)
                        | ((p2 as u64) << 21)
                        | ((p1 as u64) << 12);
                    owned.leaves.push(PagerFaultLeaf {
                        virtual_address,
                        physical_address: entry.addr().as_u64(),
                    });
                }
            }
        }
        Ok(owned)
    }
}

fn pager_fault_range_end(start: VirtAddr, page_count: usize) -> Result<u64, AddressSpaceError> {
    let span = (page_count as u64)
        .checked_mul(PAGE_4KIB_U64)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    start
        .as_u64()
        .checked_add(span)
        .ok_or(AddressSpaceError::AddressOverflow)
}

/// Calls `f` with the atomic representation of an existing 4 KiB user leaf.
///
/// The VMA publication permit serializes topology changes with this helper.
/// Its only mutation is a zero-to-present CAS; normal VMA writers continue to
/// use `with_prepared_user_leaf_mut` under the address-space mutation guard.
fn with_prepared_user_leaf_atomic<R>(
    pml4_phys: PhysAddr,
    virt: VirtAddr,
    f: impl FnOnce(&AtomicU64) -> Result<R, AddressSpaceError>,
) -> Result<R, AddressSpaceError> {
    // SAFETY: the current root is active on this CPU and the publication permit
    // excludes concurrent topology mutation for the target prepared leaf.
    let root = unsafe { kernel_vm::phys_to_table_ref(pml4_phys) };
    let pml4_entry = &root[p4_index(virt)];
    if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: the permit keeps each prepared parent stable until the CAS ends.
    let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
    let pdpt_entry = &pdpt[p3_index(virt)];
    if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: as above for the next prepared non-huge parent.
    let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
    let pd_entry = &pd[p2_index(virt)];
    if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: the prepared PTE is a repr(transparent) `u64` and naturally
    // aligned.  Only this CAS path accesses it atomically while the permit is
    // held; normal writers first withdraw and drain that permit.
    let pt = unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) };
    let raw = &pt[p1_index(virt)] as *const PageTableEntry as *mut u64;
    // SAFETY: `raw` is the naturally aligned u64 storage of that prepared PTE,
    // and the permit forces every concurrent fault mutation through atomics.
    let leaf = unsafe { AtomicU64::from_ptr(raw) };
    f(leaf)
}

/// Calls `f` with an existing 4 KiB user leaf entry.
fn with_prepared_user_leaf_mut<R>(
    pml4_phys: PhysAddr,
    virt: VirtAddr,
    f: impl FnOnce(&mut PageTableEntry) -> Result<R, AddressSpaceError>,
) -> Result<R, AddressSpaceError> {
    // SAFETY: the caller serializes this address space and supplied its exact
    // owned root. The hierarchy below is validated before each direct-map use.
    let root = unsafe { kernel_vm::phys_to_table_mut(pml4_phys) };
    let pml4_entry = &mut root[p4_index(virt)];
    if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: the present non-huge parent entry was installed by the same
    // address space and remains stable for this serialized PTE update.
    let pdpt = unsafe { kernel_vm::phys_to_table_mut(pml4_entry.addr()) };
    let pdpt_entry = &mut pdpt[p3_index(virt)];
    if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: as above, this present non-huge entry names an owned user table.
    let pd = unsafe { kernel_vm::phys_to_table_mut(pdpt_entry.addr()) };
    let pd_entry = &mut pd[p2_index(virt)];
    if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: a prepared leaf requires this final present non-huge page table.
    let pt = unsafe { kernel_vm::phys_to_table_mut(pd_entry.addr()) };
    f(&mut pt[p1_index(virt)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloned_leaf_flags_drop_the_root_owned_user_leaf_tag() {
        let tagged = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::USER_ACCESSIBLE
            | ROOT_OWNED_USER_LEAF;
        let cloned = without_root_owned_user_leaf_tag(tagged);
        assert!(!cloned.contains(ROOT_OWNED_USER_LEAF));
        assert_eq!(cloned, tagged.difference(ROOT_OWNED_USER_LEAF));
    }

    #[test]
    fn the_two_software_ownership_tags_are_distinct_available_bits() {
        assert_ne!(ROOT_OWNED_USER_LEAF, ROOT_OWNED_USER_TABLE);
        assert_ne!(ROOT_OWNED_USER_LEAF, COW_USER_LEAF);
        assert_ne!(ROOT_OWNED_USER_TABLE, COW_USER_LEAF);
        // Neither may overlap a bit the translation or protection decision
        // reads, or the tag would change what the hardware does.
        let hardware = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::WRITE_THROUGH
            | PageTableFlags::NO_CACHE
            | PageTableFlags::HUGE_PAGE
            | PageTableFlags::GLOBAL
            | PageTableFlags::NO_EXECUTE;
        assert!(!hardware.intersects(ROOT_OWNED_USER_LEAF));
        assert!(!hardware.intersects(ROOT_OWNED_USER_TABLE));
        assert!(!hardware.intersects(COW_USER_LEAF));
    }

    #[test]
    fn a_published_table_entry_must_be_a_present_non_huge_user_table() {
        let table = 0x0000_0000_0020_1000_u64;
        let published = table | user_table_flags().bits();
        assert_eq!(
            classify_user_table_entry(published),
            Ok(PhysAddr::new(table))
        );
        assert_eq!(
            classify_user_table_entry(published | PageTableFlags::HUGE_PAGE.bits()),
            Err(AddressSpaceError::HugePageConflict)
        );
        let without_user_rights =
            table | (PageTableFlags::PRESENT | PageTableFlags::WRITABLE).bits();
        assert_eq!(
            classify_user_table_entry(without_user_rights),
            Err(AddressSpaceError::ProtectionViolation)
        );
        // The ownership tag must change neither the verdict nor the address.
        assert_eq!(
            classify_user_table_entry(published | ROOT_OWNED_USER_TABLE.bits()),
            Ok(PhysAddr::new(table))
        );
    }

    #[test]
    fn an_absent_level_skips_its_whole_span_not_one_table() {
        // The point of the boundary walk: an untouched 512 GiB reservation is
        // one directory read, not one read per 2 MiB block.
        let aligned = VirtAddr::new(USER_SPACE_BASE);
        assert_eq!(pages_to_level_boundary(aligned, PD_SHIFT), 512);
        assert_eq!(pages_to_level_boundary(aligned, PDPT_SHIFT), 512 * 512);
        assert_eq!(
            pages_to_level_boundary(aligned, PML4_SHIFT),
            512 * 512 * 512
        );
    }

    #[test]
    fn a_partial_block_skips_only_to_its_own_boundary() {
        let offset = VirtAddr::new(USER_SPACE_BASE + PAGE_4KIB_U64);
        assert_eq!(pages_to_level_boundary(offset, PD_SHIFT), 511);
        assert_eq!(pages_to_level_boundary(offset, PDPT_SHIFT), 512 * 512 - 1);
    }

    #[test]
    fn pager_fault_range_end_preserves_page_exactness() {
        let start = VirtAddr::new(USER_SPACE_BASE + PAGE_4KIB_U64);
        assert_eq!(
            pager_fault_range_end(start, 3),
            Ok(USER_SPACE_BASE + 4 * PAGE_4KIB_U64)
        );
    }
}
