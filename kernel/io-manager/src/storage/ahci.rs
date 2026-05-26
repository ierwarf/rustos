use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use storage_core::BlockDevice as SharedBlockDevice;

use crate::arch::pci::PciDevice;
use crate::storage::block::{BlockDeviceOps, BlockTransportKind};
use crate::storage::fat::{DiskIoError, IoResult};

const PCI_CLASS_MASS_STORAGE: u8 = 0x01;
const PCI_SUBCLASS_SATA: u8 = 0x06;
const PCI_PROG_IF_AHCI: u8 = 0x01;
const AHCI_BAR_INDEX: usize = 5;

const HBA_CAP: usize = 0x00;
const HBA_GHC: usize = 0x04;
const HBA_PI: usize = 0x0c;
const HBA_VS: usize = 0x10;

const GHC_AE: u32 = 1 << 31;
const CAP_S64A: u32 = 1 << 31;

struct AhciController {
    mmio_base: usize,
    mmio_len: usize,
    dma_key: *mut c_void,
}

unsafe impl Send for AhciController {}
unsafe impl Sync for AhciController {}

// storaged owns port admission, command issue, and I/O. Ring0 keeps only PCI
// controller discovery and MMIO grant.
struct AhciBlockDevice {
    sector_count: u64,
    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
    model: String,
}

unsafe impl Send for AhciBlockDevice {}
unsafe impl Sync for AhciBlockDevice {}

pub(crate) fn probe_devices() -> Vec<Box<dyn BlockDeviceOps>> {
    let mut devices = Vec::new();
    crate::arch::pci::visit_devices(|pci| {
        if pci.class_code() != PCI_CLASS_MASS_STORAGE
            || pci.subclass() != PCI_SUBCLASS_SATA
            || pci.prog_if() != PCI_PROG_IF_AHCI
        {
            return false;
        }
        match probe_controller(pci) {
            Ok(mut found) => devices.append(&mut found),
            Err(_error) => {
                crate::debug::println!(
                    "ahci: controller {:02x}:{:02x}.{} skipped: {:?}",
                    pci.bus,
                    pci.device,
                    pci.function,
                    _error
                );
            }
        }
        false
    });
    devices
}

impl SharedBlockDevice for AhciBlockDevice {
    fn logical_block_size(&self) -> usize {
        512
    }

    fn block_count(&self) -> u64 {
        self.sector_count
    }

    fn read_blocks(&mut self, _lba: u64, _out: &mut [u8]) -> IoResult<()> {
        Err(DiskIoError::Unsupported)
    }

    fn write_blocks(&mut self, _lba: u64, _input: &[u8]) -> IoResult<()> {
        Err(DiskIoError::Unsupported)
    }

    fn flush(&mut self) -> IoResult<()> {
        Err(DiskIoError::Unsupported)
    }
}

impl BlockDeviceOps for AhciBlockDevice {
    fn transport_kind(&self) -> BlockTransportKind {
        BlockTransportKind::Ahci
    }

    fn readonly(&self) -> bool {
        false
    }
}

impl AhciController {
    fn read_u32(&self, offset: usize) -> u32 {
        debug_assert!(offset + 4 <= self.mmio_len);
        unsafe { ptr::read_volatile((self.mmio_base + offset) as *const u32) }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        debug_assert!(offset + 4 <= self.mmio_len);
        unsafe { ptr::write_volatile((self.mmio_base + offset) as *mut u32, value) };
    }

    fn enable_ahci_mode(&self) {
        let ghc = self.read_u32(HBA_GHC);
        self.write_u32(HBA_GHC, ghc | GHC_AE);
    }

    fn supports_64bit_dma(&self) -> bool {
        (self.read_u32(HBA_CAP) & CAP_S64A) != 0
    }

    fn implemented_ports(&self) -> u32 {
        self.read_u32(HBA_PI)
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
    fn version(&self) -> u32 {
        self.read_u32(HBA_VS)
    }
}

fn probe_controller(pci: PciDevice) -> Result<Vec<Box<dyn BlockDeviceOps>>, DiskIoError> {
    let abar = pci
        .resource(AHCI_BAR_INDEX)
        .filter(|resource| !resource.is_io && resource.size >= 0x200)
        .ok_or(DiskIoError::NotPresent)?;

    pci.enable_memory_bus_master();
    let mmio = crate::driver::mmio::map(abar.start, abar.size as usize, false);
    if mmio.is_null() {
        return Err(DiskIoError::NotPresent);
    }

    let controller = AhciController {
        mmio_base: mmio as usize,
        mmio_len: abar.size as usize,
        dma_key: mmio,
    };
    let dma_mask = if controller.supports_64bit_dma() {
        u64::MAX
    } else {
        u32::MAX as u64
    };
    crate::driver::dma::set_mask_and_coherent(controller.dma_key, dma_mask);
    controller.enable_ahci_mode();

    let ports = controller.implemented_ports();
    crate::debug::println!(
        "ahci: controller {:02x}:{:02x}.{} version={:#x} ports={:#x}",
        pci.bus,
        pci.device,
        pci.function,
        controller.version(),
        ports
    );

    // RING3-MIGRATION: storaged owns port admission and I/O. Ring0 exposes the
    // MMIO region via a grant; storaged probes ports directly in ring3.
    Ok(Vec::new())
}
