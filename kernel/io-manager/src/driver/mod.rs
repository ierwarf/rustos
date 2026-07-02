// RING3-MIGRATION-REFERENCE START: hardware-probe exception: driverd owns
// driver/provider policy and service-driver selection. Ring0 keeps explicit
// broker hooks for Linux .ko loading, DMA/MMIO/IRQ substrate, and hardware
// alias probes over privileged bus state.
mod bus;
mod class;
mod devres;
mod export;
pub mod input;
mod kernel_api;
pub mod linux;
mod loader;
mod module_registry;
pub mod pci;
pub mod serio;

// Kernel driver role: privileged DMA/MMIO/IRQ substrate and narrow broker hooks.
// Driver/provider policy belongs in driverd/devmgrd. Driver-domain isolation is
// out of scope; Linux/Windows compatibility requires some kernel data paths.
pub mod dma;
pub mod iommu;
pub mod irq;
pub mod mmio;

use alloc::string::ToString;
use driver_abi::{DriverBus, DriverClass};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverLoadError {
    /// Caller-supplied class/bus is unknown to the kernel.
    UnsupportedTopology,
    /// The kernel broker has no in-kernel loader for this driver; the module is
    /// expected to be hosted by a user-space driver service that does not yet exist.
    LoaderUnimplemented,
    /// A supported loader was invoked but it failed for an operational reason
    /// (e.g. boot framebuffer registration rejected by the GUI subsystem).
    LoaderFailed,
    /// Fault injection rejected the load.
    FaultInjected,
}

const BOOTFB_DRIVER_NAME: &str = "bootfb";
const BOOTFB_DRIVER_MODULE_PATH: &str = "system/drivers/display/bootfb.ko";
const BOOTFB_ALIAS: &str = "platform:bootfb";
const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
const PCI_DEVICE_VIRTIO_NET_TRANSITIONAL: u16 = 0x1000;
const PCI_DEVICE_VIRTIO_GPU_TRANSITIONAL: u16 = 0x1010;
const PCI_DEVICE_VIRTIO_MODERN_BASE: u16 = 0x1040;
const PCI_DEVICE_VIRTIO_MODERN_END: u16 = 0x107f;

pub(crate) fn exported_kernel_api() -> *const driver_abi::DriverKernelApiV1 {
    kernel_api::exported_kernel_api()
}

pub(crate) fn runtime_executable_addr_is_known(addr: usize) -> bool {
    loader::runtime_executable_addr_is_known(addr)
}

pub fn initialize_loadable_modules_for_class(class: DriverClass) -> bool {
    class::is_supported(class)
}

pub(crate) fn register_kernel_builtin(name: &'static str, class: DriverClass, bus: DriverBus) {
    debug_assert!(class::is_supported(class));
    debug_assert!(bus::is_supported(bus));
    let _ = name;
}

pub fn load_module_image_from_policy(
    name: &str,
    class: u32,
    bus: u32,
    image_path: &str,
    linux_driver_names: &str,
    _policy_flags: u64,
    _preferred_width: u32,
    _preferred_height: u32,
) -> Result<(), DriverLoadError> {
    if nucleus_core::util::fault_injection::should_fail("driver.module.load") {
        return Err(DriverLoadError::FaultInjected);
    }

    let class = decode_class(class).ok_or(DriverLoadError::UnsupportedTopology)?;
    let bus = decode_bus(bus).ok_or(DriverLoadError::UnsupportedTopology)?;
    if !class::is_supported(class) || !bus::is_supported(bus) {
        return Err(DriverLoadError::UnsupportedTopology);
    }

    let name = leak_policy_text(name)?;
    let image_path = leak_policy_text(image_path)?;
    let linux_driver_names = leak_policy_text(linux_driver_names)?;
    match loader::load_module_image_explicit(name, class, bus, image_path, linux_driver_names) {
        Ok(_) => Ok(()),
        Err(_)
            if name == BOOTFB_DRIVER_NAME
                && class == DriverClass::Display
                && bus == DriverBus::Platform
                && image_path == BOOTFB_DRIVER_MODULE_PATH
                && (linux_driver_names.is_empty() || linux_driver_names == BOOTFB_DRIVER_NAME) =>
        {
            load_boot_framebuffer_provider()
        }
        Err(_) => Err(DriverLoadError::LoaderFailed),
    }
}

pub fn hardware_alias_present(alias: &str, class: u32, bus: u32) -> bool {
    let Some(class) = decode_class(class) else {
        return false;
    };
    let Some(bus) = decode_bus(bus) else {
        return false;
    };

    match bus {
        DriverBus::Platform => {
            matches!((class, alias), (DriverClass::Display, BOOTFB_ALIAS))
                && !display_primary_provider_active()
                && crate::storage::boot_volume::boot_framebuffer_info().is_some()
        }
        DriverBus::Pci => pci_alias_present(alias),
        DriverBus::Virtio => {
            if class == DriverClass::Display && display_primary_provider_active() {
                return false;
            }
            virtio_alias_present(alias)
        }
        DriverBus::Usb => usb_alias_present(alias, class),
        DriverBus::Serio => serio_alias_present(alias),
        _ => false,
    }
}

