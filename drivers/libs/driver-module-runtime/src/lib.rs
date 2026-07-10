#![no_std]

use core::mem::size_of;
use core::ptr;

use driver_abi::{
    DisplayFramebufferRegistration, DriverKernelApiV1, DriverLogLevel, DriverMmioCachePolicy,
    DriverPciBarInfo, DriverPciDeviceInfo,
};

static mut KERNEL_API: *const DriverKernelApiV1 = ptr::null();

/// # Safety
///
/// `api_ptr` must reference a readable `DriverKernelApiV1` that remains valid
/// for the lifetime of the loaded driver module.
pub unsafe fn bind(api_ptr: *const DriverKernelApiV1) -> Result<(), i32> {
    if api_ptr.is_null() {
        return Err(-22);
    }

    let api = unsafe { &*api_ptr };
    if api.abi_version != driver_abi::DRIVER_MODULE_ABI_VERSION {
        return Err(-22);
    }
    if api.struct_size < size_of::<DriverKernelApiV1>() as u32 {
        return Err(-22);
    }

    unsafe {
        KERNEL_API = api_ptr;
    }
    Ok(())
}

pub fn log_error(message: &str) {
    let _ = log(DriverLogLevel::Error, message);
}

pub fn log_warn(message: &str) {
    let _ = log(DriverLogLevel::Warn, message);
}

pub fn log_info(message: &str) {
    let _ = log(DriverLogLevel::Info, message);
}

pub fn find_pci_device(
    vendor_id: u16,
    device_id: u16,
    index: u32,
) -> Result<DriverPciDeviceInfo, i32> {
    let mut info = DriverPciDeviceInfo::default();
    let api = kernel_api()?;
    let func = api.pci_find_device.ok_or(-95)?;
    let status = unsafe { func(vendor_id, device_id, index, &mut info) };
    if status != 0 {
        return Err(status);
    }
    Ok(info)
}

pub fn read_pci_config_u32(device: DriverPciDeviceInfo, offset: u32) -> Result<u32, i32> {
    let mut value = 0_u32;
    let api = kernel_api()?;
    let func = api.pci_read_config_u32.ok_or(-95)?;
    let status = unsafe {
        func(
            device.segment,
            device.bus,
            device.device,
            device.function,
            offset,
            &mut value,
        )
    };
    if status != 0 {
        return Err(status);
    }
    Ok(value)
}

pub fn write_pci_config_u32(
    device: DriverPciDeviceInfo,
    offset: u32,
    value: u32,
) -> Result<(), i32> {
    let api = kernel_api()?;
    let func = api.pci_write_config_u32.ok_or(-95)?;
    let status = unsafe {
        func(
            device.segment,
            device.bus,
            device.device,
            device.function,
            offset,
            value,
        )
    };
    if status != 0 {
        return Err(status);
    }
    Ok(())
}

pub fn get_pci_bar(device: DriverPciDeviceInfo, bar_index: u32) -> Result<DriverPciBarInfo, i32> {
    let mut info = DriverPciBarInfo::default();
    let api = kernel_api()?;
    let func = api.pci_get_bar_info.ok_or(-95)?;
    let status = unsafe {
        func(
            device.segment,
            device.bus,
            device.device,
            device.function,
            bar_index,
            &mut info,
        )
    };
    if status != 0 {
        return Err(status);
    }
    Ok(info)
}

pub fn map_mmio(phys_addr: u64, size: u64, policy: DriverMmioCachePolicy) -> Result<u64, i32> {
    let mut virt_addr = 0_u64;
    let api = kernel_api()?;
    let func = api.map_mmio.ok_or(-95)?;
    let status = unsafe { func(phys_addr, size, policy as u32, &mut virt_addr) };
    if status != 0 {
        return Err(status);
    }
    if virt_addr == 0 {
        return Err(-12);
    }
    Ok(virt_addr)
}

pub fn boot_file_len(path: &str) -> Result<u64, i32> {
    let mut read_len = 0_u64;
    let api = kernel_api()?;
    let func = api.read_boot_file.ok_or(-95)?;
    let status = unsafe {
        func(
            path.as_ptr(),
            path.len() as u32,
            ptr::null_mut(),
            0,
            &mut read_len,
        )
    };
    if status != 0 {
        return Err(status);
    }
    Ok(read_len)
}

pub fn read_boot_file(path: &str, dest: &mut [u8]) -> Result<usize, i32> {
    let mut read_len = 0_u64;
    let api = kernel_api()?;
    let func = api.read_boot_file.ok_or(-95)?;
    let status = unsafe {
        func(
            path.as_ptr(),
            path.len() as u32,
            dest.as_mut_ptr(),
            dest.len() as u64,
            &mut read_len,
        )
    };
    if status != 0 {
        return Err(status);
    }
    Ok(read_len as usize)
}

pub fn query_boot_framebuffer() -> Result<DisplayFramebufferRegistration, i32> {
    let mut framebuffer = DisplayFramebufferRegistration::default();
    let api = kernel_api()?;
    let func = api.query_boot_framebuffer.ok_or(-95)?;
    let status = unsafe { func(&mut framebuffer) };
    if status != 0 {
        return Err(status);
    }
    Ok(framebuffer)
}

pub fn register_display_framebuffer(
    framebuffer: &DisplayFramebufferRegistration,
) -> Result<(), i32> {
    let api = kernel_api()?;
    let func = api.register_display_framebuffer.ok_or(-95)?;
    let status = unsafe { func(framebuffer) };
    if status != 0 {
        return Err(status);
    }
    Ok(())
}

fn log(level: DriverLogLevel, message: &str) -> Result<(), i32> {
    let api = kernel_api()?;
    let func = api.log.ok_or(-95)?;
    let status = unsafe { func(level as u32, message.as_ptr(), message.len() as u32) };
    if status != 0 {
        return Err(status);
    }
    Ok(())
}

fn kernel_api() -> Result<&'static DriverKernelApiV1, i32> {
    let ptr = unsafe { KERNEL_API };
    if ptr.is_null() {
        return Err(-95);
    }
    Ok(unsafe { &*ptr })
}
