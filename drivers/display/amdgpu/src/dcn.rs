use driver_abi::{DisplayFramebufferRegistration, DisplayPixelFormat};

use crate::mmio::RegisterWindow;
use crate::probe::Phoenix3Device;

pub fn validate_inherited_scanout(
    device: &Phoenix3Device,
    registers: &RegisterWindow,
    framebuffer: &DisplayFramebufferRegistration,
) -> Result<(), &'static str> {
    if registers.virt_addr == 0 || registers.size < 512 * 1024 {
        return Err("amdgpu: register window is not large enough for DCN 3.1.4");
    }
    if framebuffer.addr == 0 || framebuffer.size == 0 {
        return Err("amdgpu: boot framebuffer descriptor is empty");
    }
    if framebuffer.width == 0 || framebuffer.height == 0 || framebuffer.stride == 0 {
        return Err("amdgpu: boot framebuffer geometry is invalid");
    }
    if framebuffer.bytes_per_pixel != 4 {
        return Err("amdgpu: only 32bpp inherited Phoenix3 framebuffers are supported");
    }
    if framebuffer.pixel_format != DisplayPixelFormat::Rgb as u32
        && framebuffer.pixel_format != DisplayPixelFormat::Bgr as u32
    {
        return Err("amdgpu: unsupported inherited framebuffer pixel format");
    }

    let min_size = (framebuffer.stride as u64)
        .saturating_mul(framebuffer.height as u64)
        .saturating_mul(framebuffer.bytes_per_pixel as u64);
    if min_size == 0 || min_size > framebuffer.size {
        return Err("amdgpu: inherited framebuffer is smaller than its geometry");
    }

    if device.framebuffer_bar.base == 0 || device.framebuffer_bar.size == 0 {
        return Err("amdgpu: Phoenix3 framebuffer BAR is unavailable");
    }

    Ok(())
}
