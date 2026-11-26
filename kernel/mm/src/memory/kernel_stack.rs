//! Page-granular virtual kernel stacks with unmapped hardware guard zones and
//! demand-committed physical pages.
//!
//! - **Owner:** `kernel-mm` owns virtual stack slots in `PML4[259]`, their
//!   backing page tables (`stack_pdp`), physical frame commit/release, and
//!   TLB shootdown prior to slot/frame reuse.
//! - **Boundary:** Callers supply only desired usable bytes and task metadata;
//!   guard geometry and PTE state remain private to `kernel-mm`.
//! - **Lifecycle:** `alloc_kernel_stack` reserves a virtual slot, initializes
//!   page tables up to the leaf PT, pre-commits the top `STACK_PRECOMMIT_BYTES`
//!   zeroed pages, and leaves lower usable pages uncommitted (`!PRESENT`) and
//!   the guard zone permanently unmapped. `#PF` demand-commits uncommitted
//!   usable pages or diagnoses guard faults. `Drop` unmaps all committed leaf
//!   PTEs, runs a global TLB shootdown, frees frames, and returns the virtual slot.
//! - **Concurrency:** `STACK_VM_LOCK` serializes only virtual slot reuse and
//!   PDPT/PD/PT installation, never nested across `phys::alloc_frame`, `Box`,
//!   or TLB shootdown. `#PF` and `#DF` inspect chunked `SlotMeta` and leaf PTEs
//!   completely lock-free using pre-zeroed emergency frames if locks are held.
//! - **Failure:** Exhaustion of virtual slots or physical frames returns `None`;
//!   access to the guard zone or inactive slot triggers a diagnostic panic.
//! - **Forbidden:** Never allocate heap memory inside `#PF`/`#DF`, never
//!   dereference unmapped guard pages, and never free physical frames before
//!   global TLB shootdown completes.
//! - **Evidence:** Unit tests in `kernel_stack::tests` and KVM guard-fault runs.

#[cfg(any(rustos_boot_image, test))]
use alloc::boxed::Box;
#[cfg(not(rustos_boot_image))]
use alloc::vec::Vec;
#[cfg(any(rustos_boot_image, test))]
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
#[cfg(rustos_boot_image)]
use core::sync::atomic::AtomicPtr;

#[cfg(rustos_boot_image)]
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
#[cfg(rustos_boot_image)]
use x86_64::PhysAddr;
#[cfg(rustos_boot_image)]
use x86_64::VirtAddr;
#[cfg(rustos_boot_image)]
use x86_64::structures::paging::PageTableFlags;

pub const KERNEL_STACK_PAGE_SIZE: usize = 4096;
/// Virtual unmapped guard zone below every kernel stack (`64` x 4 KiB pages).
pub const KERNEL_STACK_GUARD_BYTES: usize = 256 * 1024;
/// Usable virtual span per kernel stack (`64` x 4 KiB pages = 256 KiB).
pub const KERNEL_STACK_DEFAULT_USABLE_BYTES: usize = 256 * 1024;
/// Virtual slot stride (`512 KiB`, aligned cleanly to 4 slots per 2 MiB PT).
pub const KERNEL_STACK_SLOT_STRIDE: usize =
    KERNEL_STACK_GUARD_BYTES + KERNEL_STACK_DEFAULT_USABLE_BYTES;
/// Initial physical RAM committed at the top of each new kernel stack (`32 KiB`).
pub const KERNEL_STACK_PRECOMMIT_BYTES: usize = 32 * 1024;

#[cfg(rustos_boot_image)]
const MAX_USABLE_PAGES: usize = KERNEL_STACK_DEFAULT_USABLE_BYTES / KERNEL_STACK_PAGE_SIZE;
#[cfg(any(rustos_boot_image, test))]
const CHUNK_SIZE: usize = 256;
#[cfg(rustos_boot_image)]
const NUM_CHUNKS: usize = 1024;
#[cfg(rustos_boot_image)]
const MAX_STACK_SLOTS: usize = CHUNK_SIZE * NUM_CHUNKS;
#[cfg(any(rustos_boot_image, test))]
const NO_FREE_SLOT: u32 = u32::MAX;

