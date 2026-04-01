use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ops::Range;
use core::sync::atomic::{AtomicUsize, Ordering};

use driver_abi::PointerPacket;
use hidreport::{ArrayField, Field, FieldAttributes, Report, ReportDescriptor};
use spin::Mutex;

use super::hid_translation::{hid_usage_to_keycode, pointer_buttons_from_report};
use crate::driver::linux::compat::{LinuxCompatHidDevice, LinuxCompatUrb, LinuxCompatUsbDevice};

const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
const USB_REQ_SET_CONFIGURATION: u8 = 0x09;
const USB_REQ_GET_INTERFACE: u8 = 0x0a;
const USB_REQ_SET_INTERFACE: u8 = 0x0b;
const USB_REQ_GET_PROTOCOL: u8 = 0x03;
const USB_REQ_SET_PROTOCOL: u8 = 0x0b;
const USB_REQ_GET_IDLE: u8 = 0x02;
const USB_REQ_SET_IDLE: u8 = 0x0a;

const USB_DT_DEVICE: u8 = 0x01;
const USB_DT_HID: u8 = 0x21;
const USB_DT_REPORT: u8 = 0x22;

const PIPE_INTERRUPT: u32 = 1;
const PIPE_CONTROL: u32 = 2;
const USB_DIR_IN: u32 = 0x80;
const USB_URB_STATUS_UNLINKED: i32 = -2;
const REPORT_QUEUE_CAPACITY: usize = 256;
const PENDING_COMPLETION_CAPACITY: usize = 64;
const COMPLETIONS_PER_SERVICE: usize = 128;
const REPORT_DROP_LOG_INTERVAL: u64 = 128;
const MAX_REPORT_BYTES: usize = 64;
const KEYBOARD_TRANSLATION_LOG_LIMIT: usize = 0;
const POINTER_TRANSLATION_LOG_LIMIT: usize = 0;
const URB_SUBMIT_LOG_LIMIT: usize = 0;
const REPORT_ENQUEUE_LOG_LIMIT: usize = 0;
const REPORT_DESCRIPTOR_LOG_LIMIT: usize = 0;
const HID_REPORT_ENTRY_LOG_LIMIT: usize = 0;

#[derive(Clone)]
struct RuntimeUsbDevice {
    usb_device_ptr: usize,
    interface_ptr: usize,
    interface_number: u8,
    device_descriptor: [u8; 18],
    hid_descriptor: Box<[u8]>,
    report_descriptor: Box<[u8]>,
    layout_hint: Option<Arc<HidReportLayout>>,
    queued_reports: VecDeque<RuntimeReport>,
    pending_urbs: VecDeque<usize>,
    dropped_reports: u64,
}

#[derive(Clone, Copy)]
struct RuntimeReport {
    len: usize,
    bytes: [u8; MAX_REPORT_BYTES],
}

#[derive(Clone, Copy)]
struct UrbCompletion {
    urb_ptr: usize,
    callback: unsafe extern "C" fn(*mut LinuxCompatUrb),
}

#[derive(Clone, Debug)]
enum HidReportLayout {
    BootKeyboard(BootKeyboardLayout),
    Pointer(PointerLayout),
}

#[derive(Clone, Debug)]
struct HidValueField {
    bits: Range<usize>,
    signed: bool,
    relative: bool,
    logical_minimum: i32,
    logical_maximum: i32,
}

#[derive(Clone, Debug)]
struct HidArrayField {
    bits: Range<usize>,
    count: usize,
    signed: bool,
}

#[derive(Clone, Debug)]
struct BootKeyboardLayout {
    report_id: u8,
    required_bytes: usize,
    modifier_fields: [Option<HidValueField>; 8],
    key_array: HidArrayField,
}

#[derive(Clone, Debug)]
struct PointerLayout {
    report_id: u8,
    required_bytes: usize,
    button_fields: Vec<HidValueField>,
    x_field: HidValueField,
    y_field: HidValueField,
    wheel_field: Option<HidValueField>,
    relative: bool,
    logical_min_x: i32,
    logical_max_x: i32,
    logical_min_y: i32,
    logical_max_y: i32,
}

#[derive(Clone, Debug)]
struct HidReportState {
    hid_device_ptr: usize,
    layout: Option<Arc<HidReportLayout>>,
    last_modifiers: u8,
    last_keys: [u8; 16],
    last_key_count: usize,
    last_pointer_buttons: u8,
    last_pointer_x: i32,
    last_pointer_y: i32,
    have_pointer_origin: bool,
}

impl Default for HidReportState {
    fn default() -> Self {
        Self {
            hid_device_ptr: 0,
            layout: None,
            last_modifiers: 0,
            last_keys: [0; 16],
            last_key_count: 0,
            last_pointer_buttons: 0,
            last_pointer_x: 0,
            last_pointer_y: 0,
            have_pointer_origin: false,
        }
    }
}

static USB_RUNTIME_DEVICES: Mutex<Vec<RuntimeUsbDevice>> = Mutex::new(Vec::new());
static HID_REPORT_STATES: Mutex<Vec<HidReportState>> = Mutex::new(Vec::new());
static PENDING_COMPLETIONS: Mutex<VecDeque<UrbCompletion>> = Mutex::new(VecDeque::new());
static KEYBOARD_TRANSLATION_LOGS: AtomicUsize = AtomicUsize::new(0);
static POINTER_TRANSLATION_LOGS: AtomicUsize = AtomicUsize::new(0);
static URB_SUBMIT_LOGS: AtomicUsize = AtomicUsize::new(0);
static REPORT_ENQUEUE_LOGS: AtomicUsize = AtomicUsize::new(0);
static REPORT_DESCRIPTOR_LOGS: AtomicUsize = AtomicUsize::new(0);
static HID_REPORT_ENTRY_LOGS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn prepare() {
    let mut pending = PENDING_COMPLETIONS.lock();
    let current_capacity = pending.capacity();
    if current_capacity < PENDING_COMPLETION_CAPACITY {
        pending.reserve(PENDING_COMPLETION_CAPACITY - current_capacity);
    }
}

