use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::sync::KernelSpinLock as Mutex;
use heapless::Deque as HeaplessDeque;
use rustos_user_abi::syscall::{
    INPUTD_HID_POLICY_DESCRIPTOR_CAPACITY, INPUTD_HID_POLICY_KIND_KEYBOARD,
    INPUTD_HID_POLICY_KIND_POINTER, INPUTD_HID_POLICY_KIND_UNKNOWN,
    INPUTD_HID_POLICY_REPORT_CAPACITY, InputHidPolicyWire,
};

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
const USB_URB_STATUS_IO_ERROR: i32 = -5;
const USB_URB_STATUS_UNLINKED: i32 = -2;
const REPORT_QUEUE_CAPACITY: usize = 256;
const PENDING_COMPLETION_CAPACITY: usize = 64;
const COMPLETIONS_PER_SERVICE: usize = 32;
const URB_SUBMIT_LOG_LIMIT: usize = 0;
const REPORT_DESCRIPTOR_LOG_LIMIT: usize = 0;
const REPORT_DROP_LOG_INTERVAL: u64 = 1024;
const MAX_RUNTIME_REPORT_BYTES: usize = 64;

#[derive(Clone)]
struct RuntimeUsbDevice {
    usb_device_ptr: usize,
    interface_ptr: usize,
    interface_number: u8,
    device_descriptor: [u8; 18],
    hid_descriptor: Box<[u8]>,
    report_descriptor: Box<[u8]>,
    queued_reports: HeaplessDeque<RuntimeReport, REPORT_QUEUE_CAPACITY>,
    pending_urbs: HeaplessDeque<usize, REPORT_QUEUE_CAPACITY>,
    dropped_reports: u64,
}

#[derive(Clone)]
struct RuntimeReport {
    bytes: Box<[u8]>,
}

#[derive(Clone, Copy)]
struct UrbCompletion {
    urb_ptr: usize,
    callback: unsafe extern "C" fn(*mut LinuxCompatUrb),
}

static USB_RUNTIME_DEVICES: Mutex<Vec<RuntimeUsbDevice>> = Mutex::new(Vec::new());
static PENDING_COMPLETIONS: Mutex<HeaplessDeque<UrbCompletion, PENDING_COMPLETION_CAPACITY>> =
    Mutex::new(HeaplessDeque::new());
static URB_SUBMIT_LOGS: AtomicUsize = AtomicUsize::new(0);
static REPORT_DESCRIPTOR_LOGS: AtomicUsize = AtomicUsize::new(0);
static HID_POINTER_REPORT_COUNT: AtomicU64 = AtomicU64::new(0);

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

