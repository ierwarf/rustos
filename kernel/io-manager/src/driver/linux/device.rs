use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_char, c_void};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::Mutex;

use super::compat::{LinuxCompatDevice, LinuxCompatDeviceDriver};

#[repr(C)]
struct OpaqueHandle(usize);

#[derive(Clone, Copy)]
struct RegisteredBus {
    ptr: usize,
}

#[derive(Clone, Copy)]
struct RegisteredClass {
    ptr: usize,
}

#[derive(Clone, Copy)]
struct RegisteredDriver {
    ptr: usize,
    bus: usize,
}

#[derive(Clone, Copy)]
struct RegisteredDevice {
    ptr: usize,
    bus: usize,
    class: usize,
    devt: u32,
    owned: bool,
}

struct DeviceNameRecord {
    owner: usize,
    bytes: Box<[u8]>,
}

static I2C_BUS_TYPE: [u8; 64] = [0; 64];
static I2C_ADAPTER_TYPE: [u8; 64] = [0; 64];
static I2C_CLIENT_TYPE: [u8; 64] = [0; 64];
static NEXT_HANDLE_ID: AtomicUsize = AtomicUsize::new(1);
static REGISTERED_BUSES: Mutex<Vec<RegisteredBus>> = Mutex::new(Vec::new());
static REGISTERED_CLASSES: Mutex<Vec<RegisteredClass>> = Mutex::new(Vec::new());
static REGISTERED_DRIVERS: Mutex<Vec<RegisteredDriver>> = Mutex::new(Vec::new());
static REGISTERED_DEVICES: Mutex<Vec<RegisteredDevice>> = Mutex::new(Vec::new());
static DEVICE_NAMES: Mutex<Vec<DeviceNameRecord>> = Mutex::new(Vec::new());

type DeviceProbeFn = unsafe extern "C" fn(dev: *mut LinuxCompatDevice) -> i32;
type DeviceIterFn = unsafe extern "C" fn(dev: *mut LinuxCompatDevice, data: *mut c_void) -> i32;
type DriverIterFn =
    unsafe extern "C" fn(drv: *mut LinuxCompatDeviceDriver, data: *mut c_void) -> i32;

pub(crate) unsafe extern "C" fn bus_register_notifier(
    bus: *mut c_void,
    notifier: *mut c_void,
) -> i32 {
    crate::debug::write_debugcon_only_line(
        alloc::format!(
            "linux compat: bus_register_notifier bus={:#x} notifier={:#x}",
            bus as usize,
            notifier as usize
        )
        .as_bytes(),
    );
    0
}

pub(crate) unsafe extern "C" fn bus_unregister_notifier(
    bus: *mut c_void,
    notifier: *mut c_void,
) -> i32 {
    crate::debug::write_debugcon_only_line(
        alloc::format!(
            "linux compat: bus_unregister_notifier bus={:#x} notifier={:#x}",
            bus as usize,
            notifier as usize
        )
        .as_bytes(),
    );
    0
}

pub(crate) unsafe extern "C" fn bus_register(bus: *mut c_void) -> i32 {
    crate::debug::write_debugcon_only_line(
        alloc::format!("linux compat: bus_register begin ptr={:#x}", bus as usize).as_bytes(),
    );
    let Some(ptr) = ptr_key(bus) else {
        crate::debug::write_debugcon_only_line(b"linux compat: bus_register invalid");
        return -22;
    };
    let mut buses = REGISTERED_BUSES.lock();
    if !buses.iter().any(|entry| entry.ptr == ptr) {
        buses.push(RegisteredBus { ptr });
    }
    crate::debug::write_debugcon_only_line(
        alloc::format!("linux compat: bus_register end ptr={:#x}", ptr).as_bytes(),
    );
    0
}

pub(crate) unsafe extern "C" fn bus_unregister(bus: *mut c_void) {
    let Some(ptr) = ptr_key(bus) else {
        return;
    };
    let mut buses = REGISTERED_BUSES.lock();
    if let Some(index) = buses.iter().position(|entry| entry.ptr == ptr) {
        buses.remove(index);
    }
}

pub(crate) unsafe extern "C" fn bus_for_each_dev(
    bus: *mut c_void,
    _start: *mut c_void,
    data: *mut c_void,
    callback: *mut c_void,
) -> i32 {
    let Some(bus_ptr) = ptr_key(bus) else {
        return -22;
    };
    let Some(callback) = (unsafe { callback_fn::<DeviceIterFn>(callback) }) else {
        return -22;
    };
    let devices = REGISTERED_DEVICES.lock();
    for device in devices.iter().copied().filter(|entry| entry.bus == bus_ptr) {
        let status = unsafe { callback(device.ptr as *mut LinuxCompatDevice, data) };
        if status != 0 {
            return status;
        }
    }
    0
}

