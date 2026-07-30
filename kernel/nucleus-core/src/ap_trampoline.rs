//! Fixed low-memory x86 application-processor trampoline contract.
//!
//! - **Owner:** `nucleus-core` owns the byte image and mailbox layout;
//!   `kernel-mm` owns the physical range and page permissions, while
//!   `kernel-hal` owns INIT/SIPI delivery.
//! - **Boundary:** Linker-produced bytes and one generation-bound mailbox are
//!   copied into the architecturally constrained sub-1MiB startup range.
//! - **Lifecycle:** Claim both pages, copy and verify code, publish one AP
//!   mailbox, start and acknowledge that AP, repeat serially, then retire both
//!   pages read-only and no-execute.
//! - **Concurrency:** The BSP is the sole mailbox writer and never targets the
//!   next AP until the current generation reaches OnlineParked.
//! - **Failure:** Oversize code, bad alignment, a torn copy, malformed mailbox,
//!   or reuse after sealing is a boot-fatal invariant violation.
//! - **Forbidden:** No allocator alias, executable writable page, concurrent
//!   mailbox consumers, raw APIC indexing, or post-retirement retry.
//! - **Evidence:** `cpu-online-lifecycle` and `physical-frame-lifecycle`.

use core::sync::atomic::{AtomicU8, Ordering, fence};

pub const TRAMPOLINE_PHYS: u64 = 0x8000;
pub const MAILBOX_PHYS: u64 = 0x9000;
pub const PAGE_SIZE: usize = 4096;
pub const RESERVED_BYTES: u64 = (PAGE_SIZE * 2) as u64;
pub const STARTUP_VECTOR: u8 = (TRAMPOLINE_PHYS >> 12) as u8;
pub const MAILBOX_MAGIC: u64 = 0x5255_5354_4f53_4150;

const INSTALL_EMPTY: u8 = 0;
const INSTALL_LIVE: u8 = 1;
const INSTALL_SEALED: u8 = 2;

static INSTALL_STATE: AtomicU8 = AtomicU8::new(INSTALL_EMPTY);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct ApStartupMailbox {
    pub magic: u64,
    pub magic_complement: u64,
    pub generation: u64,
    pub stack_top: u64,
    pub entry: u64,
    pub cr3: u64,
    pub logical_index: u64,
    pub expected_apic_id: u64,
}

const _: [(); 0] = [(); core::mem::offset_of!(ApStartupMailbox, magic)];
const _: [(); 8] = [(); core::mem::offset_of!(ApStartupMailbox, magic_complement)];
const _: [(); 16] = [(); core::mem::offset_of!(ApStartupMailbox, generation)];
const _: [(); 24] = [(); core::mem::offset_of!(ApStartupMailbox, stack_top)];
const _: [(); 32] = [(); core::mem::offset_of!(ApStartupMailbox, entry)];
const _: [(); 40] = [(); core::mem::offset_of!(ApStartupMailbox, cr3)];
const _: [(); 48] = [(); core::mem::offset_of!(ApStartupMailbox, logical_index)];
const _: [(); 56] = [(); core::mem::offset_of!(ApStartupMailbox, expected_apic_id)];

impl ApStartupMailbox {
    pub fn new(
        generation: u64,
        stack_top: u64,
        entry: u64,
        cr3: u64,
        logical_index: u8,
        expected_apic_id: u32,
    ) -> Self {
        assert_ne!(generation, 0, "AP mailbox generation must be non-zero");
        assert_ne!(logical_index, 0, "AP mailbox cannot target the BSP");
        assert_eq!(stack_top & 0xf, 0, "AP mailbox stack must be aligned");
        assert_ne!(entry, 0, "AP mailbox entry must be non-zero");
        assert!(
            cr3 != 0 && cr3 <= u64::from(u32::MAX) && cr3.is_multiple_of(PAGE_SIZE as u64),
            "AP mailbox CR3 must be a 32-bit page-aligned physical address"
        );
        Self {
            magic: MAILBOX_MAGIC,
            magic_complement: !MAILBOX_MAGIC,
            generation,
            stack_top,
            entry,
            cr3,
            logical_index: u64::from(logical_index),
            expected_apic_id: u64::from(expected_apic_id),
        }
    }

    pub const fn is_valid(self) -> bool {
        self.magic == MAILBOX_MAGIC
            && self.magic_complement == !MAILBOX_MAGIC
            && self.generation != 0
            && self.logical_index != 0
            && self.logical_index < 8
            && self.stack_top != 0
            && self.stack_top & 0xf == 0
            && self.entry != 0
            && self.cr3 != 0
            && self.cr3 <= u32::MAX as u64
            && self.cr3.is_multiple_of(PAGE_SIZE as u64)
            && self.expected_apic_id <= u32::MAX as u64
    }
}

