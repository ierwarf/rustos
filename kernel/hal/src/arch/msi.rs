//! Narrow x86 MSI/MSI-X receive substrate.
//!
//! Device policy stays with the owning driver domain. This module only owns a
//! bounded interrupt-vector allocator, IDT dispatch, and the local-APIC EOI
//! required for a PCI device to signal a fixed event queue. It deliberately
//! exposes function pointers rather than a general IRQ object graph so an IRQ
//! handler cannot allocate, block, or acquire a policy-service lock.

use core::arch::x86_64::__cpuid;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use x86_64::registers::model_specific::Msr;

pub const MSI_VECTOR_FIRST: u8 = 0x40;
pub const MSI_VECTOR_LAST: u8 = 0xdf;
const MSI_VECTOR_COUNT: usize = (MSI_VECTOR_LAST - MSI_VECTOR_FIRST + 1) as usize;
const IA32_APIC_BASE: u32 = 0x1b;
const APIC_BASE_ADDRESS_MASK: u64 = 0xffff_f000;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const APIC_SPURIOUS_VECTOR_OFFSET: usize = 0x0f0;
const APIC_EOI_OFFSET: usize = 0x0b0;
const APIC_SPURIOUS_ENABLE: u32 = 1 << 8;
const APIC_SPURIOUS_VECTOR: u32 = 0xff;
const CPUID_FEATURE_APIC: u32 = 1 << 9;

pub type MsiHandler = fn(u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MsiMessage {
    pub address: u64,
    pub data: u32,
}

static HANDLERS: [AtomicUsize; MSI_VECTOR_COUNT] =
    [const { AtomicUsize::new(0) }; MSI_VECTOR_COUNT];
static NEXT_VECTOR: AtomicU8 = AtomicU8::new(MSI_VECTOR_FIRST);
static LOCAL_APIC_BASE: AtomicUsize = AtomicUsize::new(0);
static LOCAL_APIC_READY: AtomicBool = AtomicBool::new(false);

/// Enable the local xAPIC path necessary for MSI delivery. This intentionally
/// fails closed on x2APIC-only configuration: extended-destination and
/// interrupt-remapping support must be introduced as one coherent substrate,
/// not guessed from a truncated xAPIC destination ID.
pub fn init() -> bool {
    if LOCAL_APIC_READY.load(Ordering::Acquire) {
        return true;
    }
    let leaf1 = __cpuid(1);
    if leaf1.edx & CPUID_FEATURE_APIC == 0 {
        return false;
    }

    let mut apic_base_msr = Msr::new(IA32_APIC_BASE);
    let mut apic_base = unsafe { apic_base_msr.read() };
    if apic_base & APIC_BASE_X2APIC != 0 {
        return false;
    }
    if apic_base & APIC_BASE_ENABLE == 0 {
        apic_base |= APIC_BASE_ENABLE;
        unsafe {
            apic_base_msr.write(apic_base);
        }
    }
    let physical_base = apic_base & APIC_BASE_ADDRESS_MASK;
    if physical_base == 0 {
        return false;
    }
    let base = kernel_lowlevel::address::higher_half_addr(physical_base) as usize;
    unsafe {
        let spurious = (base + APIC_SPURIOUS_VECTOR_OFFSET) as *mut u32;
        let current = spurious.read_volatile();
        spurious.write_volatile((current & !0xff) | APIC_SPURIOUS_ENABLE | APIC_SPURIOUS_VECTOR);
    }
    LOCAL_APIC_BASE.store(base, Ordering::Release);
    LOCAL_APIC_READY.store(true, Ordering::Release);
    true
}

pub const fn vector_is_valid(vector: u8) -> bool {
    vector >= MSI_VECTOR_FIRST && vector <= MSI_VECTOR_LAST
}

/// Allocate one permanently reserved vector. GUI-DVM uses one notification
/// vector; a future device may request another only through this bounded pool.
pub fn allocate_vector() -> Option<u8> {
    let mut current = NEXT_VECTOR.load(Ordering::Acquire);
    loop {
        if !vector_is_valid(current) {
            return None;
        }
        let next = current.checked_add(1).unwrap_or(0);
        match NEXT_VECTOR.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(current),
            Err(observed) => current = observed,
        }
    }
}

