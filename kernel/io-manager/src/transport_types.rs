//! Kernel-private records shared by the built-in display transports.
//!
//! These are not a loadable-driver ABI. Their layout is checked locally
//! because the built-in DVM front-ends exchange the records across internal
//! modules, while ring3 services receive separate versioned user ABI records.

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

pub const DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER: u8 = 1 << 1;
/// The scanout is relayed by an isolated driver domain. This provenance bit
/// can only reduce trust; it never attests a display path.
pub const DISPLAY_FRAMEBUFFER_FLAG_DVM_SCANOUT: u8 = 1 << 2;
#[cfg(test)]
mod tests {
    use super::DisplayFramebufferRegistration;
    use core::mem::size_of;

    #[test]
    fn built_in_transport_layout_is_stable() {
        assert_eq!(size_of::<DisplayFramebufferRegistration>(), 56);
    }
}
