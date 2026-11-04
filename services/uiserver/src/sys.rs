use core::arch::asm;
use core::ffi::{c_char, c_void};
use core::mem::size_of;
use core::ptr::NonNull;
use core::slice;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread;

use keyboard_core::{KeyAction, KeyCode, KeyboardDriver};
use rustos_user_abi::{console as console_abi, device as device_abi, syscall as syscall_abi};

use console_abi::{
    ConsoleSendInputEventRequest, ConsoleSetFocusRequest, ConsoleSnapshotSessionOutputRequest,
    ConsoleSnapshotSessionsRequest, ConsoleWaitGraphRequest,
};
pub(crate) use console_abi::{ConsoleSessionInfo, ConsoleStateInfo};
pub(crate) use device_abi::{
    DISPLAY_GPU_ABI_VERSION, DISPLAY_GPU_INFO_FLAG_DIRECT_DMABUF,
    DISPLAY_GPU_INFO_FLAG_STAGED_COPY, DISPLAY_GPU_SUBMIT_FLAG_STAGED_COPY,
    DisplayGpuCompletionQuery, DisplayGpuInfo, DisplayInfo, DisplaySurfaceCreate,
    INPUT_ACTION_NONE, INPUT_ACTION_PRESSED, INPUT_ACTION_RELEASED, INPUT_ACTION_REPEATED,
    INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON, INPUT_KIND_POINTER_MOTION,
    INPUT_KIND_POINTER_POSITION, INPUT_KIND_POINTER_SCROLL, InputEvent, PIXEL_FORMAT_BGRA8888,
    POINTER_BUTTON_LEFT, POINTER_BUTTON_MIDDLE, POINTER_BUTTON_RIGHT, POINTER_BUTTON_X1,
    POINTER_BUTTON_X2,
};
use device_abi::{
    DISPLAY_SURFACE_FLAG_GPU_ATLAS, DisplayGpuDamage, DisplayGpuSubmitRequest,
    DisplayPresentRectRequest, DisplayPresentRequest,
};

const SYS_READ: usize = 0;
const SYS_POLL: usize = 7;
const SYS_MMAP: usize = 9;
const SYS_MUNMAP: usize = 11;
const SYS_IOCTL: usize = 16;
const SYS_GETTID: usize = 186;
const SYS_OPENAT: usize = 257;
const SYS_RUSTOS_IPC_CALL: usize = syscall_abi::SYS_RUSTOS_IPC_CALL as usize;
const SYS_RUSTOS_IPC_RECV_WITH_SENDER: usize =
    syscall_abi::SYS_RUSTOS_IPC_RECV_WITH_SENDER as usize;
const SYS_RUSTOS_IPC_REPLY: usize = syscall_abi::SYS_RUSTOS_IPC_REPLY as usize;
const SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT: usize =
    syscall_abi::SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT as usize;
const SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT: usize =
    syscall_abi::SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT as usize;
const SYS_RUSTOS_SCHED_DEMOTE_SELF: usize = syscall_abi::SYS_RUSTOS_SCHED_DEMOTE_SELF as usize;

const AT_FDCWD: isize = -100;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_NONBLOCK: usize = 0o4000;

const POLLIN: i16 = 0x001;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
const POLLNVAL: i16 = 0x020;

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;

const MAP_SHARED: usize = 0x01;
const PAGE_SIZE: usize = 4096;
const MAX_SURFACE_MAPPING_BYTES: usize = device_abi::DISPLAY_SURFACE_MAX_MAPPING_BYTES as usize;
const DISPLAY_POLICY_MAX_WIDTH: u64 = 7680;
const DISPLAY_POLICY_MAX_HEIGHT: u64 = 4320;
const MAX_EVDEV_EVENTS_PER_READ: usize = 512;
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
pub(crate) const CONSOLE_SESSION_CAPACITY: usize = console_abi::MAX_CONSOLE_SESSIONS;
pub(crate) const MAX_CONSOLE_SNAPSHOT_BYTES: usize = 4096;
pub(crate) const EAGAIN: i32 = 11;
pub(crate) const ENOENT: i32 = 2;
pub(crate) const EINVAL: i32 = 22;
pub(crate) const ENODEV: i32 = 19;

pub(crate) const ESTALE: i32 = 116;
pub(crate) type ConsoleSessionHandle = u64;

static DISPLAY_POLICY_READY: AtomicBool = AtomicBool::new(false);
static DISPLAY_POLICY_WIDTH: AtomicU32 = AtomicU32::new(0);
static DISPLAY_POLICY_HEIGHT: AtomicU32 = AtomicU32::new(0);
static DISPLAY_POLICY_STRIDE: AtomicU32 = AtomicU32::new(0);
static DISPLAY_POLICY_BPP: AtomicU32 = AtomicU32::new(0);
static DISPLAY_POLICY_FORMAT: AtomicU32 = AtomicU32::new(0);
static DISPLAY_POLICY_FLAGS: AtomicU32 = AtomicU32::new(0);
static DISPLAY_POLICY_GENERATION: AtomicU64 = AtomicU64::new(0);
static BACKGROUND_THREAD_DEMOTIONS: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxInputTimeval {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxInputEvent {
    time: LinuxInputTimeval,
    kind: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxPollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[derive(Default)]
struct InputTranslationState {
    keyboard: KeyboardDriver,
    abs_x: i32,
    abs_y: i32,
}

fn input_translation_state() -> &'static Mutex<InputTranslationState> {
    static STATE: OnceLock<Mutex<InputTranslationState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(InputTranslationState::default()))
}