#[cfg(any(rustos_boot_image, test))]
const SLOT_FREE: u8 = 0;
#[cfg(rustos_boot_image)]
const SLOT_ACTIVE: u8 = 1;
#[cfg(rustos_boot_image)]
const SLOT_RECLAIMING: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelStackFaultDiagnostic {
    pub vslot: u32,
    pub task_id: u64,
    pub task_slot: u32,
    pub guard_base: u64,
    pub usable_base: u64,
    pub usable_top: u64,
    pub committed_pages: u32,
    pub is_guard_page: bool,
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelStackFaultOutcome {
    Resumed,
    GuardOrInvalid(KernelStackFaultDiagnostic),
    NotKernelStack,
}

#[cfg(any(rustos_boot_image, test))]
#[repr(C)]
struct SlotMeta {
    state: AtomicU8,
    pt_phys: AtomicU64,
    task_id: AtomicU64,
    task_slot: AtomicU32,
    usable_len: AtomicU32,
    committed_pages: AtomicU32,
    next_free: AtomicU32,
}

#[cfg(any(rustos_boot_image, test))]
struct MetaChunk {
    slots: [SlotMeta; CHUNK_SIZE],
}

#[cfg(any(rustos_boot_image, test))]
impl MetaChunk {
    fn boxed() -> Box<Self> {
        let layout = alloc::alloc::Layout::new::<Self>();
        // SAFETY: `MetaChunk` is an array of all-zero-valid atomics except
        // `next_free = NO_FREE_SLOT` (`u32::MAX`), initialized in-place below.
        let raw = unsafe { alloc::alloc::alloc_zeroed(layout) }.cast::<Self>();
        if raw.is_null() {
            alloc::alloc::handle_alloc_error(layout);
        }
        // SAFETY: `raw` is a fresh unique allocation properly aligned for `MetaChunk`.
        unsafe {
            for slot in &(*raw).slots {
                slot.next_free.store(NO_FREE_SLOT, Ordering::Relaxed);
            }
            Box::from_raw(raw)
        }
    }
}

#[cfg(rustos_boot_image)]
static META_CHUNKS: [AtomicPtr<MetaChunk>; NUM_CHUNKS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; NUM_CHUNKS];

#[cfg(rustos_boot_image)]
fn meta_for(vslot: usize) -> Option<&'static SlotMeta> {
    if vslot >= MAX_STACK_SLOTS {
        return None;
    }
    // ORDERING: Acquire pairs with Release chunk publication in `ensure_meta_chunk`.
    let ptr = META_CHUNKS[vslot / CHUNK_SIZE].load(Ordering::Acquire);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: Chunks published in `META_CHUNKS` remain valid for `'static`.
    Some(unsafe { &(*ptr).slots[vslot % CHUNK_SIZE] })
}

#[cfg(rustos_boot_image)]
fn ensure_meta_chunk(vslot: usize) -> Option<&'static SlotMeta> {
    if vslot >= MAX_STACK_SLOTS {
        return None;
    }
    let chunk_idx = vslot / CHUNK_SIZE;
    // ORDERING: Acquire pairs with Release chunk publication below.
    let existing = META_CHUNKS[chunk_idx].load(Ordering::Acquire);
    let ptr = if !existing.is_null() {
        existing
    } else {
        let candidate = Box::into_raw(MetaChunk::boxed());
        // ORDERING: AcqRel publishes initialized chunk contents to lock-free readers.
        match META_CHUNKS[chunk_idx].compare_exchange(
            core::ptr::null_mut(),
            candidate,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => candidate,
            Err(winner) => {
                // SAFETY: `candidate` lost publication race and is uniquely owned.
                unsafe {
                    drop(Box::from_raw(candidate));
                }
                winner
            }
        }
    };
    // SAFETY: `ptr` points to a permanently published `MetaChunk`.
    Some(unsafe { &(*ptr).slots[vslot % CHUNK_SIZE] })
}

#[cfg(rustos_boot_image)]
struct StackVmAllocator {
    free_head: u32,
    next_vslot: u32,
}

#[cfg(rustos_boot_image)]
impl StackVmAllocator {
    const fn new() -> Self {
        Self {
            free_head: NO_FREE_SLOT,
            next_vslot: 0,
        }
    }
}

#[cfg(rustos_boot_image)]
static STACK_VM_ALLOCATOR: TrackedSpinLock<StackVmAllocator, { LockClass::KernelStackVm as u8 }> =
    TrackedSpinLock::new(StackVmAllocator::new());

#[cfg(rustos_boot_image)]
const EMERGENCY_RESERVE_SLOTS: usize = 64;
#[cfg(rustos_boot_image)]
const EMERGENCY_RESERVE_TARGET: usize = 24;
#[cfg(rustos_boot_image)]
static EMERGENCY_FRAMES: [AtomicU64; EMERGENCY_RESERVE_SLOTS] =
    [const { AtomicU64::new(0) }; EMERGENCY_RESERVE_SLOTS];