pub(crate) unsafe extern "C" fn bus_for_each_drv(
    bus: *mut c_void,
    _start: *mut c_void,
    data: *mut c_void,
    callback: *mut c_void,
) -> i32 {
    let Some(bus_ptr) = ptr_key(bus) else {
        return -22;
    };
    let Some(callback) = (unsafe { callback_fn::<DriverIterFn>(callback) }) else {
        return -22;
    };
    let drivers = REGISTERED_DRIVERS.lock();
    for driver in drivers.iter().copied().filter(|entry| entry.bus == bus_ptr) {
        let status = unsafe { callback(driver.ptr as *mut LinuxCompatDeviceDriver, data) };
        if status != 0 {
            return status;
        }
    }
    0
}

pub(crate) unsafe extern "C" fn bus_rescan_devices(bus: *mut c_void) -> i32 {
    let Some(bus_ptr) = ptr_key(bus) else {
        return -22;
    };
    let drivers = REGISTERED_DRIVERS.lock().clone();
    for driver in drivers.into_iter().filter(|driver| driver.bus == bus_ptr) {
        let status = unsafe { driver_attach(driver.ptr as *mut LinuxCompatDeviceDriver) };
        if status != 0 {
            return status;
        }
    }
    0
}

pub(crate) unsafe extern "C" fn class_register(class: *mut c_void) -> i32 {
    crate::debug::write_debugcon_only_line(
        alloc::format!(
            "linux compat: class_register begin ptr={:#x}",
            class as usize
        )
        .as_bytes(),
    );
    let Some(ptr) = ptr_key(class) else {
        crate::debug::write_debugcon_only_line(b"linux compat: class_register invalid");
        return -22;
    };
    let mut classes = REGISTERED_CLASSES.lock();
    if !classes.iter().any(|entry| entry.ptr == ptr) {
        classes.push(RegisteredClass { ptr });
    }
    crate::debug::write_debugcon_only_line(
        alloc::format!("linux compat: class_register end ptr={:#x}", ptr).as_bytes(),
    );
    0
}

pub(crate) unsafe extern "C" fn class_unregister(class: *mut c_void) {
    let Some(ptr) = ptr_key(class) else {
        return;
    };
    let mut classes = REGISTERED_CLASSES.lock();
    if let Some(index) = classes.iter().position(|entry| entry.ptr == ptr) {
        classes.remove(index);
    }
}

pub(crate) unsafe extern "C" fn driver_register(driver: *mut LinuxCompatDeviceDriver) -> i32 {
    if driver.is_null() {
        return -22;
    }
    let bus = unsafe { (*driver).bus as usize };
    if bus != 0 {
        let _ = unsafe { bus_register(bus as *mut c_void) };
    }

    let mut drivers = REGISTERED_DRIVERS.lock();
    if !drivers
        .iter()
        .any(|entry| entry.ptr == driver as usize && entry.bus == bus)
    {
        drivers.push(RegisteredDriver {
            ptr: driver as usize,
            bus,
        });
    }
    0
}

pub(crate) unsafe extern "C" fn driver_unregister(driver: *mut LinuxCompatDeviceDriver) {
    let Some(driver_ptr) = ptr_key(driver.cast()) else {
        return;
    };
    let mut drivers = REGISTERED_DRIVERS.lock();
    if let Some(index) = drivers.iter().position(|entry| entry.ptr == driver_ptr) {
        drivers.remove(index);
    }
}

pub(crate) unsafe extern "C" fn driver_attach(driver: *mut LinuxCompatDeviceDriver) -> i32 {
    if driver.is_null() {
        return -22;
    }

    let bus = unsafe { (*driver).bus as usize };
    if bus == 0 {
        return 0;
    }

    let probe = unsafe { callback_fn::<DeviceProbeFn>((*driver).probe.cast_mut()) };
    let Some(probe) = probe else {
        return 0;
    };

    let devices = REGISTERED_DEVICES.lock().clone();
    for device in devices.into_iter().filter(|device| device.bus == bus) {
        let dev_ptr = device.ptr as *mut LinuxCompatDevice;
        if unsafe { !(*dev_ptr).driver.is_null() } {
            continue;
        }
        unsafe {
            (*dev_ptr).driver = driver;
        }
        let status = unsafe { probe(dev_ptr) };
        if status != 0 {
            unsafe {
                (*dev_ptr).driver = ptr::null_mut();
            }
            return status;
        }
    }
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

pub(crate) unsafe extern "C" fn device_link_remove(link: *mut c_void, _supplier: *mut c_void) {
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

pub(crate) unsafe extern "C" fn devres_open_group(
    _dev: *mut c_void,
    _id: *mut c_void,
    _gfp: u32,
    _name: *const c_char,
) -> *mut c_void {
    Box::into_raw(Box::new(OpaqueHandle(
        NEXT_HANDLE_ID.fetch_add(1, Ordering::Relaxed),
    ))) as *mut c_void
}

pub(crate) unsafe extern "C" fn devres_release_group(_dev: *mut c_void, id: *mut c_void) {
    if id.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(id as *mut OpaqueHandle));
    }
}