pub(crate) fn service_pending() -> usize {
    let mut completed = 0usize;
    while completed < COMPLETIONS_PER_SERVICE {
        let completion = {
            let mut pending = PENDING_COMPLETIONS.lock();
            pending.pop_front()
        };
        let Some(completion) = completion else {
            break;
        };
        dispatch_urb_completion(Some(completion));
        completed += 1;
    }
    completed
}

pub(crate) fn handles_device(dev: *mut LinuxCompatUsbDevice) -> bool {
    if dev.is_null() {
        return false;
    }
    USB_RUNTIME_DEVICES
        .lock()
        .iter()
        .any(|entry| entry.usb_device_ptr == dev as usize)
}

pub(crate) fn owns_hid_device(dev: *mut LinuxCompatHidDevice) -> bool {
    if dev.is_null() {
        return false;
    }
    let (vendor, product, bus) =
        unsafe { ((*dev).vendor as u16, (*dev).product as u16, (*dev).bus) };
    if bus != 0x03 {
        return false;
    }
    USB_RUNTIME_DEVICES.lock().iter().any(|entry| {
        let entry_vendor =
            u16::from_le_bytes([entry.device_descriptor[8], entry.device_descriptor[9]]);
        let entry_product =
            u16::from_le_bytes([entry.device_descriptor[10], entry.device_descriptor[11]]);
        entry_vendor == vendor && entry_product == product
    })
}

pub(crate) fn register_device(
    usb_device: *mut LinuxCompatUsbDevice,
    interface: *mut crate::driver::linux::compat::LinuxCompatUsbInterface,
    interface_number: u8,
    hid_descriptor: &[u8],
    report_descriptor: &[u8],
) {
    if usb_device.is_null()
        || interface.is_null()
        || hid_descriptor.is_empty()
        || report_descriptor.is_empty()
    {
        return;
    }
    let layout_hint = parse_hid_layout(report_descriptor).map(Arc::new);

    let mut device_descriptor = [0u8; 18];
    unsafe {
        let descriptor = &(*usb_device).descriptor;
        device_descriptor.copy_from_slice(&descriptor[..18]);
    }

    let mut devices = USB_RUNTIME_DEVICES.lock();
    if devices
        .iter()
        .any(|entry| entry.interface_ptr == interface as usize)
    {
        return;
    }

    devices.push(RuntimeUsbDevice {
        usb_device_ptr: usb_device as usize,
        interface_ptr: interface as usize,
        interface_number,
        device_descriptor,
        hid_descriptor: hid_descriptor.to_vec().into_boxed_slice(),
        report_descriptor: report_descriptor.to_vec().into_boxed_slice(),
        layout_hint,
        queued_reports: VecDeque::with_capacity(REPORT_QUEUE_CAPACITY.min(32)),
        pending_urbs: VecDeque::new(),
        dropped_reports: 0,
    });

    if REPORT_DESCRIPTOR_LOGS.fetch_add(1, Ordering::Relaxed) < REPORT_DESCRIPTOR_LOG_LIMIT {
        crate::debug::println!(
            "usb runtime descriptor: usb_dev={:#x} intf={:#x} report_len={} hid_len={} report={:02x?}",
            usb_device as usize,
            interface as usize,
            report_descriptor.len(),
            hid_descriptor.len(),
            report_descriptor
        );
    }

    crate::debug::println!(
        "usb runtime device registered: usb_dev={:#x} intf={:#x} intf_num={} report_len={} hid_len={}",
        usb_device as usize,
        interface as usize,
        interface_number,
        report_descriptor.len(),
        hid_descriptor.len(),
    );
}

// Retained for future USB hot-unplug cleanup once runtime HID detachment is wired up.
#[allow(dead_code)]
pub(crate) fn unregister_interface(
    interface: *mut crate::driver::linux::compat::LinuxCompatUsbInterface,
) {
    if interface.is_null() {
        return;
    }

    let removed = {
        let mut devices = USB_RUNTIME_DEVICES.lock();
        let Some(index) = devices
            .iter()
            .position(|entry| entry.interface_ptr == interface as usize)
        else {
            return;
        };
        devices.swap_remove(index)
    };

    for urb_ptr in removed.pending_urbs {
        let urb = urb_ptr as *mut LinuxCompatUrb;
        if !urb.is_null() {
            unsafe {
                (*urb).status = USB_URB_STATUS_UNLINKED;
                (*urb).actual_length = 0;
            }
        }
    }

    crate::debug::println!(
        "usb runtime device removed: usb_dev={:#x} intf={:#x}",
        removed.usb_device_ptr,
        removed.interface_ptr
    );
}

