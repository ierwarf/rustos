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

pub const CONSOLE_IOCTL_GET_STATE: u64 = ioctl::ior::<ConsoleStateInfo>(CONSOLE_IOCTL_TYPE, 1);
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
