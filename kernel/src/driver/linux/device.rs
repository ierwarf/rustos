use alloc::boxed::Box;
use core::ffi::{c_char, c_void};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::compat::LinuxCompatDevice;

#[repr(C)]
struct OpaqueHandle(usize);

static I2C_BUS_TYPE: [u8; 64] = [0; 64];
static I2C_ADAPTER_TYPE: [u8; 64] = [0; 64];
static I2C_CLIENT_TYPE: [u8; 64] = [0; 64];
static NEXT_HANDLE_ID: AtomicUsize = AtomicUsize::new(1);

pub(crate) unsafe extern "C" fn bus_register_notifier(
    _bus: *mut c_void,
    _notifier: *mut c_void,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn bus_unregister_notifier(
    _bus: *mut c_void,
    _notifier: *mut c_void,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn device_add_groups(
    _dev: *mut c_void,
    _groups: *const *const c_void,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn device_remove_groups(
    _dev: *mut c_void,
    _groups: *const *const c_void,
) {
}

pub(crate) unsafe extern "C" fn device_create_file(_dev: *mut c_void, _attr: *const c_void) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn device_remove_file(_dev: *mut c_void, _attr: *const c_void) {}

pub(crate) unsafe extern "C" fn sysfs_create_group(
    _kobj: *mut c_void,
    _group: *const c_void,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn sysfs_remove_group(_kobj: *mut c_void, _group: *const c_void) {}

pub(crate) unsafe extern "C" fn device_link_add(
    _consumer: *mut c_void,
    _supplier: *mut c_void,
    _flags: u32,
) -> *mut c_void {
    Box::into_raw(Box::new(OpaqueHandle(
        NEXT_HANDLE_ID.fetch_add(1, Ordering::Relaxed),
    ))) as *mut c_void
}

pub(crate) unsafe extern "C" fn device_link_remove(link: *mut c_void) {
    if link.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(link as *mut OpaqueHandle));
    }
}

pub(crate) unsafe extern "C" fn fwnode_create_software_node(
    _node: *const c_void,
    _parent: *mut c_void,
) -> *mut c_void {
    Box::into_raw(Box::new(OpaqueHandle(
        NEXT_HANDLE_ID.fetch_add(1, Ordering::Relaxed),
    ))) as *mut c_void
}

pub(crate) unsafe extern "C" fn dmi_check_system(_table: *const c_void) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn dmi_get_system_info(_field: i32) -> *const c_char {
    ptr::null()
}

pub(crate) unsafe extern "C" fn i2c_verify_adapter(dev: *mut c_void) -> *mut c_void {
    dev
}

pub(crate) unsafe extern "C" fn i2c_for_each_dev(
    _data: *mut c_void,
    _callback: *mut c_void,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn i2c_new_scanned_device(
    _adapter: *mut c_void,
    _info: *const c_void,
    _addr_list: *const u16,
    _probe: *mut c_void,
) -> *mut c_void {
    ptr::null_mut()
}

pub(crate) unsafe extern "C" fn i2c_unregister_device(_client: *mut c_void) {}

pub(crate) unsafe extern "C" fn pm_wakeup_dev_event(_dev: *mut c_void, _msec: u32, _hard: bool) {}

pub(crate) unsafe extern "C" fn dev_set_drvdata(dev: *mut c_void, data: *mut c_void) {
    if dev.is_null() {
        return;
    }
    unsafe {
        (*(dev as *mut LinuxCompatDevice)).driver_data = data;
    }
}

pub(crate) unsafe extern "C" fn dev_get_drvdata(dev: *const c_void) -> *mut c_void {
    if dev.is_null() {
        return ptr::null_mut();
    }
    unsafe { (*(dev as *const LinuxCompatDevice)).driver_data }
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "bus_register_notifier" => Some(bus_register_notifier as *const () as usize),
        "bus_unregister_notifier" => Some(bus_unregister_notifier as *const () as usize),
        "device_add_groups" => Some(device_add_groups as *const () as usize),
        "device_remove_groups" => Some(device_remove_groups as *const () as usize),
        "device_create_file" => Some(device_create_file as *const () as usize),
        "device_remove_file" => Some(device_remove_file as *const () as usize),
        "sysfs_create_group" => Some(sysfs_create_group as *const () as usize),
        "sysfs_remove_group" => Some(sysfs_remove_group as *const () as usize),
        "device_link_add" => Some(device_link_add as *const () as usize),
        "device_link_remove" => Some(device_link_remove as *const () as usize),
        "fwnode_create_software_node" => Some(fwnode_create_software_node as *const () as usize),
        "dmi_check_system" => Some(dmi_check_system as *const () as usize),
        "dmi_get_system_info" => Some(dmi_get_system_info as *const () as usize),
        "i2c_bus_type" => Some(&I2C_BUS_TYPE as *const [u8; 64] as usize),
        "i2c_adapter_type" => Some(&I2C_ADAPTER_TYPE as *const [u8; 64] as usize),
        "i2c_client_type" => Some(&I2C_CLIENT_TYPE as *const [u8; 64] as usize),
        "i2c_verify_adapter" => Some(i2c_verify_adapter as *const () as usize),
        "i2c_for_each_dev" => Some(i2c_for_each_dev as *const () as usize),
        "i2c_new_scanned_device" => Some(i2c_new_scanned_device as *const () as usize),
        "i2c_unregister_device" => Some(i2c_unregister_device as *const () as usize),
        "pm_wakeup_dev_event" => Some(pm_wakeup_dev_event as *const () as usize),
        "dev_set_drvdata" => Some(dev_set_drvdata as *const () as usize),
        "dev_get_drvdata" => Some(dev_get_drvdata as *const () as usize),
        _ => None,
    }
}
