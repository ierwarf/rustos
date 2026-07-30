//! Lock-free emergency evidence for a corrupted raw-guard release.

pub(super) fn write_guard_release_marker(
    class: u8,
    owner_cpu: usize,
    release_cpu: usize,
    owner_apic: u32,
    release_apic: u32,
    acquire_depth: usize,
    release_depth: usize,
    held_depth: usize,
) {
    write_bytes(b"\n!RAW-GUARD:");
    for value in [
        u64::from(class),
        owner_cpu as u64,
        release_cpu as u64,
        u64::from(owner_apic),
        u64::from(release_apic),
        acquire_depth as u64,
        release_depth as u64,
        held_depth as u64,
    ] {
        write_hex(value);
        write_bytes(b":");
    }
    write_bytes(b"\n");
}

fn write_hex(value: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for shift in (0..16).rev() {
        write_bytes(&[HEX[((value >> (shift * 4)) & 0xf) as usize]]);
    }
}

fn write_bytes(bytes: &[u8]) {
    for &byte in bytes {
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") 0x00e9_u16,
                in("al") byte,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}
