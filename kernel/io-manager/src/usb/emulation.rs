// RING3-MIGRATION-REFERENCE START: usbdrv/inputd should own USB runtime
// emulation policy. Ring0 keeps Linux .ko URB/HID callback forwarding into the
// current native USB substrate.
use core::ffi::c_void;

use crate::driver::linux::compat::{LinuxCompatHidDevice, LinuxCompatUrb, LinuxCompatUsbDevice};

use super::runtime;

pub(crate) fn service_pending() -> usize {
    runtime::service_pending()
}

pub(crate) fn has_runtime_pointer_device() -> bool {
    runtime::has_pointer_device()
}

pub(crate) fn control_msg(
    dev: *mut LinuxCompatUsbDevice,
    request: u8,
    request_type: u8,
    value: u16,
    index: u16,
    data: *mut c_void,
    size: u16,
) -> i32 {
    runtime::control_msg(dev, request, request_type, value, index, data, size)
}

pub(crate) fn interrupt_msg(
    dev: *mut LinuxCompatUsbDevice,
    data: *mut c_void,
    len: i32,
    actual_length: *mut i32,
) -> i32 {
    runtime::interrupt_msg(dev, data, len, actual_length)
}

pub(crate) fn submit_urb(urb: *mut LinuxCompatUrb) -> i32 {
    runtime::submit_urb(urb)
}

pub(crate) fn cancel_urb(urb: *mut LinuxCompatUrb) -> bool {
    runtime::cancel_urb(urb)
}

pub(crate) fn hid_input_report(dev: *mut LinuxCompatHidDevice, data: *mut u8, size: u32) -> i32 {
    runtime::hid_input_report(dev, data, size)
}

pub(crate) fn hid_remove_device(dev: *mut LinuxCompatHidDevice) {
    runtime::hid_remove_device(dev);
}
// RING3-MIGRATION-REFERENCE END: usbdrv/inputd-owned USB runtime emulation policy.
