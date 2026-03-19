use driver_abi::{PointerPacket, SerioDriverRegistration};

use super::{input, linux, module_registry, serio};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustos_register_serio_driver(
    driver: *const SerioDriverRegistration,
) -> i32 {
    unsafe { serio::register_driver(driver) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustos_report_pointer_packet(packet: *const PointerPacket) -> i32 {
    unsafe { input::report_pointer_packet(packet) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustos_serio_write(port_id: u32, byte: u8) -> i32 {
    serio::write(port_id, byte)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustos_serio_open(port_id: u32) -> i32 {
    serio::open(port_id)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustos_serio_close(port_id: u32) {
    serio::close(port_id)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustos_serio_ps2_command(
    port_id: u32,
    command: u8,
    data_ptr: *const u8,
    data_len: u32,
    response_ptr: *mut u8,
    response_len: u32,
) -> i32 {
    unsafe {
        serio::ps2_command(
            port_id,
            command,
            data_ptr,
            data_len,
            response_ptr,
            response_len,
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustos_serio_update_port_info(
    port_id: u32,
    proto: u32,
    id: u32,
    extra: u32,
) -> i32 {
    serio::update_port_info(port_id, proto, id, extra)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustos_serio_get_drvdata(port_id: u32) -> usize {
    serio::driver_data(port_id)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustos_serio_set_drvdata(port_id: u32, drvdata: usize) -> i32 {
    serio::set_driver_data(port_id, drvdata)
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "rustos_register_serio_driver" => Some(rustos_register_serio_driver as *const () as usize),
        "rustos_report_pointer_packet" => Some(rustos_report_pointer_packet as *const () as usize),
        "rustos_serio_write" => Some(rustos_serio_write as *const () as usize),
        "rustos_serio_open" => Some(rustos_serio_open as *const () as usize),
        "rustos_serio_close" => Some(rustos_serio_close as *const () as usize),
        "rustos_serio_ps2_command" => Some(rustos_serio_ps2_command as *const () as usize),
        "rustos_serio_update_port_info" => {
            Some(rustos_serio_update_port_info as *const () as usize)
        }
        "rustos_serio_get_drvdata" => Some(rustos_serio_get_drvdata as *const () as usize),
        "rustos_serio_set_drvdata" => Some(rustos_serio_set_drvdata as *const () as usize),
        _ => linux::resolve_symbol(name).or_else(|| module_registry::resolve_symbol(name)),
    }
}