pub(crate) fn start_display_policy_endpoint() -> Result<(), i32> {
    let endpoint = rustos_svc_runtime::ipc::endpoint_create_with_limits(size_of::<syscall_abi::CommercialMaxProtocolRequest>() as u64, 0) as isize;
    if is_syscall_error(endpoint) {
        return Err(errno_from_result(endpoint));
    }
    let register = unsafe {
        syscall2(
            SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
            syscall_abi::IPC_SERVICE_UISERVER as usize,
            endpoint as usize,
        )
    };
    if is_syscall_error(register) {
        return Err(errno_from_result(register));
    }
    boot_line("uiserver: display policy worker spawn begin");
    spawn_ui_thread(
        UiThreadRole::DisplayPolicy,
        "uiserver-display-policy",
        move || serve_display_policy(endpoint as u64),
    )
    .map_err(|_| EINVAL)?;
    boot_line("uiserver: display policy worker spawn done");
    diag_line("uiserver: display policy endpoint registered");
    Ok(())
}

pub(crate) fn publish_display_policy_metadata(display: DisplayInfo) {
    DISPLAY_POLICY_WIDTH.store(display.width, Ordering::Release);
    DISPLAY_POLICY_HEIGHT.store(display.height, Ordering::Release);
    DISPLAY_POLICY_STRIDE.store(display.stride_bytes, Ordering::Release);
    DISPLAY_POLICY_BPP.store(display.bytes_per_pixel, Ordering::Release);
    DISPLAY_POLICY_FORMAT.store(display.pixel_format, Ordering::Release);
    DISPLAY_POLICY_FLAGS.store(display.flags, Ordering::Release);
    DISPLAY_POLICY_GENERATION.store(display.generation, Ordering::Release);
}

