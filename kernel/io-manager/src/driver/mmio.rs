use alloc::vec::Vec;
use core::ffi::c_void;

use spin::Mutex;
use x86_64::instructions::interrupts;
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

static MMIO_MAPPINGS: Mutex<Vec<MmioMapping>> = Mutex::new(Vec::new());
static DIRECT_MAP_PAGE_OVERRIDES: Mutex<Vec<DirectMapPageOverride>> = Mutex::new(Vec::new());

pub(crate) fn map(phys_start: u64, size: usize, write_combine: bool) -> *mut c_void {
    if size == 0 {
        return core::ptr::null_mut();
    }

    let cache_mode = if write_combine {
        MmioCacheMode::WriteCombine
    } else {
        MmioCacheMode::Uncached
    };

    irq_safe(|| {
        let mut mappings = MMIO_MAPPINGS.lock();
        if let Some(mapping) = mappings.iter_mut().find(|mapping| {
            mapping.phys_start == phys_start
                && mapping.size == size
                && mapping.cache_mode == cache_mode
        }) {
            mapping.refcount += 1;
            return mapping.virt_start as *mut c_void;
        }

        let (virt_start, backing, page_base, page_count) =
            if let Some((virt_start, page_base, page_count)) = direct_map_mapping(phys_start, size)
            {
                if !apply_direct_map_cache_mode(page_base, page_count, cache_mode) {
                    return core::ptr::null_mut();
                }
                (virt_start, MmioBacking::DirectMap, page_base, page_count)
            } else {
                let virt_start = match cache_mode {
                    MmioCacheMode::Uncached => {
                        crate::memory::paging::map_mmio_range(phys_start, size)
                    }
                    MmioCacheMode::WriteCombine => {
                        crate::memory::paging::map_mmio_range_wc(phys_start, size)
                    }
                };
                let Some(virt_start) = virt_start else {
                    return core::ptr::null_mut();
                };
                (virt_start, MmioBacking::Window, 0, 0)
            };
        let Some(virt_start) = Some(virt_start) else {
            return core::ptr::null_mut();
        };

        mappings.push(MmioMapping {
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
    })
}

pub(crate) fn unmap(addr: *mut c_void) {
    if addr.is_null() {
        return;
    }

    let removed = irq_safe(|| {
        let mut mappings = MMIO_MAPPINGS.lock();
        let index = mappings
            .iter()
            .position(|mapping| mapping.virt_start == addr as usize)?;
        if mappings[index].refcount > 1 {
            mappings[index].refcount -= 1;
            return None;
        }
        Some(mappings.remove(index))
    });

    if let Some(mapping) = removed {
        match mapping.backing {
            MmioBacking::DirectMap => {
                restore_direct_map_cache_mode(mapping.page_base, mapping.page_count);
            }
            MmioBacking::Window => {
                let _ = crate::memory::paging::unmap_mmio_range(
                    mapping.virt_start as u64,
                    mapping.size,
                );
            }
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
    page_base: u64,
    page_count: usize,
    cache_mode: MmioCacheMode,
) -> bool {
    let desired_flags = cache_mode_flags(cache_mode);
    let mut overrides = DIRECT_MAP_PAGE_OVERRIDES.lock();
    let mut new_pages = Vec::new();

    for page_index in 0..page_count {
        let phys_page = page_base + page_index as u64 * PAGE_4KIB;
        if let Some(existing) = overrides
            .iter()
            .find(|override_entry| override_entry.phys_page_base == phys_page)
        {
            if existing.cache_mode != cache_mode {
                return false;
            }
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

    let mut applied = 0usize;
    for page in &new_pages {
        if !crate::memory::paging::update_direct_map_range_flags(
            page.phys_page_base,
            PAGE_4KIB as usize,
            desired_flags,
            DIRECT_MAP_CACHE_FLAG_MASK,
        ) {
            for restored in &new_pages[..applied] {
                let _ = crate::memory::paging::update_direct_map_range_flags(
                    restored.phys_page_base,
                    PAGE_4KIB as usize,
                    restored.original_cache_flags,
                    DIRECT_MAP_CACHE_FLAG_MASK,
                );
            }
            return false;
        }
        applied += 1;
    }

    for page in &new_pages {
        overrides.push(DirectMapPageOverride {
            phys_page_base: page.phys_page_base,
            cache_mode: page.cache_mode,
            original_cache_flags: page.original_cache_flags,
            refcount: 1,
        });
    }

    for page_index in 0..page_count {
        let phys_page = page_base + page_index as u64 * PAGE_4KIB;
        if new_pages
            .iter()
            .any(|page| page.phys_page_base == phys_page)
        {
            continue;
        }
        if let Some(existing) = overrides
            .iter_mut()
            .find(|override_entry| override_entry.phys_page_base == phys_page)
        {
            existing.refcount += 1;
        }
    }

    true
}

fn restore_direct_map_cache_mode(page_base: u64, page_count: usize) {
    let mut overrides = DIRECT_MAP_PAGE_OVERRIDES.lock();

    for page_index in 0..page_count {
        let phys_page = page_base + page_index as u64 * PAGE_4KIB;
        let Some(index) = overrides
            .iter()
            .position(|override_entry| override_entry.phys_page_base == phys_page)
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

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(f)
}
