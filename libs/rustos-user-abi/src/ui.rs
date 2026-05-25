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