#[cfg(rustos_boot_image)]
fn zero_phys_page(phys: PhysAddr) {
    let ptr = crate::memory::kernel_vm::higher_half_addr(phys.as_u64()) as *mut u8;
    // SAFETY: `phys` is a freshly claimed 4 KiB frame accessible through the higher-half direct map.
    unsafe {
        ptr.write_bytes(0, KERNEL_STACK_PAGE_SIZE);
    }
}

#[cfg(rustos_boot_image)]
fn replenish_emergency_frames() {
    let mut current = 0usize;
    for slot in &EMERGENCY_FRAMES {
        if slot.load(Ordering::Relaxed) != 0 {
            current += 1;
        }
    }
    while current < EMERGENCY_RESERVE_TARGET {
        let Some(phys) = crate::memory::phys::alloc_frame() else {
            break;
        };
        zero_phys_page(phys);
        let mut stored = false;
        for slot in &EMERGENCY_FRAMES {
            if slot.load(Ordering::Relaxed) == 0 {
                // ORDERING: AcqRel publishes zeroed page contents before any #PF pops it.
                if slot
                    .compare_exchange(0, phys.as_u64(), Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    stored = true;
                    current += 1;
                    break;
                }
            }
        }
        if !stored {
            crate::memory::phys::free_frame(phys);
            break;
        }
    }
}

#[cfg(rustos_boot_image)]
fn pop_emergency_zeroed_frame() -> Option<PhysAddr> {
    for slot in &EMERGENCY_FRAMES {
        if slot.load(Ordering::Relaxed) != 0 {
            // ORDERING: AcqRel claims unique ownership of the pre-zeroed frame.
            let raw = slot.swap(0, Ordering::AcqRel);
            if raw != 0 {
                return Some(PhysAddr::new(raw));
            }
        }
    }
    None
}

#[cfg(rustos_boot_image)]
fn alloc_vslot() -> Option<usize> {
    let mut guard = STACK_VM_ALLOCATOR.lock();
    if guard.free_head != NO_FREE_SLOT {
        let vslot = guard.free_head as usize;
        let next = meta_for(vslot)
            .map(|meta| meta.next_free.swap(NO_FREE_SLOT, Ordering::Relaxed))
            .unwrap_or(NO_FREE_SLOT);
        guard.free_head = next;
        return Some(vslot);
    }
    let next = guard.next_vslot as usize;
    if next >= MAX_STACK_SLOTS {
        return None;
    }
    guard.next_vslot = (next + 1) as u32;
    Some(next)
}

#[cfg(rustos_boot_image)]
fn free_vslot(vslot: usize) {
    let meta = meta_for(vslot).expect("freed stack vslot lacks metadata");
    let mut guard = STACK_VM_ALLOCATOR.lock();
    meta.next_free.store(guard.free_head, Ordering::Relaxed);
    guard.free_head = vslot as u32;
}

#[cfg(rustos_boot_image)]
#[inline]
pub const fn slot_base_addr(vslot: usize) -> u64 {
    crate::memory::kernel_vm::KERNEL_STACK_WINDOW_BASE + (vslot as u64) * (KERNEL_STACK_SLOT_STRIDE as u64)
}

#[cfg(rustos_boot_image)]
#[inline]
fn decode_stack_addr(addr: u64) -> Option<(usize, usize)> {
    let base = crate::memory::kernel_vm::KERNEL_STACK_WINDOW_BASE;
    let end = crate::memory::kernel_vm::KERNEL_STACK_WINDOW_END;
    if addr < base || addr >= end {
        return None;
    }
    let rel = (addr - base) as usize;
    let vslot = rel / KERNEL_STACK_SLOT_STRIDE;
    let offset = rel % KERNEL_STACK_SLOT_STRIDE;
    if vslot >= MAX_STACK_SLOTS {
        return None;
    }
    Some((vslot, offset))
}

