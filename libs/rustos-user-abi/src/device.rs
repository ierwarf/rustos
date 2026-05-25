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

pub const DISPLAY_IOCTL_GET_INFO: u64 = ioctl::ior::<DisplayInfo>(DISPLAY_IOCTL_TYPE, 1);
pub const DISPLAY_IOCTL_CREATE_SURFACE: u64 =
    ioctl::iowr::<DisplaySurfaceCreate>(DISPLAY_IOCTL_TYPE, 2);
pub const DISPLAY_IOCTL_PRESENT: u64 =
    ioctl::iow::<DisplayPresentRequest>(DISPLAY_IOCTL_TYPE, 3);
pub const DISPLAY_IOCTL_PRESENT_RECT: u64 =
    ioctl::iow::<DisplayPresentRectRequest>(DISPLAY_IOCTL_TYPE, 4);