#[cfg(rustos_boot_image)]
unsafe extern "C" {
    static rustos_ap_trampoline_start: u8;
    static rustos_ap_trampoline_end: u8;
}

#[cfg(rustos_boot_image)]
fn image() -> &'static [u8] {
    let start = core::ptr::addr_of!(rustos_ap_trampoline_start) as usize;
    let end = core::ptr::addr_of!(rustos_ap_trampoline_end) as usize;
    let len = end
        .checked_sub(start)
        .expect("AP trampoline linker symbols are reversed");
    assert!(
        (1..=PAGE_SIZE).contains(&len),
        "AP trampoline image exceeds its executable page"
    );
    // SAFETY: the linker symbols bound one immutable section in the loaded
    // kernel image, and the length was checked above.
    unsafe { core::slice::from_raw_parts(start as *const u8, len) }
}

/// Copy and byte-verify the trampoline after kernel-mm claims both low pages.
#[cfg(rustos_boot_image)]
pub fn install() {
    assert!(
        INSTALL_STATE
            .compare_exchange(
                INSTALL_EMPTY,
                INSTALL_LIVE,
                // ORDERING: AcqRel gives the BSP unique ownership of installation.
                Ordering::AcqRel,
                // ORDERING: Acquire observes a prior installer or seal operation.
                Ordering::Acquire,
            )
            .is_ok(),
        "AP trampoline installed more than once"
    );
    let image = image();
    let destination = TRAMPOLINE_PHYS as *mut u8;
    // SAFETY: kernel-mm has removed both fixed pages from allocation and the
    // executable page remains writable/no-execute until this copy completes.
    unsafe {
        core::ptr::write_bytes(destination, 0, PAGE_SIZE);
        core::ptr::copy_nonoverlapping(image.as_ptr(), destination, image.len());
        for (offset, expected) in image.iter().copied().enumerate() {
            assert_eq!(
                destination.add(offset).read_volatile(),
                expected,
                "AP trampoline copy verification failed at byte {offset}"
            );
        }
    }
}

#[cfg(not(rustos_boot_image))]
pub fn install() {
    panic!("AP trampoline installation is available only in a RustOS boot image");
}

pub fn publish_mailbox(mailbox: ApStartupMailbox) {
    assert!(mailbox.is_valid(), "refusing malformed AP startup mailbox");
    // ORDERING: Acquire rejects publication before the verified code copy or
    // after the BSP has retired the fixed startup pages.
    assert_eq!(
        INSTALL_STATE.load(Ordering::Acquire),
        INSTALL_LIVE,
        "AP mailbox publication requires a live unsealed trampoline"
    );
    let destination = MAILBOX_PHYS as *mut ApStartupMailbox;
    // SAFETY: kernel-mm reserved the mailbox page; the BSP is its sole writer
    // and serializes AP consumers through lifecycle acknowledgement.
    unsafe {
        destination.write_volatile(mailbox);
    }
    // ORDERING: SeqCst orders normal-memory mailbox publication before the
    // following device-memory ICR write on every supported x86 target.
    fence(Ordering::SeqCst);
    // SAFETY: the same reserved, aligned mailbox remains BSP-owned here.
    let observed = unsafe { destination.read_volatile() };
    assert_eq!(observed, mailbox, "AP startup mailbox write was torn");
}

pub fn seal() {
    assert!(
        INSTALL_STATE
            .compare_exchange(
                INSTALL_LIVE,
                INSTALL_SEALED,
                // ORDERING: AcqRel closes mailbox publication after all AP acks.
                Ordering::AcqRel,
                // ORDERING: Acquire reports the exact conflicting lifecycle.
                Ordering::Acquire,
            )
            .is_ok(),
        "AP trampoline sealed outside its live state"
    );
}

#[cfg(test)]
mod tests {
    use super::{ApStartupMailbox, MAILBOX_MAGIC, PAGE_SIZE, STARTUP_VECTOR, TRAMPOLINE_PHYS};

    #[test]
    fn mailbox_layout_and_startup_vector_are_exact() {
        let mailbox = ApStartupMailbox::new(
            7,
            0xffff_8000_0010_0000,
            0xffff_8000_0020_0000,
            0x3000,
            3,
            42,
        );
        assert!(mailbox.is_valid());
        assert_eq!(mailbox.magic, MAILBOX_MAGIC);
        assert_eq!(
            usize::from(STARTUP_VECTOR) * PAGE_SIZE,
            TRAMPOLINE_PHYS as usize
        );

        let mut corrupt = mailbox;
        corrupt.magic_complement ^= 1;
        assert!(!corrupt.is_valid());
    }
}