pub(crate) fn enqueue_report(usb_device: *mut LinuxCompatUsbDevice, report: &[u8]) {
    if usb_device.is_null() || report.is_empty() {
        return;
    }
    let Some(report) = RuntimeReport::from_slice(report) else {
        return;
    };

    let completion = {
        let mut devices = USB_RUNTIME_DEVICES.lock();
        let Some(device) = devices
            .iter_mut()
            .find(|entry| entry.usb_device_ptr == usb_device as usize)
        else {
            return;
        };

        if let Some(urb_ptr) = device.pending_urbs.pop_front() {
            completion_from_report(urb_ptr as *mut LinuxCompatUrb, report)
        } else {
            if let Some(last) = device.queued_reports.back_mut() {
                if runtime_reports_can_coalesce(*last, report, device.layout_hint.as_deref()) {
                    *last = report;
                    return;
                }
            }
            if device.queued_reports.len() >= REPORT_QUEUE_CAPACITY {
                device.dropped_reports = device.dropped_reports.saturating_add(1);
                if device
                    .dropped_reports
                    .is_multiple_of(REPORT_DROP_LOG_INTERVAL)
                {
                    crate::debug::println!(
                        "usb runtime report overload: usb_dev={:#x} dropped={} queued={}",
                        device.usb_device_ptr,
                        device.dropped_reports,
                        device.queued_reports.len()
                    );
                }
                return;
            }
            device.queued_reports.push_back(report);
            None
        }
    };

    if REPORT_ENQUEUE_LOGS.fetch_add(1, Ordering::Relaxed) < REPORT_ENQUEUE_LOG_LIMIT {
        crate::debug::println!(
            "usb runtime report queued: usb_dev={:#x} len={} first={:02x} {:02x} {:02x} {:02x}",
            usb_device as usize,
            report.len,
            report.bytes[0],
            report.bytes[1],
            report.bytes[2],
            report.bytes[3]
        );
    }

    queue_urb_completion(completion);
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
    if dev.is_null() || data.is_null() {
        return -22;
    }

    let Some(response) = control_response(dev as usize, request, request_type, value, index) else {
        return -38;
    };
    let copy_len = core::cmp::min(response.len(), size as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(response.as_ptr(), data.cast::<u8>(), copy_len);
    }
    copy_len as i32
}

pub(crate) fn interrupt_msg(
    dev: *mut LinuxCompatUsbDevice,
    data: *mut c_void,
    len: i32,
    actual_length: *mut i32,
) -> i32 {
    if dev.is_null() || data.is_null() || len < 0 {
        return -22;
    }

    let Some(report) = pop_report_for_device(dev as usize) else {
        if !actual_length.is_null() {
            unsafe {
                *actual_length = 0;
            }
        }
        return -11;
    };

    let copied = copy_report_bytes(report, data.cast::<u8>(), len as usize);
    if !actual_length.is_null() {
        unsafe {
            *actual_length = copied as i32;
        }
    }
    copied as i32
}

pub(crate) fn submit_urb(urb: *mut LinuxCompatUrb) -> i32 {
    if urb.is_null() {
        return -22;
    }

    let pipe = unsafe { (*urb).pipe };
    if usb_pipecontrol(pipe) {
        return submit_control_urb(urb);
    }
    if !usb_pipeint(pipe) || (pipe & USB_DIR_IN) == 0 {
        return 0;
    }

    let completion = {
        let dev = unsafe { (*urb).dev };
        let Some(device_ptr) = (!dev.is_null()).then_some(dev as usize) else {
            return -19;
        };
        let mut devices = USB_RUNTIME_DEVICES.lock();
        let Some(device) = devices
            .iter_mut()
            .find(|entry| entry.usb_device_ptr == device_ptr)
        else {
            return -19;
        };
        if unsafe { (*urb).ep.is_null() } {
            unsafe {
                (*urb).ep = usb_pipe_endpoint(dev, pipe).cast();
            }
        }
        if let Some(report) = device.queued_reports.pop_front() {
            completion_from_report(urb, report)
        } else {
            device.pending_urbs.push_back(urb as usize);
            None
        }
    };

    if URB_SUBMIT_LOGS.fetch_add(1, Ordering::Relaxed) < URB_SUBMIT_LOG_LIMIT {
        #[cfg(rustos_debug_print_enabled)]
        unsafe {
            crate::debug::println!(
                "usb runtime urb submit: urb={:#x} dev={:#x} pipe={:#x} len={} ep={:#x}",
                urb as usize,
                (*urb).dev as usize,
                (*urb).pipe,
                (*urb).transfer_buffer_length,
                (*urb).ep as usize
            );
        }
    }

    queue_urb_completion(completion);
    0
}

