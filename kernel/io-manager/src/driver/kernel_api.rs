use core::{ptr, slice, str};

use driver_abi::{
    DisplayFramebufferRegistration, DriverKernelApiV1, DriverLogLevel, DriverMmioCachePolicy,
    DriverPciBarInfo, DriverPciDeviceInfo, DISPLAY_FRAMEBUFFER_FLAG_BOOT_FRAMEBUFFER,
    PCI_BAR_FLAG_64BIT, PCI_BAR_FLAG_IO_SPACE, PCI_BAR_FLAG_PREFETCHABLE,
};

use super::mmio;

static DRIVER_KERNEL_API: DriverKernelApiV1 = DriverKernelApiV1::new(
    None,
    None,
    Some(register_display_framebuffer),
    Some(driver_log),
    Some(driver_pci_find_device),
    Some(driver_pci_read_config_u32),
    Some(driver_pci_write_config_u32),
    Some(driver_pci_get_bar_info),
    Some(driver_map_mmio),
    Some(driver_read_boot_file),
    Some(driver_query_boot_framebuffer),
);

pub(super) fn exported_kernel_api() -> *const DriverKernelApiV1 {
    &DRIVER_KERNEL_API
}

unsafe extern "C" fn register_display_framebuffer(
    framebuffer: *const DisplayFramebufferRegistration,
) -> i32 {
    unsafe { crate::io::gui::register_driver_framebuffer(framebuffer) }
}

unsafe extern "C" fn driver_log(level: u32, message_ptr: *const u8, message_len: u32) -> i32 {
    if message_len == 0 {
        return 0;
    }
    if message_ptr.is_null() {
        return -14;
    }

    let bytes = unsafe { slice::from_raw_parts(message_ptr, message_len as usize) };
    let Ok(message) = str::from_utf8(bytes) else {
        return -22;
    };

    match level {
        value if value == DriverLogLevel::Error as u32 => {
            crate::debug::error!(driver, "{}", message);
        }
        value if value == DriverLogLevel::Warn as u32 => {
            crate::debug::warn!(driver, "{}", message);
        }
        value if value == DriverLogLevel::Info as u32 => {
            crate::debug::info!(driver, "{}", message);
        }
        value if value == DriverLogLevel::Debug as u32 => {
            crate::debug::debug!(driver, "{}", message);
        }
        _ => return -22,
    }

    0
}

unsafe extern "C" fn driver_pci_find_device(
    vendor_id: u16,
    device_id: u16,
    index: u32,
    out_info: *mut DriverPciDeviceInfo,
) -> i32 {
    if out_info.is_null() {
        return -14;
    }

    let mut match_index = 0_u32;
    let mut found = None;
    crate::arch::pci::visit_devices(|device| {
        if device.vendor_id() != vendor_id || device.device_id() != device_id {
            return false;
        }
        if match_index == index {
            found = Some(device);
            return true;
        }
        match_index = match_index.saturating_add(1);
        false
    });

    let Some(device) = found else {
        return -19;
    };

    unsafe {
        ptr::write(out_info, driver_pci_device_info(device));
    }
    0
}

unsafe extern "C" fn driver_pci_read_config_u32(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u32,
    out_value: *mut u32,
) -> i32 {
    if out_value.is_null() {
        return -14;
    }
    let Ok(offset) = u8::try_from(offset) else {
        return -22;
    };
    if (offset & 0x3) != 0 {
        return -22;
    }

    let device = match driver_pci_device(segment, bus, device, function) {
        Ok(device) => device,
        Err(status) => return status,
    };
    if usize::from(offset) >= device.config_size() as usize {
        return -22;
    }

    unsafe {
        ptr::write(out_value, device.read_u32(offset));
    }
    0
}

unsafe extern "C" fn driver_pci_write_config_u32(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u32,
    value: u32,
) -> i32 {
    let Ok(offset) = u8::try_from(offset) else {
        return -22;
    };
    if (offset & 0x3) != 0 {
        return -22;
    }

    let device = match driver_pci_device(segment, bus, device, function) {
        Ok(device) => device,
        Err(status) => return status,
    };
    if usize::from(offset) >= device.config_size() as usize {
        return -22;
    }

    device.write_u32(offset, value);
    0
}

unsafe extern "C" fn driver_pci_get_bar_info(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    bar_index: u32,
    out_info: *mut DriverPciBarInfo,
) -> i32 {
    if out_info.is_null() {
        return -14;
    }

    let device = match driver_pci_device(segment, bus, device, function) {
        Ok(device) => device,
        Err(status) => return status,
    };

    let Some(resource) = device.resource(bar_index as usize) else {
        return -19;
    };

    let mut flags = 0_u32;
    if resource.is_io {
        flags |= PCI_BAR_FLAG_IO_SPACE;
    }
    if resource.prefetchable {
        flags |= PCI_BAR_FLAG_PREFETCHABLE;
    }
    if resource.is_64bit {
        flags |= PCI_BAR_FLAG_64BIT;
    }

    unsafe {
        ptr::write(
            out_info,
            DriverPciBarInfo {
                base: resource.start,
                size: resource.size,
                flags,
                reserved0: 0,
            },
        );
    }
    0
}

