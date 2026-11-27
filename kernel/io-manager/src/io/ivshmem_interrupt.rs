//! Shared, revocable MSI-X custody for admitted single-vector ivshmem rings.

use core::sync::atomic::Ordering;

const VECTOR_COUNT: u16 = 1;
const MSIX_ENTRY_BYTES: usize = 16;
const MSIX_ENTRY_ADDRESS_LOW_OFFSET: usize = 0;
const MSIX_ENTRY_ADDRESS_HIGH_OFFSET: usize = 4;
const MSIX_ENTRY_DATA_OFFSET: usize = 8;
const MSIX_ENTRY_VECTOR_CONTROL_OFFSET: usize = 12;
const MSIX_ENTRY_VECTOR_MASKED: u32 = 1;

pub(super) struct SingleVectorInstall {
    pub(super) capability: crate::arch::pci::MsixCapability,
    /// Exclusive claim on the function. Held for the transaction's whole life
    /// so no other driver can reprogram the interrupt this one armed, and so
    /// teardown still owns the function it is quiescing.
    pub(super) attach: Option<crate::arch::pci::PciAttach>,
    pub(super) vector: Option<crate::arch::msi::CommittedMsiVector>,
}

impl SingleVectorInstall {
    pub(super) fn retain_permanent(mut self) -> u8 {
        self.attach
            .take()
            .expect("DVM ring interrupt transaction lost its device claim")
            .retain_permanent();
        self.vector
            .take()
            .expect("DVM ring interrupt transaction lost vector ownership")
            .retain_permanent()
    }
}

impl Drop for SingleVectorInstall {
    fn drop(&mut self) {
        if let (Some(attach), true) = (self.attach.as_ref(), self.vector.is_some()) {
            // Reverse device publication before revoking handler/vector
            // authority. Config-space readback in these helpers is the posted
            // write completion barrier.
            self.capability.set_function_masked(attach, true);
            self.capability.set_enabled(attach, false);
            drop(self.vector.take());
        }
    }
}

unsafe fn program_msix_entry(entry: *mut u8, message: crate::arch::msi::MsiMessage) {
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

pub(super) fn arm_single_ring_interrupt(
    device: crate::arch::pci::PciDevice,
    handler: crate::arch::msi::MsiHandler,
) -> Option<SingleVectorInstall> {
    // Claim the function before the first configuration write. An exclusive
    // claim is the correct strength here: this transport owns its ivshmem
    // function outright and nothing else may reprogram its MSI-X state.
    let attach = crate::arch::pci::attach(device, crate::arch::pci::PciAttachMode::Exclusive)?;
    let Some(capability) = device.msix_capability() else {
        return None;
    };
    if capability.table_entries() != VECTOR_COUNT {
        return None;
    }
    let Some(table_resource) = capability.table_resource(device) else {
        return None;
    };
    let Ok(table_len) = usize::try_from(table_resource.size) else {
        return None;
    };
    let Ok(table_offset) = usize::try_from(capability.table_offset()) else {
        return None;
    };
    if table_offset
        .checked_add(MSIX_ENTRY_BYTES)
        .is_none_or(|end| end > table_len)
    {
        return None;
    }
    capability.set_function_masked(&attach, true);
    capability.set_enabled(&attach, false);
    let Some(mut vector_lease) = crate::arch::msi::MsiVectorLease::allocate() else {
        return None;
    };
    if !vector_lease.register_handler(handler) {
        return None;
    }
    let Some(message) = vector_lease.message() else {
        return None;
    };
    let table = crate::driver::mmio::map_uncached(table_resource.start, table_len).cast::<u8>();
    if table.is_null() {
        return None;
    }
    // SAFETY: `table` maps `table_len` bytes of uncached MSI-X table and table_offset + MSIX_ENTRY_BYTES <= table_len was
    // checked above; the entry offsets are 4-byte aligned, volatile MMIO, and the mapping outlives the write.
    unsafe {
        program_msix_entry(table.add(table_offset), message);
        // ORDERING: SeqCst fence completes programming the entry before it is unmasked.
        core::sync::atomic::fence(Ordering::SeqCst);
        table
            .add(table_offset + MSIX_ENTRY_VECTOR_CONTROL_OFFSET)
            .cast::<u32>()
            .write_volatile(0);
    }
    // ORDERING: SeqCst fence completes the unmask before MSI-X is enabled in PCI config space.
    core::sync::atomic::fence(Ordering::SeqCst);
    capability.set_enabled(&attach, true);
    capability.set_function_masked(&attach, false);
    crate::driver::mmio::unmap(table.cast());
    Some(SingleVectorInstall {
        capability,
        attach: Some(attach),
        vector: Some(vector_lease.commit()),
    })
}

