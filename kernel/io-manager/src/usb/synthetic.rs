use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

use driver_abi::PointerPacket;
use heapless::Deque as HeaplessDeque;
use spin::Mutex;

use super::hid_translation::{
    clamp_i8, hid_modifier_mask, hid_usage_to_keycode, keycode_to_hid_usage, mouse_buttons,
    pointer_buttons_from_report,
};
use crate::driver::linux::compat::{LinuxCompatHidDevice, LinuxCompatUrb, LinuxCompatUsbDevice};
use crate::input::keyboard::{KeyAction, KeyboardEvent};

const KEYBOARD_REPORT_LEN: usize = 8;
const POINTER_REPORT_LEN: usize = 4;
const MAX_REPORT_LEN: usize = KEYBOARD_REPORT_LEN;
const REPORT_QUEUE_CAPACITY: usize = 256;
const REPORT_DROP_LOG_INTERVAL: u64 = 128;
const COMPLETIONS_PER_SERVICE: usize = 128;
const PENDING_COMPLETION_CAPACITY: usize = 64;
const USB_URB_STATUS_UNLINKED: i32 = -2;
const USB_URB_STATUS_IO_ERROR: i32 = -5;

const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
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

const PRODUCT_ID_KEYBOARD: u16 = 0x0001;
const PRODUCT_ID_POINTER: u16 = 0x0002;
const KEYBOARD_TRANSLATION_LOG_LIMIT: usize = 0;
const POINTER_TRANSLATION_LOG_LIMIT: usize = 0;

const KEYBOARD_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

const KEYBOARD_HID_DESCRIPTOR: [u8; 9] = [
    9,
    USB_DT_HID,
    0x11,
    0x01,
    0,
    1,
    USB_DT_REPORT,
    KEYBOARD_REPORT_DESCRIPTOR.len() as u8,
    (KEYBOARD_REPORT_DESCRIPTOR.len() >> 8) as u8,
];

const POINTER_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03,
    0x81, 0x06, 0xC0, 0xC0,
];

