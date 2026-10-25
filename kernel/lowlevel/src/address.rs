use core::sync::atomic::{AtomicU64, Ordering};

pub const KERNEL_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
pub const KERNEL_IMAGE_WINDOW: u64 = KERNEL_VIRT_OFFSET + (2 << 39);
pub const KERNEL_IMAGE_WINDOW_SIZE: u64 = 1 << 39;
pub const KERNEL_IMAGE_PHYS_BASE: u64 = 0x20_0000;
static KERNEL_IMAGE_BASE: AtomicU64 = AtomicU64::new(0);
static KERNEL_IMAGE_SIZE: AtomicU64 = AtomicU64::new(0);

/// BSP-only publication before switching CR3 or starting APs. The direct-map
/// offset remains fixed; only image pointers use this independently chosen base.
pub fn publish_kernel_image_mapping(base: u64, size: u64) {
    assert!(size > 0);
    assert!(base >= KERNEL_IMAGE_WINDOW);
    assert!(
        base.checked_add(size)
            .is_some_and(|end| end <= KERNEL_IMAGE_WINDOW + KERNEL_IMAGE_WINDOW_SIZE)
    );
    KERNEL_IMAGE_SIZE.store(size, Ordering::Relaxed);
    assert!(
        KERNEL_IMAGE_BASE
            .compare_exchange(0, base, Ordering::Release, Ordering::Relaxed)
            .is_ok()
    );
}

pub fn kernel_image_addr(physical: u64) -> u64 {
    let base = KERNEL_IMAGE_BASE.load(Ordering::Acquire);
    assert!(base != 0, "kernel image mapping is not published");
    let size = KERNEL_IMAGE_SIZE.load(Ordering::Relaxed);
    if image_physical_addr(physical, base, size).is_some() {
        return physical;
    }
    let offset = physical
        .checked_sub(KERNEL_IMAGE_PHYS_BASE)
        .expect("address precedes kernel image");
    assert!(offset < size);
    base + offset
}

fn image_physical_addr(addr: u64, base: u64, size: u64) -> Option<u64> {
    let offset = addr.checked_sub(base)?;
    if base != 0 && offset < size {
        KERNEL_IMAGE_PHYS_BASE.checked_add(offset)
    } else {
        None
    }
}

pub const fn higher_half_addr(addr: u64) -> u64 {
    if addr >= KERNEL_VIRT_OFFSET {
        addr
    } else {
        match addr.checked_add(KERNEL_VIRT_OFFSET) {
            Some(mapped) => mapped,
            None => panic!("physical/direct-map address exceeds higher-half window"),
        }
    }
}

pub fn lower_half_addr(addr: u64) -> u64 {
    let base = KERNEL_IMAGE_BASE.load(Ordering::Acquire);
    if let Some(physical) =
        image_physical_addr(addr, base, KERNEL_IMAGE_SIZE.load(Ordering::Relaxed))
    {
        return physical;
    }
    if addr >= KERNEL_VIRT_OFFSET {
        addr - KERNEL_VIRT_OFFSET
    } else {
        addr
    }
}

#[cfg(test)]
mod tests {
    use super::{KERNEL_VIRT_OFFSET, higher_half_addr, lower_half_addr};

    #[test]
    fn direct_map_translation_round_trips_its_supported_window() {
        for physical in [0, 4096, (1_u64 << 39) - 1] {
            assert_eq!(lower_half_addr(higher_half_addr(physical)), physical);
        }
    }

    #[test]
    fn image_translation_does_not_confuse_direct_map_or_window_gaps() {
        let base = super::KERNEL_IMAGE_WINDOW + 0x4000_0000;
        let size = 0x800000;
        assert_eq!(
            super::image_physical_addr(base + 123, base, size),
            Some(0x200000 + 123)
        );
        for addr in [base - 1, base + size, KERNEL_VIRT_OFFSET + 0x200000] {
            assert_eq!(super::image_physical_addr(addr, base, size), None);
        }
        assert_eq!(super::image_physical_addr(0, 0, size), None);
    }

    #[test]
    #[should_panic(expected = "exceeds higher-half window")]
    fn direct_map_translation_rejects_wrapping_input() {
        let _ = higher_half_addr(KERNEL_VIRT_OFFSET - 1);
    }
}
