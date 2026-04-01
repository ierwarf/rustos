use super::compat::{
    LinuxCompatUrb, LinuxCompatUsbClassDriver, LinuxCompatUsbDevice, LinuxCompatUsbDriver,
    LinuxCompatUsbInterface,
};
use alloc::alloc::{Layout, alloc, dealloc};
use alloc::boxed::Box;
use core::ffi::{c_char, c_void};
use core::ptr;

static USB_BUS_TYPE: [u8; 128] = [0; 128];

pub(crate) fn bus_type_ptr() -> *const c_void {
    &USB_BUS_TYPE as *const [u8; 128] as *const c_void
}

pub(crate) unsafe extern "C" fn usb_register_driver(
    driver: *mut LinuxCompatUsbDriver,
    _owner: *mut c_void,
    _mod_name: *const c_char,
) -> i32 {
    if driver.is_null() {
        return -22;
    }

    unsafe {
        if (*driver).driver.name.is_null() {
            (*driver).driver.name = (*driver).name;
        }
        (*driver).driver.bus = bus_type_ptr();
        let _driver_name = crate::driver::linux::compat::compat_cstr((*driver).name).unwrap_or("?");
        crate::debug::println!(
            "usb_register_driver: driver={:#x} name={} probe={:#x} disconnect={:#x} id_table={:#x}",
            driver as usize,
            _driver_name,
            (*driver).probe.map(|f| f as usize).unwrap_or(0),
            (*driver).disconnect.map(|f| f as usize).unwrap_or(0),
            (*driver).id_table as usize,
        );
    }

    let _ = unsafe { crate::driver::linux::device::bus_register(bus_type_ptr() as *mut c_void) };
    let status =
        unsafe { crate::driver::linux::device::driver_register(&mut (*driver).driver as *mut _) };
    if status != 0 {
        return status;
    }

    crate::usb::register_linux_driver(driver)
}

pub(crate) unsafe extern "C" fn usb_deregister(driver: *mut LinuxCompatUsbDriver) {
    if driver.is_null() {
        return;
    }
    crate::usb::unregister_linux_driver(driver);
    unsafe {
        crate::driver::linux::device::driver_unregister(&mut (*driver).driver as *mut _);
    }
}

pub(crate) unsafe extern "C" fn usb_alloc_urb(
    _iso_packets: i32,
    _mem_flags: u32,
) -> *mut LinuxCompatUrb {
    Box::into_raw(Box::<LinuxCompatUrb>::default())
}

pub(crate) unsafe extern "C" fn usb_free_urb(urb: *mut LinuxCompatUrb) {
    if urb.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(urb));
    }
}

pub(crate) unsafe extern "C" fn usb_submit_urb(urb: *mut LinuxCompatUrb, _mem_flags: u32) -> i32 {
    if urb.is_null() {
        return -22;
    }
    if unsafe { (*urb).dev.is_null() } {
        return -19;
    }
    crate::usb::submit_urb(urb)
}

pub(crate) unsafe extern "C" fn usb_unlink_urb(urb: *mut LinuxCompatUrb) -> i32 {
    if urb.is_null() {
        return -22;
    }
    let _ = crate::usb::cancel_urb(urb);
    0
}

pub(crate) unsafe extern "C" fn usb_kill_urb(urb: *mut LinuxCompatUrb) {
    if urb.is_null() {
        return;
    }
    let _ = crate::usb::cancel_urb(urb);
}

pub(crate) unsafe extern "C" fn usb_unpoison_urb(_urb: *mut LinuxCompatUrb) {}

pub(crate) unsafe extern "C" fn usb_block_urb(_urb: *mut LinuxCompatUrb) {}

pub(crate) unsafe extern "C" fn usb_control_msg(
    dev: *mut LinuxCompatUsbDevice,
    _pipe: u32,
    request: u8,
    request_type: u8,
    value: u16,
    index: u16,
    data: *mut c_void,
    size: u16,
    _timeout: i32,
) -> i32 {
    crate::usb::control_msg(dev, request, request_type, value, index, data, size)
}