pub(crate) fn publish_input_pointer_surface(display: DisplayInfo) -> Result<(), i32> {
    let request = syscall_abi::InputdPointerSurfaceRequest {
        version: syscall_abi::INPUTD_IPC_ABI_VERSION,
        op: syscall_abi::INPUTD_IPC_OP_SET_POINTER_SURFACE,
        width: display.width,
        height: display.height,
        generation: display.generation,
        ..syscall_abi::InputdPointerSurfaceRequest::default()
    };
    let mut last_errno = ENOENT;
    for _ in 0..10 {
        let endpoint = unsafe {
            syscall1(
                SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
                syscall_abi::IPC_SERVICE_INPUTD as usize,
            )
        };
        if is_syscall_error(endpoint) {
            last_errno = errno_from_result(endpoint);
            thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        let mut response = syscall_abi::InputdIpcResponse::default();
        let call = unsafe {
            syscall6(
                SYS_RUSTOS_IPC_CALL,
                endpoint as usize,
                (&request as *const syscall_abi::InputdPointerSurfaceRequest) as usize,
                size_of::<syscall_abi::InputdPointerSurfaceRequest>(),
                (&mut response as *mut syscall_abi::InputdIpcResponse) as usize,
                size_of::<syscall_abi::InputdIpcResponse>(),
                0,
            )
        };
        if is_syscall_error(call) {
            last_errno = errno_from_result(call);
            thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        if call as usize != size_of::<syscall_abi::InputdIpcResponse>()
            || response.version != syscall_abi::INPUTD_IPC_ABI_VERSION
            || response.op != syscall_abi::INPUTD_IPC_OP_SET_POINTER_SURFACE
            || response.flags != 0
        {
            return Err(EINVAL);
        }
        if response.status != 0 {
            return Err(response.status.unsigned_abs().min(i32::MAX as u32) as i32);
        }
        return Ok(());
    }
    Err(last_errno)
}

pub(crate) fn mark_display_policy_ready() {
    DISPLAY_POLICY_READY.store(true, Ordering::Release);
}

fn serve_display_policy(endpoint: u64) {
    boot_line("uiserver: display policy worker enter");
    let mut first_request = true;
    loop {
        let mut request = syscall_abi::CommercialMaxProtocolRequest::default();
        let mut reply_cap = 0_u64;
        let mut sender_pid = 0_u64;
        let mut sender_tid = 0_u64;
        let received = unsafe {
            syscall6(
                SYS_RUSTOS_IPC_RECV_WITH_SENDER,
                endpoint as usize,
                (&mut request as *mut syscall_abi::CommercialMaxProtocolRequest) as usize,
                size_of::<syscall_abi::CommercialMaxProtocolRequest>(),
                (&mut reply_cap as *mut u64) as usize,
                (&mut sender_pid as *mut u64) as usize,
                (&mut sender_tid as *mut u64) as usize,
            )
        };
        if is_syscall_error(received) {
            thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        let trace_first_request = first_request;
        if trace_first_request {
            boot_line("uiserver: display policy first request received");
            first_request = false;
        }
        let response =
            dispatch_display_policy_request(received as usize, &request, sender_pid, sender_tid);
        if trace_first_request {
            boot_line("uiserver: display policy first response built");
        }
        let reply = unsafe {
            syscall3(
                SYS_RUSTOS_IPC_REPLY,
                reply_cap as usize,
                (&response as *const syscall_abi::CommercialMaxProtocolResponse) as usize,
                size_of::<syscall_abi::CommercialMaxProtocolResponse>(),
            )
        };
        if trace_first_request {
            boot_line("uiserver: display policy first reply returned");
        }
        if is_syscall_error(reply) {
            diag_line(format!(
                "uiserver: display policy reply failed errno={}",
                errno_from_result(reply)
            ));
        }
    }
}

fn dispatch_display_policy_request(
    received: usize,
    request: &syscall_abi::CommercialMaxProtocolRequest,
    sender_pid: u64,
    sender_tid: u64,
) -> syscall_abi::CommercialMaxProtocolResponse {
    let mut response = syscall_abi::CommercialMaxProtocolResponse {
        header: request.header,
        ..syscall_abi::CommercialMaxProtocolResponse::default()
    };
    response.header.version = syscall_abi::COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status =
        match validate_display_policy_request(received, request, sender_pid, sender_tid) {
            Ok(()) => fill_display_policy_response(request, &mut response),
            Err(errno) => errno,
        };
    response
}

fn validate_display_policy_request(
    received: usize,
    request: &syscall_abi::CommercialMaxProtocolRequest,
    sender_pid: u64,
    sender_tid: u64,
) -> Result<(), i32> {
    if received != size_of::<syscall_abi::CommercialMaxProtocolRequest>()
        || !request.has_valid_envelope()
        || request.header.protocol != syscall_abi::COMMERCIAL_MAX_PROTOCOL_UISERVER
    {
        return Err(libc::EINVAL);
    }
    if !request.subject_is_exact_sender(sender_pid, sender_tid)
        || rustos_svc_runtime::ipc::validate_service_owner(
            syscall_abi::IPC_SERVICE_DEVMGRD,
            sender_pid,
        ) < 0
    {
        return Err(libc::EACCES);
    }
    match request.header.op {
        syscall_abi::COMMERCIAL_MAX_UISERVER_OP_DISPLAY_READINESS
        | syscall_abi::COMMERCIAL_MAX_UISERVER_OP_DISPLAY_METADATA
        | syscall_abi::COMMERCIAL_MAX_UISERVER_OP_SURFACE_POLICY
        | syscall_abi::COMMERCIAL_MAX_UISERVER_OP_PRESENT_POLICY
        | syscall_abi::COMMERCIAL_MAX_UISERVER_OP_TERMINAL_PRESENT_POLICY
        | syscall_abi::COMMERCIAL_MAX_UISERVER_OP_TRUSTED_UI_STATUS => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn fill_display_policy_response(
    request: &syscall_abi::CommercialMaxProtocolRequest,
    response: &mut syscall_abi::CommercialMaxProtocolResponse,
) -> i32 {
    match request.header.op {
        syscall_abi::COMMERCIAL_MAX_UISERVER_OP_DISPLAY_READINESS => {
            response.value0 = if DISPLAY_POLICY_READY.load(Ordering::Acquire) {
                1
            } else {
                0
            };
            response.value1 = DISPLAY_POLICY_GENERATION.load(Ordering::Acquire);
            response.capability = display_policy_capability("display-readiness", request.header.op);
            0
        }
        syscall_abi::COMMERCIAL_MAX_UISERVER_OP_DISPLAY_METADATA => {
            response.value0 = (u64::from(DISPLAY_POLICY_WIDTH.load(Ordering::Acquire)) << 32)
                | u64::from(DISPLAY_POLICY_HEIGHT.load(Ordering::Acquire));
            response.value1 = (u64::from(DISPLAY_POLICY_STRIDE.load(Ordering::Acquire)) << 32)
                | u64::from(DISPLAY_POLICY_BPP.load(Ordering::Acquire));
            response.capability = display_policy_capability("display-metadata", request.header.op);
            response.descriptor_count = 1;
            response.descriptors[0] =
                display_policy_descriptor("display-metadata", request.header.op);
            0
        }
        syscall_abi::COMMERCIAL_MAX_UISERVER_OP_SURFACE_POLICY => {
            if request.arg1 > DISPLAY_POLICY_MAX_WIDTH || request.arg2 > DISPLAY_POLICY_MAX_HEIGHT {
                return libc::EINVAL;
            }
            if request.arg3 != 0 && request.arg3 != u64::from(device_abi::PIXEL_FORMAT_BGRA8888) {
                return libc::EINVAL;
            }
            response.capability = display_policy_capability("surface-policy", request.header.op);
            0
        }
        syscall_abi::COMMERCIAL_MAX_UISERVER_OP_PRESENT_POLICY => {
            response.capability = display_policy_capability("present-policy", request.header.op);
            0
        }
        syscall_abi::COMMERCIAL_MAX_UISERVER_OP_TERMINAL_PRESENT_POLICY => {
            response.capability =
                display_policy_capability("terminal-present-policy", request.header.op);
            0
        }
        syscall_abi::COMMERCIAL_MAX_UISERVER_OP_TRUSTED_UI_STATUS => {
            response.value0 = trusted_ui_status(DISPLAY_POLICY_FLAGS.load(Ordering::Acquire));
            response.value1 = DISPLAY_POLICY_GENERATION.load(Ordering::Acquire);
            response.capability = display_policy_capability("trusted-ui-status", request.header.op);
            0
        }
        _ => libc::EINVAL,
    }
}

fn display_policy_descriptor(
    label: &str,
    op: u16,
) -> syscall_abi::CommercialMaxProtocolDescriptorWire {
    let mut descriptor = syscall_abi::CommercialMaxProtocolDescriptorWire {
        protocol: syscall_abi::COMMERCIAL_MAX_PROTOCOL_UISERVER,
        op,
        service_id: syscall_abi::IPC_SERVICE_UISERVER,
        value0: (u64::from(DISPLAY_POLICY_WIDTH.load(Ordering::Acquire)) << 32)
            | u64::from(DISPLAY_POLICY_HEIGHT.load(Ordering::Acquire)),
        value1: DISPLAY_POLICY_GENERATION.load(Ordering::Acquire),
        ..syscall_abi::CommercialMaxProtocolDescriptorWire::default()
    };
    copy_policy_label(label, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn display_policy_capability(
    label: &str,
    op: u16,
) -> syscall_abi::CommercialMaxCapabilityLeaseWire {
    let mut capability = syscall_abi::CommercialMaxCapabilityLeaseWire {
        lease_id: ((syscall_abi::COMMERCIAL_MAX_PROTOCOL_UISERVER as u64) << 32) | u64::from(op),
        service_id: syscall_abi::IPC_SERVICE_UISERVER,
        ..syscall_abi::CommercialMaxCapabilityLeaseWire::default()
    };
    copy_policy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

/// Current RustOS DVM transports deliberately provide no attested physical
/// display or human-input path. A future trusted UI implementation may clear
/// these blockers only after independently attesting both channels; a normal
/// primary provider, including a native one, is not sufficient.
fn trusted_ui_status(display_flags: u32) -> u64 {
    let mut status = syscall_abi::UISERVER_TRUSTED_UI_STATUS_UNATTESTED_SCANOUT
        | syscall_abi::UISERVER_TRUSTED_UI_STATUS_UNATTESTED_INPUT;
    if display_flags & device_abi::DISPLAY_INFO_FLAG_DVM_SCANOUT != 0 {
        status |= syscall_abi::UISERVER_TRUSTED_UI_STATUS_DVM_SCANOUT;
    }
    status
}

fn copy_policy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = bytes.len().min(target.len());
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
}

const DISPLAY_IOCTL_GET_INFO: usize = device_abi::DISPLAY_IOCTL_GET_INFO as usize;
const DISPLAY_IOCTL_CREATE_SURFACE: usize = device_abi::DISPLAY_IOCTL_CREATE_SURFACE as usize;
const DISPLAY_IOCTL_PRESENT: usize = device_abi::DISPLAY_IOCTL_PRESENT as usize;
const DISPLAY_IOCTL_PRESENT_RECT: usize = device_abi::DISPLAY_IOCTL_PRESENT_RECT as usize;
const DISPLAY_IOCTL_GPU_GET_INFO: usize = device_abi::DISPLAY_IOCTL_GPU_GET_INFO as usize;
const DISPLAY_IOCTL_GPU_SUBMIT: usize = device_abi::DISPLAY_IOCTL_GPU_SUBMIT as usize;
const DISPLAY_IOCTL_GPU_QUERY_COMPLETION: usize =
    device_abi::DISPLAY_IOCTL_GPU_QUERY_COMPLETION as usize;
const CONSOLE_IOCTL_GET_STATE: usize = console_abi::CONSOLE_IOCTL_GET_STATE as usize;
const CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT: usize =
    console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT as usize;
const CONSOLE_IOCTL_SET_FOCUS: usize = console_abi::CONSOLE_IOCTL_SET_FOCUS as usize;
const CONSOLE_IOCTL_WAIT_GRAPH: usize = console_abi::CONSOLE_IOCTL_WAIT_GRAPH as usize;
const CONSOLE_IOCTL_SEND_INPUT_EVENT: usize = console_abi::CONSOLE_IOCTL_SEND_INPUT_EVENT as usize;
const CONSOLE_IOCTL_SNAPSHOT_SESSIONS: usize =
    console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSIONS as usize;

pub(crate) struct SurfaceMapping {
    base: NonNull<u32>,
    len_bytes: usize,
}

// A surface mapping has unique ownership and is never aliased across the
// initialization handoff. Moving it from the bounded GPU initialization
// worker to the UI thread does not change the mapping or its process address.
unsafe impl Send for SurfaceMapping {}

impl SurfaceMapping {
    pub(crate) fn pixels_mut(&mut self) -> &mut [u32] {
        unsafe { slice::from_raw_parts_mut(self.base.as_ptr(), self.len_bytes / size_of::<u32>()) }
    }
}

impl Drop for SurfaceMapping {
    fn drop(&mut self) {
        let _ = munmap(self.base.as_ptr().cast::<c_void>(), self.len_bytes);
    }
}

pub(crate) fn open_display() -> Result<OwnedFd, i32> {
    if running_on_rustos() {
        // RustOS exposes exactly one brokered display endpoint.  Do not fall
        // through to host DRM discovery: accepting a different device would
        // silently bypass the display-domain provenance contract.
        return open_device(device_abi::DISPLAY_PATH, O_RDWR);
    }
    open_first_host_device("/dev/dri", "card", O_RDWR, "/dev/dri/card0")
}

pub(crate) fn open_console() -> Result<OwnedFd, i32> {
    open_device(console_abi::CONSOLE_PATH, O_RDWR)
}

pub(crate) fn open_input() -> Result<Vec<OwnedFd>, i32> {
    if running_on_rustos() {
        return open_device(device_abi::INPUT_PATH, O_RDONLY | O_NONBLOCK).map(|fd| vec![fd]);
    }
    open_all_host_devices(
        "/dev/input",
        "event",
        O_RDONLY | O_NONBLOCK,
        "/dev/input/event0",
    )
}

pub(crate) fn diag_line(message: impl Into<String>) {
    static SENDER: OnceLock<mpsc::SyncSender<String>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<String>(128);
        spawn_ui_thread(
            UiThreadRole::Background,
            "uiserver-diagnostics",
            move || {
                while let Ok(message) = receiver.recv() {
                    observability_client::info!("uiserver", service, "{}", message);
                }
            },
        )
        .unwrap_or_else(|_| {
            debug_line("uiserver: diagnostics worker spawn failed");
            std::process::exit(134);
        });
        sender
    });
    let _ = sender.try_send(message.into());
}

pub(crate) fn boot_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("RUSTOS_UI_BOOT_TRACE") {
        Ok(value) => matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"),
        Err(_) => false,
    })
}

pub(crate) fn ui_profile_enabled() -> bool {
    crate::acceptance_profile::enabled()
}

pub(crate) fn boot_line(message: &str) {
    if !boot_trace_enabled() {
        return;
    }
    let _ = std::io::stderr().write_all(message.as_bytes());
    let _ = std::io::stderr().write_all(b"\n");
}

pub(crate) fn profile_line(message: &str) {
    if !crate::acceptance_profile::enabled() {
        return;
    }
    // The profile path is enabled only by the KVM acceptance harness and is
    // emitted after the accounting lock is released. Use the bounded,
    // nonblocking diagnostics queue instead of the kernel debugcon try-lock:
    // sustained cursor/present traffic can make every immediately-adjacent
    // try-lock sample lose while ordinary heartbeats happen to land between
    // writers. Queue saturation still fails closed at the harness, but
    // contention no longer fabricates a missing one-second profile stream.
    diag_line(message.to_string());
}

/// Production liveness telemetry. It cannot wait behind diagnostics: strict
/// priority scheduling may legitimately starve a User-class relay, while a UI
/// heartbeat must describe the interactive loop that created it.
pub(crate) fn heartbeat_line(message: &str) {
    // This is a lossy try-lock debugcon attempt. A contended logger drops the
    // sample, rather than delaying a present; the next heartbeat retries.
    debug_line(message);
}

pub(crate) fn debug_line(message: &str) {
    let mut line = message.as_bytes().to_vec();
    line.push(b'\n');
    rustos_svc_runtime::ipc::debug_bytes(&line);
}

/// Makes an auxiliary uiserver thread surrender the System-class admission and
/// elevated fair share it inherited from the interactive process. This has no
/// host-test effect.
///
/// The ABI is self-demotion only, so failure must not leave a background or
/// untrusted worker competing with input/present at elevated priority.  Exit
/// the process rather than silently accepting that invalid scheduling model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UiThreadRole {
    Background,
    Protocol,
    DisplayPolicy,
    Watchdog,
    BootstrapGpu,
    BootstrapDesktop,
}

impl UiThreadRole {
    const fn retains_boot_class(self) -> bool {
        matches!(self, Self::BootstrapGpu | Self::BootstrapDesktop)
    }
}

/// The only production thread-spawn boundary in uiserver. A role is selected
/// before the child can execute service work, so inherited process priority
/// never becomes implicit authority. Only the bounded GPU initializer and
/// first-scene desktop seed may retain boot class; the desktop seed explicitly
/// self-demotes before the optional full background build.
pub(crate) fn spawn_ui_thread<F, T>(
    role: UiThreadRole,
    name: impl Into<String>,
    body: F,
) -> std::io::Result<thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new().name(name.into()).spawn(move || {
        if !role.retains_boot_class() {
            require_background_thread_class();
        }
        body()
    })
}

pub(crate) fn require_background_thread_class() {
    if !running_on_rustos() {
        return;
    }
    let result = unsafe { syscall0(SYS_RUSTOS_SCHED_DEMOTE_SELF) };
    if result == 0 {
        BACKGROUND_THREAD_DEMOTIONS.fetch_add(1, Ordering::Relaxed);
        // One boot-time identity record makes scheduler top-task attribution
        // actionable without adding any sample or syscall to the frame loop.
        // The kernel stamps gettid; a Rust thread name is diagnostic only and
        // never grants scheduling authority.
        let current = thread::current();
        let name = current.name().unwrap_or("<unnamed>");
        let tid = unsafe { syscall0(SYS_GETTID) };
        debug_line(&format!(
            "uiserver: background scheduling class=user name={name} tid={tid}"
        ));
        return;
    }
    debug_line("uiserver: background scheduling demotion failed");
    std::process::exit(134);
}

pub(crate) fn background_thread_demotion_count() -> u64 {
    BACKGROUND_THREAD_DEMOTIONS.load(Ordering::Relaxed)
}

pub(crate) fn display_get_info(fd: RawFd) -> Result<DisplayInfo, i32> {
    let mut info = DisplayInfo::default();
    ioctl_with_mut(fd, DISPLAY_IOCTL_GET_INFO, &mut info)?;
    Ok(info)
}

pub(crate) fn display_create_surface(
    fd: RawFd,
    width: u32,
    height: u32,
) -> Result<DisplaySurfaceCreate, i32> {
    let mut surface = DisplaySurfaceCreate::request(width, height, PIXEL_FORMAT_BGRA8888);
    ioctl_with_mut(fd, DISPLAY_IOCTL_CREATE_SURFACE, &mut surface)?;
    Ok(surface)
}

pub(crate) fn display_gpu_get_info(fd: RawFd) -> Result<DisplayGpuInfo, i32> {
    let mut info = DisplayGpuInfo::default();
    ioctl_with_mut(fd, DISPLAY_IOCTL_GPU_GET_INFO, &mut info)?;
    if info.version != DISPLAY_GPU_ABI_VERSION
        || !matches!(
            info.flags,
            DISPLAY_GPU_INFO_FLAG_STAGED_COPY | DISPLAY_GPU_INFO_FLAG_DIRECT_DMABUF
        )
        || info.atlas_width == 0
        || info.atlas_height == 0
        || info.atlas_stride_bytes < info.atlas_width.saturating_mul(4)
        || info.slot_count != driver_domain_protocol::DVM_GPU_RENDER_MAX_IN_FLIGHT
        || info.max_commands != driver_domain_protocol::DVM_GPU_RENDER_MAX_COMMANDS
        || info.max_batch_bytes as usize != driver_domain_protocol::DVM_GPU_RENDER_MAX_BATCH_BYTES
        || info.generation == 0
        || info.context_id == 0
        || info.context_epoch == 0
        || info.prime_fence_value == 0
        || info.prime_duration_ns == 0
        || info.prime_duration_ns
            > u64::from(driver_domain_protocol::DVM_GPU_PIPELINE_PRIME_MAX_US) * 1_000
    {
        return Err(EINVAL);
    }
    Ok(info)
}

pub(crate) fn display_create_gpu_atlas_surface(
    fd: RawFd,
    info: DisplayGpuInfo,
) -> Result<DisplaySurfaceCreate, i32> {
    let mut surface =
        DisplaySurfaceCreate::request(info.atlas_width, info.atlas_height, PIXEL_FORMAT_BGRA8888);
    surface.flags = DISPLAY_SURFACE_FLAG_GPU_ATLAS;
    ioctl_with_mut(fd, DISPLAY_IOCTL_CREATE_SURFACE, &mut surface)?;
    if surface.flags != DISPLAY_SURFACE_FLAG_GPU_ATLAS
        || surface.handle == 0
        || surface.bytes_per_pixel != 4
        || surface.stride_bytes != info.atlas_stride_bytes
        || surface.reserved >= info.slot_count
        || surface.mapping_len == 0
        || surface.generation != info.generation
    {
        return Err(EINVAL);
    }
    Ok(surface)
}

pub(crate) fn display_gpu_submit(
    fd: RawFd,
    surface_handle: u32,
    damage: &[driver_domain_protocol::DvmGpuAtlasDamage],
    batch: &[u8],
) -> Result<(), i32> {
    if batch.is_empty()
        || batch.len() > driver_domain_protocol::DVM_GPU_RENDER_MAX_BATCH_BYTES
        || damage.len() > driver_domain_protocol::DVM_GPU_ATLAS_MAX_DAMAGE_RECTS as usize
    {
        return Err(EINVAL);
    }
    let damage: Vec<DisplayGpuDamage> = damage
        .iter()
        .map(|rect| DisplayGpuDamage {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        })
        .collect();
    let mut request = DisplayGpuSubmitRequest {
        surface_handle,
        flags: DISPLAY_GPU_SUBMIT_FLAG_STAGED_COPY,
        reserved: 0,
        batch_ptr: batch.as_ptr() as u64,
        batch_len: u32::try_from(batch.len()).map_err(|_| EINVAL)?,
        damage_count: u32::try_from(damage.len()).map_err(|_| EINVAL)?,
        damage_ptr: if damage.is_empty() {
            0
        } else {
            damage.as_ptr() as u64
        },
    };
    ioctl_with_mut(fd, DISPLAY_IOCTL_GPU_SUBMIT, &mut request)
}

pub(crate) fn display_gpu_query_completion(
    fd: RawFd,
    surface_handle: u32,
) -> Result<DisplayGpuCompletionQuery, i32> {
    let mut query = DisplayGpuCompletionQuery {
        surface_handle,
        ..DisplayGpuCompletionQuery::default()
    };
    ioctl_with_mut(fd, DISPLAY_IOCTL_GPU_QUERY_COMPLETION, &mut query)?;
    Ok(query)
}

pub(crate) fn display_present(fd: RawFd, surface_handle: u32) -> Result<(), i32> {
    let mut request = DisplayPresentRequest::new(surface_handle);
    ioctl_with_mut(fd, DISPLAY_IOCTL_PRESENT, &mut request)
}

pub(crate) fn display_present_rect(
    fd: RawFd,
    surface_handle: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<(), i32> {
    let mut request = DisplayPresentRectRequest::new(surface_handle, x, y, width, height);
    ioctl_with_mut(fd, DISPLAY_IOCTL_PRESENT_RECT, &mut request)
}

pub(crate) fn map_surface(surface_fd: RawFd, mapping_len: usize) -> Result<SurfaceMapping, i32> {
    if mapping_len == 0
        || mapping_len > MAX_SURFACE_MAPPING_BYTES
        || !mapping_len.is_multiple_of(PAGE_SIZE)
        || !mapping_len.is_multiple_of(size_of::<u32>())
    {
        return Err(22);
    }
    let mapped = mmap(
        core::ptr::null_mut(),
        mapping_len,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        surface_fd,
        0,
    )?;
    let base = NonNull::new(mapped.cast::<u32>()).ok_or(22)?;
    Ok(SurfaceMapping {
        base,
        len_bytes: mapping_len,
    })
}

pub(crate) fn console_get_state(fd: RawFd) -> Result<ConsoleStateInfo, i32> {
    let mut state = ConsoleStateInfo::default();
    ioctl_with_mut(fd, CONSOLE_IOCTL_GET_STATE, &mut state)?;
    Ok(state)
}

pub(crate) fn console_snapshot_sessions(
    fd: RawFd,
    sessions: &mut [ConsoleSessionInfo],
) -> Result<usize, i32> {
    let mut request =
        ConsoleSnapshotSessionsRequest::new(sessions.as_mut_ptr() as u64, sessions.len() as u64);
    ioctl_with_mut(fd, CONSOLE_IOCTL_SNAPSHOT_SESSIONS, &mut request)?;
    let count = usize::try_from(request.count).unwrap_or(sessions.len());
    Ok(count.min(sessions.len()))
}

pub(crate) fn console_snapshot_session_output(
    fd: RawFd,
    session_handle: ConsoleSessionHandle,
    bytes: &mut [u8],
) -> Result<usize, i32> {
    let mut request = ConsoleSnapshotSessionOutputRequest::new(
        session_handle,
        bytes.as_mut_ptr() as u64,
        bytes.len() as u64,
    );
    ioctl_with_mut(fd, CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT, &mut request)?;
    let count = usize::try_from(request.count).unwrap_or(bytes.len());
    Ok(count.min(bytes.len()))
}

/// Block until the console graph differs from `generation`, for at most
/// `wait`. Returns the generation the broker reports; an unchanged value means
/// the wait re-armed rather than observed a change.
pub(crate) fn console_wait_graph(
    fd: RawFd,
    generation: u64,
    wait: std::time::Duration,
) -> Result<u64, i32> {
    let mut request = ConsoleWaitGraphRequest {
        generation,
        wait_ms: u32::try_from(wait.as_millis()).unwrap_or(u32::MAX),
        reserved: 0,
    };
    ioctl_with_mut(fd, CONSOLE_IOCTL_WAIT_GRAPH, &mut request)?;
    Ok(request.generation)
}

pub(crate) fn console_set_focus(
    fd: RawFd,
    session_handle: ConsoleSessionHandle,
) -> Result<(), i32> {
    let mut request = ConsoleSetFocusRequest::new(session_handle);
    ioctl_with_mut(fd, CONSOLE_IOCTL_SET_FOCUS, &mut request)
}

pub(crate) fn console_send_input_event(
    fd: RawFd,
    session_handle: ConsoleSessionHandle,
    event: InputEvent,
) -> Result<(), i32> {
    let mut request = ConsoleSendInputEventRequest::new(session_handle, event);
    ioctl_with_mut(fd, CONSOLE_IOCTL_SEND_INPUT_EVENT, &mut request)
}

fn open_device(path: &str, flags: usize) -> Result<OwnedFd, i32> {
    let path = CString::new(path).map_err(|_| 22)?;
    let raw_fd = openat(AT_FDCWD, &path, flags, 0)?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    Ok(fd)
}

fn open_first_host_device(
    directory: &str,
    prefix: &str,
    flags: usize,
    preferred_path: &str,
) -> Result<OwnedFd, i32> {
    if let Ok(fd) = open_device(preferred_path, flags) {
        return Ok(fd);
    }
    let mut last_err = ENOENT;
    for path in enumerate_host_device_paths(directory, prefix, preferred_path) {
        if path == preferred_path {
            continue;
        }
        match open_device(path.as_str(), flags) {
            Ok(fd) => return Ok(fd),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

fn open_all_host_devices(
    directory: &str,
    prefix: &str,
    flags: usize,
    preferred_path: &str,
) -> Result<Vec<OwnedFd>, i32> {
    let mut opened = Vec::new();
    let mut last_err = ENOENT;
    if let Ok(fd) = open_device(preferred_path, flags) {
        opened.push(fd);
        last_err = 0;
    }
    for path in enumerate_host_device_paths(directory, prefix, preferred_path) {
        if path == preferred_path {
            continue;
        }
        match open_device(path.as_str(), flags) {
            Ok(fd) => opened.push(fd),
            Err(err) => last_err = err,
        }
    }
    if opened.is_empty() {
        return Err(last_err);
    }
    Ok(opened)
}

fn enumerate_host_device_paths(directory: &str, prefix: &str, preferred_path: &str) -> Vec<String> {
    let mut paths = Vec::new();
    paths.push(String::from(preferred_path));
    if let Ok(entries) = fs::read_dir(directory) {
        let mut discovered = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.starts_with(prefix))
            .map(|name| format!("{directory}/{name}"))
            .collect::<Vec<_>>();
        discovered.sort();
        for path in discovered {
            if !paths.iter().any(|existing| existing == &path) {
                paths.push(path);
            }
        }
    }
    paths
}

pub(crate) fn running_on_rustos() -> bool {
    static IS_RUSTOS: OnceLock<bool> = OnceLock::new();
    *IS_RUSTOS.get_or_init(|| open_device(console_abi::CONSOLE_PATH, O_RDWR).is_ok())
}

fn ioctl_with_mut<T>(fd: RawFd, request: usize, arg: &mut T) -> Result<(), i32> {
    let result = unsafe { syscall3(SYS_IOCTL, fd as usize, request, arg as *mut T as usize) };
    syscall_unit(result)
}

fn openat(dirfd: isize, path: &CString, flags: usize, mode: usize) -> Result<RawFd, i32> {
    let result = unsafe {
        syscall4(
            SYS_OPENAT,
            dirfd as usize,
            path.as_ptr() as *const c_char as usize,
            flags,
            mode,
        )
    };
    syscall_fd(result)
}

fn read(fd: RawFd, buffer: *mut c_void, len: usize) -> Result<usize, i32> {
    let result = unsafe { syscall3(SYS_READ, fd as usize, buffer as usize, len) };
    syscall_usize(result)
}

fn mmap(
    requested_addr: *mut c_void,
    len: usize,
    prot: usize,
    flags: usize,
    fd: RawFd,
    offset: u64,
) -> Result<*mut c_void, i32> {
    let result = unsafe {
        syscall6(
            SYS_MMAP,
            requested_addr as usize,
            len,
            prot,
            flags,
            fd as usize,
            offset as usize,
        )
    };
    if is_syscall_error(result) {
        return Err(errno_from_result(result));
    }
    Ok(result as *mut c_void)
}

fn munmap(start: *mut c_void, len: usize) -> Result<(), i32> {
    let result = unsafe { syscall2(SYS_MUNMAP, start as usize, len) };
    syscall_unit(result)
}

fn syscall_fd(result: isize) -> Result<RawFd, i32> {
    let value = syscall_usize(result)?;
    i32::try_from(value).map_err(|_| 22)
}

fn syscall_usize(result: isize) -> Result<usize, i32> {
    if is_syscall_error(result) {
        return Err(errno_from_result(result));
    }
    usize::try_from(result).map_err(|_| 22)
}

fn syscall_unit(result: isize) -> Result<(), i32> {
    if is_syscall_error(result) {
        return Err(errno_from_result(result));
    }
    Ok(())
}

const fn is_syscall_error(result: isize) -> bool {
    result < 0 && result >= -4095
}

const fn errno_from_result(result: isize) -> i32 {
    (-result) as i32
}

unsafe fn syscall2(number: usize, arg0: usize, arg1: usize) -> isize {
    if let Some(result) = unsafe {
        rustos_svc_runtime::syscall::try_fast_ipc(
            number as u64,
            [arg0 as u64, arg1 as u64, 0, 0, 0, 0],
        )
    } {
        return result as isize;
    }
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            in("rsi") arg1 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

unsafe fn syscall0(number: usize) -> isize {
    if let Some(result) =
        unsafe { rustos_svc_runtime::syscall::try_fast_ipc(number as u64, [0; 6]) }
    {
        return result as isize;
    }
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

unsafe fn syscall1(number: usize, arg0: usize) -> isize {
    if let Some(result) = unsafe {
        rustos_svc_runtime::syscall::try_fast_ipc(number as u64, [arg0 as u64, 0, 0, 0, 0, 0])
    } {
        return result as isize;
    }
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

mod input;
pub(crate) use self::input::*;

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests;


unsafe fn syscall3(number: usize, arg0: usize, arg1: usize, arg2: usize) -> isize {
    if let Some(result) = unsafe {
        rustos_svc_runtime::syscall::try_fast_ipc(
            number as u64,
            [arg0 as u64, arg1 as u64, arg2 as u64, 0, 0, 0],
        )
    } {
        return result as isize;
    }
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            in("rsi") arg1 as isize,
            in("rdx") arg2 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

unsafe fn syscall4(number: usize, arg0: usize, arg1: usize, arg2: usize, arg3: usize) -> isize {
    if let Some(result) = unsafe {
        rustos_svc_runtime::syscall::try_fast_ipc(
            number as u64,
            [arg0 as u64, arg1 as u64, arg2 as u64, arg3 as u64, 0, 0],
        )
    } {
        return result as isize;
    }
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            in("rsi") arg1 as isize,
            in("rdx") arg2 as isize,
            in("r10") arg3 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

unsafe fn syscall6(
    number: usize,
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> isize {
    if let Some(result) = unsafe {
        rustos_svc_runtime::syscall::try_fast_ipc(
            number as u64,
            [
                arg0 as u64,
                arg1 as u64,
                arg2 as u64,
                arg3 as u64,
                arg4 as u64,
                arg5 as u64,
            ],
        )
    } {
        return result as isize;
    }
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            in("rsi") arg1 as isize,
            in("rdx") arg2 as isize,
            in("r10") arg3 as isize,
            in("r8") arg4 as isize,
            in("r9") arg5 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}