unsafe extern "C" fn driver_map_mmio(
    phys_addr: u64,
    size: u64,
    cache_policy: u32,
    out_virt_addr: *mut u64,
) -> i32 {
    if out_virt_addr.is_null() {
        return -14;
    }
    let Ok(size) = usize::try_from(size) else {
        return -22;
    };
    if size == 0 {
        return -22;
    }

    let write_combine = match cache_policy {
        value if value == DriverMmioCachePolicy::Uncached as u32 => false,
        value if value == DriverMmioCachePolicy::WriteCombine as u32 => true,
        _ => return -22,
    };

    let addr = mmio::map(phys_addr, size, write_combine);
    if addr.is_null() {
        return -12;
    }

    unsafe {
        ptr::write(out_virt_addr, addr as usize as u64);
    }
    0
}

unsafe extern "C" fn driver_read_boot_file(
    path_ptr: *const u8,
    path_len: u32,
    dst: *mut u8,
    dst_len: u64,
    out_read_len: *mut u64,
) -> i32 {
    if path_len == 0 || path_ptr.is_null() {
        return -22;
    }

    let path_bytes = unsafe { slice::from_raw_parts(path_ptr, path_len as usize) };
    let Ok(path) = str::from_utf8(path_bytes) else {
        return -22;
    };

    let bytes = match crate::storage::boot_volume::read_file_to_vec(path) {
        Ok(bytes) => bytes,
        Err(_) => return -2,
    };

    if !out_read_len.is_null() {
        unsafe {
            ptr::write(out_read_len, bytes.len() as u64);
        }
    }

    if dst.is_null() {
        return if dst_len == 0 { 0 } else { -14 };
    }
    if bytes.len() > dst_len as usize {
        return -75;
    }

    unsafe {
        crate::arch::simd::copy_fast(bytes.as_ptr(), dst, bytes.len());
    }
    0
}

unsafe extern "C" fn driver_query_boot_framebuffer(
    out_info: *mut DisplayFramebufferRegistration,
) -> i32 {
    if out_info.is_null() {
        return -14;
    }
    let Some(info) = crate::storage::boot_volume::boot_framebuffer_info() else {
        return -19;
    };

    let pixel_format = match info.pixel_format {
        boot_protocol::BootPixelFormat::Rgb => driver_abi::DisplayPixelFormat::Rgb as u32,
        boot_protocol::BootPixelFormat::Bgr => driver_abi::DisplayPixelFormat::Bgr as u32,
        boot_protocol::BootPixelFormat::Bitmask => driver_abi::DisplayPixelFormat::Bitmask as u32,
        boot_protocol::BootPixelFormat::Unknown => driver_abi::DisplayPixelFormat::Unknown as u32,
    };

    unsafe {
        ptr::write(
            out_info,
            DisplayFramebufferRegistration {
                addr: info.addr,
                size: info.size,
                back_buffer_addr: info.back_buffer_addr,
                back_buffer_size: info.back_buffer_size,
                width: info.width,
                height: info.height,
                stride: info.stride,
                pixel_format,
                bytes_per_pixel: info.bytes_per_pixel,
                flags: DISPLAY_FRAMEBUFFER_FLAG_BOOT_FRAMEBUFFER,
                reserved: [0; 2],
            },
        );
    }
    0
}

fn driver_pci_device(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
) -> Result<crate::arch::pci::PciDevice, i32> {
    let device = crate::arch::pci::PciDevice {
        segment,
        bus,
        device,
        function,
    };
    if !device.is_present() {
        return Err(-19);
    }
    Ok(device)
}

fn driver_pci_device_info(device: crate::arch::pci::PciDevice) -> DriverPciDeviceInfo {
    DriverPciDeviceInfo {
        segment: device.segment,
        bus: device.bus,
        device: device.device,
        function: device.function,
        revision: device.revision(),
        prog_if: device.prog_if(),
        subclass: device.subclass(),
        class_code: device.class_code(),
        subsystem_vendor_id: device.subsystem_vendor_id(),
        subsystem_device_id: device.subsystem_device_id(),
        interrupt_line: device.interrupt_line(),
        interrupt_pin: device.interrupt_pin(),
        config_size: device.config_size() as u16,
        reserved0: 0,
    }
}
