// RING3-MIGRATION-REFERENCE START: usb-runtime-substrate exception:
// usbdrv/driverd/inputd own USB runtime facade policy. Ring0 keeps this narrow
// facade as the current privileged native-host and Linux-.ko callback substrate.
mod core;
mod emulation;
mod host;
mod manager;
mod runtime;
mod xhci;

pub use xhci::XhciInputDebugSnapshot;

pub fn init() {
    manager::init();
}

pub(crate) fn service_pending() -> usize {
    manager::service_pending()
}

pub(crate) fn uses_polled_input_completion() -> bool {
    manager::uses_polled_input_completion()
}

pub(crate) fn host_controllers_available() -> bool {
    manager::host_controllers_available()
}

pub(crate) fn hid_interfaces_available() -> bool {
    core::hid_interfaces_available()
}

pub fn debug_transfer_event_count() -> u64 {
    xhci::debug_transfer_event_count()
}

pub fn debug_input_snapshot() -> XhciInputDebugSnapshot {
    xhci::debug_input_snapshot()
}

pub fn debug_pointer_report_count() -> u64 {
    runtime::debug_pointer_report_count()
}

pub(crate) fn register_linux_driver(
    driver: *mut crate::driver::linux::compat::LinuxCompatUsbDriver,
) -> i32 {
    core::register_linux_driver(driver)
}

pub(crate) fn unregister_linux_driver(
    driver: *mut crate::driver::linux::compat::LinuxCompatUsbDriver,
) {
    core::unregister_linux_driver(driver);
}

pub(crate) fn register_owned_interface(
    registration: core::UsbInterfaceRegistration<'_>,
) -> *mut crate::driver::linux::compat::LinuxCompatUsbInterface {
    core::register_owned_interface(registration)
}

pub(crate) fn set_interface_minor(
    interface: *mut crate::driver::linux::compat::LinuxCompatUsbInterface,
    minor: i32,
) -> Result<(), i32> {
    core::set_interface_minor(interface, minor)
}

pub(crate) fn find_interface(
    driver: *mut crate::driver::linux::compat::LinuxCompatUsbDriver,
    minor: i32,
) -> *mut crate::driver::linux::compat::LinuxCompatUsbInterface {
    core::find_interface(driver, minor)
}

pub(crate) fn has_runtime_pointer_device() -> bool {
    emulation::has_runtime_pointer_device()
}

pub(crate) fn control_msg(
    dev: *mut crate::driver::linux::compat::LinuxCompatUsbDevice,
    request: u8,
    request_type: u8,
    value: u16,
    index: u16,
    data: *mut ::core::ffi::c_void,
    size: u16,
) -> i32 {
    emulation::control_msg(dev, request, request_type, value, index, data, size)
}

pub(crate) fn interrupt_msg(
    dev: *mut crate::driver::linux::compat::LinuxCompatUsbDevice,
    data: *mut ::core::ffi::c_void,
    len: i32,
    actual_length: *mut i32,
) -> i32 {
    emulation::interrupt_msg(dev, data, len, actual_length)
}

pub(crate) fn submit_urb(urb: *mut crate::driver::linux::compat::LinuxCompatUrb) -> i32 {
    emulation::submit_urb(urb)
}

pub(crate) fn cancel_urb(urb: *mut crate::driver::linux::compat::LinuxCompatUrb) -> bool {
    emulation::cancel_urb(urb)
}

pub(crate) fn hid_input_report(
    dev: *mut crate::driver::linux::compat::LinuxCompatHidDevice,
    data: *mut u8,
    size: u32,
) -> i32 {
    emulation::hid_input_report(dev, data, size)
}

pub(crate) fn hid_remove_device(dev: *mut crate::driver::linux::compat::LinuxCompatHidDevice) {
    emulation::hid_remove_device(dev);
}
// RING3-MIGRATION-REFERENCE END: USB runtime facade substrate exception.