pub(crate) fn cancel_urb(urb: *mut LinuxCompatUrb) -> bool {
    if urb.is_null() {
        return false;
    }
    let urb_ptr = urb as usize;
    let mut cancelled = false;

    {
        let mut devices = USB_RUNTIME_DEVICES.lock();
        for device in devices.iter_mut() {
            let before = device.pending_urbs.len();
            device.pending_urbs.retain(|pending| *pending != urb_ptr);
            cancelled |= before != device.pending_urbs.len();
        }
    }

    {
        let mut pending = PENDING_COMPLETIONS.lock();
        let mut retained = VecDeque::with_capacity(pending.len());
        while let Some(completion) = pending.pop_front() {
            if completion.urb_ptr == urb_ptr {
                cancelled = true;
            } else {
                retained.push_back(completion);
            }
        }
        *pending = retained;
    }

    if cancelled {
        unsafe {
            (*urb).status = USB_URB_STATUS_UNLINKED;
            (*urb).actual_length = 0;
        }
    }
    cancelled
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub(crate) fn hid_input_report(
    dev: *mut LinuxCompatHidDevice,
    data: *mut u8,
    size: u32,
) -> Option<i32> {
    if dev.is_null() || data.is_null() {
        return Some(-22);
    }

    let report = unsafe { core::slice::from_raw_parts(data, size as usize) };
    let layout = match classify_hid_layout(dev, report.len()) {
        Some(layout) => layout,
        None => {
            if HID_REPORT_ENTRY_LOGS.fetch_add(1, Ordering::Relaxed) < HID_REPORT_ENTRY_LOG_LIMIT {
                crate::debug::println!(
                    "usb hid report layout missing: dev={:#x} size={} first={:02x} {:02x} {:02x} {:02x}",
                    dev as usize,
                    report.len(),
                    report.first().copied().unwrap_or(0),
                    report.get(1).copied().unwrap_or(0),
                    report.get(2).copied().unwrap_or(0),
                    report.get(3).copied().unwrap_or(0),
                );
            }
            return None;
        }
    };

    if HID_REPORT_ENTRY_LOGS.fetch_add(1, Ordering::Relaxed) < HID_REPORT_ENTRY_LOG_LIMIT {
        let (kind, report_id, required_bytes) = hid_layout_summary(layout.as_ref());
        crate::debug::println!(
            "usb hid input report: dev={:#x} size={} kind={} report_id={} required={} first={:02x} {:02x} {:02x} {:02x}",
            dev as usize,
            report.len(),
            kind,
            report_id,
            required_bytes,
            report.first().copied().unwrap_or(0),
            report.get(1).copied().unwrap_or(0),
            report.get(2).copied().unwrap_or(0),
            report.get(3).copied().unwrap_or(0),
        );
    }

    let status = match layout.as_ref() {
        HidReportLayout::BootKeyboard(layout) => {
            handle_keyboard_report(dev as usize, report, layout)
        }
        HidReportLayout::Pointer(layout) => handle_pointer_report(dev as usize, report, layout),
    };
    Some(status)
}

pub(crate) fn hid_remove_device(dev: *mut LinuxCompatHidDevice) {
    if dev.is_null() {
        return;
    }
    let dev_ptr = dev as usize;
    let mut states = HID_REPORT_STATES.lock();
    if let Some(index) = states
        .iter()
        .position(|state| state.hid_device_ptr == dev_ptr)
    {
        states.remove(index);
    }
}

impl RuntimeReport {
    fn from_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > MAX_REPORT_BYTES {
            return None;
        }
        let mut report = RuntimeReport {
            len: bytes.len(),
            bytes: [0; MAX_REPORT_BYTES],
        };
        report.bytes[..bytes.len()].copy_from_slice(bytes);
        Some(report)
    }
}

fn submit_control_urb(urb: *mut LinuxCompatUrb) -> i32 {
    let setup = unsafe { (*urb).setup_packet };
    if setup.is_null() {
        return -22;
    }
    let setup = unsafe { core::slice::from_raw_parts(setup, 8) };
    let request = setup[1];
    let request_type = setup[0];
    let value = u16::from_le_bytes([setup[2], setup[3]]);
    let index = u16::from_le_bytes([setup[4], setup[5]]);
    let size = u16::from_le_bytes([setup[6], setup[7]]);
    let status = control_msg(
        unsafe { (*urb).dev },
        request,
        request_type,
        value,
        index,
        unsafe { (*urb).transfer_buffer },
        size,
    );
    if status < 0 {
        return status;
    }
    unsafe {
        (*urb).status = 0;
        (*urb).actual_length = status as u32;
    }
    let completion = unsafe {
        (*urb).complete.map(|callback| UrbCompletion {
            urb_ptr: urb as usize,
            callback,
        })
    };
    dispatch_urb_completion(completion);
    0
}

fn control_response(
    usb_device_ptr: usize,
    request: u8,
    request_type: u8,
    value: u16,
    index: u16,
) -> Option<Vec<u8>> {
    let devices = USB_RUNTIME_DEVICES.lock();
    let device = devices
        .iter()
        .find(|entry| entry.usb_device_ptr == usb_device_ptr)?;

    match request {
        USB_REQ_GET_DESCRIPTOR => match (value >> 8) as u8 {
            USB_DT_DEVICE => Some(device.device_descriptor.to_vec()),
            USB_DT_HID => Some(device.hid_descriptor.to_vec()),
            USB_DT_REPORT => Some(device.report_descriptor.to_vec()),
            _ => None,
        },
        USB_REQ_SET_CONFIGURATION if request_type == 0x00 => Some(Vec::new()),
        USB_REQ_GET_INTERFACE if request_type == 0x81 && index as u8 == device.interface_number => {
            Some(vec![0])
        }
        USB_REQ_SET_INTERFACE if request_type == 0x01 && index as u8 == device.interface_number => {
            Some(Vec::new())
        }
        USB_REQ_GET_PROTOCOL if request_type == 0xA1 => Some(vec![0]),
        USB_REQ_SET_PROTOCOL if request_type == 0x21 => Some(Vec::new()),
        USB_REQ_GET_IDLE if request_type == 0xA1 => Some(vec![0]),
        USB_REQ_SET_IDLE if request_type == 0x21 => Some(Vec::new()),
        _ => None,
    }
}

fn pop_report_for_device(usb_device_ptr: usize) -> Option<RuntimeReport> {
    let mut devices = USB_RUNTIME_DEVICES.lock();
    let device = devices
        .iter_mut()
        .find(|entry| entry.usb_device_ptr == usb_device_ptr)?;
    device.queued_reports.pop_front()
}

fn completion_from_report(
    urb: *mut LinuxCompatUrb,
    report: RuntimeReport,
) -> Option<UrbCompletion> {
    if urb.is_null() {
        return None;
    }
    let copied = copy_report_bytes(
        report,
        unsafe { (*urb).transfer_buffer }.cast::<u8>(),
        unsafe { (*urb).transfer_buffer_length as usize },
    );
    unsafe {
        (*urb).status = 0;
        (*urb).actual_length = copied as u32;
    }
    unsafe {
        (*urb).complete.map(|callback| UrbCompletion {
            urb_ptr: urb as usize,
            callback,
        })
    }
}

fn copy_report_bytes(report: RuntimeReport, dest: *mut u8, max_len: usize) -> usize {
    if dest.is_null() {
        return 0;
    }
    let copied = core::cmp::min(report.len, max_len);
    unsafe {
        core::ptr::copy_nonoverlapping(report.bytes.as_ptr(), dest, copied);
    }
    copied
}