#[cfg(rustos_boot_image)]
fn ensure_pt_for_slot(vslot: usize, meta: &SlotMeta) -> Option<PhysAddr> {
    // ORDERING: Acquire pairs with Release `pt_phys` publication below.
    let existing = meta.pt_phys.load(Ordering::Acquire);
    if existing != 0 {
        return Some(PhysAddr::new(existing));
    }
    let pdpt_phys = crate::memory::kernel_vm::stack_pdp_phys()?;
    let offset = (vslot as u64) * (KERNEL_STACK_SLOT_STRIDE as u64);
    let pdpt_idx = ((offset >> 30) & 511) as usize;
    let pd_idx = ((offset >> 21) & 511) as usize;

    // Pre-allocate candidate PD / PT frames outside `STACK_VM_LOCK` so we never
    // nest `KernelStackVm` across `PhysicalFrameCache`/`PhysicalAllocator`.
    let spare_pd = crate::memory::phys::alloc_frame()?;
    zero_phys_page(spare_pd);
    let spare_pt = match crate::memory::phys::alloc_frame() {
        Some(frame) => frame,
        None => {
            crate::memory::phys::free_frame(spare_pd);
            return None;
        }
    };
    zero_phys_page(spare_pt);

    let (pt_phys, used_spare_pd, used_spare_pt) = {
        let _guard = STACK_VM_ALLOCATOR.lock();
        let table_flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        // SAFETY: `pdpt_phys` is the shared boot-initialized `stack_pdp` and `STACK_VM_ALLOCATOR`
        // serializes PDPT/PD population.
        let pdpt = unsafe { crate::memory::kernel_vm::phys_to_table_mut(pdpt_phys) };
        let (pd_phys, used_pd) = if pdpt[pdpt_idx].is_unused() {
            pdpt[pdpt_idx].set_addr(spare_pd, table_flags);
            (spare_pd, true)
        } else {
            (pdpt[pdpt_idx].addr(), false)
        };
        // SAFETY: `pd_phys` points to the 4 KiB PD frame linked above.
        let pd = unsafe { crate::memory::kernel_vm::phys_to_table_mut(pd_phys) };
        let (pt_phys, used_pt) = if pd[pd_idx].is_unused() {
            pd[pd_idx].set_addr(spare_pt, table_flags);
            (spare_pt, true)
        } else {
            (pd[pd_idx].addr(), false)
        };
        (pt_phys, used_pd, used_pt)
    };

    if !used_spare_pd {
        crate::memory::phys::free_frame(spare_pd);
    }
    if !used_spare_pt {
        crate::memory::phys::free_frame(spare_pt);
    }
    // ORDERING: Release publishes `pt_phys` to future fault/guard readers.
    meta.pt_phys.store(pt_phys.as_u64(), Ordering::Release);
    Some(pt_phys)
}

#[cfg(rustos_boot_image)]
#[inline]
unsafe fn leaf_pte_atomic(pt_phys: PhysAddr, virt_addr: u64) -> &'static AtomicU64 {
    let pt_virt = crate::memory::kernel_vm::higher_half_addr(pt_phys.as_u64());
    let pte_idx = ((virt_addr >> 12) & 511) as usize;
    // SAFETY: Caller guarantees `pt_phys` is a valid aligned leaf PageTable mapped via direct map.
    unsafe { &*(pt_virt as *const AtomicU64).add(pte_idx) }
}

/// Owned page-granular kernel stack backed by `PML4[259]` in bare-metal builds
/// and a heap byte buffer in host unit tests.
pub struct KernelStack {
    #[cfg(rustos_boot_image)]
    vslot: u32,
    #[cfg(rustos_boot_image)]
    usable_base: u64,
    #[cfg(rustos_boot_image)]
    usable_len: usize,
    #[cfg(rustos_boot_image)]
    pt_phys: u64,
    #[cfg(not(rustos_boot_image))]
    storage: Vec<u8>,
}

