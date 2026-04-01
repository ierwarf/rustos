use alloc::alloc::{alloc, Layout};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::{c_char, c_void};
use core::ptr;

use spin::Mutex;

use super::compat::{
    compat_cstr, LinuxCompatHidDevice, LinuxCompatHidDeviceId, LinuxCompatHidDriver,
    LinuxCompatHidField, LinuxCompatHidReport,
};

const HID_BUS_ANY: u16 = 0xffff;
const HID_GROUP_ANY: u16 = 0x0000;
const HID_ANY_ID: u32 = !0;
const HID_STAT_PARSED: usize = 1 << 1;
const HID_INPUT_REPORT: u32 = 0;
const HID_OUTPUT_REPORT: u32 = 1;
const HID_FEATURE_REPORT: u32 = 2;
const HID_REPORT_TYPE_COUNT: usize = 3;
const HID_MIN_BUFFER_SIZE: usize = 64;
const HID_MAX_BUFFER_SIZE: usize = 16 * 1024;
const HID_BUS_USB: u16 = 0x03;
const HID_QUIRK_IGNORE: u64 = 1 << 2;
const HID_QUIRK_NO_IGNORE: u64 = 1 << 30;

static HID_BUS_TYPE: [u8; 128] = [0; 128];
static HID_OPS: [u8; 64] = [0; 64];
static HID_DRIVERS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static HID_DEVICES: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static HID_REPORT_ALLOCATIONS: Mutex<Vec<HidOwnedReports>> = Mutex::new(Vec::new());

#[derive(Default)]
struct HidOwnedReports {
    dev_ptr: usize,
    reports: Vec<usize>,
}

#[derive(Clone, Copy, Default)]
struct HidGlobalState {
    report_id: u32,
    report_size: u32,
    report_count: u32,
}

pub(crate) fn bus_type_ptr() -> *const c_void {
    &HID_BUS_TYPE as *const [u8; 128] as *const c_void
}

pub(crate) unsafe extern "C" fn __hid_register_driver(
    driver: *mut LinuxCompatHidDriver,
    _owner: *mut c_void,
    _mod_name: *const c_char,
) -> i32 {
    if driver.is_null() {
        return -22;
    }

    unsafe {
        if (*driver).driver.name.is_null() {
            (*driver).driver.name = (*driver).name.cast_const();
        }
        (*driver).driver.bus = bus_type_ptr();
    }

    let _ = unsafe { crate::driver::linux::device::bus_register(bus_type_ptr() as *mut c_void) };
    let status =
        unsafe { crate::driver::linux::device::driver_register(&mut (*driver).driver as *mut _) };
    if status != 0 {
        return status;
    }

    let mut drivers = HID_DRIVERS.lock();
    if !drivers.iter().any(|entry| *entry == driver as usize) {
        drivers.push(driver as usize);
    }
    drop(drivers);

    crate::debug::println!(
        "hid register driver: ptr={:#x} name={}",
        driver as usize,
        unsafe { compat_cstr((*driver).name).unwrap_or("invalid") }
    );

    bind_driver(driver);
    0
}

pub(crate) unsafe extern "C" fn hid_unregister_driver(driver: *mut LinuxCompatHidDriver) {
    if driver.is_null() {
        return;
    }
    {
        let mut drivers = HID_DRIVERS.lock();
        if let Some(index) = drivers.iter().position(|entry| *entry == driver as usize) {
            drivers.remove(index);
        }
    }
    unsafe {
        crate::driver::linux::device::driver_unregister(&mut (*driver).driver as *mut _);
    }
}

pub(crate) unsafe extern "C" fn hid_allocate_device() -> *mut LinuxCompatHidDevice {
    let mut dev = Box::<LinuxCompatHidDevice>::default();
    initialize_hid_device(dev.as_mut());
    Box::into_raw(dev)
}

pub(crate) unsafe extern "C" fn hid_destroy_device(dev: *mut LinuxCompatHidDevice) {
    if dev.is_null() {
        return;
    }
    crate::usb::hid_remove_device(dev);
    clear_owned_reports(dev as usize);
    {
        let mut devices = HID_DEVICES.lock();
        if let Some(index) = devices.iter().position(|entry| *entry == dev as usize) {
            devices.remove(index);
        }
    }
    unsafe {
        drop(Box::from_raw(dev));
    }
}

