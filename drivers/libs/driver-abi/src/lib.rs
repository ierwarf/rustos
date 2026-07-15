#![no_std]

//! Fixed DVM-facing display and input transport records.
//!
//! Linux driver-module registration, PCI callbacks, serio/i8042 objects, and
//! in-kernel `.ko` runtime tables were retired with the DVM-only driver model.
//! This crate remains only for the narrow records shared by the DVM transport
//! substrate and user-space policy services.

pub const POINTER_BUTTON_LEFT: u8 = 1 << 0;
pub const POINTER_BUTTON_RIGHT: u8 = 1 << 1;
pub const POINTER_BUTTON_MIDDLE: u8 = 1 << 2;
pub const POINTER_BUTTON_X1: u8 = 1 << 3;
pub const POINTER_BUTTON_X2: u8 = 1 << 4;

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayPixelFormat {
    Rgb = 0,
    Bgr = 1,
    Bitmask = 2,
    Unknown = 0xff,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PointerPacket {
    pub buttons: u8,
    pub reserved0: u8,
    pub reserved1: u8,
    pub reserved2: u8,
    pub dx: i16,
    pub dy: i16,
    pub wheel_vertical: i16,
    pub wheel_horizontal: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayFramebufferRegistration {
    pub addr: u64,
    pub size: u64,
    pub back_buffer_addr: u64,
    pub back_buffer_size: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub bytes_per_pixel: u8,
    pub flags: u8,
    pub reserved: [u8; 2],
}

pub const DISPLAY_FRAMEBUFFER_FLAG_BOOT_FRAMEBUFFER: u8 = 1 << 0;
pub const DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER: u8 = 1 << 1;
/// The scanout is relayed by an isolated driver domain. This is provenance
/// only: it can only make a presentation less trusted, never attest one.
pub const DISPLAY_FRAMEBUFFER_FLAG_DVM_SCANOUT: u8 = 1 << 2;
pub const DISPLAY_FRAMEBUFFER_KNOWN_FLAGS: u8 = DISPLAY_FRAMEBUFFER_FLAG_BOOT_FRAMEBUFFER
    | DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER
    | DISPLAY_FRAMEBUFFER_FLAG_DVM_SCANOUT;
