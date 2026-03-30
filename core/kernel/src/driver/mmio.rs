use alloc::vec::Vec;
use core::ffi::c_void;

use spin::Mutex;
use x86_64::instructions::interrupts;

#[derive(Clone, Copy, Eq, PartialEq)]
enum MmioCacheMode {
    Uncached,
    WriteCombine,
}

struct MmioMapping {
    phys_start: u64,
    size: usize,
    virt_start: usize,
    cache_mode: MmioCacheMode,
    refcount: usize,
}

static MMIO_MAPPINGS: Mutex<Vec<MmioMapping>> = Mutex::new(Vec::new());

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

        let virt_start = match cache_mode {
            MmioCacheMode::Uncached => crate::memory::paging::map_mmio_range(phys_start, size),
            MmioCacheMode::WriteCombine => {
                crate::memory::paging::map_mmio_range_wc(phys_start, size)
            }
        };
        let Some(virt_start) = virt_start else {
            return core::ptr::null_mut();
        };

        mappings.push(MmioMapping {
            phys_start,
            size,
            virt_start: virt_start as usize,
            cache_mode,
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
        let _ = crate::memory::paging::unmap_mmio_range(mapping.virt_start as u64, mapping.size);
    }
}

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(f)
}
