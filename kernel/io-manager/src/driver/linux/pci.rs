// RING3-MIGRATION-REFERENCE START: Linux .ko PCI shim compatibility
// substrate exception. driverd owns load/provider policy; ring0 keeps Linux
// PCI symbol dispatch and privileged PCI/MMIO resource access for .ko modules.
use core::ffi::{c_char, c_void};

use super::compat::{LinuxCompatPciDev, LinuxCompatPciDriver};

pub(crate) unsafe extern "C" fn __pci_register_driver(
    driver: *mut LinuxCompatPciDriver,
    _owner: *mut c_void,
    mod_name: *const c_char,
) -> i32 {
    crate::driver::symbol_events::record_pci_probe_init_symbol("__pci_register_driver", mod_name);
    crate::driver::pci::register_linux_driver(driver)
}

pub(crate) unsafe extern "C" fn pci_unregister_driver(driver: *mut LinuxCompatPciDriver) {
    crate::driver::pci::unregister_linux_driver(driver);
}

pub(crate) unsafe extern "C" fn pci_enable_device(dev: *mut LinuxCompatPciDev) -> i32 {
    let status = crate::driver::pci::enable_device(dev);
    crate::driver::symbol_events::record_pci_resource_symbol(
        "pci_enable_device",
        dev,
        0,
        status as u32 as u64,
    );
    status
}

pub(crate) unsafe extern "C" fn pcim_enable_device(dev: *mut LinuxCompatPciDev) -> i32 {
    let status = crate::driver::pci::enable_device(dev);
    crate::driver::symbol_events::record_pci_resource_symbol(
        "pcim_enable_device",
        dev,
        0,
        status as u32 as u64,
    );
    if status == 0 && !dev.is_null() {
        let dev_ptr = unsafe { &mut (*dev).dev as *mut _ as *mut c_void };
        crate::driver::devres::register_pci_disable(dev_ptr, dev);
    }
    status
}

pub(crate) unsafe extern "C" fn pci_disable_device(dev: *mut LinuxCompatPciDev) {
    if !dev.is_null() {
        let dev_ptr = unsafe { &mut (*dev).dev as *mut _ as *mut c_void };
        crate::driver::devres::forget_pci_disable(dev_ptr, dev);
    }
    crate::driver::pci::disable_device(dev);
    crate::driver::symbol_events::record_pci_resource_symbol("pci_disable_device", dev, 0, 0);
}

pub(crate) unsafe extern "C" fn pci_set_master(dev: *mut LinuxCompatPciDev) {
    crate::driver::pci::set_master(dev);
    crate::driver::symbol_events::record_pci_resource_symbol("pci_set_master", dev, 0, 0);
}

pub(crate) unsafe extern "C" fn pci_clear_master(dev: *mut LinuxCompatPciDev) {
    crate::driver::pci::clear_master(dev);
    crate::driver::symbol_events::record_pci_resource_symbol("pci_clear_master", dev, 0, 0);
}

pub(crate) unsafe extern "C" fn pci_resource_start(dev: *mut LinuxCompatPciDev, bar: u32) -> u64 {
    crate::driver::pci::resource_start(dev, bar)
}

pub(crate) unsafe extern "C" fn pci_resource_end(dev: *mut LinuxCompatPciDev, bar: u32) -> u64 {
    crate::driver::pci::resource_end(dev, bar)
}

pub(crate) unsafe extern "C" fn pci_resource_len(dev: *mut LinuxCompatPciDev, bar: u32) -> u64 {
    crate::driver::pci::resource_len(dev, bar)
}

pub(crate) unsafe extern "C" fn pci_resource_flags(dev: *mut LinuxCompatPciDev, bar: u32) -> usize {
    crate::driver::pci::resource_flags(dev, bar)
}

pub(crate) unsafe extern "C" fn pci_read_config_byte(
    dev: *mut LinuxCompatPciDev,
    offset: i32,
    value: *mut u8,
) -> i32 {
    crate::driver::pci::read_config_byte(dev, offset, value)
}

pub(crate) unsafe extern "C" fn pci_read_config_word(
    dev: *mut LinuxCompatPciDev,
    offset: i32,
    value: *mut u16,
) -> i32 {
    crate::driver::pci::read_config_word(dev, offset, value)
}

pub(crate) unsafe extern "C" fn pci_read_config_dword(
    dev: *mut LinuxCompatPciDev,
    offset: i32,
    value: *mut u32,
) -> i32 {
    crate::driver::pci::read_config_dword(dev, offset, value)
}

pub(crate) unsafe extern "C" fn pci_write_config_byte(
    dev: *mut LinuxCompatPciDev,
    offset: i32,
    value: u8,
) -> i32 {
    let status = crate::driver::pci::write_config_byte(dev, offset, value);
    if status == 0 {
        crate::driver::symbol_events::record_pci_config_symbol(
            "pci_write_config_byte",
            dev,
            offset,
            encode_config_value(value.into(), 1),
        );
    }
    status
}

pub(crate) unsafe extern "C" fn pci_write_config_word(
    dev: *mut LinuxCompatPciDev,
    offset: i32,
    value: u16,
) -> i32 {
    let status = crate::driver::pci::write_config_word(dev, offset, value);
    if status == 0 {
        crate::driver::symbol_events::record_pci_config_symbol(
            "pci_write_config_word",
            dev,
            offset,
            encode_config_value(value.into(), 2),
        );
    }
    status
}

