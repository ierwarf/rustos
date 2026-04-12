#![allow(dead_code)]

const LINUX_IOC_NRBITS: u64 = 8;
const LINUX_IOC_TYPEBITS: u64 = 8;
const LINUX_IOC_SIZEBITS: u64 = 14;

const LINUX_IOC_NRSHIFT: u64 = 0;
const LINUX_IOC_TYPESHIFT: u64 = LINUX_IOC_NRSHIFT + LINUX_IOC_NRBITS;
const LINUX_IOC_SIZESHIFT: u64 = LINUX_IOC_TYPESHIFT + LINUX_IOC_TYPEBITS;
const LINUX_IOC_DIRSHIFT: u64 = LINUX_IOC_SIZESHIFT + LINUX_IOC_SIZEBITS;

const LINUX_IOC_NONE: u64 = 0;
const LINUX_IOC_WRITE: u64 = 1;
const LINUX_IOC_READ: u64 = 2;

const fn linux_ioc(dir: u64, type_: u8, nr: u8, size: u64) -> u64 {
    (dir << LINUX_IOC_DIRSHIFT)
        | ((type_ as u64) << LINUX_IOC_TYPESHIFT)
        | ((nr as u64) << LINUX_IOC_NRSHIFT)
        | (size << LINUX_IOC_SIZESHIFT)
}

const fn linux_ior<T>(type_: u8, nr: u8) -> u64 {
    linux_ioc(LINUX_IOC_READ, type_, nr, core::mem::size_of::<T>() as u64)
}

const fn linux_iow<T>(type_: u8, nr: u8) -> u64 {
    linux_ioc(LINUX_IOC_WRITE, type_, nr, core::mem::size_of::<T>() as u64)
}

const fn linux_iowr<T>(type_: u8, nr: u8) -> u64 {
    linux_ioc(
        LINUX_IOC_READ | LINUX_IOC_WRITE,
        type_,
        nr,
        core::mem::size_of::<T>() as u64,
    )
}

pub use nucleus_core::user_abi::UserAbi;

pub mod ui {
    pub const PIXEL_FORMAT_BGRA8888: u32 = 1;

    pub const INPUT_KIND_KEYBOARD: u16 = 1;
    pub const INPUT_KIND_POINTER_MOTION: u16 = 2;
    pub const INPUT_KIND_POINTER_BUTTON: u16 = 3;
    pub const INPUT_KIND_POINTER_SCROLL: u16 = 4;
    pub const INPUT_KIND_POINTER_POSITION: u16 = 5;

    pub const INPUT_ACTION_NONE: u16 = 0;
    pub const INPUT_ACTION_PRESSED: u16 = 1;
    pub const INPUT_ACTION_RELEASED: u16 = 2;
    pub const INPUT_ACTION_REPEATED: u16 = 3;

