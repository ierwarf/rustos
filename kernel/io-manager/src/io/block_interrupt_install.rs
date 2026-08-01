//! Revocable MSI-X custody for the DVM block transport install transaction.

use core::sync::atomic::Ordering;

use super::{
    IRQ_VECTOR, MSIX_ENTRY_ADDRESS_HIGH_OFFSET, MSIX_ENTRY_ADDRESS_LOW_OFFSET,
    MSIX_ENTRY_DATA_OFFSET, MSIX_ENTRY_VECTOR_CONTROL_OFFSET, MSIX_ENTRY_VECTOR_MASKED,
};

pub(super) struct BlockInterruptInstall {
    pub(super) capability: crate::arch::pci::MsixCapability,
    pub(super) device: crate::arch::pci::PciDevice,
    pub(super) vector: Option<crate::arch::msi::CommittedMsiVector>,
}

impl BlockInterruptInstall {
    pub(super) fn retain_permanent(mut self) {
        let vector = self
            .vector
            .take()
            .expect("DVM block interrupt transaction lost vector ownership")
            .retain_permanent();
        // ORDERING: release publishes the boot-lifetime vector only after the
        // transport and installed flag are externally visible.
        IRQ_VECTOR.store(vector, Ordering::Release);
    }
}

impl Drop for BlockInterruptInstall {
    fn drop(&mut self) {
        if self.vector.is_some() {
            // Reverse device publication before revoking handler/vector
            // authority. Config-space readback in these helpers is the posted
            // write completion barrier.
            self.capability.set_function_masked(self.device, true);
            self.capability.set_enabled(self.device, false);
            drop(self.vector.take());
        }
    }
}

pub(super) unsafe fn program_msix_entry(entry: *mut u8, message: crate::arch::msi::MsiMessage) {
    // SAFETY: the caller validated the MSI-X table BAR bounds and passes one
    // aligned 16-byte entry owned by this install transaction.
    unsafe {
        entry
            .add(MSIX_ENTRY_VECTOR_CONTROL_OFFSET)
            .cast::<u32>()
            .write_volatile(MSIX_ENTRY_VECTOR_MASKED.to_le());
        entry
            .add(MSIX_ENTRY_ADDRESS_LOW_OFFSET)
            .cast::<u32>()
            .write_volatile((message.address as u32).to_le());
        entry
            .add(MSIX_ENTRY_ADDRESS_HIGH_OFFSET)
            .cast::<u32>()
            .write_volatile(((message.address >> 32) as u32).to_le());
        entry
            .add(MSIX_ENTRY_DATA_OFFSET)
            .cast::<u32>()
            .write_volatile(message.data.to_le());
    }
}