pub(crate) unsafe extern "C" fn usb_interrupt_msg(
    dev: *mut LinuxCompatUsbDevice,
    _pipe: u32,
    data: *mut c_void,
    len: i32,
    actual_length: *mut i32,
    _timeout: i32,
) -> i32 {
    crate::usb::interrupt_msg(dev, data, len, actual_length)
}

pub(crate) unsafe extern "C" fn usb_clear_halt(_dev: *mut LinuxCompatUsbDevice, _pipe: i32) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn usb_alloc_coherent(
    _dev: *mut LinuxCompatUsbDevice,
    size: usize,
    _mem_flags: u32,
    dma: *mut u64,
) -> *mut c_void {
    let Ok(layout) = Layout::array::<u8>(size.max(1)) else {
        return ptr::null_mut();
    };
    let ptr = unsafe { alloc(layout) };
    if !dma.is_null() {
        unsafe {
            *dma = ptr as u64;
        }
    }
    ptr.cast()
}

pub(crate) unsafe extern "C" fn usb_free_coherent(
    _dev: *mut LinuxCompatUsbDevice,
    size: usize,
    addr: *mut c_void,
    _dma: u64,
) {
    if addr.is_null() {
        return;
    }
    let Ok(layout) = Layout::array::<u8>(size.max(1)) else {
        return;
    };
    unsafe {
        dealloc(addr.cast::<u8>(), layout);
    }
}

pub(crate) unsafe extern "C" fn usb_register_dev(
    intf: *mut LinuxCompatUsbInterface,
    class_driver: *const LinuxCompatUsbClassDriver,
) -> i32 {
    if intf.is_null() {
        return -22;
    }
    let minor = if class_driver.is_null() {
        0
    } else {
        unsafe { (*class_driver).minor_base }
    };
    crate::usb::set_interface_minor(intf, minor).map_or_else(|err| err, |_| 0)
}

pub(crate) unsafe extern "C" fn usb_deregister_dev(
    intf: *mut LinuxCompatUsbInterface,
    _class_driver: *const LinuxCompatUsbClassDriver,
) {
    if intf.is_null() {
        return;
    }
    let _ = crate::usb::set_interface_minor(intf, -1);
}

pub(crate) unsafe extern "C" fn usb_find_interface(
    driver: *mut LinuxCompatUsbDriver,
    minor: i32,
) -> *mut LinuxCompatUsbInterface {
    crate::usb::find_interface(driver, minor)
}

pub(crate) unsafe extern "C" fn usb_string(
    dev: *mut LinuxCompatUsbDevice,
    index: i32,
    buf: *mut c_char,
    size: usize,
) -> i32 {
    if dev.is_null() || buf.is_null() || size == 0 {
        return -22;
    }
    let source = match index {
        1 => unsafe { (*dev).manufacturer },
        2 => unsafe { (*dev).product },
        3 => unsafe { (*dev).serial },
        _ => ptr::null(),
    };
    if source.is_null() {
        unsafe {
            *buf = 0;
        }
        return -61;
    }
    let mut len = 0usize;
    while len + 1 < size && unsafe { *source.add(len) } != 0 {
        unsafe {
            *buf.add(len) = *source.add(len);
        }
        len += 1;
    }
    unsafe {
        *buf.add(len) = 0;
    }
    len as i32
}

pub(crate) unsafe extern "C" fn usb_queue_reset_device(_intf: *mut LinuxCompatUsbInterface) {}