/// Register a leaf IRQ callback for an allocated vector. A vector cannot be
/// rebound, preventing a later driver from stealing an event source.
pub fn register_handler(vector: u8, handler: MsiHandler) -> bool {
    let Some(index) = vector_index(vector).filter(|_| vector_was_allocated(vector)) else {
        return false;
    };
    HANDLERS[index]
        .compare_exchange(0, handler as usize, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Return the exact xAPIC MSI address/data tuple for a registered vector.
/// The caller writes it into a device MSI-X table while that table is masked.
pub fn message_for(vector: u8) -> Option<MsiMessage> {
    let index = vector_index(vector)?;
    if !LOCAL_APIC_READY.load(Ordering::Acquire)
        || !vector_was_allocated(vector)
        || HANDLERS[index].load(Ordering::Acquire) == 0
    {
        return None;
    }
    let leaf1 = __cpuid(1);
    let apic_id = (leaf1.ebx >> 24) as u64;
    Some(MsiMessage {
        address: 0xfee0_0000 | (apic_id << 12),
        data: u32::from(vector),
    })
}

/// Called only from the generic MSI IDT entries. The callback must only set a
/// lock-free pending flag or acknowledge a bounded device register; policy and
/// buffer work run later in the owning service/broker turn.
pub fn dispatch(vector: u8) {
    if let Some(index) = vector_index(vector) {
        let raw = HANDLERS[index].load(Ordering::Acquire);
        if raw != 0 {
            let handler: MsiHandler = unsafe { core::mem::transmute(raw) };
            handler(vector);
        }
    }
    end_of_interrupt();
}

fn vector_index(vector: u8) -> Option<usize> {
    vector_is_valid(vector).then_some((vector - MSI_VECTOR_FIRST) as usize)
}

fn vector_was_allocated(vector: u8) -> bool {
    vector_is_allocated_at_cursor(vector, NEXT_VECTOR.load(Ordering::Acquire))
}

const fn vector_is_allocated_at_cursor(vector: u8, next_vector: u8) -> bool {
    vector_is_valid(vector) && vector < next_vector
}

fn end_of_interrupt() {
    let base = LOCAL_APIC_BASE.load(Ordering::Acquire);
    if base == 0 {
        return;
    }
    unsafe {
        ((base + APIC_EOI_OFFSET) as *mut u32).write_volatile(0);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MSI_VECTOR_FIRST, MSI_VECTOR_LAST, vector_is_allocated_at_cursor, vector_is_valid,
    };

    #[test]
    fn msi_vector_pool_excludes_exceptions_pic_and_spurious_vectors() {
        assert!(!vector_is_valid(0x1f));
        assert!(vector_is_valid(MSI_VECTOR_FIRST));
        assert!(vector_is_valid(MSI_VECTOR_LAST));
        assert!(!vector_is_valid(0xe0));
    }

    #[test]
    fn unallocated_vector_has_no_registration_authority() {
        assert!(!vector_is_allocated_at_cursor(
            MSI_VECTOR_FIRST,
            MSI_VECTOR_FIRST
        ));
        assert!(vector_is_allocated_at_cursor(
            MSI_VECTOR_FIRST,
            MSI_VECTOR_FIRST + 1
        ));
        assert!(!vector_is_allocated_at_cursor(0x20, MSI_VECTOR_FIRST + 1));
        assert!(!vector_is_allocated_at_cursor(
            MSI_VECTOR_FIRST + 1,
            MSI_VECTOR_FIRST + 1
        ));
    }
}