pub(crate) unsafe extern "C" fn pci_write_config_dword(
    dev: *mut LinuxCompatPciDev,
    offset: i32,
    value: u32,
) -> i32 {
    let status = crate::driver::pci::write_config_dword(dev, offset, value);
    if status == 0 {
        crate::driver::symbol_events::record_pci_config_symbol(
            "pci_write_config_dword",
            dev,
            offset,
            encode_config_value(value.into(), 4),
        );
    }
    status
}

pub(crate) unsafe extern "C" fn pci_set_drvdata(dev: *mut LinuxCompatPciDev, drvdata: usize) {
    crate::driver::pci::set_drvdata(dev, drvdata);
}

pub(crate) unsafe extern "C" fn pci_get_drvdata(dev: *mut LinuxCompatPciDev) -> usize {
    crate::driver::pci::get_drvdata(dev)
}

pub(crate) unsafe extern "C" fn pci_iomap(
    dev: *mut LinuxCompatPciDev,
    bar: i32,
    max_len: u64,
) -> *mut c_void {
    map_bar(dev, bar, max_len, false)
}

pub(crate) unsafe extern "C" fn pci_iomap_wc(
    dev: *mut LinuxCompatPciDev,
    bar: i32,
    max_len: u64,
) -> *mut c_void {
    map_bar(dev, bar, max_len, true)
}

pub(crate) unsafe extern "C" fn pci_iounmap(_dev: *mut LinuxCompatPciDev, addr: *mut c_void) {
    unsafe { crate::driver::linux::mmio::iounmap(addr) };
    crate::driver::symbol_events::record_pci_resource_symbol(
        "pci_iounmap",
        _dev,
        addr as usize as u64,
        0,
    );
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "__pci_register_driver" => Some(__pci_register_driver as *const () as usize),
        "pci_unregister_driver" => Some(pci_unregister_driver as *const () as usize),
        "pci_enable_device" => Some(pci_enable_device as *const () as usize),
        "pcim_enable_device" => Some(pcim_enable_device as *const () as usize),
        "pci_disable_device" => Some(pci_disable_device as *const () as usize),
        "pci_set_master" => Some(pci_set_master as *const () as usize),
        "pci_clear_master" => Some(pci_clear_master as *const () as usize),
        "pci_resource_start" => Some(pci_resource_start as *const () as usize),
        "pci_resource_end" => Some(pci_resource_end as *const () as usize),
        "pci_resource_len" => Some(pci_resource_len as *const () as usize),
        "pci_resource_flags" => Some(pci_resource_flags as *const () as usize),
        "pci_read_config_byte" => Some(pci_read_config_byte as *const () as usize),
        "pci_read_config_word" => Some(pci_read_config_word as *const () as usize),
        "pci_read_config_dword" => Some(pci_read_config_dword as *const () as usize),
        "pci_write_config_byte" => Some(pci_write_config_byte as *const () as usize),
        "pci_write_config_word" => Some(pci_write_config_word as *const () as usize),
        "pci_write_config_dword" => Some(pci_write_config_dword as *const () as usize),
        "pci_set_drvdata" => Some(pci_set_drvdata as *const () as usize),
        "pci_get_drvdata" => Some(pci_get_drvdata as *const () as usize),
        "pci_iomap" => Some(pci_iomap as *const () as usize),
        "pci_iomap_wc" => Some(pci_iomap_wc as *const () as usize),
        "pci_iounmap" => Some(pci_iounmap as *const () as usize),
        "pci_bus_type" => Some(crate::driver::pci::bus_type_ptr() as usize),
        _ => None,
    }
}
// RING3-MIGRATION-REFERENCE END: Linux .ko PCI shim compatibility substrate exception.

fn map_bar(
    dev: *mut LinuxCompatPciDev,
    bar: i32,
    max_len: u64,
    write_combine: bool,
) -> *mut c_void {
    if dev.is_null() || bar < 0 {
        return core::ptr::null_mut();
    }

    let start = crate::driver::pci::resource_start(dev, bar as u32);
    let len = crate::driver::pci::resource_len(dev, bar as u32);
    if start == 0 || len == 0 {
        return core::ptr::null_mut();
    }

    let selected_len = if max_len == 0 || max_len > len {
        len
    } else {
        max_len
    };
    let Some(size) = usize::try_from(selected_len).ok() else {
        return core::ptr::null_mut();
    };
    let mapped = unsafe {
        if write_combine {
            crate::driver::linux::mmio::ioremap_wc(start, size)
        } else {
            crate::driver::linux::mmio::ioremap(start, size)
        }
    };
    if !mapped.is_null() {
        crate::driver::symbol_events::record_pci_resource_symbol(
            if write_combine {
                "pci_iomap_wc"
            } else {
                "pci_iomap"
            },
            dev,
            start,
            encode_iomap_value(bar as u32, write_combine, selected_len),
        );
    }
    mapped
}

fn encode_config_value(value: u64, width: u64) -> u64 {
    (width << 56) | (value & 0x00ff_ffff_ffff_ffff)
}

fn encode_iomap_value(bar: u32, write_combine: bool, selected_len: u64) -> u64 {
    (u64::from(bar) << 56) | ((write_combine as u64) << 55) | (selected_len & 0x007f_ffff_ffff_ffff)
}
