use core::ffi::c_void;

use driver_abi::PointerPacket;

use crate::driver::linux::compat::{LinuxCompatHidDevice, LinuxCompatUrb, LinuxCompatUsbDevice};
use crate::input::keyboard::KeyboardEvent;

use super::runtime;
use super::synthetic;

pub(crate) fn service_pending() -> usize {
    runtime::service_pending() + synthetic::service_pending()
}

pub(crate) fn capture_keyboard_event(event: KeyboardEvent) -> bool {
    synthetic::capture_keyboard_event(event)
}

pub(crate) fn capture_pointer_packet(packet: PointerPacket) -> bool {
    synthetic::capture_pointer_packet(packet)
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
    if runtime::handles_device(dev) {
        runtime::control_msg(dev, request, request_type, value, index, data, size)
    } else {
        synthetic::control_msg(dev, request, request_type, value, index, data, size)
    }
}

pub(crate) fn interrupt_msg(
    dev: *mut LinuxCompatUsbDevice,
    data: *mut c_void,
    len: i32,
    actual_length: *mut i32,
) -> i32 {
    if runtime::handles_device(dev) {
        runtime::interrupt_msg(dev, data, len, actual_length)
    } else {
        synthetic::interrupt_msg(dev, data, len, actual_length)
    }
}

pub(crate) fn submit_urb(urb: *mut LinuxCompatUrb) -> i32 {
    let dev = unsafe { (*urb).dev };
    if runtime::handles_device(dev) {
        runtime::submit_urb(urb)
    } else {
        synthetic::submit_urb(urb)
    }
}

pub(crate) fn cancel_urb(urb: *mut LinuxCompatUrb) -> bool {
    let dev = unsafe { (*urb).dev };
    if runtime::handles_device(dev) {
        runtime::cancel_urb(urb)
    } else {
        synthetic::cancel_urb(urb)
    }
}

pub(crate) fn hid_input_report(dev: *mut LinuxCompatHidDevice, data: *mut u8, size: u32) -> i32 {
    if runtime::owns_hid_device(dev) {
        return runtime::hid_input_report(dev, data, size).unwrap_or(0);
    }
    if let Some(status) = runtime::hid_input_report(dev, data, size) {
        status
    } else {
        synthetic::hid_input_report(dev, data, size)
    }
}

pub(crate) fn hid_remove_device(dev: *mut LinuxCompatHidDevice) {
    runtime::hid_remove_device(dev);
    synthetic::hid_remove_device(dev);
}
