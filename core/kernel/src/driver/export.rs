use alloc::alloc::{alloc, alloc_zeroed, dealloc, handle_alloc_error, realloc};
use core::alloc::Layout;
use core::sync::atomic::{AtomicUsize, Ordering};

use driver_abi::{PointerPacket, SerioDriverRegistration};

use super::{input, linux, module_registry, serio};

static HID_SYMBOL_RESOLUTION_LOGS: AtomicUsize = AtomicUsize::new(0);

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
        "_RNvCsaHtlzDUaK2V_7___rustc12___rust_alloc" => {
            Some(rust_module_alloc as *const () as usize)
        }
        "_RNvCsaHtlzDUaK2V_7___rustc14___rust_dealloc" => {
            Some(rust_module_dealloc as *const () as usize)
        }
        "_RNvCsaHtlzDUaK2V_7___rustc14___rust_realloc" => {
            Some(rust_module_realloc as *const () as usize)
        }
        "_RNvCsaHtlzDUaK2V_7___rustc19___rust_alloc_zeroed" => {
            Some(rust_module_alloc_zeroed as *const () as usize)
        }
        "_RNvCsaHtlzDUaK2V_7___rustc26___rust_alloc_error_handler" => {
            Some(rust_module_alloc_error_handler as *const () as usize)
        }
        "_RNvCsaHtlzDUaK2V_7___rustc35___rust_no_alloc_shim_is_unstable_v2" => {
            Some(rust_module_no_alloc_shim_is_unstable_v2 as *const () as usize)
        }
        "_RNvNtCsfnHfWp76JbT_4core9panicking18panic_bounds_check" => {
            Some(rust_module_panic_bounds_check as *const () as usize)
        }
        "_RNvNtNtCsfnHfWp76JbT_4core3str8converts9from_utf8" => {
            Some(rust_module_from_utf8 as *const () as usize)
        }
        "_RNvNtNtCsfnHfWp76JbT_4core5slice5index16slice_index_fail" => {
            Some(rust_module_slice_index_fail as *const () as usize)
        }
        _ if prefer_linux_compat_symbol(name) => {
            let linux_address = linux::resolve_symbol(name);
            let module_address = module_registry::resolve_symbol(name);
            if should_log_hid_symbol_resolution(name) {
                let log_index = HID_SYMBOL_RESOLUTION_LOGS.fetch_add(1, Ordering::Relaxed);
                if log_index < 32 {
                    crate::debug::println!(
                        "driver export resolve: symbol={} linux={:#x} module={:#x} chosen={:#x}",
                        name,
                        linux_address.unwrap_or(0),
                        module_address.unwrap_or(0),
                        linux_address.or(module_address).unwrap_or(0),
                    );
                }
            }
            linux_address.or(module_address)
        }
        _ => module_registry::resolve_symbol(name).or_else(|| linux::resolve_symbol(name)),
    }
}

fn rust_module_layout(size: usize, align: usize) -> Option<Layout> {
    Layout::from_size_align(size, align).ok()
}

unsafe fn rust_module_alloc(size: usize, align: usize) -> *mut u8 {
    rust_module_layout(size, align)
        .map(|layout| unsafe { alloc(layout) })
        .unwrap_or(core::ptr::null_mut())
}

unsafe fn rust_module_dealloc(ptr: *mut u8, size: usize, align: usize) {
    if let Some(layout) = rust_module_layout(size, align) {
        unsafe { dealloc(ptr, layout) };
    }
}

unsafe fn rust_module_realloc(
    ptr: *mut u8,
    old_size: usize,
    align: usize,
    new_size: usize,
) -> *mut u8 {
    rust_module_layout(old_size, align)
        .map(|layout| unsafe { realloc(ptr, layout, new_size) })
        .unwrap_or(core::ptr::null_mut())
}

unsafe fn rust_module_alloc_zeroed(size: usize, align: usize) -> *mut u8 {
    rust_module_layout(size, align)
        .map(|layout| unsafe { alloc_zeroed(layout) })
        .unwrap_or(core::ptr::null_mut())
}

fn rust_module_alloc_error_handler(size: usize, align: usize) -> ! {
    let Some(layout) = rust_module_layout(size, align) else {
        panic!("module allocation failed with invalid layout: size={size} align={align}");
    };
    handle_alloc_error(layout)
}

fn rust_module_no_alloc_shim_is_unstable_v2() {}

fn rust_module_panic_bounds_check(index: usize, len: usize) -> ! {
    panic!("module index out of bounds: len={len} index={index}");
}

fn rust_module_from_utf8(bytes: &[u8]) -> Result<&str, core::str::Utf8Error> {
    core::str::from_utf8(bytes)
}

fn rust_module_slice_index_fail(start: usize, end: usize, len: usize) -> ! {
    panic!("module slice index out of bounds: start={start} end={end} len={len}");
}

fn prefer_linux_compat_symbol(name: &str) -> bool {
    matches!(
        name,
        "__hid_register_driver"
            | "hid_unregister_driver"
            | "hid_allocate_device"
            | "hid_destroy_device"
            | "hid_add_device"
            | "hid_parse_report"
            | "hid_input_report"
            | "hid_output_report"
            | "hid_hw_start"
            | "hid_hw_open"
            | "hid_hw_close"
            | "hid_hw_request"
            | "hid_lookup_quirk"
            | "hid_open_report"
            | "hid_alloc_report_buf"
            | "hid_set_field"
            | "hid_check_keys_pressed"
            | "hidinput_count_leds"
            | "hid_driver_suspend"
            | "hid_driver_resume"
            | "hid_driver_reset_resume"
            | "hid_quirks_init"
            | "hid_quirks_exit"
            | "hid_match_device"
            | "dispatch_hid_bpf_device_event"
            | "dispatch_hid_bpf_raw_requests"
            | "dispatch_hid_bpf_output_report"
            | "call_hid_bpf_rdesc_fixup"
            | "hid_bpf_connect_device"
            | "hid_bpf_disconnect_device"
            | "hid_bpf_destroy_device"
            | "hid_bpf_device_init"
            | "hid_bus_type"
            | "hid_ops"
    )
}

fn should_log_hid_symbol_resolution(name: &str) -> bool {
    matches!(
        name,
        "__hid_register_driver"
            | "hid_bus_type"
            | "hid_ops"
            | "hid_match_device"
            | "hid_hw_start"
            | "hid_parse_report"
            | "hid_add_device"
    )
}
