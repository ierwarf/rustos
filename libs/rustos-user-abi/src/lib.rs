#![no_std]

pub mod ioctl {
    pub const NRBITS: u64 = 8;
    pub const TYPEBITS: u64 = 8;
    pub const SIZEBITS: u64 = 14;

    pub const NRSHIFT: u64 = 0;
    pub const TYPESHIFT: u64 = NRSHIFT + NRBITS;
    pub const SIZESHIFT: u64 = TYPESHIFT + TYPEBITS;
    pub const DIRSHIFT: u64 = SIZESHIFT + SIZEBITS;

    pub const NONE: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const READ: u64 = 2;

    pub const fn ioc(dir: u64, type_: u8, nr: u8, size: u64) -> u64 {
        (dir << DIRSHIFT)
            | ((type_ as u64) << TYPESHIFT)
            | ((nr as u64) << NRSHIFT)
            | (size << SIZESHIFT)
    }

    pub const fn ior<T>(type_: u8, nr: u8) -> u64 {
        ioc(READ, type_, nr, core::mem::size_of::<T>() as u64)
    }

    pub const fn iow<T>(type_: u8, nr: u8) -> u64 {
        ioc(WRITE, type_, nr, core::mem::size_of::<T>() as u64)
    }

    pub const fn iowr<T>(type_: u8, nr: u8) -> u64 {
        ioc(READ | WRITE, type_, nr, core::mem::size_of::<T>() as u64)
    }
}

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
    use crate::{device::InputEvent, ioctl};

    pub const CONSOLE_PATH: &str = "/dev/console0";
    pub const MAX_CONSOLE_SESSIONS: usize = 32;
    pub const CONSOLE_SESSION_TITLE_CAPACITY: usize = 48;
    pub const CONSOLE_SESSION_PATH_CAPACITY: usize = 64;
    pub const CONSOLE_IOCTL_TYPE: u8 = b'C';

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
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

    impl ConsoleSnapshotSessionsRequest {
        pub const fn new(sessions_ptr: u64, capacity: u64) -> Self {
            Self {
                sessions_ptr,
                capacity,
                count: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSnapshotSessionOutputRequest {
        pub session_handle: u64,
        pub bytes_ptr: u64,
        pub capacity: u64,
        pub count: u64,
    }

    impl ConsoleSnapshotSessionOutputRequest {
        pub const fn new(session_handle: u64, bytes_ptr: u64, capacity: u64) -> Self {
            Self {
                session_handle,
                bytes_ptr,
                capacity,
                count: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSetFocusRequest {
        pub session_handle: u64,
    }

    impl ConsoleSetFocusRequest {
        pub const fn new(session_handle: u64) -> Self {
            Self { session_handle }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSendInputEventRequest {
        pub session_handle: u64,
        pub event: InputEvent,
    }

    impl ConsoleSendInputEventRequest {
        pub const fn new(session_handle: u64, event: InputEvent) -> Self {
            Self {
                session_handle,
                event,
            }
        }
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

    impl ConsoleCreateSessionRequest {
        pub const fn new(
            program_id: u32,
            title_ptr: u64,
            title_len: u64,
            exec_path_ptr: u64,
            exec_path_len: u64,
        ) -> Self {
            Self {
                program_id,
                reserved: 0,
                title_ptr,
                title_len,
                exec_path_ptr,
                exec_path_len,
                session_handle: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleCloseSessionRequest {
        pub session_handle: u64,
    }

    impl ConsoleCloseSessionRequest {
        pub const fn new(session_handle: u64) -> Self {
            Self { session_handle }
        }
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

    impl ConsoleSetSessionStateRequest {
        pub const fn new(session_handle: u64, state: u16) -> Self {
            Self {
                session_handle,
                state,
                reserved: 0,
            }
        }
    }

    pub const CONSOLE_IOCTL_GET_STATE: u64 =
        ioctl::ior::<ConsoleStateInfo>(CONSOLE_IOCTL_TYPE, 1);
    pub const CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT: u64 =
        ioctl::iowr::<ConsoleSnapshotSessionOutputRequest>(CONSOLE_IOCTL_TYPE, 2);
    pub const CONSOLE_IOCTL_SET_FOCUS: u64 =
        ioctl::iow::<ConsoleSetFocusRequest>(CONSOLE_IOCTL_TYPE, 3);
    pub const CONSOLE_IOCTL_SEND_INPUT_EVENT: u64 =
        ioctl::iow::<ConsoleSendInputEventRequest>(CONSOLE_IOCTL_TYPE, 4);
    pub const CONSOLE_IOCTL_SNAPSHOT_SESSIONS: u64 =
        ioctl::iowr::<ConsoleSnapshotSessionsRequest>(CONSOLE_IOCTL_TYPE, 5);
    pub const CONSOLE_IOCTL_CREATE_SESSION: u64 =
        ioctl::iowr::<ConsoleCreateSessionRequest>(CONSOLE_IOCTL_TYPE, 6);
    pub const CONSOLE_IOCTL_CLOSE_SESSION: u64 =
        ioctl::iow::<ConsoleCloseSessionRequest>(CONSOLE_IOCTL_TYPE, 7);
    pub const CONSOLE_IOCTL_BIND_CURRENT_SESSION: u64 =
        ioctl::iow::<ConsoleBindCurrentSessionRequest>(CONSOLE_IOCTL_TYPE, 8);
    pub const CONSOLE_IOCTL_SET_SESSION_STATE: u64 =
        ioctl::iow::<ConsoleSetSessionStateRequest>(CONSOLE_IOCTL_TYPE, 9);
}

pub mod device {
    use crate::{ioctl, ui};

    pub const DISPLAY_PATH: &str = "/dev/display0";
    pub const INPUT_PATH: &str = "/dev/input0";
    pub const DISPLAY_IOCTL_TYPE: u8 = b'D';

    pub const DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER: u32 = 1 << 0;
    pub const DISPLAY_INFO_FLAG_PRIMARY_PROVIDER: u32 = 1 << 1;

    pub const PIXEL_FORMAT_BGRA8888: u32 = ui::PIXEL_FORMAT_BGRA8888;
    pub const INPUT_KIND_KEYBOARD: u16 = ui::INPUT_KIND_KEYBOARD;
    pub const INPUT_KIND_POINTER_MOTION: u16 = ui::INPUT_KIND_POINTER_MOTION;
    pub const INPUT_KIND_POINTER_BUTTON: u16 = ui::INPUT_KIND_POINTER_BUTTON;
    pub const INPUT_KIND_POINTER_SCROLL: u16 = ui::INPUT_KIND_POINTER_SCROLL;
    pub const INPUT_KIND_POINTER_POSITION: u16 = ui::INPUT_KIND_POINTER_POSITION;
    pub const INPUT_ACTION_NONE: u16 = ui::INPUT_ACTION_NONE;
    pub const INPUT_ACTION_PRESSED: u16 = ui::INPUT_ACTION_PRESSED;
    pub const INPUT_ACTION_RELEASED: u16 = ui::INPUT_ACTION_RELEASED;
    pub const INPUT_ACTION_REPEATED: u16 = ui::INPUT_ACTION_REPEATED;
    pub const POINTER_BUTTON_LEFT: u32 = ui::POINTER_BUTTON_LEFT;
    pub const POINTER_BUTTON_RIGHT: u32 = ui::POINTER_BUTTON_RIGHT;
    pub const POINTER_BUTTON_MIDDLE: u32 = ui::POINTER_BUTTON_MIDDLE;
    pub const POINTER_BUTTON_X1: u32 = ui::POINTER_BUTTON_X1;
    pub const POINTER_BUTTON_X2: u32 = ui::POINTER_BUTTON_X2;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplayInfo {
        pub width: u32,
        pub height: u32,
        pub stride_bytes: u32,
        pub bytes_per_pixel: u32,
        pub pixel_format: u32,
        pub flags: u32,
        pub generation: u64,
    }

    impl DisplayInfo {
        pub const fn bgra8888(
            width: u32,
            height: u32,
            stride_bytes: u32,
            bytes_per_pixel: u32,
            generation: u64,
            flags: u32,
        ) -> Self {
            Self {
                width,
                height,
                stride_bytes,
                bytes_per_pixel,
                pixel_format: PIXEL_FORMAT_BGRA8888,
                flags,
                generation,
            }
        }

        pub const fn uses_bgra8888(self) -> bool {
            self.bytes_per_pixel == 4 && self.pixel_format == PIXEL_FORMAT_BGRA8888
        }

        pub const fn is_boot_framebuffer(self) -> bool {
            self.flags & DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER != 0
        }

        pub const fn is_primary_provider(self) -> bool {
            self.flags & DISPLAY_INFO_FLAG_PRIMARY_PROVIDER != 0
        }
    }

    pub type InputEvent = ui::UiInputEvent;

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

    impl DisplaySurfaceCreate {
        pub const fn request(width: u32, height: u32, pixel_format: u32) -> Self {
            Self {
                width,
                height,
                pixel_format,
                flags: 0,
                handle: 0,
                bytes_per_pixel: 0,
                stride_bytes: 0,
                reserved: 0,
                mapping_len: 0,
                generation: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplayPresentRequest {
        pub surface_handle: u32,
        pub reserved: u32,
    }

    impl DisplayPresentRequest {
        pub const fn new(surface_handle: u32) -> Self {
            Self {
                surface_handle,
                reserved: 0,
            }
        }
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

    impl DisplayPresentRectRequest {
        pub const fn new(surface_handle: u32, x: u32, y: u32, width: u32, height: u32) -> Self {
            Self {
                surface_handle,
                reserved: 0,
                x,
                y,
                width,
                height,
            }
        }
    }

    pub const DISPLAY_IOCTL_GET_INFO: u64 =
        ioctl::ior::<DisplayInfo>(DISPLAY_IOCTL_TYPE, 1);
    pub const DISPLAY_IOCTL_CREATE_SURFACE: u64 =
        ioctl::iowr::<DisplaySurfaceCreate>(DISPLAY_IOCTL_TYPE, 2);
    pub const DISPLAY_IOCTL_PRESENT: u64 =
        ioctl::iow::<DisplayPresentRequest>(DISPLAY_IOCTL_TYPE, 3);
    pub const DISPLAY_IOCTL_PRESENT_RECT: u64 =
        ioctl::iow::<DisplayPresentRectRequest>(DISPLAY_IOCTL_TYPE, 4);
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::{console, device, ui};

    #[test]
    fn display_abi_layout_is_stable() {
        assert_eq!(size_of::<device::DisplayInfo>(), 32);
        assert_eq!(size_of::<device::DisplaySurfaceCreate>(), 48);
        assert_eq!(size_of::<device::DisplayPresentRequest>(), 8);
        assert_eq!(size_of::<device::DisplayPresentRectRequest>(), 24);
    }

    #[test]
    fn console_and_input_abi_layout_is_stable() {
        assert_eq!(size_of::<ui::UiInputEvent>(), 24);
        assert_eq!(size_of::<console::ConsoleStateInfo>(), 16);
        assert_eq!(size_of::<console::ConsoleSessionInfo>(), 72);
        assert_eq!(size_of::<console::ConsoleCreateSessionRequest>(), 48);
    }
}