impl KernelStack {
    /// Allocates a guarded kernel stack with `usable_bytes` usable capacity.
    #[cfg(rustos_boot_image)]
    pub fn allocate(task_id: u64, task_slot: u32, usable_bytes: usize) -> Option<Self> {
        let usable_len = usable_bytes
            .next_multiple_of(KERNEL_STACK_PAGE_SIZE)
            .clamp(KERNEL_STACK_PRECOMMIT_BYTES, KERNEL_STACK_DEFAULT_USABLE_BYTES);
        replenish_emergency_frames();
        let vslot = alloc_vslot()?;
        let Some(meta) = ensure_meta_chunk(vslot) else {
            free_vslot(vslot);
            return None;
        };
        let Some(pt_phys) = ensure_pt_for_slot(vslot, meta) else {
            free_vslot(vslot);
            return None;
        };

        let slot_base = slot_base_addr(vslot);
        let usable_top = slot_base + KERNEL_STACK_SLOT_STRIDE as u64;
        let usable_base = usable_top - usable_len as u64;
        let precommit_pages = KERNEL_STACK_PRECOMMIT_BYTES / KERNEL_STACK_PAGE_SIZE;
        let precommit_base = usable_top - KERNEL_STACK_PRECOMMIT_BYTES as u64;
        let leaf_flags = (PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::NO_EXECUTE)
            .bits();

        let mut mapped_pages = 0usize;
        for page_idx in 0..precommit_pages {
            let Some(frame) = crate::memory::phys::alloc_frame() else {
                // Roll back partially pre-committed frames before freeing vslot.
                for rollback_idx in 0..mapped_pages {
                    let page_va = precommit_base + (rollback_idx * KERNEL_STACK_PAGE_SIZE) as u64;
                    // SAFETY: `pt_phys` is mapped and active for `vslot`.
                    let pte = unsafe { leaf_pte_atomic(pt_phys, page_va) };
                    // ORDERING: AcqRel unmaps the rollback PTE before freeing the frame.
                    let raw = pte.swap(0, Ordering::AcqRel);
                    let phys = raw & 0x000f_ffff_ffff_f000;
                    if phys != 0 {
                        x86_64::instructions::tlb::flush(VirtAddr::new(page_va));
                        crate::memory::phys::free_frame(PhysAddr::new(phys));
                    }
                }
                free_vslot(vslot);
                return None;
            };
            zero_phys_page(frame);
            let page_va = precommit_base + (page_idx * KERNEL_STACK_PAGE_SIZE) as u64;
            // SAFETY: `pt_phys` is the PT frame covering `vslot` and no other owner holds `vslot`.
            let pte = unsafe { leaf_pte_atomic(pt_phys, page_va) };
            // ORDERING: Release ensures zeroed page contents are visible before PTE PRESENT.
            pte.store(frame.as_u64() | leaf_flags, Ordering::Release);
            x86_64::instructions::tlb::flush(VirtAddr::new(page_va));
            mapped_pages += 1;
        }

        meta.task_id.store(task_id, Ordering::Relaxed);
        meta.task_slot.store(task_slot, Ordering::Relaxed);
        meta.usable_len.store(usable_len as u32, Ordering::Relaxed);
        meta.committed_pages
            .store(precommit_pages as u32, Ordering::Relaxed);
        // ORDERING: Release publishes task metadata & precommitted stack before SLOT_ACTIVE.
        meta.state.store(SLOT_ACTIVE, Ordering::Release);

        Some(Self {
            vslot: vslot as u32,
            usable_base,
            usable_len,
            pt_phys: pt_phys.as_u64(),
        })
    }

    #[cfg(not(rustos_boot_image))]
    pub fn allocate(_task_id: u64, _task_slot: u32, usable_bytes: usize) -> Option<Self> {
        let len = usable_bytes.max(KERNEL_STACK_PAGE_SIZE);
        let mut storage = Vec::new();
        storage.try_reserve_exact(len).ok()?;
        storage.resize(len, 0);
        Some(Self { storage })
    }

    #[cfg(not(rustos_boot_image))]
    pub fn from_host_vec(storage: Vec<u8>) -> Self {
        Self { storage }
    }

    #[cfg(not(rustos_boot_image))]
    pub fn into_host_vec(mut self) -> Vec<u8> {
        core::mem::take(&mut self.storage)
    }

    #[cfg(not(rustos_boot_image))]
    pub fn as_host_slice(&self) -> &[u8] {
        &self.storage
    }

    #[cfg(not(rustos_boot_image))]
    pub fn as_host_mut_slice(&mut self) -> &mut [u8] {
        &mut self.storage
    }

    #[cfg(not(rustos_boot_image))]
    pub fn host_used_bytes(&self, canary_guard_bytes: usize) -> Option<usize> {
        used_bytes_in_slice(&self.storage, canary_guard_bytes)
    }

    #[inline]
    pub fn usable_base(&self) -> u64 {
        #[cfg(rustos_boot_image)]
        {
            self.usable_base
        }
        #[cfg(not(rustos_boot_image))]
        {
            self.storage.as_ptr() as u64
        }
    }

    #[inline]
    pub fn usable_len(&self) -> usize {
        #[cfg(rustos_boot_image)]
        {
            self.usable_len
        }
        #[cfg(not(rustos_boot_image))]
        {
            self.storage.len()
        }
    }

    #[inline]
    pub fn usable_top(&self) -> u64 {
        self.usable_base() + self.usable_len() as u64
    }

