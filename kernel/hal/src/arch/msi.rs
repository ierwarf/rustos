//! Narrow x86 MSI/MSI-X receive substrate.
//!
//! Device policy stays with the owning driver domain. This module only owns a
//! bounded interrupt-vector allocator, IDT dispatch, and the local-APIC EOI
//! required for a PCI device to signal a fixed event queue. It deliberately
//! exposes function pointers rather than a general IRQ object graph so an IRQ
//! handler cannot allocate, block, or acquire a policy-service lock.

use core::arch::x86_64::__cpuid;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
static ALLOCATED: [AtomicBool; MSI_VECTOR_COUNT] =
    [const { AtomicBool::new(false) }; MSI_VECTOR_COUNT];
static ALLOCATION_CURSOR: AtomicUsize = AtomicUsize::new(0);
static LOCAL_APIC_BASE: AtomicUsize = AtomicUsize::new(0);
static LOCAL_APIC_READY: AtomicBool = AtomicBool::new(false);

/// A not-yet-published interrupt vector. Failed MSI-X setup drops the lease,
/// clears its exact handler, and returns the slot to the bounded pool. Only a
/// fully programmed, still-masked device may commit the reservation.
pub struct MsiVectorLease {
    vector: u8,
    handler: usize,
    committed: bool,
}

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

impl MsiVectorLease {
    /// Reserve one vector without publishing any registration authority.
    pub fn allocate() -> Option<Self> {
        let start = ALLOCATION_CURSOR.fetch_add(1, Ordering::AcqRel) % MSI_VECTOR_COUNT;
        for offset in 0..MSI_VECTOR_COUNT {
            let index = (start + offset) % MSI_VECTOR_COUNT;
            if ALLOCATED[index]
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(Self {
                    vector: MSI_VECTOR_FIRST + index as u8,
                    handler: 0,
                    committed: false,
                });
            }
        }
        None
    }

    pub const fn vector(&self) -> u8 {
        self.vector
    }

    /// Bind exactly one leaf handler to this unpublished lease.
    pub fn register_handler(&mut self, handler: MsiHandler) -> bool {
        if self.handler != 0 {
            return false;
        }
        let Some(index) = vector_index(self.vector) else {
            return false;
        };
        if !vector_has_registration_authority(self.vector, ALLOCATED[index].load(Ordering::Acquire))
        {
            return false;
        }
        let raw = handler as usize;
        if HANDLERS[index]
            .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.handler = raw;
        true
    }

    /// Return the exact xAPIC MSI tuple only for this lease's live handler.
    pub fn message(&self) -> Option<MsiMessage> {
        let index = vector_index(self.vector)?;
        if self.handler == 0
            || !LOCAL_APIC_READY.load(Ordering::Acquire)
            || !ALLOCATED[index].load(Ordering::Acquire)
            || HANDLERS[index].load(Ordering::Acquire) != self.handler
        {
            return None;
        }
        let leaf1 = __cpuid(1);
        let apic_id = (leaf1.ebx >> 24) as u64;
        Some(MsiMessage {
            address: 0xfee0_0000 | (apic_id << 12),
            data: u32::from(self.vector),
        })
    }

    /// Publish the reservation after the masked MSI-X entry is complete.
    pub fn commit(mut self) -> u8 {
        self.committed = true;
        self.vector
    }
}

impl Drop for MsiVectorLease {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let Some(index) = vector_index(self.vector) else {
            return;
        };
        let handler_released = if self.handler == 0 {
            HANDLERS[index].load(Ordering::Acquire) == 0
        } else {
            HANDLERS[index]
                .compare_exchange(self.handler, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        };
        if handler_released {
            ALLOCATED[index].store(false, Ordering::Release);
        }
    }
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

const fn vector_has_registration_authority(vector: u8, allocated: bool) -> bool {
    vector_is_valid(vector) && allocated
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
    use core::sync::atomic::Ordering;

    use super::{
        ALLOCATED, HANDLERS, MSI_VECTOR_FIRST, MSI_VECTOR_LAST, MsiVectorLease,
        vector_has_registration_authority, vector_index, vector_is_valid,
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
        assert!(!vector_has_registration_authority(MSI_VECTOR_FIRST, false));
        assert!(vector_has_registration_authority(MSI_VECTOR_FIRST, true));
        assert!(!vector_has_registration_authority(0x20, true));
    }

    #[test]
    fn failed_unpublished_vector_lease_revokes_exact_handler_and_slot() {
        fn handler(_vector: u8) {}

        let mut lease = MsiVectorLease::allocate().expect("bounded test vector");
        let index = vector_index(lease.vector()).expect("allocated vector index");
        assert!(ALLOCATED[index].load(Ordering::Acquire));
        assert!(lease.register_handler(handler));
        assert_eq!(
            HANDLERS[index].load(Ordering::Acquire),
            handler as *const () as usize
        );

        drop(lease);
        assert_eq!(HANDLERS[index].load(Ordering::Acquire), 0);
        assert!(!ALLOCATED[index].load(Ordering::Acquire));
    }
}
