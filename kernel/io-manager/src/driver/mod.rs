mod bus;
mod class;

// Kernel driver role: privileged DMA/MMIO/IRQ substrate and narrow broker hooks.
// Driver/provider policy belongs in driverd/devmgrd. Driver-domain isolation is
// out of scope; Linux/Windows compatibility requires some kernel data paths.
pub mod dma;
pub mod iommu;
pub mod irq;
pub mod mmio;

use driver_abi::{DriverBus, DriverClass};

const BOOTFB_DRIVER_NAME: &str = "bootfb";
const BOOTFB_DRIVER_MODULE_PATH: &str = "system/drivers/display/bootfb.ko";
const BOOTFB_ALIAS: &str = "platform:bootfb";
const DISPLAY_PRIMARY_PROVIDER_GROUP: &str = "display-primary";
const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
const PCI_DEVICE_VIRTIO_NET_TRANSITIONAL: u16 = 0x1000;
const PCI_DEVICE_VIRTIO_GPU_TRANSITIONAL: u16 = 0x1010;
const PCI_DEVICE_VIRTIO_MODERN_BASE: u16 = 0x1040;
const PCI_DEVICE_VIRTIO_MODERN_END: u16 = 0x107f;

pub fn initialize_loadable_modules_for_class(class: DriverClass) -> bool {
    class::is_supported(class)
}

pub fn load_module_image_from_policy(
    name: &str,
    class: u32,
    bus: u32,
    image_path: &str,
    linux_driver_names: &str,
) -> Result<(), &'static str> {
    if nucleus_core::util::fault_injection::should_fail("driver.module.load") {
        return Err("fault injection rejected driver module load");
    }

    let class = decode_class(class).ok_or("driver class is unsupported")?;
    let bus = decode_bus(bus).ok_or("driver bus is unsupported")?;
    if !class::is_supported(class) || !bus::is_supported(bus) {
        return Err("driver core is unsupported");
    }

    if name == BOOTFB_DRIVER_NAME
        && class == DriverClass::Display
        && bus == DriverBus::Platform
        && image_path == BOOTFB_DRIVER_MODULE_PATH
        && (linux_driver_names.is_empty() || linux_driver_names == BOOTFB_DRIVER_NAME)
    {
        return load_boot_framebuffer_provider();
    }

    Err("driver module loader is not available through this broker")
}

pub fn device_alias_present_from_policy(alias: &str, class: u32, bus: u32) -> bool {
    let Some(class) = decode_class(class) else {
        return false;
    };
    let Some(bus) = decode_bus(bus) else {
        return false;
    };

    match bus {
        DriverBus::Platform => {
            matches!((class, alias), (DriverClass::Display, BOOTFB_ALIAS))
                && crate::storage::boot_volume::boot_framebuffer_info().is_some()
        }
        DriverBus::Pci => pci_alias_present(alias),
        DriverBus::Virtio => virtio_alias_present(alias),
        _ => false,
    }
}

pub fn provider_group_active_from_policy(group: &str) -> bool {
    match group {
        DISPLAY_PRIMARY_PROVIDER_GROUP => crate::io::gui::primary_display_provider_active(),
        _ => false,
    }
}

fn load_boot_framebuffer_provider() -> Result<(), &'static str> {
    let framebuffer = crate::storage::boot_volume::boot_framebuffer_info()
        .ok_or("boot framebuffer unavailable")?;
    if crate::io::gui::install_native_driver_framebuffer(framebuffer) {
        Ok(())
    } else {
        Err("boot framebuffer registration failed")
    }
}

fn decode_class(value: u32) -> Option<DriverClass> {
    match value {
        value if value == DriverClass::Display as u32 => Some(DriverClass::Display),
        value if value == DriverClass::Input as u32 => Some(DriverClass::Input),
        value if value == DriverClass::Network as u32 => Some(DriverClass::Network),
        _ => None,
    }
}

