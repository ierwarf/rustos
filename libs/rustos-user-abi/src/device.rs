use crate::{ioctl, ui};

pub const DISPLAY_PATH: &str = "/dev/display0";
pub const INPUT_PATH: &str = "/dev/input0";
pub const DISPLAY_IOCTL_TYPE: u8 = b'D';

pub const DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER: u32 = 1 << 0;
pub const DISPLAY_INFO_FLAG_PRIMARY_PROVIDER: u32 = 1 << 1;
/// The physical scanout is owned by a driver-domain relay. It is usable for
/// normal desktop presentation but is not an attested trusted-UI channel.
pub const DISPLAY_INFO_FLAG_DVM_SCANOUT: u32 = 1 << 2;
/// A private uiserver-only fixed atlas compositor transport is available.
/// This does not expose an application graphics ABI.
pub const DISPLAY_INFO_FLAG_GPU_COMPOSITOR: u32 = 1 << 3;
pub const DISPLAY_SURFACE_FLAG_GPU_ATLAS: u32 = 1;
pub const DISPLAY_GPU_ABI_VERSION: u32 = 5;
pub const DISPLAY_GPU_INFO_FLAG_STAGED_COPY: u32 = 1;
pub const DISPLAY_GPU_INFO_FLAG_DIRECT_DMABUF: u32 = 1 << 1;
pub const DISPLAY_GPU_SUBMIT_FLAG_STAGED_COPY: u32 = 1;
pub const DISPLAY_GPU_COMPLETION_BYTES: usize = 256;
pub const DISPLAY_SURFACE_MAX_WIDTH: u32 = 7680;
pub const DISPLAY_SURFACE_MAX_HEIGHT: u32 = 4320;
/// Padded linear scanout pitch accepted by the private compositor surface ABI.
/// A provider owns the actual pitch; width * bytes-per-pixel is only its floor.
pub const DISPLAY_SURFACE_MAX_STRIDE_BYTES: u32 =
    (DISPLAY_SURFACE_MAX_WIDTH * 4).div_ceil(4096) * 4096;
pub const DISPLAY_SURFACE_MAX_MAPPING_BYTES: u64 =
    DISPLAY_SURFACE_MAX_STRIDE_BYTES as u64 * DISPLAY_SURFACE_MAX_HEIGHT as u64;

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

    pub const fn is_dvm_scanout(self) -> bool {
        self.flags & DISPLAY_INFO_FLAG_DVM_SCANOUT != 0
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
    /// Request: zero. GPU-atlas response: the fixed physical binding slot.
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

/// Private compositor limits returned only for the uiserver display path.
/// Applications continue to use Wayland surfaces and never see GPU commands.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayGpuInfo {
    pub version: u32,
    pub flags: u32,
    pub atlas_width: u32,
    pub atlas_height: u32,
    pub atlas_stride_bytes: u32,
    pub slot_count: u32,
    pub max_commands: u32,
    pub max_batch_bytes: u32,
    pub generation: u64,
    pub context_id: u32,
    pub context_epoch: u32,
    pub prime_fence_value: u64,
    pub prime_duration_ns: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayGpuDamage {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayGpuSubmitRequest {
    pub surface_handle: u32,
    pub flags: u32,
    /// Reserved in ABI v5. Atlas pixels are written through the exact
    /// slot-scoped surface mapping before this commit request.
    pub reserved: u64,
    pub batch_ptr: u64,
    pub batch_len: u32,
    pub damage_count: u32,
    pub damage_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DisplayGpuCompletionQuery {
    pub surface_handle: u32,
    pub reserved: u32,
    pub completion: [u8; DISPLAY_GPU_COMPLETION_BYTES],
}

impl Default for DisplayGpuCompletionQuery {
    fn default() -> Self {
        Self {
            surface_handle: 0,
            reserved: 0,
            completion: [0; DISPLAY_GPU_COMPLETION_BYTES],
        }
    }
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
pub const DISPLAY_IOCTL_PRESENT: u64 = ioctl::iow::<DisplayPresentRequest>(DISPLAY_IOCTL_TYPE, 3);
pub const DISPLAY_IOCTL_PRESENT_RECT: u64 =
    ioctl::iow::<DisplayPresentRectRequest>(DISPLAY_IOCTL_TYPE, 4);
pub const DISPLAY_IOCTL_GPU_GET_INFO: u64 = ioctl::ior::<DisplayGpuInfo>(DISPLAY_IOCTL_TYPE, 5);
pub const DISPLAY_IOCTL_GPU_SUBMIT: u64 =
    ioctl::iow::<DisplayGpuSubmitRequest>(DISPLAY_IOCTL_TYPE, 6);
pub const DISPLAY_IOCTL_GPU_QUERY_COMPLETION: u64 =
    ioctl::iowr::<DisplayGpuCompletionQuery>(DISPLAY_IOCTL_TYPE, 7);