    /// Scans only committed (`PRESENT`) stack pages from bottom to top, never
    /// faulting on uncommitted usable pages or the unmapped guard zone.
    pub fn used_bytes(&self) -> Option<usize> {
        #[cfg(rustos_boot_image)]
        {
            let pt_phys = PhysAddr::new(self.pt_phys);
            let present_bit = PageTableFlags::PRESENT.bits();
            let pages = self.usable_len / KERNEL_STACK_PAGE_SIZE;
            for page_idx in 0..pages {
                let page_va = self.usable_base + (page_idx * KERNEL_STACK_PAGE_SIZE) as u64;
                // SAFETY: `pt_phys` is valid for the lifetime of `&self`.
                let pte = unsafe { leaf_pte_atomic(pt_phys, page_va) };
                // ORDERING: Acquire pairs with PTE publication on commit.
                let raw = pte.load(Ordering::Acquire);
                if raw & present_bit == 0 {
                    continue;
                }
                let phys = raw & 0x000f_ffff_ffff_f000;
                let direct_va = crate::memory::kernel_vm::higher_half_addr(phys) as *const u8;
                // SAFETY: `phys` is a live committed 4 KiB frame mapped into the direct map.
                let slice = unsafe { core::slice::from_raw_parts(direct_va, KERNEL_STACK_PAGE_SIZE) };
                if let Some(within) = slice.iter().position(|&b| b != 0) {
                    let first_nonzero_va = page_va + within as u64;
                    return Some((self.usable_top() - first_nonzero_va) as usize);
                }
            }
            None
        }
        #[cfg(not(rustos_boot_image))]
        {
            used_bytes_in_slice(&self.storage, 0)
        }
    }

    /// Rebinds diagnostic `(task_id, task_slot)` metadata without reallocating virtual pages.
    #[cfg(rustos_boot_image)]
    pub fn update_task_binding(&self, task_id: u64, task_slot: u32) {
        if let Some(meta) = meta_for(self.vslot as usize) {
            meta.task_id.store(task_id, Ordering::Relaxed);
            meta.task_slot.store(task_slot, Ordering::Relaxed);
        }
    }

    #[cfg(not(rustos_boot_image))]
    pub fn update_task_binding(&self, _task_id: u64, _task_slot: u32) {}
}

#[cfg(rustos_boot_image)]
impl Drop for KernelStack {
    fn drop(&mut self) {
        let vslot = self.vslot as usize;
        let meta = meta_for(vslot).expect("dropped stack lacks metadata");
        // ORDERING: Release marks the slot reclaiming before tearing down PTEs.
        meta.state.store(SLOT_RECLAIMING, Ordering::Release);

        let pt_phys = PhysAddr::new(self.pt_phys);
        let mut reclaimed = [PhysAddr::zero(); MAX_USABLE_PAGES];
        let mut reclaimed_count = 0usize;
        let pages = (self.usable_len / KERNEL_STACK_PAGE_SIZE).min(MAX_USABLE_PAGES);
        let present_bit = PageTableFlags::PRESENT.bits();

        let mutation = kernel_hal::api::arch::tlb::begin_global_mapping_mutation();
        for page_idx in 0..pages {
            let page_va = self.usable_base + (page_idx * KERNEL_STACK_PAGE_SIZE) as u64;
            // SAFETY: `pt_phys` is permanently valid and owned exclusively during drop.
            let pte = unsafe { leaf_pte_atomic(pt_phys, page_va) };
            // ORDERING: AcqRel unmaps the page before shootdown & physical free.
            let raw = pte.swap(0, Ordering::AcqRel);
            if raw & present_bit != 0 {
                let phys = raw & 0x000f_ffff_ffff_f000;
                if phys != 0 {
                    reclaimed[reclaimed_count] = PhysAddr::new(phys);
                    reclaimed_count += 1;
                }
            }
        }
        let flushed = mutation.flush_for_reclaim();
        drop(flushed);

        if reclaimed_count != 0 {
            let mut failures = [(
                PhysAddr::zero(),
                crate::memory::phys::FreeFrameError::AlreadyFree,
            ); MAX_USABLE_PAGES];
            let failed = crate::memory::phys::try_free_frames_batch(
                &reclaimed[..reclaimed_count],
                &mut failures,
            );
            assert_eq!(
                failed,
                0,
                "kernel stack drop failed to free {failed} reclaimed frame(s): {:?}",
                &failures[..failed.min(failures.len())]
            );
        }

        meta.task_id.store(0, Ordering::Relaxed);
        meta.task_slot.store(0, Ordering::Relaxed);
        meta.usable_len.store(0, Ordering::Relaxed);
        meta.committed_pages.store(0, Ordering::Relaxed);
        // ORDERING: Release completes teardown before returning `vslot` to the free list.
        meta.state.store(SLOT_FREE, Ordering::Release);
        free_vslot(vslot);
    }
}

