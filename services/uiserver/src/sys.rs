use core::arch::asm;
use core::ffi::{c_char, c_void};
use core::mem::size_of;
use core::ptr::NonNull;
use core::slice;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;

use keyboard_core::{KeyAction, KeyCode, KeyboardDriver};
use rustos_user_abi::{console as console_abi, device as device_abi};

use console_abi::{
    ConsoleSendInputEventRequest, ConsoleSetFocusRequest, ConsoleSnapshotSessionOutputRequest,
    ConsoleSnapshotSessionsRequest,
};
pub(crate) use console_abi::{ConsoleSessionInfo, ConsoleStateInfo};
pub(crate) use device_abi::{
    DisplayInfo, DisplaySurfaceCreate, InputEvent, INPUT_ACTION_NONE, INPUT_ACTION_PRESSED,
    INPUT_ACTION_RELEASED, INPUT_ACTION_REPEATED, INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON,
    INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION, INPUT_KIND_POINTER_SCROLL,
    PIXEL_FORMAT_BGRA8888, POINTER_BUTTON_LEFT, POINTER_BUTTON_MIDDLE, POINTER_BUTTON_RIGHT,
    POINTER_BUTTON_X1, POINTER_BUTTON_X2,
};
use device_abi::{DisplayPresentRectRequest, DisplayPresentRequest};

const SYS_READ: usize = 0;
const SYS_MMAP: usize = 9;
const SYS_MUNMAP: usize = 11;
const SYS_IOCTL: usize = 16;
const SYS_OPENAT: usize = 257;

const AT_FDCWD: isize = -100;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_NONBLOCK: usize = 0o4000;

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;

const MAP_SHARED: usize = 0x01;
const PAGE_SIZE: usize = 4096;
const MAX_SURFACE_MAPPING_BYTES: usize = 128 * 1024 * 1024;
const MAX_SHARED_FD_MAPPING_BYTES: usize = 64 * 1024 * 1024;
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
const EAGAIN: i32 = 11;
pub(crate) const ENOENT: i32 = 2;
pub(crate) const EINVAL: i32 = 22;
pub(crate) const ESTALE: i32 = 116;
pub(crate) type ConsoleSessionHandle = u64;
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

const DISPLAY_IOCTL_GET_INFO: usize = device_abi::DISPLAY_IOCTL_GET_INFO as usize;
const DISPLAY_IOCTL_CREATE_SURFACE: usize = device_abi::DISPLAY_IOCTL_CREATE_SURFACE as usize;
const DISPLAY_IOCTL_PRESENT: usize = device_abi::DISPLAY_IOCTL_PRESENT as usize;
const DISPLAY_IOCTL_PRESENT_RECT: usize = device_abi::DISPLAY_IOCTL_PRESENT_RECT as usize;
const CONSOLE_IOCTL_GET_STATE: usize = console_abi::CONSOLE_IOCTL_GET_STATE as usize;
const CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT: usize =
    console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT as usize;
const CONSOLE_IOCTL_SET_FOCUS: usize = console_abi::CONSOLE_IOCTL_SET_FOCUS as usize;
const CONSOLE_IOCTL_SEND_INPUT_EVENT: usize = console_abi::CONSOLE_IOCTL_SEND_INPUT_EVENT as usize;
const CONSOLE_IOCTL_SNAPSHOT_SESSIONS: usize =
    console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSIONS as usize;

pub(crate) struct SurfaceMapping {
    base: NonNull<u32>,
    len_bytes: usize,
}

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

pub(crate) struct SharedFdMapping {
    base: NonNull<u8>,
    len_bytes: usize,
}

unsafe impl Send for SharedFdMapping {}
unsafe impl Sync for SharedFdMapping {}

impl SharedFdMapping {
    pub(crate) fn bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.base.as_ptr(), self.len_bytes) }
    }

    pub(crate) fn len_bytes(&self) -> usize {
        self.len_bytes
    }
}

impl Drop for SharedFdMapping {
    fn drop(&mut self) {
        let _ = munmap(self.base.as_ptr().cast::<c_void>(), self.len_bytes);
    }
}

pub(crate) fn open_display() -> Result<OwnedFd, i32> {
    open_first_device("/dev/dri", "card", O_RDWR, "/dev/dri/card0")
}

pub(crate) fn open_console() -> Result<OwnedFd, i32> {
    open_device(console_abi::CONSOLE_PATH, O_RDWR)
}

