#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAbi {
    Linux,
    Windows,
}

impl UserAbi {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }
}

pub mod ui {
    pub const PIXEL_FORMAT_BGRA8888: u32 = 1;

    pub const INPUT_KIND_KEYBOARD: u16 = 1;
    pub const INPUT_KIND_POINTER_MOTION: u16 = 2;
    pub const INPUT_KIND_POINTER_BUTTON: u16 = 3;

    pub const INPUT_ACTION_NONE: u16 = 0;
    pub const INPUT_ACTION_PRESSED: u16 = 1;
    pub const INPUT_ACTION_RELEASED: u16 = 2;
    pub const INPUT_ACTION_REPEATED: u16 = 3;

    pub const POINTER_BUTTON_LEFT: u32 = 1;

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

pub mod runtime {
    pub const RUNTIME_PATH: &str = "/dev/runtime0";

    pub const RUNTIME_IOCTL_GET_GENERATION: u64 = 0x5254_0001;
    pub const RUNTIME_IOCTL_SNAPSHOT_PROGRAMS: u64 = 0x5254_0002;
    pub const RUNTIME_IOCTL_SNAPSHOT_RUNNING_PROGRAMS: u64 = 0x5254_0003;
    pub const RUNTIME_IOCTL_REQUEST_LAUNCH: u64 = 0x5254_0004;
    pub const RUNTIME_IOCTL_REQUEST_TERMINATE: u64 = 0x5254_0005;

    pub const LAUNCH_TARGET_SESSION: u16 = 1;
    pub const LAUNCH_TARGET_FIRST_AVAILABLE: u16 = 2;
    pub const LAUNCH_TARGET_ALL_SESSIONS: u16 = 3;

    pub const TERMINATE_TARGET_SESSION: u16 = 1;
    pub const TERMINATE_TARGET_PID: u16 = 2;
    pub const TERMINATE_TARGET_ALL_SESSIONS: u16 = 3;

    pub const RUNNING_PROGRAM_NAME_CAPACITY: usize = 48;
    pub const PROGRAM_NAME_CAPACITY: usize = 48;
    pub const PROGRAM_PATH_CAPACITY: usize = 64;

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct RuntimeProgramInfo {
        pub program_id: u32,
        pub reserved: u32,
        pub weight_micros: u64,
        pub display_name: [u8; PROGRAM_NAME_CAPACITY],
        pub exec_path: [u8; PROGRAM_PATH_CAPACITY],
    }

    impl Default for RuntimeProgramInfo {
        fn default() -> Self {
            Self {
                program_id: 0,
                reserved: 0,
                weight_micros: 0,
                display_name: [0; PROGRAM_NAME_CAPACITY],
                exec_path: [0; PROGRAM_PATH_CAPACITY],
            }
        }
    }

    impl RuntimeProgramInfo {
        pub fn set_display_name(&mut self, name: &str) {
            super::copy_ascii_into(&mut self.display_name, name);
        }

        pub fn set_exec_path(&mut self, path: &str) {
            super::copy_ascii_into(&mut self.exec_path, path);
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct RuntimeRunningProgramInfo {
        pub pid: u64,
        pub program_id: u32,
        pub session_index: u32,
        pub display_name: [u8; RUNNING_PROGRAM_NAME_CAPACITY],
    }

    impl Default for RuntimeRunningProgramInfo {
        fn default() -> Self {
            Self {
                pid: 0,
                program_id: 0,
                session_index: 0,
                display_name: [0; RUNNING_PROGRAM_NAME_CAPACITY],
            }
        }
    }

    impl RuntimeRunningProgramInfo {
        pub fn set_display_name(&mut self, name: &str) {
            super::copy_ascii_into(&mut self.display_name, name);
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct RuntimeGenerationInfo {
        pub generation: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct RuntimeSnapshotProgramsRequest {
        pub programs_ptr: u64,
        pub capacity: u64,
        pub count: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct RuntimeSnapshotRunningProgramsRequest {
        pub programs_ptr: u64,
        pub capacity: u64,
        pub count: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct RuntimeLaunchRequest {
        pub program_id: u64,
        pub target_kind: u16,
        pub reserved: u16,
        pub reserved2: u32,
        pub target_value: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct RuntimeTerminateRequest {
        pub target_kind: u16,
        pub reserved: u16,
        pub reserved2: u32,
        pub target_value: u64,
    }
}

pub mod device {
    pub const DISPLAY_PATH: &str = "/dev/display0";
    pub const INPUT_PATH: &str = "/dev/input0";

    pub const DISPLAY_IOCTL_GET_INFO: u64 = 0x4453_0001;
    pub const DISPLAY_IOCTL_CREATE_SURFACE: u64 = 0x4453_0002;
    pub const DISPLAY_IOCTL_PRESENT: u64 = 0x4453_0003;

    pub const PIXEL_FORMAT_BGRA8888: u32 = super::ui::PIXEL_FORMAT_BGRA8888;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplayInfo {
        pub width: u32,
        pub height: u32,
        pub stride_bytes: u32,
        pub bytes_per_pixel: u32,
        pub pixel_format: u32,
        pub reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct InputEvent {
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
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplayPresentRequest {
        pub surface_handle: u32,
        pub reserved: u32,
    }
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
