//! Global cache-type and lifetime registry for kernel device mappings.
//!
//! - **Owner:** `kernel-io-manager` exclusively owns published device mapping
//!   records and temporary direct-map cache overrides.
//! - **Boundary:** Physical BAR ranges and cache modes arrive from enumerated,
//!   caller-admitted devices but remain untrusted until interval validation.
//! - **Lifecycle:** Reserve all metadata, mutate page tables, publish exactly
//!   one interval record, reference-count exact aliases, then restore/unmap in
//!   one terminal transaction.
//! - **Concurrency:** `MMIO_REGISTRY` serializes interval admission, cache-mode
//!   changes, publication, and final release across CPUs.
//! - **Failure:** Overflow, overlap, allocation failure, partial mapping, and
//!   failed restore keep the mapping unpublished or retain its ownership record.
//! - **Forbidden:** No mixed-cache physical aliases, direct-map straddles,
//!   unowned permanent-leaf retyping, or metadata allocation after mutation.
//! - **Evidence:** MMIO interval/cache tests and the DVM block/input models.

use alloc::vec::Vec;
use core::ffi::c_void;

use crate::sync::KernelWaitLock;
use x86_64::structures::paging::PageTableFlags;

const PAGE_4KIB: u64 = 4096;
const DIRECT_MAP_CACHE_FLAG_MASK: PageTableFlags = PageTableFlags::from_bits_retain(
    PageTableFlags::NO_CACHE.bits()
        | PageTableFlags::WRITE_THROUGH.bits()
        | crate::memory::paging::WRITE_COMBINE_BIT.bits(),
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MmioCacheMode {
    SharedWriteBack,
    Uncached,
    WriteCombine,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MmioBacking {
    DirectMap,
    Window,
}

struct MmioMapping {
    phys_start: u64,
    size: usize,
    virt_start: usize,
    cache_mode: MmioCacheMode,
    backing: MmioBacking,
    page_base: u64,
    page_count: usize,
    refcount: usize,
}

struct DirectMapPageOverride {
    phys_page_base: u64,
    original_cache_flags: PageTableFlags,
}

struct MmioRegistry {
    mappings: Vec<MmioMapping>,
    direct_map_overrides: Vec<DirectMapPageOverride>,
}

impl MmioRegistry {
    const fn new() -> Self {
        Self {
            mappings: Vec::new(),
            direct_map_overrides: Vec::new(),
        }
    }
}

// Cache-mode changes, mapping publication, and final unmap are one transaction.
// This is deliberately scheduler-aware: the transaction can allocate and
// update many page-table entries, so making IRQ-off callers spin on a raw lock
// turns a large BAR mapping into system-wide interrupt latency.
static MMIO_REGISTRY: KernelWaitLock<
    MmioRegistry,
    { nucleus_core::util::lockdep::LockClass::MmioRegistryWait as u8 },
> = KernelWaitLock::new(MmioRegistry::new());

#[track_caller]
pub(crate) fn map_uncached(phys_start: u64, size: usize) -> *mut c_void {
    map_with_cache_mode(phys_start, size, MmioCacheMode::Uncached)
}

#[track_caller]
pub(crate) fn map_write_combining(phys_start: u64, size: usize) -> *mut c_void {
    map_with_cache_mode(phys_start, size, MmioCacheMode::WriteCombine)
}

/// Map an authenticated RAM-backed PCI aperture with coherent WB semantics.
///
/// Callers must reject ordinary registers and non-prefetchable BARs before
/// using this path. Keeping it separate from write-combining mappings prevents
/// atomic control records from inheriting framebuffer-style memory semantics.
#[track_caller]
pub(crate) fn map_shared_write_back(phys_start: u64, size: usize) -> *mut c_void {
    map_with_cache_mode(phys_start, size, MmioCacheMode::SharedWriteBack)
}

fn map_with_cache_mode(phys_start: u64, size: usize, cache_mode: MmioCacheMode) -> *mut c_void {
    if size == 0 {
        return core::ptr::null_mut();
    }
    let Some(phys_end) = phys_start.checked_add(size as u64) else {
        return core::ptr::null_mut();
    };
    if physical_range_straddles_direct_map_limit(phys_start, phys_end) {
        return core::ptr::null_mut();
    }

    let mut registry = MMIO_REGISTRY.lock();
    if let Some(mapping) = registry.mappings.iter_mut().find(|mapping| {
        mapping.phys_start == phys_start && mapping.size == size && mapping.cache_mode == cache_mode
    }) {
        let Some(next_refcount) = mapping.refcount.checked_add(1) else {
            return core::ptr::null_mut();
        };
        mapping.refcount = next_refcount;
        return mapping.virt_start as *mut c_void;
    }
    // Exact same-range/same-mode reuse returned above. Reject every other
    // physical overlap, even with the same cache mode: partial aliases make
    // last-unmap restoration dependent on page-granular ownership and permit
    // one caller to outlive another caller's admitted interval.
    if registry
        .mappings
        .iter()
        .any(|mapping| physical_ranges_overlap(mapping.phys_start, mapping.size, phys_start, size))
    {
        return core::ptr::null_mut();
    }
    // Reserve publication metadata before changing any PTE or allocating a
    // high-window slot. Allocation failure must leave both cache ownership and
    // page tables unchanged.
    if registry.mappings.try_reserve(1).is_err() {
        return core::ptr::null_mut();
    }

    let (virt_start, backing, page_base, page_count) =
        if let Some((virt_start, page_base, page_count)) = direct_map_mapping(phys_start, size) {
            if !apply_direct_map_cache_mode(
                &mut registry.direct_map_overrides,
                page_base,
                page_count,
                cache_mode,
            ) {
                return core::ptr::null_mut();
            }
            (virt_start, MmioBacking::DirectMap, page_base, page_count)
        } else {
            let virt_start = match cache_mode {
                MmioCacheMode::SharedWriteBack => {
                    crate::memory::paging::map_shared_memory_range(phys_start, size)
                }
                MmioCacheMode::Uncached => crate::memory::paging::map_mmio_range(phys_start, size),
                MmioCacheMode::WriteCombine => {
                    crate::memory::paging::map_mmio_range_wc(phys_start, size)
                }
            };
            let Some(virt_start) = virt_start else {
                return core::ptr::null_mut();
            };
            (virt_start, MmioBacking::Window, 0, 0)
        };

    registry.mappings.push(MmioMapping {
        phys_start,
        size,
        virt_start: virt_start as usize,
        cache_mode,
        backing,
        page_base,
        page_count,
        refcount: 1,
    });
    virt_start as *mut c_void
}

pub(crate) fn unmap(addr: *mut c_void) {
    if addr.is_null() {
        return;
    }

    let mut registry = MMIO_REGISTRY.lock();
    let Some(index) = registry
        .mappings
        .iter()
        .position(|mapping| mapping.virt_start == addr as usize)
    else {
        return;
    };
    if registry.mappings[index].refcount > 1 {
        registry.mappings[index].refcount -= 1;
        return;
    }
    let backing = registry.mappings[index].backing;
    let page_base = registry.mappings[index].page_base;
    let page_count = registry.mappings[index].page_count;
    let virt_start = registry.mappings[index].virt_start;
    let size = registry.mappings[index].size;

    let restored = match backing {
        MmioBacking::DirectMap => {
            restore_direct_map_cache_mode(&mut registry.direct_map_overrides, page_base, page_count)
        }
        MmioBacking::Window => crate::memory::paging::unmap_mmio_range(virt_start as u64, size),
    };
    if restored {
        registry.mappings.remove(index);
    }
}

fn direct_map_mapping(phys_start: u64, size: usize) -> Option<(u64, u64, usize)> {
    let last = phys_start.checked_add(size.saturating_sub(1) as u64)?;
    if last >= crate::memory::kernel_vm::DIRECT_MAP_PHYS_LIMIT {
        return None;
    }

    let page_base = align_down(phys_start, PAGE_4KIB);
    let page_end = align_up(last.checked_add(1)?, PAGE_4KIB)?;
    let page_count = ((page_end - page_base) / PAGE_4KIB) as usize;
    Some((
        crate::memory::paging::higher_half_addr(phys_start),
        page_base,
        page_count,
    ))
}

fn apply_direct_map_cache_mode(
    overrides: &mut Vec<DirectMapPageOverride>,
    page_base: u64,
    page_count: usize,
    cache_mode: MmioCacheMode,
) -> bool {
    let desired_flags = cache_mode_flags(cache_mode);
    let mut new_pages = Vec::new();
    // Both vectors are published after the direct-map PTE transaction. Reserve
    // their worst-case growth first so no allocation can fail after cache
    // attributes have changed.
    if overrides.try_reserve(page_count).is_err() || new_pages.try_reserve(page_count).is_err() {
        return false;
    }

    for page_index in 0..page_count {
        let phys_page = page_base + page_index as u64 * PAGE_4KIB;
        if overrides
            .binary_search_by_key(&phys_page, |entry| entry.phys_page_base)
            .is_ok()
        {
            return false;
        }

        let Some(flags) = crate::memory::paging::direct_map_cache_flags_for_phys(phys_page) else {
            return false;
        };
        let Some(existing_mode) = cache_mode_from_flags(flags) else {
            return false;
        };
        // An unowned non-WB leaf belongs to a permanent boot mapping (for
        // example the local APIC). Never silently retype it for a later PCI
        // aperture; an exact same-mode alias is the only safe admission.
        if existing_mode != MmioCacheMode::SharedWriteBack && existing_mode != cache_mode {
            return false;
        }
        new_pages.push(DirectMapPageOverride {
            phys_page_base: phys_page,
            original_cache_flags: flags,
        });
    }
    if new_pages.first().is_some_and(|first| {
        new_pages
            .iter()
            .any(|page| page.original_cache_flags != first.original_cache_flags)
    }) {
        return false;
    }

    // The paging layer already provides one prepared, globally serialized
    // range transaction and can retain huge leaves for complete 2 MiB spans.
    // Calling it once per 4 KiB page split the same aperture repeatedly and
    // performed two local invalidations for every page: a 48 MiB GPU atlas
    // therefore issued 24,576 INVLPG operations during UI bootstrap. Existing
    // retained pages were validated to use this exact cache mode, so applying
    // the desired flags idempotently across the complete requested range is
    // both the rollback-safe and the bounded operation.
    let range_size = page_count.checked_mul(PAGE_4KIB as usize);
    if !new_pages.is_empty()
        && range_size.is_none_or(|range_size| {
            !crate::memory::paging::update_direct_map_range_flags(
                page_base,
                range_size,
                desired_flags,
                DIRECT_MAP_CACHE_FLAG_MASK,
            )
        })
    {
        return false;
    }

    for page in new_pages {
        let index = overrides
            .binary_search_by_key(&page.phys_page_base, |entry| entry.phys_page_base)
            .unwrap_or_else(|index| index);
        overrides.insert(index, page);
    }

    true
}

fn restore_direct_map_cache_mode(
    overrides: &mut Vec<DirectMapPageOverride>,
    page_base: u64,
    page_count: usize,
) -> bool {
    if page_count == 0 {
        return false;
    }
    let mut original_cache_flags = None;
    for page_index in 0..page_count {
        let phys_page = page_base + page_index as u64 * PAGE_4KIB;
        let Ok(index) = overrides.binary_search_by_key(&phys_page, |entry| entry.phys_page_base)
        else {
            return false;
        };
        let flags = overrides[index].original_cache_flags;
        if original_cache_flags.is_some_and(|expected| expected != flags) {
            return false;
        }
        original_cache_flags = Some(flags);
    }
    let Some(original_cache_flags) = original_cache_flags else {
        return false;
    };
    let Some(range_size) = page_count.checked_mul(PAGE_4KIB as usize) else {
        return false;
    };
    if !crate::memory::paging::update_direct_map_range_flags(
        page_base,
        range_size,
        original_cache_flags,
        DIRECT_MAP_CACHE_FLAG_MASK,
    ) {
        return false;
    }
    for page_index in (0..page_count).rev() {
        let phys_page = page_base + page_index as u64 * PAGE_4KIB;
        let Ok(index) = overrides.binary_search_by_key(&phys_page, |entry| entry.phys_page_base)
        else {
            return false;
        };
        overrides.remove(index);
    }
    true
}

fn cache_mode_flags(cache_mode: MmioCacheMode) -> PageTableFlags {
    match cache_mode {
        MmioCacheMode::SharedWriteBack => PageTableFlags::empty(),
        MmioCacheMode::Uncached => PageTableFlags::NO_CACHE,
        MmioCacheMode::WriteCombine => crate::memory::paging::WRITE_COMBINE_BIT,
    }
}

fn cache_mode_from_flags(flags: PageTableFlags) -> Option<MmioCacheMode> {
    let flags = flags & DIRECT_MAP_CACHE_FLAG_MASK;
    if flags.is_empty() {
        Some(MmioCacheMode::SharedWriteBack)
    } else if flags == PageTableFlags::NO_CACHE {
        Some(MmioCacheMode::Uncached)
    } else if flags == crate::memory::paging::WRITE_COMBINE_BIT {
        Some(MmioCacheMode::WriteCombine)
    } else {
        None
    }
}

fn physical_ranges_overlap(
    left_start: u64,
    left_size: usize,
    right_start: u64,
    right_size: usize,
) -> bool {
    if left_size == 0 || right_size == 0 {
        return false;
    }
    let Some(left_end) = left_start.checked_add(left_size as u64) else {
        return true;
    };
    let Some(right_end) = right_start.checked_add(right_size as u64) else {
        return true;
    };
    left_start < right_end && right_start < left_end
}

const fn physical_range_straddles_direct_map_limit(start: u64, end_exclusive: u64) -> bool {
    start < crate::memory::kernel_vm::DIRECT_MAP_PHYS_LIMIT
        && end_exclusive > crate::memory::kernel_vm::DIRECT_MAP_PHYS_LIMIT
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_metadata_is_reserved_before_any_cache_pte_mutation() {
        let source = include_str!("mmio.rs");
        let mapping_reserve = source
            .find("registry.mappings.try_reserve(1)")
            .expect("mapping publication must reserve before mapping");
        let direct_map_mutation = source
            .find("apply_direct_map_cache_mode(")
            .expect("direct-map mutation call must remain source-visible");
        let high_window_mutation = source
            .find("crate::memory::paging::map_shared_memory_range(phys_start, size)")
            .expect("high-window mapping call must remain source-visible");
        assert!(mapping_reserve < direct_map_mutation);
        assert!(mapping_reserve < high_window_mutation);

        let transaction = source
            .split_once("fn apply_direct_map_cache_mode(")
            .expect("direct-map transaction must remain source-visible")
            .1;
        let override_reserve = transaction
            .find("overrides.try_reserve(page_count)")
            .expect("override metadata must reserve before mutation");
        let pte_mutation = transaction
            .find("update_direct_map_range_flags(")
            .expect("PTE transaction must remain source-visible");
        assert!(override_reserve < pte_mutation);
    }

    #[test]
    fn shared_write_back_cache_mode_cannot_alias_mmio_modes() {
        assert_eq!(
            cache_mode_flags(MmioCacheMode::SharedWriteBack),
            PageTableFlags::empty()
        );
        assert_ne!(
            cache_mode_flags(MmioCacheMode::SharedWriteBack),
            cache_mode_flags(MmioCacheMode::WriteCombine)
        );
        assert_ne!(
            cache_mode_flags(MmioCacheMode::SharedWriteBack),
            cache_mode_flags(MmioCacheMode::Uncached)
        );
        for mode in [
            MmioCacheMode::SharedWriteBack,
            MmioCacheMode::Uncached,
            MmioCacheMode::WriteCombine,
        ] {
            assert_eq!(cache_mode_from_flags(cache_mode_flags(mode)), Some(mode));
        }
        assert_eq!(cache_mode_from_flags(PageTableFlags::WRITE_THROUGH), None);
    }

    #[test]
    fn overlapping_physical_ranges_reject_mixed_cache_modes() {
        assert!(physical_ranges_overlap(
            0x20_0000, 0x20_0000, 0x30_0000, 0x1000,
        ));
        assert!(physical_ranges_overlap(
            0x30_0000, 0x1000, 0x20_0000, 0x20_0000,
        ));
        assert!(!physical_ranges_overlap(
            0x20_0000, 0x10_0000, 0x30_0000, 0x1000,
        ));
        assert!(physical_ranges_overlap(
            0x20_0000, 0x20_0000, 0x30_0000, 0x1000,
        ));

        let limit = crate::memory::kernel_vm::DIRECT_MAP_PHYS_LIMIT;
        assert!(physical_range_straddles_direct_map_limit(
            limit - PAGE_4KIB,
            limit + PAGE_4KIB
        ));
        assert!(!physical_range_straddles_direct_map_limit(
            limit - PAGE_4KIB,
            limit
        ));
        assert!(!physical_range_straddles_direct_map_limit(
            limit,
            limit + PAGE_4KIB
        ));
    }
}
