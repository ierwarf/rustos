// RING3-MIGRATION-REFERENCE START: Linux .ko exported shim table is an explicit
// ring0 compatibility substrate. inputd/driverd own policy; ring0 keeps symbol
// entry points that Linux modules call while executing in ring0.
use super::compat::{LinuxCompatInputDev, LinuxCompatSerio, LinuxCompatSerioDriver};

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn serio_register_driver(driver: *mut LinuxCompatSerioDriver) -> i32 {
    unsafe { crate::driver::serio::register_linux_driver(driver) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn serio_unregister_driver(driver: *mut LinuxCompatSerioDriver) {
    unsafe { crate::driver::serio::unregister_linux_driver(driver) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn serio_open(
    serio_port: *mut LinuxCompatSerio,
    _driver: *mut LinuxCompatSerioDriver,
) -> i32 {
    let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio_port) else {
        return -22;
    };
    let status = crate::driver::serio::open(port_id);
    crate::debug::println!("serio_open: port={} status={}", port_id, status);
    status
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn serio_close(serio_port: *mut LinuxCompatSerio) {
    let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio_port) else {
        return;
    };
    crate::driver::serio::close(port_id);
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn serio_write(serio_port: *mut LinuxCompatSerio, byte: u8) -> i32 {
    let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio_port) else {
        return -22;
    };
    crate::driver::serio::write(port_id, byte)
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn serio_interrupt(
    serio_port: *mut LinuxCompatSerio,
    data: u8,
    flags: u32,
) -> i32 {
    let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio_port) else {
        return -22;
    };
    crate::driver::serio::interrupt(port_id, data, flags)
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn serio_rescan(serio_port: *mut LinuxCompatSerio) {
    let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio_port) else {
        return;
    };
    let _ = crate::driver::serio::rescan(port_id);
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn serio_reconnect(serio_port: *mut LinuxCompatSerio) {
    let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio_port) else {
        return;
    };
    let _ = crate::driver::serio::reconnect(port_id);
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn serio_get_drvdata(serio_port: *mut LinuxCompatSerio) -> usize {
    let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio_port) else {
        return 0;
    };
    crate::driver::serio::driver_data(port_id)
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn serio_set_drvdata(serio_port: *mut LinuxCompatSerio, drvdata: usize) {
    let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio_port) else {
        return;
    };
    let _ = crate::driver::serio::set_driver_data(port_id, drvdata);
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_allocate_device() -> *mut LinuxCompatInputDev {
    unsafe { crate::driver::linux::input::allocate_device() }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_free_device(dev: *mut LinuxCompatInputDev) {
    unsafe { crate::driver::linux::input::free_device(dev) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_register_device(dev: *mut LinuxCompatInputDev) -> i32 {
    unsafe { crate::driver::linux::input::register_device(dev) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_unregister_device(dev: *mut LinuxCompatInputDev) {
    unsafe { crate::driver::linux::input::unregister_device(dev) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_set_capability(
    dev: *mut LinuxCompatInputDev,
    event_type: u32,
    code: u32,
) -> i32 {
    unsafe { crate::driver::linux::input::set_capability(dev, event_type, code) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_event(
    dev: *mut LinuxCompatInputDev,
    event_type: u32,
    code: u32,
    value: i32,
) -> i32 {
    unsafe { crate::driver::linux::input::event(dev, event_type, code, value) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_report_key(dev: *mut LinuxCompatInputDev, code: u32, value: i32) {
    unsafe { crate::driver::linux::input::report_key(dev, code, value) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_report_rel(dev: *mut LinuxCompatInputDev, code: u32, value: i32) {
    unsafe { crate::driver::linux::input::report_rel(dev, code, value) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_sync(dev: *mut LinuxCompatInputDev) {
    unsafe { crate::driver::linux::input::sync(dev) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_set_drvdata(dev: *mut LinuxCompatInputDev, drvdata: usize) {
    unsafe { crate::driver::linux::input::set_drvdata(dev, drvdata) }
}

#[cfg_attr(feature = "rustos_export_driver_symbols", unsafe(no_mangle))]
pub unsafe extern "C" fn input_get_drvdata(dev: *mut LinuxCompatInputDev) -> usize {
    unsafe { crate::driver::linux::input::get_drvdata(dev) }
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "serio_register_driver" => Some(serio_register_driver as *const () as usize),
        "serio_unregister_driver" => Some(serio_unregister_driver as *const () as usize),
        "serio_open" => Some(serio_open as *const () as usize),
        "serio_close" => Some(serio_close as *const () as usize),
        "serio_write" => Some(serio_write as *const () as usize),
        "serio_interrupt" => Some(serio_interrupt as *const () as usize),
        "serio_rescan" => Some(serio_rescan as *const () as usize),
        "serio_reconnect" => Some(serio_reconnect as *const () as usize),
        "serio_get_drvdata" => Some(serio_get_drvdata as *const () as usize),
        "serio_set_drvdata" => Some(serio_set_drvdata as *const () as usize),
        "input_allocate_device" => Some(input_allocate_device as *const () as usize),
        "input_free_device" => Some(input_free_device as *const () as usize),
        "input_register_device" => Some(input_register_device as *const () as usize),
        "input_unregister_device" => Some(input_unregister_device as *const () as usize),
        "input_set_capability" => Some(input_set_capability as *const () as usize),
        "input_event" => Some(input_event as *const () as usize),
        "input_report_key" => Some(input_report_key as *const () as usize),
        "input_report_rel" => Some(input_report_rel as *const () as usize),
        "input_sync" => Some(input_sync as *const () as usize),
        "input_set_drvdata" => Some(input_set_drvdata as *const () as usize),
        "input_get_drvdata" => Some(input_get_drvdata as *const () as usize),
        _ => None,
    }
}
// RING3-MIGRATION-REFERENCE END: Linux .ko exported shim compatibility substrate exception.