fn queue_urb_completion(completion: Option<UrbCompletion>) {
    let Some(completion) = completion else {
        return;
    };
    PENDING_COMPLETIONS.lock().push_back(completion);
}

fn dispatch_urb_completion(completion: Option<UrbCompletion>) {
    let Some(completion) = completion else {
        return;
    };
    unsafe {
        (completion.callback)(completion.urb_ptr as *mut LinuxCompatUrb);
    }
}

fn runtime_reports_can_coalesce(
    existing: RuntimeReport,
    incoming: RuntimeReport,
    layout: Option<&HidReportLayout>,
) -> bool {
    let Some(HidReportLayout::Pointer(pointer)) = layout else {
        return false;
    };
    if pointer.relative {
        return false;
    }
    let Some(existing) = pointer_report_signature(pointer, existing) else {
        return false;
    };
    let Some(incoming) = pointer_report_signature(pointer, incoming) else {
        return false;
    };
    existing == incoming
}

fn pointer_report_signature(layout: &PointerLayout, report: RuntimeReport) -> Option<(u8, i32)> {
    let bytes = &report.bytes[..report.len];
    if bytes.len() < layout.required_bytes
        || (layout.report_id != 0 && bytes.first().copied() != Some(layout.report_id))
    {
        return None;
    }
    let buttons = extract_pointer_buttons(layout, bytes);
    let wheel = layout
        .wheel_field
        .as_ref()
        .and_then(|field| extract_value_field_i32(field, bytes))
        .unwrap_or(0);
    Some((buttons, wheel))
}

fn classify_hid_layout(
    dev: *mut LinuxCompatHidDevice,
    report_len: usize,
) -> Option<Arc<HidReportLayout>> {
    let dev_ptr = dev as usize;
    {
        let states = HID_REPORT_STATES.lock();
        if let Some(state) = states.iter().find(|state| state.hid_device_ptr == dev_ptr) {
            if let Some(layout) = state.layout.clone() {
                return Some(layout);
            }
        }
    }

    let layout = hid_report_descriptor(dev)
        .and_then(parse_hid_layout)
        .map(Arc::new)
        .or_else(|| classify_runtime_hid_layout(dev, report_len))?;
    let mut states = HID_REPORT_STATES.lock();
    if let Some(index) = states
        .iter()
        .position(|state| state.hid_device_ptr == dev_ptr)
    {
        states[index].layout = Some(layout.clone());
        return Some(layout);
    }
    states.push(HidReportState {
        hid_device_ptr: dev_ptr,
        layout: Some(layout.clone()),
        ..HidReportState::default()
    });
    Some(layout)
}

fn hid_report_descriptor(dev: *mut LinuxCompatHidDevice) -> Option<&'static [u8]> {
    let (ptr, size) = unsafe {
        let ptr = if !(*dev).rdesc.is_null() {
            (*dev).rdesc
        } else {
            (*dev).dev_rdesc
        };
        let size = if (*dev).rsize != 0 {
            (*dev).rsize
        } else {
            (*dev).dev_rsize
        };
        (ptr, size)
    };
    if ptr.is_null() || size == 0 {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(ptr, size as usize) })
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn classify_runtime_hid_layout(
    dev: *mut LinuxCompatHidDevice,
    report_len: usize,
) -> Option<Arc<HidReportLayout>> {
    if dev.is_null() {
        return None;
    }

    let (vendor, product, bus) =
        unsafe { ((*dev).vendor as u16, (*dev).product as u16, (*dev).bus) };
    let devices = USB_RUNTIME_DEVICES.lock();
    for device in devices.iter() {
        let device_vendor =
            u16::from_le_bytes([device.device_descriptor[8], device.device_descriptor[9]]);
        let device_product =
            u16::from_le_bytes([device.device_descriptor[10], device.device_descriptor[11]]);
        if device_vendor != vendor || device_product != product {
            continue;
        }

        let Some(layout) = device.layout_hint.as_ref() else {
            continue;
        };
        if !layout_accepts_report_len(layout.as_ref(), report_len) {
            continue;
        }
        if bus == 0x03 {
            let (kind, report_id, required_bytes) = hid_layout_summary(layout.as_ref());
            crate::debug::println!(
                "usb hid layout fallback: hid_dev={:#x} vendor={:04x} product={:04x} len={} kind={} report_id={} required={}",
                dev as usize,
                vendor,
                product,
                report_len,
                kind,
                report_id,
                required_bytes
            );
            return Some(layout.clone());
        }
    }
    None
}

fn parse_hid_layout(descriptor: &[u8]) -> Option<HidReportLayout> {
    let parsed = ReportDescriptor::try_from(descriptor).ok()?;

    for report in parsed.input_reports().iter() {
        if let Some(layout) = parse_boot_keyboard_layout(report) {
            return Some(HidReportLayout::BootKeyboard(layout));
        }
    }

    for report in parsed.input_reports().iter() {
        if let Some(layout) = parse_pointer_layout(report) {
            return Some(HidReportLayout::Pointer(layout));
        }
    }

    None
}

fn layout_accepts_report_len(layout: &HidReportLayout, report_len: usize) -> bool {
    report_len >= layout_required_bytes(layout)
}

fn layout_required_bytes(layout: &HidReportLayout) -> usize {
    let required_bits = match layout {
        HidReportLayout::BootKeyboard(layout) => layout.required_bytes,
        HidReportLayout::Pointer(layout) => layout.required_bytes,
    };
    required_bits
}

