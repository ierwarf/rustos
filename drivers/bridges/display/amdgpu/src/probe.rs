use driver_abi::{
    DriverPciBarInfo, DriverPciDeviceInfo, PCI_BAR_FLAG_IO_SPACE, PCI_BAR_FLAG_PREFETCHABLE,
};

use crate::api;

const AMD_VENDOR_ID: u16 = 0x1002;
const PHOENIX3_DEVICE_ID: u16 = 0x1900;
const PCI_COMMAND_OFFSET: u32 = 0x04;
const PCI_COMMAND_MEMORY_SPACE: u16 = 1 << 1;
const PCI_COMMAND_BUS_MASTER: u16 = 1 << 2;
const REGISTER_BAR_INDEX: u32 = 5;
const FRAMEBUFFER_BAR_INDEX: u32 = 0;
const MIN_REGISTER_BAR_BYTES: u64 = 512 * 1024;

#[derive(Clone, Copy)]
pub struct Phoenix3Device {
    pub framebuffer_bar: DriverPciBarInfo,
    pub register_bar: DriverPciBarInfo,
}

pub fn probe_phoenix3() -> Option<Phoenix3Device> {
    let pci = match api::find_pci_device(AMD_VENDOR_ID, PHOENIX3_DEVICE_ID, 0) {
        Ok(pci) => pci,
        Err(-19) => return None,
        Err(_) => return None,
    };

    if ensure_command_bits(pci).is_err() {
        return None;
    }

    let framebuffer_bar = match api::get_pci_bar(pci, FRAMEBUFFER_BAR_INDEX) {
        Ok(bar) => bar,
        Err(_) => return None,
    };
    let register_bar = match api::get_pci_bar(pci, REGISTER_BAR_INDEX) {
        Ok(bar) => bar,
        Err(_) => return None,
    };

    if !framebuffer_bar_is_valid(framebuffer_bar) || !register_bar_is_valid(register_bar) {
        return None;
    }

    Some(Phoenix3Device {
        framebuffer_bar,
        register_bar,
    })
}

fn ensure_command_bits(device: DriverPciDeviceInfo) -> Result<(), i32> {
    let command = api::read_pci_config_u32(device, PCI_COMMAND_OFFSET)?;
    let command_low = command as u16;
    let want_low = command_low | PCI_COMMAND_MEMORY_SPACE | PCI_COMMAND_BUS_MASTER;
    if want_low != command_low {
        api::write_pci_config_u32(device, PCI_COMMAND_OFFSET, want_low as u32)?;
    }

    let updated = api::read_pci_config_u32(device, PCI_COMMAND_OFFSET)? as u16;
    if (updated & (PCI_COMMAND_MEMORY_SPACE | PCI_COMMAND_BUS_MASTER))
        != (PCI_COMMAND_MEMORY_SPACE | PCI_COMMAND_BUS_MASTER)
    {
        return Err(-5);
    }
    Ok(())
}

fn framebuffer_bar_is_valid(bar: DriverPciBarInfo) -> bool {
    bar.base != 0
        && bar.size != 0
        && (bar.flags & PCI_BAR_FLAG_IO_SPACE) == 0
        && (bar.flags & PCI_BAR_FLAG_PREFETCHABLE) != 0
}

fn register_bar_is_valid(bar: DriverPciBarInfo) -> bool {
    bar.base != 0 && bar.size >= MIN_REGISTER_BAR_BYTES && (bar.flags & PCI_BAR_FLAG_IO_SPACE) == 0
}
