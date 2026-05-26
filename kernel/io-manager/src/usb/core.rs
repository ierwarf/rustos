use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use crate::sync::KernelSpinLock as Mutex;

use crate::driver::linux::compat::{
    LinuxCompatUsbBus, LinuxCompatUsbDevice, LinuxCompatUsbDriver,
    LinuxCompatUsbHostEndpoint, LinuxCompatUsbHostInterface, LinuxCompatUsbInterface,
};

const USB_DT_DEVICE: u8 = 0x01;
const USB_DT_ENDPOINT: u8 = 0x05;
const USB_DT_INTERFACE: u8 = 0x04;
const USB_DIR_IN: u8 = 0x80;
const USB_DEVICE_STATE_CONFIGURED: u32 = 7;
const USB_DEVICE_FLAG_CAN_SUBMIT: u32 = 1 << 0;
const USB_DEVICE_FLAG_AUTHORIZED: u32 = 1 << 4;
const USB_INTERFACE_FLAG_NEEDS_BINDING: u32 = 1 << 5;
const USB_INTERFACE_FLAG_AUTHORIZED: u32 = 1 << 7;
const USB_DEVICE_ID_MATCH_VENDOR: u16 = 0x0001;
const USB_DEVICE_ID_MATCH_PRODUCT: u16 = 0x0002;
const USB_DEVICE_ID_MATCH_DEV_LO: u16 = 0x0004;
const USB_DEVICE_ID_MATCH_DEV_HI: u16 = 0x0008;
const USB_DEVICE_ID_MATCH_DEV_CLASS: u16 = 0x0010;
const USB_DEVICE_ID_MATCH_DEV_SUBCLASS: u16 = 0x0020;
const USB_DEVICE_ID_MATCH_DEV_PROTOCOL: u16 = 0x0040;
const USB_DEVICE_ID_MATCH_INT_CLASS: u16 = 0x0080;
const USB_DEVICE_ID_MATCH_INT_SUBCLASS: u16 = 0x0100;
const USB_DEVICE_ID_MATCH_INT_PROTOCOL: u16 = 0x0200;
const USB_DEVICE_ID_MATCH_INT_NUMBER: u16 = 0x0400;

