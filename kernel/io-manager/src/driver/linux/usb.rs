// RING3-MIGRATION-REFERENCE START: Linux .ko USB shims are explicit ring0
// compatibility substrate. USB device/provider policy belongs in driverd,
// devmgrd, and inputd.
use super::compat::{
    LinuxCompatUrb, LinuxCompatUsbClassDriver, LinuxCompatUsbDevice, LinuxCompatUsbDriver,
    LinuxCompatUsbInterface,
};
use alloc::boxed::Box;
use core::arch::asm;
use core::ffi::{c_char, c_void};
use core::ptr;

static USB_BUS_TYPE: [u8; 128] = [0; 128];
const MAX_USB_STRING_BYTES: usize = 256;
const MAX_USB_EXTRA_DESCRIPTOR_BYTES: usize = 4096;

#[inline(always)]
fn current_rsp() -> usize {
    let rsp: usize;
    unsafe {
        asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    rsp
}

macro_rules! usb_compat_diag {
    (debug, $($arg:tt)+) => {
        crate::debug::debug!(compat, $($arg)+)
    };
    (info, $($arg:tt)+) => {
        crate::debug::info!(compat, $($arg)+)
    };
    (warn, $($arg:tt)+) => {
        crate::debug::warn!(compat, $($arg)+)
    };
    (error, $($arg:tt)+) => {
        crate::debug::error!(compat, $($arg)+)
    };
}

pub(crate) fn bus_type_ptr() -> *const c_void {
    &USB_BUS_TYPE as *const [u8; 128] as *const c_void
}

pub(crate) unsafe extern "C" fn usb_register_driver(
    driver: *mut LinuxCompatUsbDriver,
    _owner: *mut c_void,
    _mod_name: *const c_char,
) -> i32 {
    crate::debug::record_milestone(
        crate::debug::LogCategory::Compat,
        "usb-register-entry",
        driver as usize as u64,
        0,
    );
    usb_compat_diag!(debug, "linux compat: usb_register_driver entry");
    if driver.is_null() {
        return -22;
    }
    crate::driver::symbol_events::record_usb_probe_init_symbol(
        "usb_register_driver",
        driver as usize,
        0,
    );
    usb_compat_diag!(debug, "linux compat: usb_register_driver nonnull");

    unsafe {
        usb_compat_diag!(
            debug,
            "linux compat: usb_register_driver fields ptr={:#x} name={:#x} drv_name={:#x} drv_bus={:#x} probe={:#x} disconnect={:#x} id_table={:#x}",
            driver as usize,
            (*driver).name as usize,
            (*driver).driver.name as usize,
            (*driver).driver.bus as usize,
            (*driver).probe.map(|f| f as usize).unwrap_or(0),
            (*driver).disconnect.map(|f| f as usize).unwrap_or(0),
            (*driver).id_table as usize,
        );
        if (*driver).driver.name.is_null() {
            (*driver).driver.name = (*driver).name;
        }
        (*driver).driver.bus = bus_type_ptr();
        usb_compat_diag!(debug, "linux compat: usb_register_driver fields updated");
        if crate::debug::enabled!(compat, debug) {
            usb_compat_diag!(debug, "linux compat: usb_register_driver compat_cstr ok");
            usb_compat_diag!(
                debug,
                "linux compat: usb_register_driver snapshot ptr={:#x} name={} probe={:#x} disconnect={:#x} id_table={:#x}",
                driver as usize,
                crate::driver::linux::compat::compat_cstr((*driver).name).unwrap_or("?"),
                (*driver).probe.map(|f| f as usize).unwrap_or(0),
                (*driver).disconnect.map(|f| f as usize).unwrap_or(0),
                (*driver).id_table as usize,
            );
        }
    }

    crate::debug::record_milestone(
        crate::debug::LogCategory::Compat,
        "usb-register-bus-begin",
        driver as usize as u64,
        0,
    );
    let _ = unsafe { crate::driver::linux::device::bus_register(bus_type_ptr() as *mut c_void) };
    crate::debug::record_milestone(
        crate::debug::LogCategory::Compat,
        "usb-register-driver-begin",
        driver as usize as u64,
        0,
    );
    let status =
        unsafe { crate::driver::linux::device::driver_register(&mut (*driver).driver as *mut _) };
    if status != 0 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Compat,
            "usb-register-driver-failed",
            driver as usize as u64,
            status as u64,
        );
        usb_compat_diag!(
            error,
            "linux compat: usb_register_driver driver_register failed driver={:#x} status={}",
            driver as usize,
            status
        );
        return status;
    }

    crate::debug::record_milestone(
        crate::debug::LogCategory::Compat,
        "usb-register-linux-begin",
        driver as usize as u64,
        0,
    );
    crate::multitask::cond_resched();
    let status = crate::usb::register_linux_driver(driver);
    crate::multitask::cond_resched();
    unsafe {
        asm!("cld", options(nomem, nostack));
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Compat,
        "usb-register-done",
        driver as usize as u64,
        status as u64,
    );
    usb_compat_diag!(
        info,
        "linux compat: usb_register_driver end driver={:#x} status={} rsp={:#x}",
        driver as usize,
        status,
        current_rsp()
    );
    status
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
    crate::multitask::cond_resched();
    let status = crate::usb::submit_urb(urb);
    crate::multitask::cond_resched();
    status
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
    request: u32,
    request_type: u32,
    value: u32,
    index: u32,
    data: *mut c_void,
    size: usize,
    _timeout: i32,
) -> i32 {
    let request = request as u8;
    let request_type = request_type as u8;
    let value = value as u16;
    let index = index as u16;
    let size = size.min(u16::MAX as usize) as u16;
    if dev.is_null() || (size != 0 && data.is_null()) {
        return -22;
    }
    super::compat_log::debugcon_line(
        alloc::format!(
            "usb_control_msg: begin dev={:#x} req={:#x} type={:#x} value={:#x} index={:#x} data={:#x} size={}",
            dev as usize,
            request,
            request_type,
            value,
            index,
            data as usize,
            size
        )
        .as_bytes(),
    );
    crate::multitask::cond_resched();
    let status = crate::usb::control_msg(dev, request, request_type, value, index, data, size);
    crate::multitask::cond_resched();
    super::compat_log::debugcon_line(
        alloc::format!(
            "usb_control_msg: end dev={:#x} status={}",
            dev as usize,
            status
        )
        .as_bytes(),
    );
    status
}

