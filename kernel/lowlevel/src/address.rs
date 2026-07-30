pub const KERNEL_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;

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

pub const fn lower_half_addr(addr: u64) -> u64 {
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
    #[should_panic(expected = "exceeds higher-half window")]
    fn direct_map_translation_rejects_wrapping_input() {
        let _ = higher_half_addr(KERNEL_VIRT_OFFSET - 1);
    }
}