pub(crate) unsafe extern "C" fn __usb_get_extra_descriptor(
    buffer: *mut c_void,
    size: u32,
    descriptor_type: u8,
    out: *mut *mut c_void,
) -> i32 {
    if !out.is_null() {
        unsafe {
            *out = ptr::null_mut();
        }
    }
    if buffer.is_null() || size < 2 {
        return -61;
    }

    let bytes = unsafe { core::slice::from_raw_parts(buffer.cast::<u8>(), size as usize) };
    let mut offset = 0usize;
    while offset + 2 <= bytes.len() {
        let descriptor_len = bytes[offset] as usize;
        if descriptor_len < 2 || offset + descriptor_len > bytes.len() {
            break;
        }
        if bytes[offset + 1] == descriptor_type {
            if !out.is_null() {
                unsafe {
                    *out = buffer.cast::<u8>().add(offset).cast();
                }
            }
            return 0;
        }
        offset += descriptor_len;
    }

    -61
}

pub(crate) unsafe extern "C" fn usb_autopm_get_interface(
    _intf: *mut LinuxCompatUsbInterface,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn usb_autopm_get_interface_async(
    _intf: *mut LinuxCompatUsbInterface,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn usb_autopm_get_interface_no_resume(
    _intf: *mut LinuxCompatUsbInterface,
) {
}

pub(crate) unsafe extern "C" fn usb_autopm_put_interface(_intf: *mut LinuxCompatUsbInterface) {}

pub(crate) unsafe extern "C" fn usb_autopm_put_interface_async(
    _intf: *mut LinuxCompatUsbInterface,
) {
}

pub(crate) unsafe extern "C" fn usb_autopm_put_interface_no_suspend(
    _intf: *mut LinuxCompatUsbInterface,
) {
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "usb_register_driver" => Some(usb_register_driver as *const () as usize),
        "usb_deregister" => Some(usb_deregister as *const () as usize),
        "usb_alloc_urb" => Some(usb_alloc_urb as *const () as usize),
        "usb_free_urb" => Some(usb_free_urb as *const () as usize),
        "usb_submit_urb" => Some(usb_submit_urb as *const () as usize),
        "usb_unlink_urb" => Some(usb_unlink_urb as *const () as usize),
        "usb_kill_urb" => Some(usb_kill_urb as *const () as usize),
        "usb_unpoison_urb" => Some(usb_unpoison_urb as *const () as usize),
        "usb_block_urb" => Some(usb_block_urb as *const () as usize),
        "usb_control_msg" => Some(usb_control_msg as *const () as usize),
        "usb_interrupt_msg" => Some(usb_interrupt_msg as *const () as usize),
        "usb_clear_halt" => Some(usb_clear_halt as *const () as usize),
        "usb_alloc_coherent" => Some(usb_alloc_coherent as *const () as usize),
        "usb_free_coherent" => Some(usb_free_coherent as *const () as usize),
        "usb_register_dev" => Some(usb_register_dev as *const () as usize),
        "usb_deregister_dev" => Some(usb_deregister_dev as *const () as usize),
        "usb_find_interface" => Some(usb_find_interface as *const () as usize),
        "usb_string" => Some(usb_string as *const () as usize),
        "usb_queue_reset_device" => Some(usb_queue_reset_device as *const () as usize),
        "__usb_get_extra_descriptor" => Some(__usb_get_extra_descriptor as *const () as usize),
        "usb_autopm_get_interface" => Some(usb_autopm_get_interface as *const () as usize),
        "usb_autopm_get_interface_async" => {
            Some(usb_autopm_get_interface_async as *const () as usize)
        }
        "usb_autopm_get_interface_no_resume" => {
            Some(usb_autopm_get_interface_no_resume as *const () as usize)
        }
        "usb_autopm_put_interface" => Some(usb_autopm_put_interface as *const () as usize),
        "usb_autopm_put_interface_async" => {
            Some(usb_autopm_put_interface_async as *const () as usize)
        }
        "usb_autopm_put_interface_no_suspend" => {
            Some(usb_autopm_put_interface_no_suspend as *const () as usize)
        }
        _ => None,
    }
}