fn decode_bus(value: u32) -> Option<DriverBus> {
    match value {
        value if value == DriverBus::Platform as u32 => Some(DriverBus::Platform),
        value if value == DriverBus::Serio as u32 => Some(DriverBus::Serio),
        value if value == DriverBus::Usb as u32 => Some(DriverBus::Usb),
        value if value == DriverBus::Pci as u32 => Some(DriverBus::Pci),
        value if value == DriverBus::Virtio as u32 => Some(DriverBus::Virtio),
        _ => None,
    }
}

fn pci_alias_present(alias: &str) -> bool {
    if !alias.starts_with("pci:") {
        return false;
    }

    let mut present = false;
    crate::arch::pci::visit_devices(|pci| {
        present = pci_alias_matches(alias, pci);
        present
    });
    present
}

fn virtio_alias_present(alias: &str) -> bool {
    if !alias.starts_with("virtio:") {
        return false;
    }

    let mut present = false;
    crate::arch::pci::visit_devices(|pci| {
        if pci.vendor_id() != PCI_VENDOR_VIRTIO {
            return false;
        }
        let Some(device_type) = virtio_device_type(pci.device_id()) else {
            return false;
        };
        let Some((matches_device, _)) = match_hex_field_at(alias, 0, "virtio:d", device_type, 8)
        else {
            return false;
        };
        present = matches_device;
        present
    });
    present
}

fn pci_alias_matches(alias: &str, pci: crate::arch::pci::PciDevice) -> bool {
    let Some((matches_vendor, mut offset)) =
        match_hex_field_at(alias, 0, "pci:v", u32::from(pci.vendor_id()), 8)
    else {
        return false;
    };
    if !matches_vendor {
        return false;
    }

    if let Some((matches_device, new_offset)) =
        match_hex_field_at(alias, offset, "d", u32::from(pci.device_id()), 8)
    {
        if !matches_device {
            return false;
        }
        offset = new_offset;
    }

    let _ = offset;
    optional_hex_field_matches(alias, "sv", u32::from(pci.subsystem_vendor_id()), 8)
        && optional_hex_field_matches(alias, "sd", u32::from(pci.subsystem_device_id()), 8)
        && optional_hex_field_matches(alias, "bc", u32::from(pci.class_code()), 2)
        && optional_hex_field_matches(alias, "sc", u32::from(pci.subclass()), 2)
        && optional_hex_field_matches(alias, "i", u32::from(pci.prog_if()), 2)
}

fn virtio_device_type(device_id: u16) -> Option<u32> {
    match device_id {
        PCI_DEVICE_VIRTIO_NET_TRANSITIONAL => Some(1),
        PCI_DEVICE_VIRTIO_GPU_TRANSITIONAL => Some(16),
        PCI_DEVICE_VIRTIO_MODERN_BASE..=PCI_DEVICE_VIRTIO_MODERN_END => {
            Some(u32::from(device_id - PCI_DEVICE_VIRTIO_MODERN_BASE))
        }
        _ => None,
    }
}

fn optional_hex_field_matches(alias: &str, marker: &str, value: u32, digits: usize) -> bool {
    let Some(offset) = alias.find(marker) else {
        return true;
    };
    match_hex_field_at(alias, offset, marker, value, digits)
        .map(|(matches, _)| matches)
        .unwrap_or(false)
}

fn match_hex_field_at(
    alias: &str,
    offset: usize,
    marker: &str,
    value: u32,
    digits: usize,
) -> Option<(bool, usize)> {
    let rest = alias.get(offset..)?;
    if !rest.starts_with(marker) {
        return None;
    }
    let value_start = offset.checked_add(marker.len())?;
    if alias.as_bytes().get(value_start).copied() == Some(b'*') {
        return Some((true, value_start + 1));
    }
    let value_end = value_start.checked_add(digits)?;
    let field = alias.get(value_start..value_end)?;
    let parsed = parse_fixed_hex(field)?;
    Some((parsed == value, value_end))
}

fn parse_fixed_hex(field: &str) -> Option<u32> {
    let mut value = 0_u32;
    for byte in field.bytes() {
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'f' => u32::from(byte - b'a' + 10),
            b'A'..=b'F' => u32::from(byte - b'A' + 10),
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit)?;
    }
    Some(value)
}