pub(crate) unsafe extern "C" fn hid_add_device(dev: *mut LinuxCompatHidDevice) -> i32 {
    if dev.is_null() {
        return -22;
    }

    let compat_dev = unsafe {
        (&mut (*dev).dev as *mut super::compat::LinuxCompatHidEmbeddedDevice)
            .cast::<super::compat::LinuxCompatDevice>()
    };
    unsafe {
        (*dev).dev.bus = bus_type_ptr();
        crate::driver::linux::device::device_initialize(compat_dev);
    }
    let status = unsafe { crate::driver::linux::device::device_add(compat_dev) };
    if status != 0 {
        return status;
    }

    let mut devices = HID_DEVICES.lock();
    if !devices.iter().any(|entry| *entry == dev as usize) {
        devices.push(dev as usize);
    }
    drop(devices);

    crate::debug::println!(
        "hid add device: dev={:#x} bus={:#x} vendor={:04x} product={:04x} group={:#x} ll_driver={:#x} rdesc={:#x} rsize={}",
        dev as usize,
        unsafe { (*dev).bus },
        unsafe { (*dev).vendor },
        unsafe { (*dev).product },
        unsafe { (*dev).group },
        unsafe { (*dev).ll_driver as usize },
        unsafe { (*dev).rdesc as usize },
        unsafe { (*dev).rsize },
    );

    bind_device(dev)
}

pub(crate) unsafe extern "C" fn hid_parse_report(
    dev: *mut LinuxCompatHidDevice,
    start: *const u8,
    size: u32,
) -> i32 {
    if dev.is_null() {
        return -22;
    }
    unsafe {
        if !start.is_null() {
            (*dev).dev_rdesc = start;
            (*dev).rdesc = start;
        }
        if size != 0 {
            (*dev).dev_rsize = size;
            (*dev).rsize = size;
        }
    }
    let status = rebuild_hid_reports(dev);
    if status == 0 {
        unsafe {
            (*dev).status |= HID_STAT_PARSED;
        }
    }
    status
}

pub(crate) unsafe extern "C" fn hid_input_report(
    dev: *mut LinuxCompatHidDevice,
    _type_: i32,
    data: *mut u8,
    size: u32,
    _interrupt: i32,
) -> i32 {
    crate::usb::hid_input_report(dev, data, size)
}

pub(crate) unsafe extern "C" fn hid_output_report(
    report: *mut LinuxCompatHidReport,
    data: *mut u8,
) {
    if report.is_null() || data.is_null() {
        return;
    }
    let report_id = unsafe { (*report).id };
    if report_id != 0 {
        unsafe {
            *data = report_id as u8;
        }
    }
}

pub(crate) unsafe extern "C" fn hid_hw_start(
    dev: *mut LinuxCompatHidDevice,
    _connect_mask: u32,
) -> i32 {
    if dev.is_null() {
        return -22;
    }
    let ll = unsafe { (*dev).ll_driver };
    if ll.is_null() {
        return 0;
    }
    let Some(start) = (unsafe { (*ll).start }) else {
        return 0;
    };
    let status = unsafe { start(dev) };
    if status == 0 {
        unsafe {
            (*dev).io_started = true;
        }
    }
    status
}

pub(crate) unsafe extern "C" fn hid_hw_open(dev: *mut LinuxCompatHidDevice) -> i32 {
    if dev.is_null() {
        return -22;
    }
    let ll = unsafe { (*dev).ll_driver };
    if ll.is_null() {
        return 0;
    }
    unsafe {
        (*dev).ll_open_count = (*dev).ll_open_count.saturating_add(1);
        if (*dev).ll_open_count > 1 {
            return 0;
        }
    }
    let Some(open) = (unsafe { (*ll).open }) else {
        return 0;
    };
    let status = unsafe { open(dev) };
    if status != 0 {
        unsafe {
            (*dev).ll_open_count = (*dev).ll_open_count.saturating_sub(1);
        }
    }
    status
}

pub(crate) unsafe extern "C" fn hid_hw_close(dev: *mut LinuxCompatHidDevice) {
    if dev.is_null() {
        return;
    }
    let ll = unsafe { (*dev).ll_driver };
    if ll.is_null() {
        return;
    }
    let previous = unsafe { (*dev).ll_open_count };
    if previous == 0 {
        return;
    }
    unsafe {
        (*dev).ll_open_count = previous - 1;
    }
    if previous > 1 {
        return;
    }
    let Some(close) = (unsafe { (*ll).close }) else {
        return;
    };
    unsafe { close(dev) };
}