pub(crate) unsafe extern "C" fn usb_interrupt_msg(
    dev: *mut LinuxCompatUsbDevice,
    _pipe: u32,
    data: *mut c_void,
    len: i32,
    actual_length: *mut i32,
    _timeout: i32,
) -> i32 {
    if dev.is_null() || data.is_null() || len < 0 {
        return -22;
    }
    crate::multitask::cond_resched();
    let status = crate::usb::interrupt_msg(dev, data, len, actual_length);
    crate::multitask::cond_resched();
    status
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
    let ptr = unsafe { super::base::__kmalloc_noprof(size.max(1), _mem_flags) };
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
    let _ = size;
    unsafe { super::base::kfree(addr) };
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
    let limit = size.min(MAX_USB_STRING_BYTES);
    let mut len = 0usize;
    while len + 1 < limit && unsafe { *source.add(len) } != 0 {
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
    let preview = if buffer.is_null() || size == 0 {
        alloc::vec::Vec::new()
    } else {
        let preview_len = core::cmp::min(size as usize, 9);
        unsafe { core::slice::from_raw_parts(buffer.cast::<u8>(), preview_len) }.to_vec()
    };
    super::compat_log::debugcon_line(
        alloc::format!(
            "__usb_get_extra_descriptor: begin buffer={:#x} size={} type={:#x} bytes={:02x?}",
            buffer as usize,
            size,
            descriptor_type,
            preview
        )
        .as_bytes(),
    );
    if !out.is_null() {
        unsafe {
            *out = ptr::null_mut();
        }
    }
    if buffer.is_null() || size < 2 || size as usize > MAX_USB_EXTRA_DESCRIPTOR_BYTES {
        super::compat_log::debugcon_line(
            alloc::format!(
                "__usb_get_extra_descriptor: invalid buffer={:#x} size={}",
                buffer as usize,
                size
            )
            .as_bytes(),
        );
        return -61;
    }

    let bytes = unsafe { core::slice::from_raw_parts(buffer.cast::<u8>(), size as usize) };
    let mut offset = 0usize;
    while offset + 2 <= bytes.len() {
        if (offset & 0x3f) == 0 {
            crate::multitask::cond_resched();
        }
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
            super::compat_log::debugcon_line(
                alloc::format!(
                    "__usb_get_extra_descriptor: found offset={} ptr={:#x}",
                    offset,
                    unsafe {
                        if out.is_null() {
                            0
                        } else {
                            *out as usize
                        }
                    }
                )
                .as_bytes(),
            );
            return 0;
        }
        offset += descriptor_len;
    }

    super::compat_log::debugcon_line(
        alloc::format!(
            "__usb_get_extra_descriptor: missing type={:#x} size={}",
            descriptor_type,
            size
        )
        .as_bytes(),
    );
    -61
}

pub(crate) unsafe extern "C" fn usb_autopm_get_interface(
    _intf: *mut LinuxCompatUsbInterface,
) -> i32 {
    usb_compat_diag!(
        debug,
        "linux compat usb_autopm_get_interface: intf={:#x}",
        _intf as usize
    );
    0
}

pub(crate) unsafe extern "C" fn usb_autopm_get_interface_async(
    _intf: *mut LinuxCompatUsbInterface,
) -> i32 {
    usb_compat_diag!(
        debug,
        "linux compat usb_autopm_get_interface_async: intf={:#x}",
        _intf as usize
    );
    0
}

pub(crate) unsafe extern "C" fn usb_autopm_get_interface_no_resume(
    _intf: *mut LinuxCompatUsbInterface,
) {
    usb_compat_diag!(
        debug,
        "linux compat usb_autopm_get_interface_no_resume: intf={:#x}",
        _intf as usize
    );
}

pub(crate) unsafe extern "C" fn usb_autopm_put_interface(_intf: *mut LinuxCompatUsbInterface) {
    usb_compat_diag!(
        debug,
        "linux compat usb_autopm_put_interface: intf={:#x}",
        _intf as usize
    );
}

pub(crate) unsafe extern "C" fn usb_autopm_put_interface_async(
    _intf: *mut LinuxCompatUsbInterface,
) {
    usb_compat_diag!(
        debug,
        "linux compat usb_autopm_put_interface_async: intf={:#x}",
        _intf as usize
    );
}

pub(crate) unsafe extern "C" fn usb_autopm_put_interface_no_suspend(
    _intf: *mut LinuxCompatUsbInterface,
) {
    usb_compat_diag!(
        debug,
        "linux compat usb_autopm_put_interface_no_suspend: intf={:#x}",
        _intf as usize
    );
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
// RING3-MIGRATION-REFERENCE END: Linux .ko USB compatibility substrate exception.