const POINTER_HID_DESCRIPTOR: [u8; 9] = [
    9,
    USB_DT_HID,
    0x11,
    0x01,
    0,
    1,
    USB_DT_REPORT,
    POINTER_REPORT_DESCRIPTOR.len() as u8,
    (POINTER_REPORT_DESCRIPTOR.len() >> 8) as u8,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyntheticHidKind {
    Keyboard,
    Pointer,
}

#[derive(Clone, Copy)]
// Retained as a full synthetic-HID descriptor bundle for future transport-backed injection.
#[allow(dead_code)]
pub(crate) struct SyntheticHidDescriptors {
    pub(crate) endpoint_address: u8,
    pub(crate) max_packet_size: u16,
    pub(crate) interval: u8,
    pub(crate) hid_descriptor: [u8; 9],
    pub(crate) report_descriptor: &'static [u8],
}

#[derive(Clone, Copy)]
struct SyntheticReport {
    bytes: [u8; KEYBOARD_REPORT_LEN],
    len: usize,
}

#[derive(Clone, Copy, Default)]
struct KeyboardReportState {
    modifiers: u8,
    keys: [u8; 6],
}

struct SyntheticHidDevice {
    usb_device_ptr: usize,
    interface_ptr: usize,
    kind: SyntheticHidKind,
    descriptors: SyntheticHidDescriptors,
    pending_urb: Option<usize>,
    bootstrap_urb_completed: bool,
    queued_reports: HeaplessDeque<SyntheticReport, REPORT_QUEUE_CAPACITY>,
    keyboard_state: KeyboardReportState,
    dropped_reports: u64,
}

#[derive(Clone, Copy, Default)]
struct HidReportState {
    hid_device_ptr: usize,
    kind: Option<SyntheticHidKind>,
    last_keyboard_report: [u8; KEYBOARD_REPORT_LEN],
    last_pointer_buttons: u8,
}

struct UrbCompletion {
    urb_ptr: usize,
    callback: unsafe extern "C" fn(*mut LinuxCompatUrb),
}

static SYNTHETIC_DEVICES: Mutex<Vec<SyntheticHidDevice>> = Mutex::new(Vec::new());
static HID_REPORT_STATES: Mutex<Vec<HidReportState>> = Mutex::new(Vec::new());
static PENDING_COMPLETIONS: Mutex<HeaplessDeque<UrbCompletion, PENDING_COMPLETION_CAPACITY>> =
    Mutex::new(HeaplessDeque::new());
static INJECTION_DEPTH: AtomicUsize = AtomicUsize::new(0);
static KEYBOARD_TRANSLATION_LOGS: AtomicUsize = AtomicUsize::new(0);
static POINTER_TRANSLATION_LOGS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn begin_injection() {
    INJECTION_DEPTH.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn end_injection() {
    INJECTION_DEPTH.fetch_sub(1, Ordering::SeqCst);
}

pub(crate) fn descriptors_for_kind(kind: SyntheticHidKind) -> SyntheticHidDescriptors {
    let report_descriptor = match kind {
        SyntheticHidKind::Keyboard => KEYBOARD_REPORT_DESCRIPTOR,
        SyntheticHidKind::Pointer => POINTER_REPORT_DESCRIPTOR,
    };
    let max_packet_size = match kind {
        SyntheticHidKind::Keyboard => KEYBOARD_REPORT_LEN as u16,
        SyntheticHidKind::Pointer => POINTER_REPORT_LEN as u16,
    };
    SyntheticHidDescriptors {
        endpoint_address: 0x81,
        max_packet_size,
        interval: 8,
        hid_descriptor: match kind {
            SyntheticHidKind::Keyboard => KEYBOARD_HID_DESCRIPTOR,
            SyntheticHidKind::Pointer => POINTER_HID_DESCRIPTOR,
        },
        report_descriptor,
    }
}

pub(crate) fn register_device(
    usb_device: *mut LinuxCompatUsbDevice,
    interface: *mut crate::driver::linux::compat::LinuxCompatUsbInterface,
    kind: SyntheticHidKind,
) {
    if usb_device.is_null() || interface.is_null() {
        return;
    }
    let descriptors = descriptors_for_kind(kind);
    let mut devices = SYNTHETIC_DEVICES.lock();
    if devices
        .iter()
        .any(|device| device.interface_ptr == interface as usize)
    {
        return;
    }
    devices.push(SyntheticHidDevice {
        usb_device_ptr: usb_device as usize,
        interface_ptr: interface as usize,
        kind,
        descriptors,
        pending_urb: None,
        bootstrap_urb_completed: false,
        queued_reports: HeaplessDeque::new(),
        keyboard_state: KeyboardReportState::default(),
        dropped_reports: 0,
    });
    crate::debug::println!(
        "usb synthetic hid registered: kind={:?} usb_dev={:#x} intf={:#x}",
        kind,
        usb_device as usize,
        interface as usize,
    );
}

pub(crate) fn unregister_interface(
    interface: *mut crate::driver::linux::compat::LinuxCompatUsbInterface,
) {
    if interface.is_null() {
        return;
    }

    let removed = {
        let mut devices = SYNTHETIC_DEVICES.lock();
        let Some(index) = devices
            .iter()
            .position(|device| device.interface_ptr == interface as usize)
        else {
            return;
        };
        devices.swap_remove(index)
    };

    if let Some(urb_ptr) = removed.pending_urb {
        let urb = urb_ptr as *mut LinuxCompatUrb;
        if !urb.is_null() {
            unsafe {
                (*urb).status = USB_URB_STATUS_UNLINKED;
                (*urb).actual_length = 0;
            }
        }
    }

    crate::debug::println!(
        "usb synthetic hid removed: kind={:?} usb_dev={:#x} intf={:#x}",
        removed.kind,
        removed.usb_device_ptr,
        removed.interface_ptr,
    );
}

pub(crate) fn capture_keyboard_event(event: KeyboardEvent) -> bool {
    if INJECTION_DEPTH.load(Ordering::Relaxed) != 0 || event.action == KeyAction::Repeated {
        return false;
    }

    let Some(usage) = keycode_to_hid_usage(event.code) else {
        return false;
    };

    let completion = {
        let mut devices = SYNTHETIC_DEVICES.lock();
        let Some(device) = devices
            .iter_mut()
            .find(|device| device.kind == SyntheticHidKind::Keyboard)
        else {
            return false;
        };
        update_keyboard_report(&mut device.keyboard_state, usage, event.action);
        let report = keyboard_report(device.keyboard_state);
        enqueue_report_locked(device, report)
    };
    queue_urb_completion(completion);
    true
}

pub(crate) fn capture_pointer_packet(packet: PointerPacket) -> bool {
    if INJECTION_DEPTH.load(Ordering::Relaxed) != 0 {
        return false;
    }

    let completions = {
        let mut devices = SYNTHETIC_DEVICES.lock();
        let Some(device) = devices
            .iter_mut()
            .find(|device| device.kind == SyntheticHidKind::Pointer)
        else {
            return false;
        };

        let mut completions = Vec::new();
        let mut remaining_dx = packet.dx as i32;
        let mut remaining_dy = packet.dy as i32;
        let mut remaining_wheel = packet.wheel_vertical as i32;
        let buttons = mouse_buttons(packet.buttons);

        while remaining_dx != 0 || remaining_dy != 0 || remaining_wheel != 0 {
            let step_dx = clamp_i8(remaining_dx);
            let step_dy = clamp_i8(remaining_dy);
            let step_wheel = clamp_i8(remaining_wheel);
            remaining_dx -= step_dx as i32;
            remaining_dy -= step_dy as i32;
            remaining_wheel -= step_wheel as i32;
            if let Some(completion) = enqueue_report_locked(
                device,
                SyntheticReport {
                    bytes: [
                        buttons,
                        step_dx as u8,
                        step_dy as u8,
                        step_wheel as u8,
                        0,
                        0,
                        0,
                        0,
                    ],
                    len: POINTER_REPORT_LEN,
                },
            ) {
                completions.push(completion);
            }
        }

        if completions.is_empty() && buttons != 0 {
            if let Some(completion) = enqueue_report_locked(
                device,
                SyntheticReport {
                    bytes: [buttons, 0, 0, 0, 0, 0, 0, 0],
                    len: POINTER_REPORT_LEN,
                },
            ) {
                completions.push(completion);
            }
        }

        completions
    };

    queue_urb_completions(completions);

    packet.dx != 0
        || packet.dy != 0
        || packet.wheel_vertical != 0
        || packet.wheel_horizontal != 0
        || packet.buttons != 0
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
    if dev.is_null() || (size != 0 && data.is_null()) {
        return -22;
    }

    let Some(response) = control_response(dev as usize, request, request_type, value, index) else {
        return -38;
    };
    let copy_len = core::cmp::min(response.len(), size as usize);
    if copy_len != 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(response.as_ptr(), data.cast::<u8>(), copy_len);
        }
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
        let mut devices = SYNTHETIC_DEVICES.lock();
        let Some(device) = devices
            .iter_mut()
            .find(|device| device.usb_device_ptr == device_ptr)
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
        } else if !device.bootstrap_urb_completed {
            device.bootstrap_urb_completed = true;
            completion_from_report(urb, idle_report(device.kind))
        } else {
            device.pending_urb = Some(urb as usize);
            None
        }
    };

    queue_urb_completion(completion);
    0
}