fn parse_boot_keyboard_layout(report: &impl Report) -> Option<BootKeyboardLayout> {
    let mut modifier_fields = core::array::from_fn(|_| None);
    let mut key_array = None;

    for field in report.fields() {
        match field {
            Field::Variable(field)
                if u16::from(field.usage.usage_page) == 0x07
                    && (0xE0..=0xE7).contains(&u16::from(field.usage.usage_id)) =>
            {
                let index = (u16::from(field.usage.usage_id) - 0xE0) as usize;
                modifier_fields[index] = Some(HidValueField {
                    bits: field.bits.clone(),
                    signed: field.is_signed(),
                    relative: field.is_relative(),
                    logical_minimum: i32::from(field.logical_minimum),
                    logical_maximum: i32::from(field.logical_maximum),
                });
            }
            Field::Array(field) if is_keyboard_usage_array(field) => {
                key_array = Some(HidArrayField {
                    bits: field.bits.clone(),
                    count: usize::from(field.report_count),
                    signed: field.is_signed(),
                });
            }
            _ => {}
        }
    }

    if modifier_fields.iter().any(|field| field.is_none()) {
        return None;
    }

    Some(BootKeyboardLayout {
        report_id: report_id(report),
        required_bytes: report.size_in_bytes(),
        modifier_fields,
        key_array: key_array?,
    })
}

fn parse_pointer_layout(report: &impl Report) -> Option<PointerLayout> {
    let mut button_fields = Vec::new();
    let mut x_field = None;
    let mut y_field = None;
    let mut wheel_field = None;

    for field in report.fields() {
        match field {
            Field::Variable(field) if u16::from(field.usage.usage_page) == 0x09 => {
                if button_fields.len() < 8 {
                    button_fields.push(HidValueField {
                        bits: field.bits.clone(),
                        signed: field.is_signed(),
                        relative: field.is_relative(),
                        logical_minimum: i32::from(field.logical_minimum),
                        logical_maximum: i32::from(field.logical_maximum),
                    });
                }
            }
            Field::Variable(field)
                if is_usage(
                    u16::from(field.usage.usage_page),
                    u16::from(field.usage.usage_id),
                    0x01,
                    0x30,
                ) =>
            {
                x_field = Some(HidValueField {
                    bits: field.bits.clone(),
                    signed: field.is_signed(),
                    relative: field.is_relative(),
                    logical_minimum: i32::from(field.logical_minimum),
                    logical_maximum: i32::from(field.logical_maximum),
                });
            }
            Field::Variable(field)
                if is_usage(
                    u16::from(field.usage.usage_page),
                    u16::from(field.usage.usage_id),
                    0x01,
                    0x31,
                ) =>
            {
                y_field = Some(HidValueField {
                    bits: field.bits.clone(),
                    signed: field.is_signed(),
                    relative: field.is_relative(),
                    logical_minimum: i32::from(field.logical_minimum),
                    logical_maximum: i32::from(field.logical_maximum),
                });
            }
            Field::Variable(field)
                if is_usage(
                    u16::from(field.usage.usage_page),
                    u16::from(field.usage.usage_id),
                    0x01,
                    0x38,
                ) =>
            {
                wheel_field = Some(HidValueField {
                    bits: field.bits.clone(),
                    signed: field.is_signed(),
                    relative: field.is_relative(),
                    logical_minimum: i32::from(field.logical_minimum),
                    logical_maximum: i32::from(field.logical_maximum),
                });
            }
            _ => {}
        }
    }

    let x_field = x_field?;
    let y_field = y_field?;

    Some(PointerLayout {
        report_id: report_id(report),
        required_bytes: report.size_in_bytes(),
        button_fields,
        relative: x_field.relative || y_field.relative,
        logical_min_x: x_field.logical_minimum,
        logical_max_x: x_field.logical_maximum,
        logical_min_y: y_field.logical_minimum,
        logical_max_y: y_field.logical_maximum,
        x_field,
        y_field,
        wheel_field,
    })
}

fn report_id(report: &impl Report) -> u8 {
    report.report_id().as_ref().map(u8::from).unwrap_or(0)
}

fn is_keyboard_usage_array(field: &ArrayField) -> bool {
    if field.bits.len() < 8 || field.bits.len() % 8 != 0 {
        return false;
    }
    if let Some(range) = field.usage_range() {
        return u16::from(range.minimum().usage_page()) == 0x07
            && u16::from(range.maximum().usage_page()) == 0x07;
    }
    field
        .usages()
        .iter()
        .any(|usage| u16::from(usage.usage_page) == 0x07)
}

fn is_usage(page: u16, id: u16, expected_page: u16, expected_id: u16) -> bool {
    page == expected_page && id == expected_id
}