pub(crate) unsafe extern "C" fn device_initialize(dev: *mut LinuxCompatDevice) {
    if dev.is_null() {
        return;
    }
    unsafe {
        (*dev).driver = ptr::null_mut();
        (*dev).driver_data = ptr::null_mut();
    }
}

pub(crate) unsafe extern "C" fn device_add(dev: *mut LinuxCompatDevice) -> i32 {
    let Some(ptr) = ptr_key(dev.cast()) else {
        return -22;
    };
    let mut devices = REGISTERED_DEVICES.lock();
    if !devices.iter().any(|entry| entry.ptr == ptr) {
        devices.push(RegisteredDevice {
            ptr,
            bus: unsafe { (*dev).bus as usize },
            class: 0,
            devt: 0,
            owned: false,
        });
    }
    0
}

pub(crate) unsafe extern "C" fn device_del(dev: *mut LinuxCompatDevice) {
    let Some(ptr) = ptr_key(dev.cast()) else {
        return;
    };
    let mut devices = REGISTERED_DEVICES.lock();
    if let Some(index) = devices.iter().position(|entry| entry.ptr == ptr) {
        devices.remove(index);
    }
}

pub(crate) unsafe extern "C" fn device_create(
    class: *mut c_void,
    parent: *mut LinuxCompatDevice,
    devt: u32,
    drvdata: *mut c_void,
    fmt: *const c_char,
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
) -> *mut LinuxCompatDevice {
    let mut device = Box::<LinuxCompatDevice>::default();
    device.parent = parent;
    device.driver_data = drvdata;
    let device_ptr = device.as_mut() as *mut LinuxCompatDevice;
    let _ = unsafe { dev_set_name(device_ptr, fmt, arg0, arg1, arg2, arg3) };

    let ptr = Box::into_raw(device);
    REGISTERED_DEVICES.lock().push(RegisteredDevice {
        ptr: ptr as usize,
        bus: unsafe { (*ptr).bus as usize },
        class: class as usize,
        devt,
        owned: true,
    });
    ptr
}

pub(crate) unsafe extern "C" fn device_destroy(class: *mut c_void, devt: u32) {
    let class = class as usize;
    let mut devices = REGISTERED_DEVICES.lock();
    let Some(index) = devices
        .iter()
        .position(|entry| entry.owned && entry.class == class && entry.devt == devt)
    else {
        return;
    };
    let ptr = devices.remove(index).ptr as *mut LinuxCompatDevice;
    unsafe {
        drop(Box::from_raw(ptr));
    }
}

pub(crate) unsafe extern "C" fn device_reprobe(dev: *mut LinuxCompatDevice) -> i32 {
    let Some(dev_ptr) = ptr_key(dev.cast()) else {
        return -22;
    };
    let devices = REGISTERED_DEVICES.lock();
    let Some(record) = devices.iter().copied().find(|entry| entry.ptr == dev_ptr) else {
        return -19;
    };
    drop(devices);

    if unsafe { !(*dev).driver.is_null() } {
        let Some(probe) =
            (unsafe { callback_fn::<DeviceProbeFn>((*(*dev).driver).probe.cast_mut()) })
        else {
            return 0;
        };
        return unsafe { probe(dev) };
    }

    let drivers = REGISTERED_DRIVERS.lock().clone();
    for driver in drivers
        .into_iter()
        .filter(|driver| driver.bus == record.bus)
    {
        let Some(probe) = (unsafe {
            callback_fn::<DeviceProbeFn>(
                (*(driver.ptr as *mut LinuxCompatDeviceDriver))
                    .probe
                    .cast_mut(),
            )
        }) else {
            continue;
        };
        unsafe {
            (*dev).driver = driver.ptr as *mut LinuxCompatDeviceDriver;
        }
        let status = unsafe { probe(dev) };
        if status == 0 {
            return 0;
        }
        unsafe {
            (*dev).driver = ptr::null_mut();
        }
    }

    -19
}