pub(crate) unsafe extern "C" fn hid_hw_request(
    dev: *mut LinuxCompatHidDevice,
    report: *mut LinuxCompatHidReport,
    reqtype: i32,
) {
    if dev.is_null() {
        return;
    }
    let ll = unsafe { (*dev).ll_driver };
    if ll.is_null() {
        return;
    }
    let Some(request) = (unsafe { (*ll).request }) else {
        return;
    };
    unsafe { request(dev, report, reqtype) };
}

pub(crate) unsafe extern "C" fn hid_lookup_quirk(dev: *const LinuxCompatHidDevice) -> u64 {
    if dev.is_null() {
        return 0;
    }
    let mut quirks = unsafe { ((*dev).initial_quirks | (*dev).quirks) as u64 };
    if (quirks & HID_QUIRK_NO_IGNORE) != 0 {
        quirks &= !HID_QUIRK_IGNORE;
    }
    if unsafe { (*dev).bus } == HID_BUS_USB && (quirks & HID_QUIRK_IGNORE) != 0 {
        crate::debug::println!(
            "hid lookup quirk: suppress ignore dev={:#x} vendor={:04x} product={:04x} quirks={:#x}",
            dev as usize,
            unsafe { (*dev).vendor },
            unsafe { (*dev).product },
            quirks,
        );
        quirks &= !HID_QUIRK_IGNORE;
    }
    quirks
}

pub(crate) unsafe extern "C" fn hid_open_report(_dev: *mut LinuxCompatHidDevice) -> i32 {
    if _dev.is_null() {
        return -22;
    }
    let ll = unsafe { (*_dev).ll_driver };
    let populated = report_lists_populated(_dev);
    if populated {
        return 0;
    }
    if !ll.is_null() {
        if let Some(parse) = unsafe { (*ll).parse } {
            let status = unsafe { parse(_dev) };
            if status != 0 {
                return status;
            }
        }
    }
    if report_lists_populated(_dev) {
        return 0;
    }
    let (rdesc, rsize) = unsafe {
        let rdesc = if !(*_dev).rdesc.is_null() {
            (*_dev).rdesc
        } else {
            (*_dev).dev_rdesc
        };
        let rsize = if (*_dev).rsize != 0 {
            (*_dev).rsize
        } else {
            (*_dev).dev_rsize
        };
        (rdesc, rsize)
    };
    if rdesc.is_null() || rsize == 0 {
        return -22;
    }
    unsafe { hid_parse_report(_dev, rdesc, rsize) }
}

pub(crate) unsafe extern "C" fn hid_alloc_report_buf(
    report: *mut LinuxCompatHidReport,
    _flags: u32,
) -> *mut u8 {
    let size = if report.is_null() {
        HID_MIN_BUFFER_SIZE
    } else {
        let report_ref = unsafe { &*report };
        let payload = (report_ref.size as usize).saturating_add(7) / 8;
        let total = payload.saturating_add((report_ref.id != 0) as usize);
        total.clamp(HID_MIN_BUFFER_SIZE, HID_MAX_BUFFER_SIZE)
    };
    let Ok(layout) = Layout::array::<u8>(size) else {
        return ptr::null_mut();
    };
    unsafe { alloc(layout) }
}

pub(crate) unsafe extern "C" fn hid_set_field(
    field: *mut LinuxCompatHidField,
    offset: u32,
    value: i32,
) -> i32 {
    if field.is_null() {
        return -22;
    }
    let value_count = unsafe { (*field).report_count };
    if offset >= value_count {
        return -22;
    }
    let values = unsafe { (*field).value };
    if values.is_null() {
        return -22;
    }
    unsafe {
        *values.add(offset as usize) = value;
    }
    0
}

pub(crate) unsafe extern "C" fn hid_check_keys_pressed(_dev: *mut LinuxCompatHidDevice) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn hidinput_count_leds(_dev: *mut LinuxCompatHidDevice) -> u32 {
    0
}

