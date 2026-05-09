use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicU32, Ordering};

use driver_abi::SerioPortInfo;

use super::compat::{
    compat_cstr, serio_any_matches, LinuxCompatSerio, LinuxCompatSerioDeviceId,
    LinuxCompatSerioDriver,
};

static NEXT_DYNAMIC_PORT_ID: AtomicU32 = AtomicU32::new(0x100);

pub(crate) unsafe extern "C" fn __serio_register_driver(
    driver: *mut LinuxCompatSerioDriver,
    _owner: *mut c_void,
    _mod_name: *const c_char,
) -> i32 {
    crate::debug::write_debugcon_only_line(
        alloc::format!(
            "__serio_register_driver enter: driver_ptr={:#x} owner={:#x} mod_name={:#x}",
            driver as usize,
            _owner as usize,
            _mod_name as usize,
        )
        .as_bytes(),
    );
    unsafe { crate::driver::serio::register_linux_driver(driver) }
}

pub(crate) unsafe extern "C" fn __serio_register_port(
    serio: *mut LinuxCompatSerio,
    _owner: *mut c_void,
) {
    if serio.is_null() {
        return;
    }

    let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio) else {
        let port = unsafe { &mut *serio };
        let port_id = NEXT_DYNAMIC_PORT_ID.fetch_add(1, Ordering::Relaxed);
        let _ = unsafe {
            crate::driver::serio::register_linux_port(
                SerioPortInfo::new(
                    port_id,
                    port.id.type_ as u32,
                    port.id.proto as u32,
                    port.id.id as u32,
                    port.id.extra as u32,
                ),
                serio,
            )
        };
        let _ = crate::driver::serio::set_driver_data(port_id, port.dev.driver_data as usize);
        return;
    };

    let port = unsafe { &mut *serio };
    let _ = crate::driver::serio::update_port_info(
        port_id,
        port.id.proto as u32,
        port.id.id as u32,
        port.id.extra as u32,
    );
    let _ = crate::driver::serio::set_driver_data(port_id, port.dev.driver_data as usize);
}

pub(crate) unsafe extern "C" fn serio_unregister_child_port(serio: *mut LinuxCompatSerio) {
    let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio) else {
        return;
    };
    let _ = crate::driver::serio::unregister_port(port_id);
}

pub(crate) unsafe extern "C" fn serio_unregister_port(serio: *mut LinuxCompatSerio) {
    unsafe { serio_unregister_child_port(serio) };
}

pub(crate) fn first_matching_driver(
    port_id: u32,
    port: *const LinuxCompatSerio,
) -> Option<(usize, *mut LinuxCompatSerioDriver)> {
    if port.is_null() {
        return None;
    }

    let port = unsafe { &*port };
    let _ = (
        port_id,
        port.id.type_,
        port.id.proto,
        port.id.id,
        port.id.extra,
    );
    crate::driver::serio::find_linux_driver(|index, driver_ptr| {
        let _ = index;
        if linux_driver_matches(driver_ptr, port) {
            Some((index, driver_ptr))
        } else {
            None
        }
    })
}

pub(crate) unsafe fn apply_port_driver(
    port: &mut LinuxCompatSerio,
    driver: *mut LinuxCompatSerioDriver,
) {
    port.drv = driver;
    let driver = unsafe { &mut *driver };
    port.dev.driver = &mut driver.driver;
}

pub(crate) fn clear_port_driver(port: &mut LinuxCompatSerio) {
    port.drv = core::ptr::null_mut();
    port.dev.driver = core::ptr::null_mut();
}

pub(crate) unsafe fn connect_driver(
    port: *mut LinuxCompatSerio,
    driver: *mut LinuxCompatSerioDriver,
) -> i32 {
    crate::debug::println!(
        "serio connect_driver enter: port={:#x} driver={:#x}",
        port as usize,
        driver as usize
    );
    let connect = unsafe { (*driver).connect };
    if let Some(connect) = connect {
        unsafe { connect(port, driver) }
    } else {
        0
    }
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
pub(crate) fn driver_name(driver: *mut LinuxCompatSerioDriver) -> &'static str {
    if driver.is_null() {
        return "invalid";
    }
    let name_ptr = unsafe { (*driver).driver.name };
    compat_cstr(name_ptr).unwrap_or("invalid")
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "__serio_register_driver" => Some(__serio_register_driver as *const () as usize),
        "__serio_register_port" => Some(__serio_register_port as *const () as usize),
        "serio_unregister_child_port" => Some(serio_unregister_child_port as *const () as usize),
        "serio_unregister_port" => Some(serio_unregister_port as *const () as usize),
        _ => None,
    }
}

fn linux_driver_matches(driver: *mut LinuxCompatSerioDriver, port: &LinuxCompatSerio) -> bool {
    if driver.is_null() {
        return false;
    }
    let table = unsafe { (*driver).id_table };
    if table.is_null() {
        return false;
    }

    let mut index = 0usize;
    while index < 64 {
        let entry = unsafe { *table.add(index) };
        if entry.is_terminator() {
            return false;
        }
        if linux_device_id_matches(entry, port) {
            return true;
        }
        index += 1;
    }

    crate::debug::println!(
        "serio linux driver id table missing terminator: driver={:#x} port_type={} port_proto={}",
        driver as usize,
        port.id.type_,
        port.id.proto
    );
    false
}

fn linux_device_id_matches(id: LinuxCompatSerioDeviceId, port: &LinuxCompatSerio) -> bool {
    serio_any_matches(id.type_, port.id.type_)
        && serio_any_matches(id.proto, port.id.proto)
        && serio_any_matches(id.id, port.id.id)
        && serio_any_matches(id.extra, port.id.extra)
}