fn handle_keyboard_report(
    hid_device_ptr: usize,
    report: &[u8],
    layout: &BootKeyboardLayout,
) -> i32 {
    if report.len() < layout.required_bytes
        || (layout.report_id != 0 && report.first().copied() != Some(layout.report_id))
    {
        return 0;
    }
    let mut modifiers = 0u8;
    for (bit, field) in layout.modifier_fields.iter().enumerate() {
        if field
            .as_ref()
            .and_then(|field| extract_value_field_u32(field, report))
            .unwrap_or(0)
            != 0
        {
            modifiers |= 1 << bit;
        }
    }

    let mut keys = [0u8; 16];
    let mut key_count = 0usize;
    for index in 0..layout.key_array.count {
        let Some(usage) = extract_array_field_u32(&layout.key_array, report, index) else {
            continue;
        };
        let usage = usage as u8;
        if usage != 0 {
            keys[key_count] = usage;
            key_count += 1;
            if key_count >= keys.len() {
                break;
            }
        }
    }

    let (previous_modifiers, previous_keys, previous_key_count) = {
        let mut states = HID_REPORT_STATES.lock();
        let state = ensure_hid_state(&mut states, hid_device_ptr);
        let previous_modifiers = state.last_modifiers;
        let previous_keys = state.last_keys;
        let previous_key_count = state.last_key_count;
        state.last_modifiers = modifiers;
        state.last_keys = keys;
        state.last_key_count = key_count;
        (previous_modifiers, previous_keys, previous_key_count)
    };

    with_injection(|| {
        for usage in 0xE0u8..=0xE7 {
            let mask = 1u8 << (usage - 0xE0);
            if (previous_modifiers & mask) == (modifiers & mask) {
                continue;
            }
            if let Some(code) = hid_usage_to_keycode(usage) {
                crate::input::keyboard::inject_key_transition(code, (modifiers & mask) == 0);
            }
        }

        for usage in previous_keys[..previous_key_count]
            .iter()
            .copied()
            .filter(|usage| !keys[..key_count].contains(usage))
        {
            if let Some(code) = hid_usage_to_keycode(usage) {
                crate::input::keyboard::inject_key_transition(code, true);
            }
        }

        for usage in keys[..key_count]
            .iter()
            .copied()
            .filter(|usage| !previous_keys[..previous_key_count].contains(usage))
        {
            if let Some(code) = hid_usage_to_keycode(usage) {
                crate::input::keyboard::inject_key_transition(code, false);
            }
        }
    });

    if KEYBOARD_TRANSLATION_LOGS.fetch_add(1, Ordering::Relaxed) < KEYBOARD_TRANSLATION_LOG_LIMIT {
        crate::debug::println!(
            "usb hid keyboard report: dev={:#x} report_id={} modifiers={:#x} keys={:02x},{:02x},{:02x},{:02x},{:02x},{:02x}",
            hid_device_ptr,
            layout.report_id,
            modifiers,
            keys[0],
            keys[1],
            keys[2],
            keys[3],
            keys[4],
            keys[5]
        );
    }

    0
}

fn handle_pointer_report(hid_device_ptr: usize, report: &[u8], layout: &PointerLayout) -> i32 {
    if report.len() < layout.required_bytes
        || (layout.report_id != 0 && report.first().copied() != Some(layout.report_id))
    {
        return 0;
    }
    let button_bits = extract_pointer_buttons(&layout, report);
    let x = extract_value_field_i32(&layout.x_field, report).unwrap_or(0);
    let y = extract_value_field_i32(&layout.y_field, report).unwrap_or(0);
    let wheel = layout
        .wheel_field
        .as_ref()
        .and_then(|field| extract_value_field_i32(field, report))
        .unwrap_or(0);

    if layout.relative {
        let packet = {
            let mut states = HID_REPORT_STATES.lock();
            let state = ensure_hid_state(&mut states, hid_device_ptr);
            state.last_pointer_buttons = button_bits;
            PointerPacket {
                buttons: pointer_buttons_from_report(button_bits),
                dx: x as i16,
                dy: y as i16,
                wheel_vertical: wheel as i16,
                wheel_horizontal: 0,
                reserved0: 0,
                reserved1: 0,
                reserved2: 0,
            }
        };

        with_injection(|| {
            crate::driver::input::submit_pointer_packet(packet);
        });

        if (packet.dx != 0 || packet.dy != 0 || packet.wheel_vertical != 0 || packet.buttons != 0)
            && POINTER_TRANSLATION_LOGS.fetch_add(1, Ordering::Relaxed)
                < POINTER_TRANSLATION_LOG_LIMIT
        {
            crate::debug::println!(
                "usb hid pointer report: dev={:#x} dx={} dy={} wheel={} buttons={:#x} relative={}",
                hid_device_ptr,
                packet.dx,
                packet.dy,
                packet.wheel_vertical,
                packet.buttons,
                layout.relative
            );
        }

        return 0;
    }

    let (target_x, target_y, buttons) = {
        let mut states = HID_REPORT_STATES.lock();
        let state = ensure_hid_state(&mut states, hid_device_ptr);
        let display = crate::io::gui::display_info();
        let target_x = scale_absolute_coordinate(
            x,
            layout.logical_min_x,
            layout.logical_max_x,
            display
                .map(|info| info.width.saturating_sub(1) as i32)
                .unwrap_or(0),
        );
        let target_y = scale_absolute_coordinate(
            y,
            layout.logical_min_y,
            layout.logical_max_y,
            display
                .map(|info| info.height.saturating_sub(1) as i32)
                .unwrap_or(0),
        );
        state.have_pointer_origin = true;
        state.last_pointer_x = target_x;
        state.last_pointer_y = target_y;
        state.last_pointer_buttons = button_bits;
        (target_x, target_y, pointer_buttons_from_report(button_bits))
    };

    with_injection(|| {
        crate::driver::input::submit_pointer_absolute(
            target_x.max(0) as u32,
            target_y.max(0) as u32,
            buttons,
            wheel as i16,
        );
    });

    if POINTER_TRANSLATION_LOGS.fetch_add(1, Ordering::Relaxed) < POINTER_TRANSLATION_LOG_LIMIT {
        crate::debug::println!(
            "usb hid pointer report: dev={:#x} abs=({}, {}) wheel={} buttons={:#x} relative={}",
            hid_device_ptr,
            target_x,
            target_y,
            wheel,
            buttons,
            layout.relative
        );
    }

    0
}

fn ensure_hid_state(
    states: &mut Vec<HidReportState>,
    hid_device_ptr: usize,
) -> &mut HidReportState {
    if let Some(index) = states
        .iter()
        .position(|state| state.hid_device_ptr == hid_device_ptr)
    {
        return &mut states[index];
    }

    states.push(HidReportState {
        hid_device_ptr,
        ..HidReportState::default()
    });
    states.last_mut().expect("hid report state just inserted")
}