pub(crate) unsafe extern "C" fn hid_driver_suspend(
    dev: *mut LinuxCompatHidDevice,
    message: u32,
) -> i32 {
    call_driver_suspend(dev, message)
}

pub(crate) unsafe extern "C" fn hid_driver_resume(dev: *mut LinuxCompatHidDevice) -> i32 {
    call_driver_resume(dev, false)
}

pub(crate) unsafe extern "C" fn hid_driver_reset_resume(dev: *mut LinuxCompatHidDevice) -> i32 {
    call_driver_resume(dev, true)
}

pub(crate) unsafe extern "C" fn hid_quirks_init(_count: usize) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn hid_quirks_exit() {}

pub(crate) unsafe extern "C" fn hid_match_device(
    dev: *mut LinuxCompatHidDevice,
    driver: *mut LinuxCompatHidDriver,
) -> *const LinuxCompatHidDeviceId {
    if dev.is_null() || driver.is_null() {
        return ptr::null();
    }
    match_id(dev, unsafe { (*driver).id_table }).unwrap_or(ptr::null())
}

pub(crate) unsafe extern "C" fn dispatch_hid_bpf_device_event(
    _dev: *mut LinuxCompatHidDevice,
    _report_type: i32,
    data: *mut u8,
    _size: u32,
) -> *mut u8 {
    data
}

