//! Optional ivshmem interrupt backend. Shared ring cursors, not this backend,
//! are the source of truth. BAR0 is never mapped into a user process.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering, fence};

const POSITION: usize = 8;
const DOORBELL: usize = 12;
const REGISTER_BYTES: usize = DOORBELL + size_of::<u32>();
const PEER_1_VECTOR_0: u32 = 1 << 16;

pub(super) struct Doorbell {
    registers: NonNull<u8>,
    vector: u8,
}

impl Doorbell {
    pub(super) fn install(device: crate::arch::pci::PciDevice) -> Result<Option<Self>, &'static str> {
        if device.msix_capability().is_none() {
            return Ok(None);
        }
        let resource = device.resource(0).ok_or("missing-network-registers")?;
        if resource.is_io || resource.prefetchable || resource.size < REGISTER_BYTES as u64 {
            return Err("invalid-network-registers");
        }
        let registers = NonNull::new(crate::driver::mmio::map_uncached(resource.start, REGISTER_BYTES).cast::<u8>())
            .ok_or("map-network-registers")?;
        // SAFETY: BAR0's aligned position register is inside the admitted mapping.
        let peer = unsafe { registers.as_ptr().add(POSITION).cast::<u32>().read_volatile() };
        if peer != 0 {
            crate::driver::mmio::unmap(registers.as_ptr().cast());
            return Err("wrong-network-peer");
        }
        let Some(install) = super::super::ivshmem_interrupt::arm_single_ring_interrupt(device, super::network_interrupt) else {
            crate::driver::mmio::unmap(registers.as_ptr().cast());
            return Err("invalid-network-msix");
        };
        Ok(Some(Self { registers, vector: install.retain_permanent() }))
    }

    pub(super) fn vector(&self) -> u8 {
        self.vector
    }

    pub(super) fn after_transmit(&self, arm: &AtomicU32) {
        if take_arm_after_producer_publish(arm) {
            // SAFETY: fixed aligned peer/vector register; the validated BAR0
            // mapping is permanent, and only the transport owner calls this.
            unsafe {
                self.registers.as_ptr().add(DOORBELL).cast::<u32>()
                    .write_volatile(PEER_1_VECTOR_0.to_le());
            }
        }
    }
}

fn take_arm_after_producer_publish(arm: &AtomicU32) -> bool {
    // Closes the store/load race with the consumer's SeqCst arm/recheck.
    // Do not issue a locked RMW to the aperture while the consumer is active.
    fence(Ordering::SeqCst);
    arm.load(Ordering::Acquire) == 1 && arm.swap(0, Ordering::AcqRel) == 1
}

#[cfg(test)]
#[test]
fn doorbell_consumes_one_arm_only_and_never_trusts_invalid_values() {
    let arm = AtomicU32::new(0);
    assert!(!take_arm_after_producer_publish(&arm));
    arm.store(2, Ordering::Release);
    assert!(!take_arm_after_producer_publish(&arm));
    arm.store(1, Ordering::SeqCst);
    assert!(take_arm_after_producer_publish(&arm));
    assert!(!take_arm_after_producer_publish(&arm));
}