pub(crate) unsafe extern "C" fn dev_set_name(
    dev: *mut LinuxCompatDevice,
    fmt: *const c_char,
    _arg0: usize,
    _arg1: usize,
    _arg2: usize,
    _arg3: usize,
) -> i32 {
    if dev.is_null() {
        return -22;
    }
    let bytes = format_name_bytes(fmt);
    let name_ptr = store_device_name(dev as usize, &bytes);
    unsafe {
        (*dev).init_name = name_ptr.cast();
    }
    0
}

pub(crate) unsafe extern "C" fn put_device(_dev: *mut LinuxCompatDevice) {}

pub(crate) unsafe extern "C" fn device_set_node(_dev: *mut LinuxCompatDevice, _node: *mut c_void) {}

pub(crate) unsafe extern "C" fn device_set_wakeup_enable(
    _dev: *mut LinuxCompatDevice,
    _enabled: bool,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn __dev_fwnode(_dev: *const LinuxCompatDevice) -> *mut c_void {
    ptr::null_mut()
}

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

pub(crate) unsafe extern "C" fn devm_kmalloc(
    _dev: *mut c_void,
    size: usize,
    gfp: u32,
) -> *mut c_void {
    unsafe { super::base::__kmalloc_noprof(size, gfp) }
}

pub(crate) unsafe extern "C" fn devm_kfree(_dev: *mut c_void, ptr: *const c_void) {
    unsafe { super::base::kfree(ptr) };
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "bus_register_notifier" => Some(bus_register_notifier as *const () as usize),
        "bus_unregister_notifier" => Some(bus_unregister_notifier as *const () as usize),
        "bus_register" => Some(bus_register as *const () as usize),
        "bus_unregister" => Some(bus_unregister as *const () as usize),
        "bus_for_each_dev" => Some(bus_for_each_dev as *const () as usize),
        "bus_for_each_drv" => Some(bus_for_each_drv as *const () as usize),
        "bus_rescan_devices" => Some(bus_rescan_devices as *const () as usize),
        "class_register" => Some(class_register as *const () as usize),
        "class_unregister" => Some(class_unregister as *const () as usize),
        "driver_register" => Some(driver_register as *const () as usize),
        "driver_unregister" => Some(driver_unregister as *const () as usize),
        "driver_attach" => Some(driver_attach as *const () as usize),
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
        "devres_open_group" => Some(devres_open_group as *const () as usize),
        "devres_release_group" => Some(devres_release_group as *const () as usize),
        "device_initialize" => Some(device_initialize as *const () as usize),
        "device_add" => Some(device_add as *const () as usize),
        "device_del" => Some(device_del as *const () as usize),
        "device_create" => Some(device_create as *const () as usize),
        "device_destroy" => Some(device_destroy as *const () as usize),
        "device_reprobe" => Some(device_reprobe as *const () as usize),
        "dev_set_name" => Some(dev_set_name as *const () as usize),
        "put_device" => Some(put_device as *const () as usize),
        "device_set_node" => Some(device_set_node as *const () as usize),
        "device_set_wakeup_enable" => Some(device_set_wakeup_enable as *const () as usize),
        "__dev_fwnode" => Some(__dev_fwnode as *const () as usize),
        "dev_set_drvdata" => Some(dev_set_drvdata as *const () as usize),
        "dev_get_drvdata" => Some(dev_get_drvdata as *const () as usize),
        "devm_kmalloc" => Some(devm_kmalloc as *const () as usize),
        "devm_kfree" => Some(devm_kfree as *const () as usize),
        _ => None,
    }
}

fn ptr_key(ptr: *mut c_void) -> Option<usize> {
    (!ptr.is_null()).then_some(ptr as usize)
}

unsafe fn callback_fn<T>(ptr: *mut c_void) -> Option<T> {
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { core::mem::transmute_copy::<*mut c_void, T>(&ptr) })
    }
}

fn format_name_bytes(fmt: *const c_char) -> Vec<u8> {
    if fmt.is_null() {
        return b"device".to_vec();
    }
    let mut bytes = Vec::new();
    let mut cursor = fmt.cast::<u8>();
    loop {
        let byte = unsafe { *cursor };
        if byte == 0 {
            break;
        }
        bytes.push(byte);
        cursor = unsafe { cursor.add(1) };
    }
    if bytes.is_empty() {
        bytes.extend_from_slice(b"device");
    }
    bytes.push(0);
    bytes
}

fn store_device_name(owner: usize, bytes: &[u8]) -> *const u8 {
    let mut names = DEVICE_NAMES.lock();
    if let Some(existing) = names.iter_mut().find(|existing| existing.owner == owner) {
        existing.bytes = bytes.to_vec().into_boxed_slice();
        return existing.bytes.as_ptr();
    }
    names.push(DeviceNameRecord {
        owner,
        bytes: bytes.to_vec().into_boxed_slice(),
    });
    names.last().expect("device name storage").bytes.as_ptr()
}