fn display_primary_provider_active() -> bool {
    crate::io::gui::display_info()
        .map(|display| {
            display.flags & crate::user::abi::device::DISPLAY_INFO_FLAG_PRIMARY_PROVIDER != 0
        })
        .unwrap_or(false)
}

fn load_boot_framebuffer_provider() -> Result<(), DriverLoadError> {
    let framebuffer = crate::storage::boot_volume::boot_framebuffer_info()
        .ok_or(DriverLoadError::LoaderFailed)?;
    if crate::io::gui::install_boot_framebuffer_fallback(framebuffer) {
        Ok(())
    } else {
        Err(DriverLoadError::LoaderFailed)
    }
}

fn leak_policy_text(value: &str) -> Result<&'static str, DriverLoadError> {
    if value
        .bytes()
        .any(|byte| byte == b'\0' || byte == b'\n' || byte == b'\r')
    {
        return Err(DriverLoadError::UnsupportedTopology);
    }
    Ok(alloc::boxed::Box::leak(value.to_string().into_boxed_str()))
}

fn decode_class(value: u32) -> Option<DriverClass> {
    match value {
        value if value == DriverClass::Display as u32 => Some(DriverClass::Display),
        value if value == DriverClass::Input as u32 => Some(DriverClass::Input),
        value if value == DriverClass::Network as u32 => Some(DriverClass::Network),
        value if value == DriverClass::Usb as u32 => Some(DriverClass::Usb),
        value if value == DriverClass::Storage as u32 => Some(DriverClass::Storage),
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

fn usb_alias_present(alias: &str, class: DriverClass) -> bool {
    if alias.starts_with("usb:") {
        return class == DriverClass::Input
            && (crate::usb::hid_interfaces_available()
                || crate::usb::host_controllers_available());
    }
    if alias.starts_with("hid:") {
        return class == DriverClass::Input && crate::usb::hid_interfaces_available();
    }
    false
}

fn serio_alias_present(alias: &str) -> bool {
    alias.starts_with("serio:") && serio::ports_available()
}

fn pci_alias_matches(alias: &str, pci: crate::arch::pci::PciDevice) -> bool {
    let Some((matches_vendor, mut offset)) =
        match_hex_field_at_with(alias, 0, "pci:v", || u32::from(pci.vendor_id()), 8)
    else {
        return false;
    };
    if !matches_vendor {
        return false;
    }

    if let Some((matches_device, new_offset)) =
        match_hex_field_at_with(alias, offset, "d", || u32::from(pci.device_id()), 8)
    {
        if !matches_device {
            return false;
        }
        offset = new_offset;
    }

    optional_hex_field_matches_at(
        alias,
        &mut offset,
        "sv",
        || u32::from(pci.subsystem_vendor_id()),
        8,
    ) && optional_hex_field_matches_at(
        alias,
        &mut offset,
        "sd",
        || u32::from(pci.subsystem_device_id()),
        8,
    ) && optional_hex_field_matches_at(alias, &mut offset, "bc", || u32::from(pci.class_code()), 2)
        && optional_hex_field_matches_at(alias, &mut offset, "sc", || u32::from(pci.subclass()), 2)
        && optional_hex_field_matches_at(alias, &mut offset, "i", || u32::from(pci.prog_if()), 2)
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

fn optional_hex_field_matches_at<F>(
    alias: &str,
    offset: &mut usize,
    marker: &str,
    value: F,
    digits: usize,
) -> bool
where
    F: FnOnce() -> u32,
{
    let Some(rest) = alias.get(*offset..) else {
        return false;
    };
    if !rest.starts_with(marker) {
        return true;
    }

    match_hex_field_at_with(alias, *offset, marker, value, digits)
        .map(|(matches, new_offset)| {
            *offset = new_offset;
            matches
        })
        .unwrap_or(false)
}

fn match_hex_field_at(
    alias: &str,
    offset: usize,
    marker: &str,
    value: u32,
    digits: usize,
) -> Option<(bool, usize)> {
    match_hex_field_at_with(alias, offset, marker, || value, digits)
}

fn match_hex_field_at_with<F>(
    alias: &str,
    offset: usize,
    marker: &str,
    value: F,
    digits: usize,
) -> Option<(bool, usize)>
where
    F: FnOnce() -> u32,
{
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
    Some((parsed == value(), value_end))
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
// RING3-MIGRATION-REFERENCE END: driverd-owned policy hardware-probe exception.
