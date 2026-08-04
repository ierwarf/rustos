use alloc::vec::Vec;
use core::ffi::c_void;

use crate::sync::KernelWaitLock;
use x86_64::structures::paging::PageTableFlags;

const PAGE_4KIB: u64 = 4096;
const DIRECT_MAP_CACHE_FLAG_MASK: PageTableFlags = PageTableFlags::from_bits_retain(
    PageTableFlags::NO_CACHE.bits() | crate::memory::paging::WRITE_COMBINE_BIT.bits(),
);

#[derive(Clone, Copy, Eq, PartialEq)]
enum MmioCacheMode {
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
    cache_mode: MmioCacheMode,
    original_cache_flags: PageTableFlags,
    refcount: usize,
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
pub(crate) fn map(phys_start: u64, size: usize, write_combine: bool) -> *mut c_void {
    if size == 0 {
        return core::ptr::null_mut();
    }

    let cache_mode = if write_combine {
        MmioCacheMode::WriteCombine
    } else {
        MmioCacheMode::Uncached
    };

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
    let removed = {
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
        registry.mappings.remove(index)
    };

    match removed.backing {
        MmioBacking::DirectMap => {
            restore_direct_map_cache_mode(
                &mut registry.direct_map_overrides,
                removed.page_base,
                removed.page_count,
            );
        }
        MmioBacking::Window => {
            let _ =
                crate::memory::paging::unmap_mmio_range(removed.virt_start as u64, removed.size);
        }
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
    let mut retained_indices = Vec::new();

    for page_index in 0..page_count {
        let phys_page = page_base + page_index as u64 * PAGE_4KIB;
        if let Ok(index) = overrides.binary_search_by_key(&phys_page, |entry| entry.phys_page_base)
        {
            let existing = &overrides[index];
            if existing.cache_mode != cache_mode {
                return false;
            }
            if existing.refcount == usize::MAX {
                return false;
            }
            retained_indices.push(index);
            continue;
        }

        let Some(flags) = crate::memory::paging::direct_map_flags_for_phys(phys_page) else {
            return false;
        };
        new_pages.push(DirectMapPageOverride {
            phys_page_base: phys_page,
            cache_mode,
            original_cache_flags: flags & DIRECT_MAP_CACHE_FLAG_MASK,
            refcount: 1,
        });
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

    for index in retained_indices {
        overrides[index].refcount += 1;
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
) {
    for page_index in 0..page_count {
        let phys_page = page_base + page_index as u64 * PAGE_4KIB;
        let Ok(index) = overrides.binary_search_by_key(&phys_page, |entry| entry.phys_page_base)
        else {
            continue;
        };

        if overrides[index].refcount > 1 {
            overrides[index].refcount -= 1;
            continue;
        }

        let entry = overrides.remove(index);
        let _ = crate::memory::paging::update_direct_map_range_flags(
            entry.phys_page_base,
            PAGE_4KIB as usize,
            entry.original_cache_flags,
            DIRECT_MAP_CACHE_FLAG_MASK,
        );
    }
}

fn cache_mode_flags(cache_mode: MmioCacheMode) -> PageTableFlags {
    match cache_mode {
        MmioCacheMode::Uncached => PageTableFlags::NO_CACHE,
        MmioCacheMode::WriteCombine => crate::memory::paging::WRITE_COMBINE_BIT,
    }
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
