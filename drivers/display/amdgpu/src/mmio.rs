use driver_abi::DriverMmioCachePolicy;

use crate::api;
use crate::probe::Phoenix3Device;

#[derive(Clone, Copy)]
pub struct RegisterWindow {
    pub virt_addr: u64,
    pub size: u64,
}

pub fn map_registers(device: &Phoenix3Device) -> Result<RegisterWindow, &'static str> {
    let virt_addr = api::map_mmio(
        device.register_bar.base,
        device.register_bar.size,
        DriverMmioCachePolicy::Uncached,
    )
    .map_err(|_| "amdgpu: register BAR could not be mapped")?;

    Ok(RegisterWindow {
        virt_addr,
        size: device.register_bar.size,
    })
}