    pub const POINTER_BUTTON_LEFT: u32 = 1;
    pub const POINTER_BUTTON_RIGHT: u32 = 2;
    pub const POINTER_BUTTON_MIDDLE: u32 = 4;
    pub const POINTER_BUTTON_X1: u32 = 8;
    pub const POINTER_BUTTON_X2: u32 = 16;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct UiDisplayInfo {
        pub width: u32,
        pub height: u32,
        pub stride_bytes: u32,
        pub bytes_per_pixel: u32,
        pub pixel_format: u32,
        pub reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct UiInputEvent {
        pub kind: u16,
        pub action: u16,
        pub code: u32,
        pub value0: i32,
        pub value1: i32,
        pub modifiers: u32,
        pub text: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct UiSurfaceInfo {
        pub address: u64,
        pub len: u64,
        pub width: u32,
        pub height: u32,
        pub stride_bytes: u32,
        pub bytes_per_pixel: u32,
        pub pixel_format: u32,
        pub reserved: u32,
    }
}

pub mod console {
    pub const CONSOLE_PATH: &str = "/dev/console0";
    pub const MAX_CONSOLE_SESSIONS: usize = 32;
    pub const CONSOLE_SESSION_TITLE_CAPACITY: usize = 48;
    pub const CONSOLE_SESSION_PATH_CAPACITY: usize = 64;
    const CONSOLE_IOCTL_TYPE: u8 = b'C';

    pub const CONSOLE_SESSION_STATE_QUEUED: u16 = 1;
    pub const CONSOLE_SESSION_STATE_LOADING_IMAGE: u16 = 2;
    pub const CONSOLE_SESSION_STATE_SPAWNING: u16 = 3;
    pub const CONSOLE_SESSION_STATE_RUNNING: u16 = 4;
    pub const CONSOLE_SESSION_STATE_CLOSING: u16 = 5;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleStateInfo {
        pub focused_session_handle: u64,
        pub session_count: u32,
        pub reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct ConsoleSessionInfo {
        pub session_handle: u64,
        pub state: u16,
        pub focused: u16,
        pub reserved: u32,
        pub output_generation: u64,
        pub title: [u8; CONSOLE_SESSION_TITLE_CAPACITY],
    }

    impl Default for ConsoleSessionInfo {
        fn default() -> Self {
            Self {
                session_handle: 0,
                state: 0,
                focused: 0,
                reserved: 0,
                output_generation: 0,
                title: [0; CONSOLE_SESSION_TITLE_CAPACITY],
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSnapshotSessionsRequest {
        pub sessions_ptr: u64,
        pub capacity: u64,
        pub count: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSnapshotSessionOutputRequest {
        pub session_handle: u64,
        pub bytes_ptr: u64,
        pub capacity: u64,
        pub count: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSetFocusRequest {
        pub session_handle: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSendInputEventRequest {
        pub session_handle: u64,
        pub event: super::device::InputEvent,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleCreateSessionRequest {
        pub program_id: u32,
        pub reserved: u32,
        pub title_ptr: u64,
        pub title_len: u64,
        pub exec_path_ptr: u64,
        pub exec_path_len: u64,
        pub session_handle: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleCloseSessionRequest {
        pub session_handle: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleBindCurrentSessionRequest {
        pub session_handle: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSetSessionStateRequest {
        pub session_handle: u64,
        pub state: u16,
        pub reserved: u16,
    }

    pub const CONSOLE_IOCTL_GET_STATE: u64 =
        super::linux_ior::<ConsoleStateInfo>(CONSOLE_IOCTL_TYPE, 1);
    pub const CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT: u64 =
        super::linux_iowr::<ConsoleSnapshotSessionOutputRequest>(CONSOLE_IOCTL_TYPE, 2);
    pub const CONSOLE_IOCTL_SET_FOCUS: u64 =
        super::linux_iow::<ConsoleSetFocusRequest>(CONSOLE_IOCTL_TYPE, 3);
    pub const CONSOLE_IOCTL_SEND_INPUT_EVENT: u64 =
        super::linux_iow::<ConsoleSendInputEventRequest>(CONSOLE_IOCTL_TYPE, 4);
    pub const CONSOLE_IOCTL_SNAPSHOT_SESSIONS: u64 =
        super::linux_iowr::<ConsoleSnapshotSessionsRequest>(CONSOLE_IOCTL_TYPE, 5);
    pub const CONSOLE_IOCTL_CREATE_SESSION: u64 =
        super::linux_iowr::<ConsoleCreateSessionRequest>(CONSOLE_IOCTL_TYPE, 6);
    pub const CONSOLE_IOCTL_CLOSE_SESSION: u64 =
        super::linux_iow::<ConsoleCloseSessionRequest>(CONSOLE_IOCTL_TYPE, 7);
    pub const CONSOLE_IOCTL_BIND_CURRENT_SESSION: u64 =
        super::linux_iow::<ConsoleBindCurrentSessionRequest>(CONSOLE_IOCTL_TYPE, 8);
    pub const CONSOLE_IOCTL_SET_SESSION_STATE: u64 =
        super::linux_iow::<ConsoleSetSessionStateRequest>(CONSOLE_IOCTL_TYPE, 9);
}

pub mod device {
    pub const DISPLAY_PATH: &str = "/dev/display0";
    pub const INPUT_PATH: &str = "/dev/input0";
    const DISPLAY_IOCTL_TYPE: u8 = b'D';

    pub const PIXEL_FORMAT_BGRA8888: u32 = super::ui::PIXEL_FORMAT_BGRA8888;
    pub const INPUT_KIND_KEYBOARD: u16 = super::ui::INPUT_KIND_KEYBOARD;
    pub const INPUT_KIND_POINTER_MOTION: u16 = super::ui::INPUT_KIND_POINTER_MOTION;
    pub const INPUT_KIND_POINTER_BUTTON: u16 = super::ui::INPUT_KIND_POINTER_BUTTON;
    pub const INPUT_KIND_POINTER_SCROLL: u16 = super::ui::INPUT_KIND_POINTER_SCROLL;
    pub const INPUT_KIND_POINTER_POSITION: u16 = super::ui::INPUT_KIND_POINTER_POSITION;
    pub const INPUT_ACTION_NONE: u16 = super::ui::INPUT_ACTION_NONE;
    pub const INPUT_ACTION_PRESSED: u16 = super::ui::INPUT_ACTION_PRESSED;
    pub const INPUT_ACTION_RELEASED: u16 = super::ui::INPUT_ACTION_RELEASED;
    pub const INPUT_ACTION_REPEATED: u16 = super::ui::INPUT_ACTION_REPEATED;
    pub const POINTER_BUTTON_LEFT: u32 = super::ui::POINTER_BUTTON_LEFT;
    pub const POINTER_BUTTON_RIGHT: u32 = super::ui::POINTER_BUTTON_RIGHT;
    pub const POINTER_BUTTON_MIDDLE: u32 = super::ui::POINTER_BUTTON_MIDDLE;
    pub const POINTER_BUTTON_X1: u32 = super::ui::POINTER_BUTTON_X1;
    pub const POINTER_BUTTON_X2: u32 = super::ui::POINTER_BUTTON_X2;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplayInfo {
        pub width: u32,
        pub height: u32,
        pub stride_bytes: u32,
        pub bytes_per_pixel: u32,
        pub pixel_format: u32,
        pub reserved: u32,
        pub generation: u64,
    }

    pub type InputEvent = super::ui::UiInputEvent;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplaySurfaceCreate {
        pub width: u32,
        pub height: u32,
        pub pixel_format: u32,
        pub flags: u32,
        pub handle: u32,
        pub bytes_per_pixel: u32,
        pub stride_bytes: u32,
        pub reserved: u32,
        pub mapping_len: u64,
        pub generation: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplayPresentRequest {
        pub surface_handle: u32,
        pub reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplayPresentRectRequest {
        pub surface_handle: u32,
        pub reserved: u32,
        pub x: u32,
        pub y: u32,
        pub width: u32,
        pub height: u32,
    }

    pub const DISPLAY_IOCTL_GET_INFO: u64 = super::linux_ior::<DisplayInfo>(DISPLAY_IOCTL_TYPE, 1);
    pub const DISPLAY_IOCTL_CREATE_SURFACE: u64 =
        super::linux_iowr::<DisplaySurfaceCreate>(DISPLAY_IOCTL_TYPE, 2);
    pub const DISPLAY_IOCTL_PRESENT: u64 =
        super::linux_iow::<DisplayPresentRequest>(DISPLAY_IOCTL_TYPE, 3);
    pub const DISPLAY_IOCTL_PRESENT_RECT: u64 =
        super::linux_iow::<DisplayPresentRectRequest>(DISPLAY_IOCTL_TYPE, 4);
}

fn copy_ascii_into(dest: &mut [u8], src: &str) {
    dest.fill(0);
    for (index, byte) in src.bytes().enumerate() {
        if index == dest.len() {
            break;
        }
        dest[index] = match byte {
            b' '..=b'~' => byte,
            _ => b'?',
        };
    }
}