pub(crate) fn has_pointer_device() -> bool {
    USB_RUNTIME_DEVICES
        .lock()
        .iter()
        .any(|device| report_descriptor_has_pointer(device.report_descriptor.as_ref()))
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
        queued_reports: HeaplessDeque::new(),
        pending_urbs: HeaplessDeque::new(),
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

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub(crate) fn enqueue_report(usb_device: *mut LinuxCompatUsbDevice, report: &[u8]) {
    if usb_device.is_null() || report.is_empty() {
        return;
    }
    let Some(report) = RuntimeReport::from_slice(report) else {
        return;
    };

    let (completion, raw_report) = {
        let mut devices = USB_RUNTIME_DEVICES.lock();
        let Some(device) = devices
            .iter_mut()
            .find(|entry| entry.usb_device_ptr == usb_device as usize)
        else {
            return;
        };
        let raw_report = hid_raw_report(device, report.as_slice());

        if let Some(urb_ptr) = device.pending_urbs.pop_front() {
            (
                completion_from_report(urb_ptr as *mut LinuxCompatUrb, report.clone()),
                raw_report,
            )
        } else if device.queued_reports.len() >= REPORT_QUEUE_CAPACITY {
            device.dropped_reports = device.dropped_reports.saturating_add(1);
            if device.dropped_reports % REPORT_DROP_LOG_INTERVAL == 0 {
                crate::debug::println!(
                    "usb runtime report overload: usb_dev={:#x} dropped={} queued={}",
                    device.usb_device_ptr,
                    device.dropped_reports,
                    device.queued_reports.len()
                );
            }
            (None, raw_report)
        } else {
            let _ = device.queued_reports.push_back(report);
            (None, raw_report)
        }
    };

    if let Some(raw_report) = raw_report {
        HID_POINTER_REPORT_COUNT.fetch_add(
            (raw_report.kind == INPUTD_HID_POLICY_KIND_POINTER) as u64,
            Ordering::Relaxed,
        );
        let _ = crate::input::event_queue::submit_hid_raw_report(raw_report);
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
    if dev.is_null() || (size != 0 && data.is_null()) {
        return -22;
    }

    let Some(copy_len) = copy_control_response(
        dev as usize,
        request,
        request_type,
        value,
        index,
        data.cast::<u8>(),
        size as usize,
    ) else {
        return -38;
    };
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

    crate::debug::println!(
        "usb runtime interrupt_msg: begin dev={:#x} data={:#x} len={}",
        dev as usize,
        data as usize,
        len,
    );

    let Some(report) = pop_report_for_device(dev as usize) else {
        if !actual_length.is_null() {
            unsafe {
                *actual_length = 0;
            }
        }
        crate::debug::println!(
            "usb runtime interrupt_msg: no report dev={:#x}",
            dev as usize
        );
        return -11;
    };

    let copied = copy_report_bytes(report, data.cast::<u8>(), len as usize);
    if !actual_length.is_null() {
        unsafe {
            *actual_length = copied as i32;
        }
    }
    crate::debug::println!(
        "usb runtime interrupt_msg: end dev={:#x} copied={}",
        dev as usize,
        copied
    );
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
            if device.pending_urbs.push_back(urb as usize).is_err() {
                return USB_URB_STATUS_IO_ERROR;
            }
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

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub(crate) fn hid_input_report(dev: *mut LinuxCompatHidDevice, data: *mut u8, size: u32) -> i32 {
    0
}

pub(crate) fn hid_remove_device(dev: *mut LinuxCompatHidDevice) {
    let _ = dev;
}

impl RuntimeReport {
    fn from_slice(report: &[u8]) -> Option<Self> {
        if report.is_empty() || report.len() > MAX_RUNTIME_REPORT_BYTES {
            return None;
        }
        Some(Self {
            bytes: report.to_vec().into_boxed_slice(),
        })
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn as_slice(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

pub(crate) fn debug_pointer_report_count() -> u64 {
    HID_POINTER_REPORT_COUNT.load(Ordering::Relaxed)
}

// RING3-MIGRATION-REFERENCE START: inputd should own HID report classification
// and descriptor-derived report metadata. Ring0 keeps raw USB report ingress and
// Linux .ko URB callback substrate only.
fn hid_raw_report(device: &RuntimeUsbDevice, report: &[u8]) -> Option<InputHidPolicyWire> {
    if report.is_empty() || report.len() > INPUTD_HID_POLICY_REPORT_CAPACITY {
        return None;
    }
    let descriptor = device.report_descriptor.as_ref();
    let kind = if report_descriptor_has_pointer(descriptor) {
        INPUTD_HID_POLICY_KIND_POINTER
    } else if report_descriptor_has_keyboard(descriptor) {
        INPUTD_HID_POLICY_KIND_KEYBOARD
    } else {
        INPUTD_HID_POLICY_KIND_UNKNOWN
    };
    if kind == INPUTD_HID_POLICY_KIND_UNKNOWN {
        return None;
    }
    let mut wire = InputHidPolicyWire {
        source_id: device.usb_device_ptr as u64,
        kind,
        report_len: report.len() as u16,
        descriptor_len: descriptor.len().min(INPUTD_HID_POLICY_DESCRIPTOR_CAPACITY) as u16,
        report_id: report_descriptor_id(descriptor),
        required_bytes: report.len() as u16,
        ..InputHidPolicyWire::default()
    };
    wire.report[..report.len()].copy_from_slice(report);
    let descriptor_len = wire.descriptor_len as usize;
    wire.descriptor_prefix[..descriptor_len].copy_from_slice(&descriptor[..descriptor_len]);
    Some(wire)
}

fn report_descriptor_has_pointer(descriptor: &[u8]) -> bool {
    let has_mouse_usage = descriptor.windows(2).any(|item| item == [0x09, 0x02]);
    let has_x_usage = descriptor.windows(2).any(|item| item == [0x09, 0x30]);
    let has_y_usage = descriptor.windows(2).any(|item| item == [0x09, 0x31]);
    has_mouse_usage && has_x_usage && has_y_usage
}

fn report_descriptor_has_keyboard(descriptor: &[u8]) -> bool {
    let has_keyboard_usage = descriptor.windows(2).any(|item| item == [0x09, 0x06]);
    let has_key_usage_page = descriptor.windows(2).any(|item| item == [0x05, 0x07]);
    has_keyboard_usage && has_key_usage_page
}

fn report_descriptor_id(descriptor: &[u8]) -> u8 {
    descriptor
        .windows(2)
        .find_map(|item| (item[0] == 0x85).then_some(item[1]))
        .unwrap_or(0)
}
// RING3-MIGRATION-REFERENCE END: inputd-owned HID report classification.

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

fn copy_control_response(
    usb_device_ptr: usize,
    request: u8,
    request_type: u8,
    value: u16,
    index: u16,
    data: *mut u8,
    size: usize,
) -> Option<usize> {
    let devices = USB_RUNTIME_DEVICES.lock();
    let device = devices
        .iter()
        .find(|entry| entry.usb_device_ptr == usb_device_ptr)?;

    let response: &[u8] = match request {
        USB_REQ_GET_DESCRIPTOR => match (value >> 8) as u8 {
            USB_DT_DEVICE => &device.device_descriptor,
            USB_DT_HID => device.hid_descriptor.as_ref(),
            USB_DT_REPORT => device.report_descriptor.as_ref(),
            _ => return None,
        },
        USB_REQ_SET_CONFIGURATION if request_type == 0x00 => &[],
        USB_REQ_GET_INTERFACE if request_type == 0x81 && index as u8 == device.interface_number => {
            &[0]
        }
        USB_REQ_SET_INTERFACE if request_type == 0x01 && index as u8 == device.interface_number => {
            &[]
        }
        USB_REQ_GET_PROTOCOL if request_type == 0xA1 => &[0],
        USB_REQ_SET_PROTOCOL if request_type == 0x21 => &[],
        USB_REQ_GET_IDLE if request_type == 0xA1 => &[0],
        USB_REQ_SET_IDLE if request_type == 0x21 => &[],
        _ => return None,
    };

    let copy_len = core::cmp::min(response.len(), size);
    if copy_len != 0 {
        if data.is_null() {
            return None;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(response.as_ptr(), data, copy_len);
        }
    }
    Some(copy_len)
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

fn copy_report_bytes(report: RuntimeReport, dest: *mut u8, max_len: usize) -> usize {
    if dest.is_null() {
        return 0;
    }
    let copied = core::cmp::min(report.len(), max_len);
    unsafe {
        core::ptr::copy_nonoverlapping(report.as_slice().as_ptr(), dest, copied);
    }
    copied
}

fn queue_urb_completion(completion: Option<UrbCompletion>) {
    let Some(completion) = completion else {
        return;
    };
    let _ = PENDING_COMPLETIONS.lock().push_back(completion);
}

fn dispatch_urb_completion(completion: Option<UrbCompletion>) {
    let Some(completion) = completion else {
        return;
    };
    if !crate::driver::runtime_executable_addr_is_known(completion.callback as usize) {
        crate::debug::println!(
            "usb runtime dropped completion with unknown callback target: urb={:#x} callback={:#x}",
            completion.urb_ptr,
            completion.callback as usize,
        );
        let urb = completion.urb_ptr as *mut LinuxCompatUrb;
        if !urb.is_null() {
            unsafe {
                (*urb).status = USB_URB_STATUS_IO_ERROR;
                (*urb).actual_length = 0;
            }
        }
        return;
    }
    unsafe {
        (completion.callback)(completion.urb_ptr as *mut LinuxCompatUrb);
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
