pub const KERNEL_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;

pub const fn higher_half_addr(addr: u64) -> u64 {
    if addr >= KERNEL_VIRT_OFFSET {
        addr
    } else {
        addr + KERNEL_VIRT_OFFSET
    }
}

pub const fn lower_half_addr(addr: u64) -> u64 {
    if addr >= KERNEL_VIRT_OFFSET {
        addr - KERNEL_VIRT_OFFSET
    } else {
        addr
    }
}
