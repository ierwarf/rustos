use driver_abi::DisplayFramebufferRegistration;

use crate::api;
use crate::dcn;
use crate::mmio::RegisterWindow;
use crate::probe::Phoenix3Device;

pub fn take_over_inherited_framebuffer(
    device: &Phoenix3Device,
    registers: &RegisterWindow,
) -> Result<DisplayFramebufferRegistration, &'static str> {
    let framebuffer = api::query_boot_framebuffer()
        .map_err(|_| "amdgpu: boot framebuffer descriptor is unavailable")?;

    dcn::validate_inherited_scanout(device, registers, &framebuffer)?;

    Ok(framebuffer)
}