pub(crate) unsafe extern "C" fn dispatch_hid_bpf_raw_requests(
    _dev: *mut LinuxCompatHidDevice,
    _report_type: i32,
    _data: *mut u8,
    _size: u32,
    _request_type: u8,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn dispatch_hid_bpf_output_report(
    _dev: *mut LinuxCompatHidDevice,
    _buf: *mut u8,
    _size: u32,
    _report_type: i32,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn call_hid_bpf_rdesc_fixup(
    _dev: *mut LinuxCompatHidDevice,
    rdesc: *const u8,
    _size: *mut u32,
) -> *const u8 {
    rdesc
}

pub(crate) unsafe extern "C" fn hid_bpf_connect_device(_dev: *mut LinuxCompatHidDevice) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn hid_bpf_disconnect_device(_dev: *mut LinuxCompatHidDevice) {}

pub(crate) unsafe extern "C" fn hid_bpf_destroy_device(_dev: *mut LinuxCompatHidDevice) {}

pub(crate) unsafe extern "C" fn hid_bpf_device_init(_dev: *mut LinuxCompatHidDevice) -> i32 {
    0
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "__hid_register_driver" => Some(__hid_register_driver as *const () as usize),
        "hid_unregister_driver" => Some(hid_unregister_driver as *const () as usize),
        "hid_allocate_device" => Some(hid_allocate_device as *const () as usize),
        "hid_destroy_device" => Some(hid_destroy_device as *const () as usize),
        "hid_add_device" => Some(hid_add_device as *const () as usize),
        "hid_parse_report" => Some(hid_parse_report as *const () as usize),
        "hid_input_report" => Some(hid_input_report as *const () as usize),
        "hid_output_report" => Some(hid_output_report as *const () as usize),
        "hid_hw_start" => Some(hid_hw_start as *const () as usize),
        "hid_hw_open" => Some(hid_hw_open as *const () as usize),
        "hid_hw_close" => Some(hid_hw_close as *const () as usize),
        "hid_hw_request" => Some(hid_hw_request as *const () as usize),
        "hid_lookup_quirk" => Some(hid_lookup_quirk as *const () as usize),
        "hid_open_report" => Some(hid_open_report as *const () as usize),
        "hid_alloc_report_buf" => Some(hid_alloc_report_buf as *const () as usize),
        "hid_set_field" => Some(hid_set_field as *const () as usize),
        "hid_check_keys_pressed" => Some(hid_check_keys_pressed as *const () as usize),
        "hidinput_count_leds" => Some(hidinput_count_leds as *const () as usize),
        "hid_driver_suspend" => Some(hid_driver_suspend as *const () as usize),
        "hid_driver_resume" => Some(hid_driver_resume as *const () as usize),
        "hid_driver_reset_resume" => Some(hid_driver_reset_resume as *const () as usize),
        "hid_quirks_init" => Some(hid_quirks_init as *const () as usize),
        "hid_quirks_exit" => Some(hid_quirks_exit as *const () as usize),
        "hid_match_device" => Some(hid_match_device as *const () as usize),
        "dispatch_hid_bpf_device_event" => {
            Some(dispatch_hid_bpf_device_event as *const () as usize)
        }
        "dispatch_hid_bpf_raw_requests" => {
            Some(dispatch_hid_bpf_raw_requests as *const () as usize)
        }
        "dispatch_hid_bpf_output_report" => {
            Some(dispatch_hid_bpf_output_report as *const () as usize)
        }
        "call_hid_bpf_rdesc_fixup" => Some(call_hid_bpf_rdesc_fixup as *const () as usize),
        "hid_bpf_connect_device" => Some(hid_bpf_connect_device as *const () as usize),
        "hid_bpf_disconnect_device" => Some(hid_bpf_disconnect_device as *const () as usize),
        "hid_bpf_destroy_device" => Some(hid_bpf_destroy_device as *const () as usize),
        "hid_bpf_device_init" => Some(hid_bpf_device_init as *const () as usize),
        "hid_bus_type" => Some(bus_type_ptr() as usize),
        "hid_ops" => Some(&HID_OPS as *const [u8; 64] as usize),
        _ => None,
    }
}

fn bind_driver(driver: *mut LinuxCompatHidDriver) {
    let devices = HID_DEVICES.lock().clone();
    for device in devices {
        let _ = bind_device_to_driver(device as *mut LinuxCompatHidDevice, driver);
    }
}

fn bind_device(dev: *mut LinuxCompatHidDevice) -> i32 {
    let drivers = HID_DRIVERS.lock().clone();
    for driver in drivers {
        let status = bind_device_to_driver(dev, driver as *mut LinuxCompatHidDriver);
        if status == 0 {
            return 0;
        }
    }
    0
}

fn bind_device_to_driver(dev: *mut LinuxCompatHidDevice, driver: *mut LinuxCompatHidDriver) -> i32 {
    let Some(id) = match_id(dev, unsafe { (*driver).id_table }) else {
        return -19;
    };
    let _driver_name = unsafe { compat_cstr((*driver).name).unwrap_or("invalid") };
    unsafe {
        (*dev).driver = driver;
        (*dev).dev.driver = &mut (*driver).driver;
    }
    crate::debug::println!(
        "hid bind probe begin: driver={} dev={:#x} vendor={:04x} product={:04x}",
        _driver_name,
        dev as usize,
        unsafe { (*dev).vendor },
        unsafe { (*dev).product }
    );
    let status = if let Some(probe) = unsafe { (*driver).probe } {
        unsafe { probe(dev, id) }
    } else {
        0
    };
    crate::debug::println!(
        "hid bind probe end: driver={} dev={:#x} status={}",
        _driver_name,
        dev as usize,
        status
    );
    if status != 0 {
        unsafe {
            (*dev).driver = ptr::null_mut();
            (*dev).dev.driver = ptr::null_mut();
        }
    } else {
        let _open_status = unsafe { hid_hw_open(dev) };
        crate::debug::println!(
            "hid bind auto-open: driver={} dev={:#x} status={}",
            _driver_name,
            dev as usize,
            _open_status
        );
    }
    status
}

fn match_id(
    dev: *mut LinuxCompatHidDevice,
    table: *const LinuxCompatHidDeviceId,
) -> Option<*const LinuxCompatHidDeviceId> {
    if dev.is_null() || table.is_null() {
        return None;
    }
    let mut current = table;
    loop {
        let id = unsafe { *current };
        if id.is_terminator() {
            return None;
        }
        let bus_matches = id.bus == HID_BUS_ANY || id.bus == unsafe { (*dev).bus };
        let group_matches = id.group == HID_GROUP_ANY || id.group == unsafe { (*dev).group };
        let vendor_matches = id.vendor == HID_ANY_ID || id.vendor == unsafe { (*dev).vendor };
        let product_matches = id.product == HID_ANY_ID || id.product == unsafe { (*dev).product };
        if bus_matches && group_matches && vendor_matches && product_matches {
            return Some(current);
        }
        current = unsafe { current.add(1) };
    }
}

fn call_driver_suspend(dev: *mut LinuxCompatHidDevice, message: u32) -> i32 {
    if dev.is_null() {
        return -22;
    }
    let driver = unsafe { (*dev).driver };
    if driver.is_null() {
        return 0;
    }
    let Some(suspend) = (unsafe { (*driver).suspend }) else {
        return 0;
    };
    unsafe { suspend(dev, message) }
}

fn call_driver_resume(dev: *mut LinuxCompatHidDevice, reset_resume: bool) -> i32 {
    if dev.is_null() {
        return -22;
    }
    let driver = unsafe { (*dev).driver };
    if driver.is_null() {
        return 0;
    }
    if reset_resume {
        if let Some(callback) = unsafe { (*driver).reset_resume } {
            return unsafe { callback(dev) };
        }
    }
    let Some(callback) = (unsafe { (*driver).resume }) else {
        return 0;
    };
    unsafe { callback(dev) }
}

fn initialize_hid_device(dev: &mut LinuxCompatHidDevice) {
    init_list_head(&mut dev.inputs);
    for report_enum in dev.report_enum.iter_mut() {
        init_list_head(&mut report_enum.report_list);
        report_enum.numbered = 0;
        report_enum.report_id_hash.fill(ptr::null_mut());
    }
}

fn init_list_head(head: &mut super::compat::LinuxCompatListHead) {
    let head_ptr = head as *mut _;
    head.next = head_ptr;
    head.prev = head_ptr;
}

fn list_add_tail(
    entry: *mut super::compat::LinuxCompatListHead,
    head: *mut super::compat::LinuxCompatListHead,
) {
    unsafe {
        (*entry).next = head;
        (*entry).prev = (*head).prev;
        (*(*head).prev).next = entry;
        (*head).prev = entry;
    }
}

fn report_lists_populated(dev: *mut LinuxCompatHidDevice) -> bool {
    if dev.is_null() {
        return false;
    }
    unsafe {
        (*dev).report_enum.iter().any(|report_enum| {
            let head =
                (&raw const report_enum.report_list) as *mut super::compat::LinuxCompatListHead;
            report_enum.report_list.next != head
        })
    }
}

fn clear_owned_reports(dev_ptr: usize) {
    let mut allocations = HID_REPORT_ALLOCATIONS.lock();
    let Some(index) = allocations
        .iter()
        .position(|entry| entry.dev_ptr == dev_ptr)
    else {
        return;
    };
    let entry = allocations.swap_remove(index);
    drop(allocations);
    for report in entry.reports {
        unsafe {
            drop(Box::from_raw(report as *mut LinuxCompatHidReport));
        }
    }
}

fn remember_owned_report(dev_ptr: usize, report_ptr: *mut LinuxCompatHidReport) {
    let mut allocations = HID_REPORT_ALLOCATIONS.lock();
    if let Some(entry) = allocations
        .iter_mut()
        .find(|entry| entry.dev_ptr == dev_ptr)
    {
        entry.reports.push(report_ptr as usize);
        return;
    }
    allocations.push(HidOwnedReports {
        dev_ptr,
        reports: vec![report_ptr as usize],
    });
}

fn rebuild_hid_reports(dev: *mut LinuxCompatHidDevice) -> i32 {
    if dev.is_null() {
        return -22;
    }

    let (rdesc, rsize) = unsafe {
        let rdesc = if !(*dev).rdesc.is_null() {
            (*dev).rdesc
        } else {
            (*dev).dev_rdesc
        };
        let rsize = if (*dev).rsize != 0 {
            (*dev).rsize
        } else {
            (*dev).dev_rsize
        };
        (rdesc, rsize)
    };
    if rdesc.is_null() || rsize == 0 {
        return -22;
    }

    clear_owned_reports(dev as usize);
    unsafe {
        for report_enum in (*dev).report_enum.iter_mut() {
            init_list_head(&mut report_enum.report_list);
            report_enum.numbered = 0;
            report_enum.report_id_hash.fill(ptr::null_mut());
        }
    }

    let descriptor = unsafe { core::slice::from_raw_parts(rdesc, rsize as usize) };
    let mut global = HidGlobalState::default();
    let mut global_stack = Vec::<HidGlobalState>::new();
    let mut offset = 0usize;

    while offset < descriptor.len() {
        let prefix = descriptor[offset];
        offset += 1;
        if prefix == 0xfe {
            if offset + 2 > descriptor.len() {
                return -22;
            }
            let item_len = descriptor[offset] as usize;
            offset = offset.saturating_add(2);
            if offset + item_len > descriptor.len() {
                return -22;
            }
            offset += item_len;
            continue;
        }

        let size = match prefix & 0x3 {
            0 => 0usize,
            1 => 1usize,
            2 => 2usize,
            _ => 4usize,
        };
        if offset + size > descriptor.len() {
            return -22;
        }
        let payload = &descriptor[offset..offset + size];
        offset += size;

        match ((prefix >> 2) & 0x3, (prefix >> 4) & 0xf) {
            (1, 7) => global.report_size = hid_item_u32(payload),
            (1, 8) => global.report_id = hid_item_u32(payload),
            (1, 9) => global.report_count = hid_item_u32(payload),
            (1, 10) => global_stack.push(global),
            (1, 11) => {
                let Some(saved) = global_stack.pop() else {
                    return -22;
                };
                global = saved;
            }
            (0, 8) => {
                let status = add_report_bits(
                    dev,
                    HID_INPUT_REPORT,
                    global.report_id,
                    global.report_size,
                    global.report_count,
                );
                if status != 0 {
                    return status;
                }
            }
            (0, 9) => {
                let status = add_report_bits(
                    dev,
                    HID_OUTPUT_REPORT,
                    global.report_id,
                    global.report_size,
                    global.report_count,
                );
                if status != 0 {
                    return status;
                }
            }
            (0, 11) => {
                let status = add_report_bits(
                    dev,
                    HID_FEATURE_REPORT,
                    global.report_id,
                    global.report_size,
                    global.report_count,
                );
                if status != 0 {
                    return status;
                }
            }
            _ => {}
        }
    }

    crate::debug::println!(
        "hid parse report done: dev={:#x} input={} output={} feature={}",
        dev as usize,
        report_count(dev, HID_INPUT_REPORT),
        report_count(dev, HID_OUTPUT_REPORT),
        report_count(dev, HID_FEATURE_REPORT),
    );
    0
}

fn add_report_bits(
    dev: *mut LinuxCompatHidDevice,
    report_type: u32,
    report_id: u32,
    report_size: u32,
    report_count: u32,
) -> i32 {
    if report_type as usize >= HID_REPORT_TYPE_COUNT || report_id >= 256 {
        return -22;
    }
    let report = ensure_report(dev, report_type, report_id);
    if report.is_null() {
        return -12;
    }
    let bits = report_size.saturating_mul(report_count);
    unsafe {
        (*report).size = (*report).size.saturating_add(bits);
    }
    0
}

fn ensure_report(
    dev: *mut LinuxCompatHidDevice,
    report_type: u32,
    report_id: u32,
) -> *mut LinuxCompatHidReport {
    if dev.is_null() || report_type as usize >= HID_REPORT_TYPE_COUNT || report_id >= 256 {
        return ptr::null_mut();
    }
    let report_enum = unsafe { &mut (*dev).report_enum[report_type as usize] };
    if !report_enum.report_id_hash[report_id as usize].is_null() {
        return report_enum.report_id_hash[report_id as usize];
    }

    let mut report = Box::<LinuxCompatHidReport>::default();
    init_list_head(&mut report.list);
    init_list_head(&mut report.hidinput_list);
    init_list_head(&mut report.field_entry_list);
    report.id = report_id;
    report.type_ = report_type;
    report.device = dev;

    let report_ptr = Box::into_raw(report);
    list_add_tail(
        unsafe { &mut (*report_ptr).list },
        &mut report_enum.report_list as *mut _,
    );
    report_enum.report_id_hash[report_id as usize] = report_ptr;
    if report_id != 0 {
        report_enum.numbered = 1;
    }
    remember_owned_report(dev as usize, report_ptr);
    report_ptr
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
fn report_count(dev: *mut LinuxCompatHidDevice, report_type: u32) -> usize {
    if dev.is_null() || report_type as usize >= HID_REPORT_TYPE_COUNT {
        return 0;
    }
    let report_enum = unsafe { &(*dev).report_enum[report_type as usize] };
    report_enum
        .report_id_hash
        .iter()
        .filter(|report| !report.is_null())
        .count()
}

fn hid_item_u32(payload: &[u8]) -> u32 {
    match payload.len() {
        0 => 0,
        1 => payload[0] as u32,
        2 => u16::from_le_bytes([payload[0], payload[1]]) as u32,
        4 => u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]),
        _ => 0,
    }
}