/// Checks lock-free whether the guard page immediately below `usable_base` is
/// non-present in `PML4[259]` for a live virtual stack.
pub fn guard_page_unmapped(usable_base: u64) -> bool {
    #[cfg(rustos_boot_image)]
    {
        let Some(guard_page) = usable_base.checked_sub(KERNEL_STACK_PAGE_SIZE as u64) else {
            return false;
        };
        let Some((vslot, _)) = decode_stack_addr(guard_page) else {
            return false;
        };
        let Some(meta) = meta_for(vslot) else {
            return false;
        };
        // ORDERING: Acquire observes slot state and `pt_phys` published at creation.
        if meta.state.load(Ordering::Acquire) != SLOT_ACTIVE {
            return false;
        }
        let pt_raw = meta.pt_phys.load(Ordering::Acquire);
        if pt_raw == 0 {
            return false;
        }
        // SAFETY: `pt_raw` is a permanently valid `stack_pdp` leaf PT frame.
        let pte = unsafe { leaf_pte_atomic(PhysAddr::new(pt_raw), guard_page) };
        // ORDERING: Acquire observes latest PTE flags.
        pte.load(Ordering::Acquire) & PageTableFlags::PRESENT.bits() == 0
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = usable_base;
        true
    }
}

/// Lock-free address inspection for `#PF` and `#DF` diagnostics.
pub fn describe_fault_address(addr: u64) -> Option<KernelStackFaultDiagnostic> {
    #[cfg(rustos_boot_image)]
    {
        let (vslot, _offset) = decode_stack_addr(addr)?;
        let slot_base = slot_base_addr(vslot);
        let usable_top = slot_base + KERNEL_STACK_SLOT_STRIDE as u64;
        let meta = meta_for(vslot)?;
        // ORDERING: Acquire pairs with Release state/binding stores.
        let state = meta.state.load(Ordering::Acquire);
        let usable_len = meta.usable_len.load(Ordering::Relaxed) as u64;
        let usable_base = if usable_len != 0 && usable_len <= KERNEL_STACK_DEFAULT_USABLE_BYTES as u64 {
            usable_top - usable_len
        } else {
            slot_base + KERNEL_STACK_GUARD_BYTES as u64
        };
        Some(KernelStackFaultDiagnostic {
            vslot: vslot as u32,
            task_id: meta.task_id.load(Ordering::Relaxed),
            task_slot: meta.task_slot.load(Ordering::Relaxed),
            guard_base: slot_base,
            usable_base,
            usable_top,
            committed_pages: meta.committed_pages.load(Ordering::Relaxed),
            is_guard_page: addr >= slot_base && addr < usable_base,
            active: state == SLOT_ACTIVE,
        })
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = addr;
        None
    }
}