static USB_SYNTHETIC_BUS_NAME: [u8; 4] = *b"usb\0";
static USB_SYNTHETIC_BUS: LinuxCompatUsbBus = LinuxCompatUsbBus {
    _pad0: [0; 24],
    bus_name: USB_SYNTHETIC_BUS_NAME.as_ptr().cast(),
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct UsbInterfaceRegistration<'a> {
    pub(crate) devnum: u32,
    pub(crate) speed: u32,
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    pub(crate) device_bcd: u16,
    pub(crate) device_class: u8,
    pub(crate) device_sub_class: u8,
    pub(crate) device_protocol: u8,
    pub(crate) interface_class: u8,
    pub(crate) interface_sub_class: u8,
    pub(crate) interface_protocol: u8,
    pub(crate) interface_number: u8,
    pub(crate) endpoint_address: u8,
    pub(crate) endpoint_attributes: u8,
    pub(crate) endpoint_max_packet_size: u16,
    pub(crate) endpoint_interval: u8,
    pub(crate) interface_extra: Option<&'a [u8]>,
    pub(crate) manufacturer: Option<&'a str>,
    pub(crate) product: Option<&'a str>,
    pub(crate) serial: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct RegisteredUsbDriver {
    ptr: usize,
}

struct OwnedUsbDeviceRecord {
    compat: Box<LinuxCompatUsbDevice>,
    _manufacturer: Option<Box<[u8]>>,
    _product: Option<Box<[u8]>>,
    _serial: Option<Box<[u8]>>,
}

struct RegisteredUsbInterface {
    ptr: usize,
    device_ptr: usize,
    _altsetting_ptr: usize,
    vendor_id: u16,
    product_id: u16,
    device_bcd: u16,
    device_class: u8,
    device_sub_class: u8,
    device_protocol: u8,
    interface_class: u8,
    interface_sub_class: u8,
    interface_protocol: u8,
    interface_number: u8,
    bound_driver: Option<usize>,
    owned: bool,
}

struct OwnedUsbInterfaceRecord {
    compat: Box<LinuxCompatUsbInterface>,
    _altsetting: Box<LinuxCompatUsbHostInterface>,
    _endpoint: Box<LinuxCompatUsbHostEndpoint>,
    _extra: Option<Box<[u8]>>,
}

unsafe impl Send for RegisteredUsbDriver {}
unsafe impl Send for RegisteredUsbInterface {}
unsafe impl Send for OwnedUsbDeviceRecord {}
unsafe impl Send for OwnedUsbInterfaceRecord {}

static USB_DRIVERS: Mutex<Vec<RegisteredUsbDriver>> = Mutex::new(Vec::new());
static USB_DEVICES: Mutex<Vec<OwnedUsbDeviceRecord>> = Mutex::new(Vec::new());
static USB_INTERFACES: Mutex<Vec<RegisteredUsbInterface>> = Mutex::new(Vec::new());
static USB_OWNED_INTERFACES: Mutex<Vec<OwnedUsbInterfaceRecord>> = Mutex::new(Vec::new());

pub(crate) fn hid_interfaces_available() -> bool {
    USB_INTERFACES
        .lock()
        .iter()
        .any(|interface| interface.interface_class == 0x03)
}

pub(crate) fn register_linux_driver(driver: *mut LinuxCompatUsbDriver) -> i32 {
    if driver.is_null() {
        return -22;
    }

    let driver_index = {
        let mut drivers = USB_DRIVERS.lock();
        if let Some(index) = drivers
            .iter()
            .position(|entry| entry.ptr == driver as usize)
        {
            index
        } else {
            drivers.push(RegisteredUsbDriver {
                ptr: driver as usize,
            });
            drivers.len() - 1
        }
    };

    crate::debug::debug!(
        usb,
        "usb core register driver: driver={:#x} index={} interfaces={}",
        driver as usize,
        driver_index,
        USB_INTERFACES.lock().len(),
    );
    0
}

pub(crate) fn unregister_linux_driver(driver: *mut LinuxCompatUsbDriver) {
    if driver.is_null() {
        return;
    }

    let removed_index = {
        let mut drivers = USB_DRIVERS.lock();
        let Some(index) = drivers
            .iter()
            .position(|entry| entry.ptr == driver as usize)
        else {
            return;
        };
        drivers.remove(index);
        index
    };

    let disconnects = {
        let mut interfaces = USB_INTERFACES.lock();
        let mut pending = Vec::new();
        for interface in interfaces.iter_mut() {
            match interface.bound_driver {
                Some(index) if index == removed_index => {
                    interface.bound_driver = None;
                    pending.push(interface.ptr);
                }
                Some(index) if index > removed_index => {
                    interface.bound_driver = Some(index - 1);
                }
                _ => {}
            }
        }
        pending
    };

    let disconnect = unsafe { (*driver).disconnect };
    if let Some(disconnect) = disconnect {
        for interface_ptr in disconnects {
            let interface = interface_ptr as *mut LinuxCompatUsbInterface;
            unsafe {
                disconnect(interface);
                (*interface).dev.driver = ptr::null_mut();
            }
        }
    }
}

pub(crate) fn register_owned_interface(
    registration: UsbInterfaceRegistration<'_>,
) -> *mut LinuxCompatUsbInterface {
    let mut device = Box::<LinuxCompatUsbDevice>::default();
    let manufacturer = registration.manufacturer.map(c_string_bytes);
    let product = registration.product.map(c_string_bytes);
    let serial = registration.serial.map(c_string_bytes);
    device.speed = registration.speed;
    device.state = USB_DEVICE_STATE_CONFIGURED;
    device.devnum = registration.devnum as i32;
    device.devaddr = registration.devnum as u8;
    device.flags_bits = USB_DEVICE_FLAG_CAN_SUBMIT | USB_DEVICE_FLAG_AUTHORIZED;
    device.bus = (&raw const USB_SYNTHETIC_BUS) as *const LinuxCompatUsbBus as *mut c_void;
    device.dev.bus = crate::driver::linux::usb::bus_type_ptr();
    device.descriptor[0] = 18;
    device.descriptor[1] = USB_DT_DEVICE;
    device.descriptor[2..4].copy_from_slice(&0x0200u16.to_le_bytes());
    device.descriptor[4] = registration.device_class;
    device.descriptor[5] = registration.device_sub_class;
    device.descriptor[6] = registration.device_protocol;
    device.descriptor[7] = 64;
    device.descriptor[8..10].copy_from_slice(&registration.vendor_id.to_le_bytes());
    device.descriptor[10..12].copy_from_slice(&registration.product_id.to_le_bytes());
    device.descriptor[12..14].copy_from_slice(&registration.device_bcd.to_le_bytes());
    device.descriptor[14] = 1;
    device.descriptor[15] = 2;
    device.descriptor[16] = 3;
    device.descriptor[17] = 1;
    device.product = product
        .as_ref()
        .map_or(ptr::null(), |bytes| bytes.as_ptr().cast());
    device.manufacturer = manufacturer
        .as_ref()
        .map_or(ptr::null(), |bytes| bytes.as_ptr().cast());
    device.serial = serial
        .as_ref()
        .map_or(ptr::null(), |bytes| bytes.as_ptr().cast());

    let mut altsetting = Box::<LinuxCompatUsbHostInterface>::default();
    altsetting.desc[0] = 9;
    altsetting.desc[1] = USB_DT_INTERFACE;
    altsetting.desc[2] = registration.interface_number;
    altsetting.desc[3] = 0;
    altsetting.desc[4] = 1;
    altsetting.desc[5] = registration.interface_class;
    altsetting.desc[6] = registration.interface_sub_class;
    altsetting.desc[7] = registration.interface_protocol;
    altsetting.desc[8] = 0;

    let mut endpoint = Box::<LinuxCompatUsbHostEndpoint>::default();
    let extra = registration.interface_extra.map(|descriptor| {
        let mut bytes = descriptor.to_vec();
        bytes.shrink_to_fit();
        bytes.into_boxed_slice()
    });

    endpoint.desc[0] = 7;
    endpoint.desc[1] = USB_DT_ENDPOINT;
    endpoint.desc[2] = registration.endpoint_address;
    endpoint.desc[3] = registration.endpoint_attributes;
    endpoint.desc[4..6].copy_from_slice(&registration.endpoint_max_packet_size.to_le_bytes());
    endpoint.desc[6] = registration.endpoint_interval;
    endpoint.enabled = 1;
    let endpoint_ptr = endpoint.as_mut() as *mut LinuxCompatUsbHostEndpoint;
    altsetting.endpoint = endpoint_ptr;
    if let Some(extra) = extra.as_ref() {
        altsetting.extra = extra.as_ptr() as *mut u8;
        altsetting.extralen = extra.len() as i32;
        crate::debug::write_debugcon_only_line(
            alloc::format!(
                "usb core interface extra: intf={} ptr={:#x} len={} bytes={:02x?}",
                registration.interface_number,
                altsetting.extra as usize,
                altsetting.extralen,
                extra
            )
            .as_bytes(),
        );
    }

    let device_ptr = device.as_mut() as *mut LinuxCompatUsbDevice;
    let device_dev_ptr = &mut device.dev as *mut _;
    let altsetting_ptr = altsetting.as_mut() as *mut LinuxCompatUsbHostInterface;
    let mut interface = Box::<LinuxCompatUsbInterface>::default();
    interface.altsetting = altsetting_ptr;
    interface.cur_altsetting = altsetting_ptr;
    interface.num_altsetting = 1;
    interface.flags_bits = USB_INTERFACE_FLAG_AUTHORIZED;
    interface.dev.bus = crate::driver::linux::usb::bus_type_ptr();
    interface.dev.parent = device_dev_ptr;
    interface.usb_dev = device_ptr;

    if !endpoint_ptr.is_null() {
        let endpoint_index = (endpoint.desc[2] & 0x0f) as usize;
        if endpoint_index < device.ep_in.len() && (endpoint.desc[2] & USB_DIR_IN) != 0 {
            device.ep_in[endpoint_index] = endpoint_ptr;
        }
        if endpoint_index < device.ep_out.len() && (endpoint.desc[2] & USB_DIR_IN) == 0 {
            device.ep_out[endpoint_index] = endpoint_ptr;
        }
    }
    device.ep0.desc[0] = 7;
    device.ep0.desc[1] = USB_DT_ENDPOINT;
    device.ep0.desc[2] = 0;

    let interface_ptr = interface.as_mut() as *mut LinuxCompatUsbInterface;

    unsafe {
        crate::driver::linux::device::device_initialize(device_dev_ptr);
        crate::driver::linux::device::device_add(device_dev_ptr);
        crate::driver::linux::device::device_initialize(&mut interface.dev as *mut _);
        crate::driver::linux::device::device_add(&mut interface.dev as *mut _);
    }

    USB_DEVICES.lock().push(OwnedUsbDeviceRecord {
        compat: device,
        _manufacturer: manufacturer,
        _product: product,
        _serial: serial,
    });
    USB_OWNED_INTERFACES.lock().push(OwnedUsbInterfaceRecord {
        compat: interface,
        _altsetting: altsetting,
        _endpoint: endpoint,
        _extra: extra,
    });
    let interface_index = {
        let mut interfaces = USB_INTERFACES.lock();
        interfaces.push(RegisteredUsbInterface {
            ptr: interface_ptr as usize,
            device_ptr: device_ptr as usize,
            _altsetting_ptr: altsetting_ptr as usize,
            vendor_id: registration.vendor_id,
            product_id: registration.product_id,
            device_bcd: registration.device_bcd,
            device_class: registration.device_class,
            device_sub_class: registration.device_sub_class,
            device_protocol: registration.device_protocol,
            interface_class: registration.interface_class,
            interface_sub_class: registration.interface_sub_class,
            interface_protocol: registration.interface_protocol,
            interface_number: registration.interface_number,
            bound_driver: None,
            owned: true,
        });
        interfaces.len() - 1
    };
    crate::debug::debug!(
        usb,
        "usb core interface registered: index={} intf={:#x} vendor={:04x} product={:04x} class={:#x}/{:#x}/{:#x}",
        interface_index,
        interface_ptr as usize,
        registration.vendor_id,
        registration.product_id,
        registration.interface_class,
        registration.interface_sub_class,
        registration.interface_protocol,
    );

    interface_ptr
}

// Future hot-unplug paths will consume this directly.
#[allow(dead_code)]
pub(crate) fn unregister_owned_interface(interface: *mut LinuxCompatUsbInterface) -> bool {
    if interface.is_null() {
        return false;
    }

    let interface_ptr = interface as usize;
    let (device_ptr, owned, bound_driver) = {
        let mut interfaces = USB_INTERFACES.lock();
        let Some(index) = interfaces
            .iter()
            .position(|entry| entry.ptr == interface_ptr)
        else {
            return false;
        };
        let entry = interfaces.remove(index);
        (entry.device_ptr, entry.owned, entry.bound_driver)
    };

    if !owned {
        return false;
    }

    if let Some(driver_index) = bound_driver {
        let driver_ptr = {
            let drivers = USB_DRIVERS.lock();
            drivers.get(driver_index).map(|entry| entry.ptr)
        };
        if let Some(driver_ptr) = driver_ptr {
            let driver = driver_ptr as *mut LinuxCompatUsbDriver;
            let disconnect = unsafe { (*driver).disconnect };
            if let Some(disconnect) = disconnect {
                unsafe {
                    disconnect(interface);
                }
            }
        }
    }

    unsafe {
        (*interface).dev.driver = ptr::null_mut();
        crate::driver::linux::device::device_del(&mut (*interface).dev as *mut _);
    }

    let owned_interface = {
        let mut owned_interfaces = USB_OWNED_INTERFACES.lock();
        owned_interfaces
            .iter()
            .position(|entry| {
                entry.compat.as_ref() as *const LinuxCompatUsbInterface as usize == interface_ptr
            })
            .map(|index| owned_interfaces.remove(index))
    };

    let owned_device = {
        let mut devices = USB_DEVICES.lock();
        devices
            .iter()
            .position(|entry| {
                entry.compat.as_ref() as *const LinuxCompatUsbDevice as usize == device_ptr
            })
            .map(|index| devices.remove(index))
    };

    if let Some(record) = owned_device.as_ref() {
        let device =
            record.compat.as_ref() as *const LinuxCompatUsbDevice as *mut LinuxCompatUsbDevice;
        unsafe {
            crate::driver::linux::device::device_del(&mut (*device).dev as *mut _);
        }
    }

    drop(owned_interface);
    drop(owned_device);

    true
}

pub(crate) fn set_interface_minor(
    interface: *mut LinuxCompatUsbInterface,
    minor: i32,
) -> Result<(), i32> {
    if interface.is_null() {
        return Err(-22);
    }
    unsafe {
        (*interface).minor = minor;
    }
    Ok(())
}

pub(crate) fn find_interface(
    driver: *mut LinuxCompatUsbDriver,
    minor: i32,
) -> *mut LinuxCompatUsbInterface {
    if driver.is_null() {
        return ptr::null_mut();
    }

    let expected = unsafe { &mut (*driver).driver as *mut _ };
    let interfaces = USB_INTERFACES.lock();
    for interface in interfaces.iter() {
        let interface_ptr = interface.ptr as *mut LinuxCompatUsbInterface;
        if unsafe { (*interface_ptr).minor } != minor {
            continue;
        }
        let bound = unsafe { (*interface_ptr).dev.driver };
        if ptr::eq(bound, expected) {
            return interface_ptr;
        }
    }
    ptr::null_mut()
}

// RING3-MIGRATION-COMMENTED-OUT START: devmgrd/inputd should own USB interface
// class routing and driver-match policy. Ring0 keeps Linux .ko callback
// invocation and compat object lifetime as substrate.
/*
fn bind_all_drivers_to_interface(interface_index: usize) {
    let drivers = USB_DRIVERS.lock().clone();
    for (driver_index, driver) in drivers.iter().enumerate() {
        let driver_ptr = driver.ptr as *mut LinuxCompatUsbDriver;
        if try_bind_interface_to_driver(interface_index, driver_index, driver_ptr) {
            return;
        }
    }
}

fn bind_driver_to_interfaces(driver_index: usize, driver: *mut LinuxCompatUsbDriver) {
    let interfaces = USB_INTERFACES.lock().len();
    for interface_index in 0..interfaces {
        if try_bind_interface_to_driver(interface_index, driver_index, driver) {
            continue;
        }
    }
}

fn try_bind_interface_to_driver(
    interface_index: usize,
    driver_index: usize,
    driver: *mut LinuxCompatUsbDriver,
) -> bool {
    if driver.is_null() {
        return false;
    }

    let (interface_ptr, id_match) = {
        let mut interfaces = USB_INTERFACES.lock();
        let Some(interface) = interfaces.get_mut(interface_index) else {
            return false;
        };
        if interface.bound_driver.is_some() {
            return false;
        }
        let Some(id_match) = match_interface_id(interface, unsafe { (*driver).id_table }) else {
            return false;
        };
        let interface_ptr = interface.ptr as *mut LinuxCompatUsbInterface;
        unsafe {
            (*interface_ptr).dev.driver = &mut (*driver).driver;
            (*interface_ptr).flags_bits &= !USB_INTERFACE_FLAG_NEEDS_BINDING;
        }
        (interface_ptr, id_match)
    };

    let status = unsafe {
        if let Some(probe) = (*driver).probe {
            probe(interface_ptr, id_match)
        } else {
            0
        }
    };
    if status != 0 {
        unsafe {
            (*interface_ptr).dev.driver = ptr::null_mut();
        }
        return false;
    }

    let mut interfaces = USB_INTERFACES.lock();
    if let Some(interface) = interfaces.get_mut(interface_index) {
        interface.bound_driver = Some(driver_index);
    }
    true
}

fn match_interface_id<'a>(
    interface: &RegisteredUsbInterface,
    id_table: *const LinuxCompatUsbDeviceId,
) -> Option<*const LinuxCompatUsbDeviceId> {
    if id_table.is_null() {
        return None;
    }

    let mut index = 0usize;
    loop {
        let id = unsafe { *id_table.add(index) };
        if id.is_terminator() {
            return None;
        }
        if id_matches(interface, &id) {
            return Some(unsafe { id_table.add(index) });
        }
        index += 1;
    }
}

fn id_matches(interface: &RegisteredUsbInterface, id: &LinuxCompatUsbDeviceId) -> bool {
    if id.match_flags & USB_DEVICE_ID_MATCH_VENDOR != 0 && id.id_vendor != interface.vendor_id {
        return false;
    }
    if id.match_flags & USB_DEVICE_ID_MATCH_PRODUCT != 0 && id.id_product != interface.product_id {
        return false;
    }
    if id.match_flags & USB_DEVICE_ID_MATCH_DEV_LO != 0 && interface.device_bcd < id.bcd_device_lo {
        return false;
    }
    if id.match_flags & USB_DEVICE_ID_MATCH_DEV_HI != 0 && interface.device_bcd > id.bcd_device_hi {
        return false;
    }
    if id.match_flags & USB_DEVICE_ID_MATCH_DEV_CLASS != 0
        && id.b_device_class != interface.device_class
    {
        return false;
    }
    if id.match_flags & USB_DEVICE_ID_MATCH_DEV_SUBCLASS != 0
        && id.b_device_sub_class != interface.device_sub_class
    {
        return false;
    }
    if id.match_flags & USB_DEVICE_ID_MATCH_DEV_PROTOCOL != 0
        && id.b_device_protocol != interface.device_protocol
    {
        return false;
    }
    if id.match_flags & USB_DEVICE_ID_MATCH_INT_CLASS != 0
        && id.b_interface_class != interface.interface_class
    {
        return false;
    }
    if id.match_flags & USB_DEVICE_ID_MATCH_INT_SUBCLASS != 0
        && id.b_interface_sub_class != interface.interface_sub_class
    {
        return false;
    }
    if id.match_flags & USB_DEVICE_ID_MATCH_INT_PROTOCOL != 0
        && id.b_interface_protocol != interface.interface_protocol
    {
        return false;
    }
    if id.match_flags & USB_DEVICE_ID_MATCH_INT_NUMBER != 0
        && id.b_interface_number != interface.interface_number
    {
        return false;
    }
    true
}
*/
// RING3-MIGRATION-COMMENTED-OUT END

fn c_string_bytes(value: &str) -> Box<[u8]> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    bytes.into_boxed_slice()
}