pub(crate) fn open_input() -> Result<Vec<OwnedFd>, i32> {
    if running_on_rustos() {
        return open_device(device_abi::INPUT_PATH, O_RDONLY | O_NONBLOCK).map(|fd| vec![fd]);
    }
    open_all_devices(
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
        thread::spawn(move || {
            while let Ok(message) = receiver.recv() {
                observability_client::info!("uiserver", service, "{}", message);
            }
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
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("RUSTOS_UI_PROFILE") {
        Ok(value) => matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"),
        Err(_) => false,
    })
}

pub(crate) fn boot_line(message: &str) {
    if !boot_trace_enabled() {
        return;
    }
    let _ = std::io::stderr().write_all(message.as_bytes());
    let _ = std::io::stderr().write_all(b"\n");
}

pub(crate) fn profile_line(message: &str) {
    if !ui_profile_enabled() {
        return;
    }
    let _ = std::io::stderr().write_all(message.as_bytes());
    let _ = std::io::stderr().write_all(b"\n");
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
        || mapping_len % PAGE_SIZE != 0
        || mapping_len % size_of::<u32>() != 0
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

pub(crate) fn map_shared_fd_readable(
    fd: RawFd,
    mapping_len: usize,
) -> Result<SharedFdMapping, i32> {
    if mapping_len == 0 || mapping_len > MAX_SHARED_FD_MAPPING_BYTES {
        return Err(22);
    }
    let mapped = mmap(
        core::ptr::null_mut(),
        mapping_len,
        PROT_READ,
        MAP_SHARED,
        fd,
        0,
    )?;
    let base = NonNull::new(mapped.cast::<u8>()).ok_or(22)?;
    Ok(SharedFdMapping {
        base,
        len_bytes: mapping_len,
    })
}

pub(crate) fn read_input(fds: &[OwnedFd], events: &mut [InputEvent]) -> Result<usize, i32> {
    if fds.is_empty() || events.is_empty() {
        return Ok(0);
    }
    if running_on_rustos() {
        let fd = fds[0].as_raw_fd();
        let bytes = match read(
            fd,
            events.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of_val(events),
        ) {
            Ok(bytes) => bytes,
            Err(EAGAIN) => return Ok(0),
            Err(err) => return Err(err),
        };
        if bytes == 0 {
            return Ok(0);
        }
        if bytes % size_of::<InputEvent>() != 0 {
            return Err(5);
        }
        return Ok(bytes / size_of::<InputEvent>());
    }

    let mut raw_events = [LinuxInputEvent::default(); MAX_EVDEV_EVENTS_PER_READ];
    let mut written = 0usize;
    let mut state = input_translation_state().lock().unwrap();
    for fd in fds {
        if written >= events.len() {
            break;
        }

        let bytes = match read(
            fd.as_raw_fd(),
            raw_events.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of_val(&raw_events),
        ) {
            Ok(bytes) => bytes,
            Err(EAGAIN) => continue,
            Err(err) => return Err(err),
        };
        if bytes == 0 {
            continue;
        }
        if bytes % size_of::<LinuxInputEvent>() != 0 {
            return Err(5);
        }
        let count = bytes / size_of::<LinuxInputEvent>();
        for raw in &raw_events[..count] {
            if written >= events.len() {
                break;
            }
            if let Some(event) = translate_linux_input_event(&mut state, *raw) {
                events[written] = event;
                written += 1;
            }
        }
    }
    Ok(written)
}

fn translate_linux_input_event(
    state: &mut InputTranslationState,
    raw: LinuxInputEvent,
) -> Option<InputEvent> {
    match raw.kind {
        EV_KEY => {
            if raw.code >= BTN_LEFT {
                let code = linux_pointer_button_to_rustos(raw.code)?;
                return Some(InputEvent {
                    kind: INPUT_KIND_POINTER_BUTTON,
                    action: linux_key_value_to_action(raw.value)?,
                    code,
                    value0: 0,
                    value1: 0,
                    modifiers: 0,
                    text: 0,
                });
            }

            let key_code = linux_key_code_to_rustos(u32::from(raw.code))?;
            state
                .keyboard
                .inject_key_transition(key_code, raw.value == 0);
            let keyboard_event = state.keyboard.pop_event()?;
            Some(InputEvent {
                kind: INPUT_KIND_KEYBOARD,
                action: keyboard_action_to_input(keyboard_event.action),
                code: keyboard_event.code as u32,
                value0: 0,
                value1: 0,
                modifiers: keyboard_event.modifiers.bits() as u32,
                text: keyboard_event.text.unwrap_or(0) as u32,
            })
        }
        EV_REL => match raw.code {
            REL_X => Some(InputEvent {
                kind: INPUT_KIND_POINTER_MOTION,
                action: INPUT_ACTION_NONE,
                code: 0,
                value0: raw.value,
                value1: 0,
                modifiers: 0,
                text: 0,
            }),
            REL_Y => Some(InputEvent {
                kind: INPUT_KIND_POINTER_MOTION,
                action: INPUT_ACTION_NONE,
                code: 0,
                value0: 0,
                value1: raw.value,
                modifiers: 0,
                text: 0,
            }),
            REL_WHEEL => Some(InputEvent {
                kind: INPUT_KIND_POINTER_SCROLL,
                action: INPUT_ACTION_NONE,
                code: 0,
                value0: raw.value,
                value1: 0,
                modifiers: 0,
                text: 0,
            }),
            REL_HWHEEL => Some(InputEvent {
                kind: INPUT_KIND_POINTER_SCROLL,
                action: INPUT_ACTION_NONE,
                code: 0,
                value0: 0,
                value1: raw.value,
                modifiers: 0,
                text: 0,
            }),
            _ => None,
        },
        EV_ABS => match raw.code {
            ABS_X => {
                state.abs_x = raw.value;
                Some(InputEvent {
                    kind: INPUT_KIND_POINTER_POSITION,
                    action: INPUT_ACTION_NONE,
                    code: 0,
                    value0: state.abs_x,
                    value1: state.abs_y,
                    modifiers: 0,
                    text: 0,
                })
            }
            ABS_Y => {
                state.abs_y = raw.value;
                Some(InputEvent {
                    kind: INPUT_KIND_POINTER_POSITION,
                    action: INPUT_ACTION_NONE,
                    code: 0,
                    value0: state.abs_x,
                    value1: state.abs_y,
                    modifiers: 0,
                    text: 0,
                })
            }
            _ => None,
        },
        EV_SYN if raw.code == SYN_REPORT => None,
        _ => None,
    }
}

fn linux_key_value_to_action(value: i32) -> Option<u16> {
    match value {
        0 => Some(INPUT_ACTION_RELEASED),
        1 => Some(INPUT_ACTION_PRESSED),
        2 => Some(INPUT_ACTION_REPEATED),
        _ => None,
    }
}

fn keyboard_action_to_input(action: KeyAction) -> u16 {
    match action {
        KeyAction::Pressed => INPUT_ACTION_PRESSED,
        KeyAction::Released => INPUT_ACTION_RELEASED,
        KeyAction::Repeated => INPUT_ACTION_REPEATED,
    }
}

fn linux_pointer_button_to_rustos(code: u16) -> Option<u32> {
    Some(match code {
        BTN_LEFT => POINTER_BUTTON_LEFT,
        BTN_RIGHT => POINTER_BUTTON_RIGHT,
        BTN_MIDDLE => POINTER_BUTTON_MIDDLE,
        BTN_SIDE => POINTER_BUTTON_X1,
        BTN_EXTRA => POINTER_BUTTON_X2,
        _ => return None,
    })
}

fn linux_key_code_to_rustos(code: u32) -> Option<KeyCode> {
    Some(match code {
        1 => KeyCode::Escape,
        2 => KeyCode::Digit1,
        3 => KeyCode::Digit2,
        4 => KeyCode::Digit3,
        5 => KeyCode::Digit4,
        6 => KeyCode::Digit5,
        7 => KeyCode::Digit6,
        8 => KeyCode::Digit7,
        9 => KeyCode::Digit8,
        10 => KeyCode::Digit9,
        11 => KeyCode::Digit0,
        12 => KeyCode::Minus,
        13 => KeyCode::Equal,
        14 => KeyCode::Backspace,
        15 => KeyCode::Tab,
        16 => KeyCode::Q,
        17 => KeyCode::W,
        18 => KeyCode::E,
        19 => KeyCode::R,
        20 => KeyCode::T,
        21 => KeyCode::Y,
        22 => KeyCode::U,
        23 => KeyCode::I,
        24 => KeyCode::O,
        25 => KeyCode::P,
        26 => KeyCode::LeftBracket,
        27 => KeyCode::RightBracket,
        28 => KeyCode::Enter,
        29 => KeyCode::LeftCtrl,
        30 => KeyCode::A,
        31 => KeyCode::S,
        32 => KeyCode::D,
        33 => KeyCode::F,
        34 => KeyCode::G,
        35 => KeyCode::H,
        36 => KeyCode::J,
        37 => KeyCode::K,
        38 => KeyCode::L,
        39 => KeyCode::Semicolon,
        40 => KeyCode::Apostrophe,
        41 => KeyCode::Grave,
        42 => KeyCode::LeftShift,
        43 => KeyCode::Backslash,
        44 => KeyCode::Z,
        45 => KeyCode::X,
        46 => KeyCode::C,
        47 => KeyCode::V,
        48 => KeyCode::B,
        49 => KeyCode::N,
        50 => KeyCode::M,
        51 => KeyCode::Comma,
        52 => KeyCode::Dot,
        53 => KeyCode::Slash,
        54 => KeyCode::RightShift,
        55 => KeyCode::NumpadStar,
        56 => KeyCode::LeftAlt,
        57 => KeyCode::Space,
        58 => KeyCode::CapsLock,
        59 => KeyCode::F1,
        60 => KeyCode::F2,
        61 => KeyCode::F3,
        62 => KeyCode::F4,
        63 => KeyCode::F5,
        64 => KeyCode::F6,
        65 => KeyCode::F7,
        66 => KeyCode::F8,
        67 => KeyCode::F9,
        68 => KeyCode::F10,
        69 => KeyCode::NumLock,
        70 => KeyCode::ScrollLock,
        71 => KeyCode::Numpad7,
        72 => KeyCode::Numpad8,
        73 => KeyCode::Numpad9,
        74 => KeyCode::NumpadMinus,
        75 => KeyCode::Numpad4,
        76 => KeyCode::Numpad5,
        77 => KeyCode::Numpad6,
        78 => KeyCode::NumpadPlus,
        79 => KeyCode::Numpad1,
        80 => KeyCode::Numpad2,
        81 => KeyCode::Numpad3,
        82 => KeyCode::Numpad0,
        83 => KeyCode::NumpadDot,
        87 => KeyCode::F11,
        88 => KeyCode::F12,
        96 => KeyCode::NumpadEnter,
        97 => KeyCode::RightCtrl,
        98 => KeyCode::NumpadSlash,
        100 => KeyCode::RightAlt,
        102 => KeyCode::Home,
        103 => KeyCode::ArrowUp,
        104 => KeyCode::PageUp,
        105 => KeyCode::ArrowLeft,
        106 => KeyCode::ArrowRight,
        107 => KeyCode::End,
        108 => KeyCode::ArrowDown,
        109 => KeyCode::PageDown,
        110 => KeyCode::Insert,
        111 => KeyCode::Delete,
        125 => KeyCode::LeftMeta,
        126 => KeyCode::RightMeta,
        127 => KeyCode::Menu,
        _ => return None,
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

fn open_first_device(
    directory: &str,
    prefix: &str,
    flags: usize,
    fallback: &str,
) -> Result<OwnedFd, i32> {
    if let Ok(fd) = open_device(fallback, flags) {
        return Ok(fd);
    }
    let mut last_err = ENOENT;
    for path in enumerate_device_paths(directory, prefix, fallback) {
        if path == fallback {
            continue;
        }
        match open_device(path.as_str(), flags) {
            Ok(fd) => return Ok(fd),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

fn open_all_devices(
    directory: &str,
    prefix: &str,
    flags: usize,
    fallback: &str,
) -> Result<Vec<OwnedFd>, i32> {
    let mut opened = Vec::new();
    let mut last_err = ENOENT;
    if let Ok(fd) = open_device(fallback, flags) {
        opened.push(fd);
        last_err = 0;
    }
    for path in enumerate_device_paths(directory, prefix, fallback) {
        if path == fallback {
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

fn enumerate_device_paths(directory: &str, prefix: &str, fallback: &str) -> Vec<String> {
    let mut paths = Vec::new();
    paths.push(String::from(fallback));
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

fn running_on_rustos() -> bool {
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

unsafe fn syscall3(number: usize, arg0: usize, arg1: usize, arg2: usize) -> isize {
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