/// Handles supervisor `#PF` on `PML4[259]` kernel stack addresses.
pub fn handle_page_fault(error_code: Option<u64>, cr2: u64) -> KernelStackFaultOutcome {
    #[cfg(rustos_boot_image)]
    {
        let Some((vslot, _)) = decode_stack_addr(cr2) else {
            return KernelStackFaultOutcome::NotKernelStack;
        };
        let Some(diag) = describe_fault_address(cr2) else {
            return KernelStackFaultOutcome::GuardOrInvalid(KernelStackFaultDiagnostic {
                vslot: vslot as u32,
                task_id: 0,
                task_slot: 0,
                guard_base: slot_base_addr(vslot),
                usable_base: slot_base_addr(vslot) + KERNEL_STACK_GUARD_BYTES as u64,
                usable_top: slot_base_addr(vslot) + KERNEL_STACK_SLOT_STRIDE as u64,
                committed_pages: 0,
                is_guard_page: false,
                active: false,
            });
        };
        if !diag.active || diag.is_guard_page || cr2 < diag.usable_base || cr2 >= diag.usable_top {
            return KernelStackFaultOutcome::GuardOrInvalid(diag);
        }
        const PRESENT: u64 = 1 << 0;
        const RESERVED: u64 = 1 << 3;
        const INSTRUCTION: u64 = 1 << 4;
        const PROTECTION_KEY: u64 = 1 << 5;
        const SHADOW_STACK: u64 = 1 << 6;
        const SGX: u64 = 1 << 15;
        if let Some(err) = error_code
            && err & (RESERVED | INSTRUCTION | PROTECTION_KEY | SHADOW_STACK | SGX) != 0
        {
            return KernelStackFaultOutcome::GuardOrInvalid(diag);
        }

        let meta = meta_for(vslot).expect("active stack metadata disappeared");
        // ORDERING: Acquire observes `pt_phys` published at creation.
        let pt_raw = meta.pt_phys.load(Ordering::Acquire);
        if pt_raw == 0 {
            return KernelStackFaultOutcome::GuardOrInvalid(diag);
        }
        let page_va = cr2 & !(KERNEL_STACK_PAGE_SIZE as u64 - 1);
        // SAFETY: `pt_raw` points to the active PT frame for `vslot`.
        let pte = unsafe { leaf_pte_atomic(PhysAddr::new(pt_raw), page_va) };
        // ORDERING: Acquire checks whether another access already mapped `page_va`.
        if pte.load(Ordering::Acquire) & PRESENT != 0 {
            if error_code.is_some_and(|e| e & PRESENT != 0) {
                return KernelStackFaultOutcome::GuardOrInvalid(diag);
            }
            x86_64::instructions::tlb::flush(VirtAddr::new(page_va));
            return KernelStackFaultOutcome::Resumed;
        }

        let can_alloc_direct = nucleus_core::util::lockdep::held_spin_lock_depth() == 0
            && nucleus_core::util::lockdep::irq_context_depth() == 0;
        let frame = if can_alloc_direct {
            match crate::memory::phys::alloc_frame() {
                Some(phys) => {
                    zero_phys_page(phys);
                    Some(phys)
                }
                None => pop_emergency_zeroed_frame(),
            }
        } else {
            pop_emergency_zeroed_frame().or_else(|| {
                let phys = crate::memory::phys::alloc_frame()?;
                zero_phys_page(phys);
                Some(phys)
            })
        };
        let Some(frame) = frame else {
            return KernelStackFaultOutcome::GuardOrInvalid(diag);
        };

        let leaf_flags = (PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::NO_EXECUTE)
            .bits();
        // ORDERING: AcqRel publishes zeroed page contents before PTE becomes present.
        match pte.compare_exchange(0, frame.as_u64() | leaf_flags, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                meta.committed_pages.fetch_add(1, Ordering::Relaxed);
                x86_64::instructions::tlb::flush(VirtAddr::new(page_va));
                KernelStackFaultOutcome::Resumed
            }
            Err(_) => {
                crate::memory::phys::free_frame(frame);
                x86_64::instructions::tlb::flush(VirtAddr::new(page_va));
                KernelStackFaultOutcome::Resumed
            }
        }
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = (error_code, cr2);
        KernelStackFaultOutcome::NotKernelStack
    }
}

pub fn used_bytes_in_slice(slice: &[u8], guard_prefix_bytes: usize) -> Option<usize> {
    let region = slice.get(guard_prefix_bytes..)?;
    let page = region
        .chunks(KERNEL_STACK_PAGE_SIZE)
        .position(|chunk| chunk.iter().any(|&byte| byte != 0))?;
    let start = page * KERNEL_STACK_PAGE_SIZE;
    let within = region[start..].iter().position(|&byte| byte != 0)?;
    Some(region.len() - (start + within))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_chunk_initializes_all_slots_free_with_no_free_sentinel() {
        let chunk = MetaChunk::boxed();
        for meta in &chunk.slots {
            assert_eq!(meta.state.load(Ordering::Relaxed), SLOT_FREE);
            assert_eq!(meta.next_free.load(Ordering::Relaxed), NO_FREE_SLOT);
            assert_eq!(meta.pt_phys.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn stack_slot_geometry_aligns_four_slots_per_two_mib_page_table() {
        assert_eq!(KERNEL_STACK_SLOT_STRIDE % KERNEL_STACK_PAGE_SIZE, 0);
        assert_eq!(2 * 1024 * 1024 / KERNEL_STACK_SLOT_STRIDE, 4);
        assert_eq!(KERNEL_STACK_GUARD_BYTES % KERNEL_STACK_PAGE_SIZE, 0);
        assert_eq!(KERNEL_STACK_PRECOMMIT_BYTES % KERNEL_STACK_PAGE_SIZE, 0);
    }
}