pub(crate) fn hid_input_report(dev: *mut LinuxCompatHidDevice, data: *mut u8, size: u32) -> i32 {
    if dev.is_null() || data.is_null() || size == 0 || size as usize > MAX_REPORT_LEN {
        return -22;
    }

    let kind = hid_kind(dev);
    let Some(kind) = kind else {
        return 0;
    };
    let report = unsafe { core::slice::from_raw_parts(data, size as usize) };

    match kind {
        SyntheticHidKind::Keyboard => handle_keyboard_report(dev as usize, report),
        SyntheticHidKind::Pointer => handle_pointer_report(dev as usize, report),
    }
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
    let transfer_buffer = unsafe { (*urb).transfer_buffer };
    if size != 0 && transfer_buffer.is_null() {
        return -22;
    }
    let status = control_msg(
        unsafe { (*urb).dev },
        request,
        request_type,
        value,
        index,
        transfer_buffer,
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

pub(crate) fn cancel_urb(urb: *mut LinuxCompatUrb) -> bool {
    if urb.is_null() {
        return false;
    }
    let urb_ptr = urb as usize;
    let mut cancelled = false;

    {
        let mut devices = SYNTHETIC_DEVICES.lock();
        for device in devices.iter_mut() {
            if device.pending_urb == Some(urb_ptr) {
                device.pending_urb = None;
                cancelled = true;
            }
        }
    }

    {
        let mut pending = PENDING_COMPLETIONS.lock();
        let mut retained = HeaplessDeque::new();
        while let Some(completion) = pending.pop_front() {
            if completion.urb_ptr == urb_ptr {
                cancelled = true;
            } else {
                let _ = retained.push_back(completion);
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

fn control_response(
    usb_device_ptr: usize,
    request: u8,
    request_type: u8,
    value: u16,
    _index: u16,
) -> Option<&'static [u8]> {
    let devices = SYNTHETIC_DEVICES.lock();
    let device = devices
        .iter()
        .find(|device| device.usb_device_ptr == usb_device_ptr)?;

    match request {
        USB_REQ_GET_DESCRIPTOR => match (value >> 8) as u8 {
            USB_DT_REPORT => Some(device.descriptors.report_descriptor),
            USB_DT_HID => Some(match device.kind {
                SyntheticHidKind::Keyboard => &KEYBOARD_HID_DESCRIPTOR,
                SyntheticHidKind::Pointer => &POINTER_HID_DESCRIPTOR,
            }),
            USB_DT_DEVICE => Some(unsafe {
                let device_ptr = usb_device_ptr as *const LinuxCompatUsbDevice;
                core::slice::from_raw_parts((*device_ptr).descriptor.as_ptr(), 18)
            }),
            _ => None,
        },
        USB_REQ_GET_INTERFACE if request_type == 0x81 => Some(&[0]),
        USB_REQ_SET_INTERFACE if request_type == 0x01 => Some(&[]),
        USB_REQ_GET_PROTOCOL if request_type == 0xA1 => Some(&[0]),
        USB_REQ_SET_PROTOCOL if request_type == 0x21 => Some(&[]),
        USB_REQ_GET_IDLE if request_type == 0xA1 => Some(&[0]),
        USB_REQ_SET_IDLE if request_type == 0x21 => Some(&[]),
        _ => None,
    }
}

fn pop_report_for_device(usb_device_ptr: usize) -> Option<SyntheticReport> {
    let mut devices = SYNTHETIC_DEVICES.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.usb_device_ptr == usb_device_ptr)?;
    device.queued_reports.pop_front()
}

fn enqueue_report_locked(
    device: &mut SyntheticHidDevice,
    report: SyntheticReport,
) -> Option<UrbCompletion> {
    if let Some(urb_ptr) = device.pending_urb.take() {
        return completion_from_report(urb_ptr as *mut LinuxCompatUrb, report);
    }

    if device.kind == SyntheticHidKind::Pointer {
        if let Some(last) = device.queued_reports.back_mut() {
            if last.len == POINTER_REPORT_LEN {
                last.bytes[0] = report.bytes[0];
                last.bytes[1] = (last.bytes[1] as i8).saturating_add(report.bytes[1] as i8) as u8;
                last.bytes[2] = (last.bytes[2] as i8).saturating_add(report.bytes[2] as i8) as u8;
                last.bytes[3] = (last.bytes[3] as i8).saturating_add(report.bytes[3] as i8) as u8;
                return None;
            }
        }
    }

    if device.queued_reports.len() >= REPORT_QUEUE_CAPACITY {
        device.dropped_reports = device.dropped_reports.saturating_add(1);
        if device
            .dropped_reports
            .is_multiple_of(REPORT_DROP_LOG_INTERVAL)
        {
            crate::debug::println!(
                "usb synthetic report overload: kind={:?} dropped={} queued={}",
                device.kind,
                device.dropped_reports,
                device.queued_reports.len()
            );
        }
        return None;
    }

    let _ = device.queued_reports.push_back(report);
    None
}

fn completion_from_report(
    urb: *mut LinuxCompatUrb,
    report: SyntheticReport,
) -> Option<UrbCompletion> {
    if urb.is_null() {
        return None;
    }
    let transfer_buffer = unsafe { (*urb).transfer_buffer }.cast::<u8>();
    let transfer_buffer_length = unsafe { (*urb).transfer_buffer_length as usize };
    if transfer_buffer.is_null() && transfer_buffer_length != 0 {
        unsafe {
            (*urb).status = USB_URB_STATUS_IO_ERROR;
            (*urb).actual_length = 0;
        }
        return None;
    }
    let copied = copy_report_bytes(report, transfer_buffer, transfer_buffer_length);
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

fn copy_report_bytes(report: SyntheticReport, dest: *mut u8, max_len: usize) -> usize {
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
    let _ = PENDING_COMPLETIONS.lock().push_back(completion);
}

fn queue_urb_completions(completions: Vec<UrbCompletion>) {
    if completions.is_empty() {
        return;
    }
    let mut pending = PENDING_COMPLETIONS.lock();
    for completion in completions {
        if pending.push_back(completion).is_err() {
            break;
        }
    }
}

fn dispatch_urb_completion(completion: Option<UrbCompletion>) {
    let Some(completion) = completion else {
        return;
    };
    if !crate::driver::runtime_executable_addr_is_known(completion.callback as usize) {
        let urb = completion.urb_ptr as *mut LinuxCompatUrb;
        if !urb.is_null() {
            unsafe {
                (*urb).status = USB_URB_STATUS_IO_ERROR;
                (*urb).actual_length = 0;
            }
        }
        crate::debug::println!(
            "usb synthetic dropped completion with unknown callback target: urb={:#x} callback={:#x}",
            completion.urb_ptr,
            completion.callback as usize
        );
        return;
    }
    unsafe {
        (completion.callback)(completion.urb_ptr as *mut LinuxCompatUrb);
    }
}

fn keyboard_report(state: KeyboardReportState) -> SyntheticReport {
    let mut bytes = [0u8; KEYBOARD_REPORT_LEN];
    bytes[0] = state.modifiers;
    bytes[2..8].copy_from_slice(&state.keys);
    SyntheticReport {
        bytes,
        len: KEYBOARD_REPORT_LEN,
    }
}

fn idle_report(kind: SyntheticHidKind) -> SyntheticReport {
    match kind {
        SyntheticHidKind::Keyboard => keyboard_report(KeyboardReportState::default()),
        SyntheticHidKind::Pointer => SyntheticReport {
            bytes: [0; KEYBOARD_REPORT_LEN],
            len: POINTER_REPORT_LEN,
        },
    }
}

fn update_keyboard_report(state: &mut KeyboardReportState, usage: u8, action: KeyAction) {
    if let Some(mask) = hid_modifier_mask(usage) {
        match action {
            KeyAction::Pressed => state.modifiers |= mask,
            KeyAction::Released => state.modifiers &= !mask,
            KeyAction::Repeated => {}
        }
        return;
    }

    match action {
        KeyAction::Pressed => {
            if state.keys.contains(&usage) {
                return;
            }
            if let Some(slot) = state.keys.iter_mut().find(|slot| **slot == 0) {
                *slot = usage;
            }
        }
        KeyAction::Released => {
            for slot in state.keys.iter_mut() {
                if *slot == usage {
                    *slot = 0;
                }
            }
            compact_keyboard_keys(state);
        }
        KeyAction::Repeated => {}
    }
}

fn compact_keyboard_keys(state: &mut KeyboardReportState) {
    let mut next = [0u8; 6];
    let mut count = 0usize;
    for key in state.keys {
        if key == 0 || count >= next.len() {
            continue;
        }
        next[count] = key;
        count += 1;
    }
    state.keys = next;
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

fn hid_kind(dev: *mut LinuxCompatHidDevice) -> Option<SyntheticHidKind> {
    let product = unsafe { (*dev).product as u16 };
    match product {
        PRODUCT_ID_KEYBOARD => Some(SyntheticHidKind::Keyboard),
        PRODUCT_ID_POINTER => Some(SyntheticHidKind::Pointer),
        _ => None,
    }
}

fn handle_keyboard_report(hid_device_ptr: usize, report: &[u8]) -> i32 {
    if report.len() < KEYBOARD_REPORT_LEN {
        return 0;
    }

    let (previous, current) = {
        let mut states = HID_REPORT_STATES.lock();
        let state = ensure_hid_state(&mut states, hid_device_ptr, SyntheticHidKind::Keyboard);
        let previous = state.last_keyboard_report;
        let current = [
            report[0], report[1], report[2], report[3], report[4], report[5], report[6], report[7],
        ];
        state.last_keyboard_report = current;
        (previous, current)
    };

    with_injection(|| {
        for usage in 0xE0u8..=0xE7 {
            let mask = 1u8 << (usage - 0xE0);
            if (previous[0] & mask) == (current[0] & mask) {
                continue;
            }
            if let Some(code) = hid_usage_to_keycode(usage) {
                crate::input::keyboard::inject_key_transition(code, (current[0] & mask) == 0);
            }
        }

        for usage in previous[2..8]
            .iter()
            .copied()
            .filter(|usage| *usage != 0 && !current[2..8].contains(usage))
        {
            if let Some(code) = hid_usage_to_keycode(usage) {
                crate::input::keyboard::inject_key_transition(code, true);
            }
        }

        for usage in current[2..8]
            .iter()
            .copied()
            .filter(|usage| *usage != 0 && !previous[2..8].contains(usage))
        {
            if let Some(code) = hid_usage_to_keycode(usage) {
                crate::input::keyboard::inject_key_transition(code, false);
            }
        }
    });

    if KEYBOARD_TRANSLATION_LOG_LIMIT != 0
        && KEYBOARD_TRANSLATION_LOGS.fetch_add(1, Ordering::Relaxed)
            < KEYBOARD_TRANSLATION_LOG_LIMIT
    {
        crate::debug::println!(
            "usb hid keyboard report: dev={:#x} modifiers={:#x} keys={:02x},{:02x},{:02x},{:02x},{:02x},{:02x}",
            hid_device_ptr,
            current[0],
            current[2],
            current[3],
            current[4],
            current[5],
            current[6],
            current[7]
        );
    }

    0
}

fn handle_pointer_report(hid_device_ptr: usize, report: &[u8]) -> i32 {
    if report.len() < 3 {
        return 0;
    }

    let buttons = report[0] & 0x07;
    {
        let mut states = HID_REPORT_STATES.lock();
        let state = ensure_hid_state(&mut states, hid_device_ptr, SyntheticHidKind::Pointer);
        state.last_pointer_buttons = buttons;
    }

    let packet = PointerPacket {
        buttons: pointer_buttons_from_report(buttons),
        dx: report[1] as i8 as i16,
        dy: report[2] as i8 as i16,
        wheel_vertical: report.get(3).copied().unwrap_or(0) as i8 as i16,
        wheel_horizontal: 0,
        reserved0: 0,
        reserved1: 0,
        reserved2: 0,
    };

    with_injection(|| {
        crate::driver::input::submit_pointer_packet(packet);
    });

    if POINTER_TRANSLATION_LOG_LIMIT != 0
        && (packet.dx != 0 || packet.dy != 0 || packet.wheel_vertical != 0 || packet.buttons != 0)
        && POINTER_TRANSLATION_LOGS.fetch_add(1, Ordering::Relaxed) < POINTER_TRANSLATION_LOG_LIMIT
    {
        crate::debug::println!(
            "usb hid pointer report: dev={:#x} dx={} dy={} wheel={} buttons={:#x}",
            hid_device_ptr,
            packet.dx,
            packet.dy,
            packet.wheel_vertical,
            packet.buttons
        );
    }
    0
}

fn ensure_hid_state(
    states: &mut Vec<HidReportState>,
    hid_device_ptr: usize,
    kind: SyntheticHidKind,
) -> &mut HidReportState {
    if let Some(index) = states
        .iter()
        .position(|state| state.hid_device_ptr == hid_device_ptr)
    {
        let state = &mut states[index];
        state.kind = Some(kind);
        return state;
    }

    states.push(HidReportState {
        hid_device_ptr,
        kind: Some(kind),
        ..HidReportState::default()
    });
    states.last_mut().expect("hid report state just inserted")
}

fn with_injection(f: impl FnOnce()) {
    begin_injection();
    f();
    end_injection();
}