fn hid_layout_summary(layout: &HidReportLayout) -> (&'static str, u8, usize) {
    match layout {
        HidReportLayout::BootKeyboard(layout) => {
            ("keyboard", layout.report_id, layout.required_bytes)
        }
        HidReportLayout::Pointer(layout) => ("pointer", layout.report_id, layout.required_bytes),
    }
}

fn scale_absolute_coordinate(
    value: i32,
    logical_min: i32,
    logical_max: i32,
    target_max: i32,
) -> i32 {
    if target_max <= 0 || logical_max <= logical_min {
        return 0;
    }
    let clamped = value.clamp(logical_min, logical_max);
    let numer = (clamped - logical_min) as i64 * target_max as i64;
    let denom = (logical_max - logical_min) as i64;
    (numer / denom) as i32
}

fn extract_pointer_buttons(layout: &PointerLayout, report: &[u8]) -> u8 {
    layout
        .button_fields
        .iter()
        .take(8)
        .enumerate()
        .fold(0u8, |acc, (bit, field)| {
            let value = extract_value_field_u32(field, report).unwrap_or(0);
            acc | (((value != 0) as u8) << bit)
        })
}

fn extract_value_field_u32(field: &HidValueField, report: &[u8]) -> Option<u32> {
    let raw = extract_bits_u32(report, &field.bits)?;
    if field.signed {
        Some(sign_extend_u32(raw, field.bits.len()) as u32)
    } else {
        Some(raw)
    }
}

fn extract_value_field_i32(field: &HidValueField, report: &[u8]) -> Option<i32> {
    let raw = extract_bits_u32(report, &field.bits)?;
    Some(if field.signed {
        sign_extend_u32(raw, field.bits.len())
    } else {
        raw as i32
    })
}

fn extract_array_field_u32(field: &HidArrayField, report: &[u8], index: usize) -> Option<u32> {
    if index >= field.count {
        return None;
    }
    let bits_per_value = field.bits.len().checked_div(field.count)?;
    if bits_per_value == 0 || bits_per_value > 32 {
        return None;
    }
    let start = field
        .bits
        .start
        .checked_add(bits_per_value.checked_mul(index)?)?;
    let end = start.checked_add(bits_per_value)?;
    let raw = extract_bits_u32(report, &(start..end))?;
    if field.signed {
        Some(sign_extend_u32(raw, bits_per_value) as u32)
    } else {
        Some(raw)
    }
}

fn extract_bits_u32(report: &[u8], bits: &Range<usize>) -> Option<u32> {
    let bit_len = bits.len();
    if bit_len == 0 || bit_len > 32 {
        return None;
    }
    let start_byte = bits.start / 8;
    let end_byte = (bits.end.checked_sub(1)?) / 8;
    let bytes = report.get(start_byte..=end_byte)?;
    let mut value = 0u64;
    for (index, byte) in bytes.iter().enumerate() {
        value |= u64::from(*byte) << (index * 8);
    }
    let shifted = (value >> (bits.start % 8)) as u32;
    let mask = if bit_len == 32 {
        u32::MAX
    } else {
        (1u32 << bit_len) - 1
    };
    Some(shifted & mask)
}

fn sign_extend_u32(value: u32, bit_len: usize) -> i32 {
    debug_assert!(bit_len > 0 && bit_len <= 32);
    if bit_len == 32 {
        return value as i32;
    }
    let sign_bit = 1u32 << (bit_len - 1);
    if (value & sign_bit) == 0 {
        value as i32
    } else {
        (value | (!0u32 << bit_len)) as i32
    }
}

fn usb_pipeint(pipe: u32) -> bool {
    ((pipe >> 30) & 3) == PIPE_INTERRUPT
}

fn usb_pipecontrol(pipe: u32) -> bool {
    ((pipe >> 30) & 3) == PIPE_CONTROL
}

fn usb_pipe_endpoint(dev: *mut LinuxCompatUsbDevice, pipe: u32) -> *mut c_void {
    if dev.is_null() {
        return core::ptr::null_mut();
    }
    let endpoint = ((pipe >> 15) & 0xf) as usize;
    unsafe {
        if (pipe & USB_DIR_IN) != 0 {
            (*dev)
                .ep_in
                .get(endpoint)
                .copied()
                .unwrap_or(core::ptr::null_mut()) as *mut c_void
        } else {
            (*dev)
                .ep_out
                .get(endpoint)
                .copied()
                .unwrap_or(core::ptr::null_mut()) as *mut c_void
        }
    }
}

fn with_injection(f: impl FnOnce()) {
    super::synthetic::begin_injection();
    f();
    super::synthetic::end_injection();
}

#[cfg(test)]
mod tests {
    use super::{parse_hid_layout, HidReportLayout};

    #[test]
    fn parses_qemu_usb_keyboard_layout() {
        let descriptor = [
            0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x75, 0x01, 0x95, 0x08, 0x05, 0x07, 0x19, 0xe0,
            0x29, 0xe7, 0x15, 0x00, 0x25, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
            0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01,
            0x75, 0x03, 0x91, 0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0xff, 0x05, 0x07,
            0x19, 0x00, 0x29, 0xff, 0x81, 0x00, 0xc0,
        ];
        let layout = parse_hid_layout(&descriptor);
        assert!(matches!(layout, Some(HidReportLayout::BootKeyboard(_))));
    }

    #[test]
    fn parses_qemu_usb_tablet_layout() {
        let descriptor = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01,
            0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01,
            0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff,
            0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01,
            0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01,
            0x81, 0x06, 0xc0, 0xc0,
        ];
        let layout = parse_hid_layout(&descriptor);
        assert!(matches!(layout, Some(HidReportLayout::Pointer(_))));
    }
}
