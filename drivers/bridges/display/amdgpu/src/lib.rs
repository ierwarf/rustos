#![no_std]

#[cfg(feature = "module-image")]
mod api;
#[cfg(feature = "module-image")]
mod dcn;
#[cfg(feature = "module-image")]
mod fw;
#[cfg(feature = "module-image")]
mod mmio;
#[cfg(feature = "module-image")]
mod probe;
#[cfg(feature = "module-image")]
mod scanout;

#[cfg(feature = "module-image")]
use driver_abi::{DriverBus, DriverClass, DriverKernelApiV1, DriverModuleHeader};

#[cfg(feature = "module-image")]
pub const AMDGPU_DRIVER_NAME: &str = "amdgpu";
#[cfg(feature = "module-image")]
pub const AMDGPU_DRIVER_MODULE_PATH: &str = "system/drivers/display/amdgpu.ko";

#[cfg(feature = "module-image")]
#[unsafe(no_mangle)]
pub static RUSTOS_DRIVER_HEADER: DriverModuleHeader = DriverModuleHeader::new(
    DriverClass::Display,
    DriverBus::Pci,
    AMDGPU_DRIVER_MODULE_PATH,
    AMDGPU_DRIVER_NAME,
);

#[cfg(feature = "module-image")]
#[unsafe(no_mangle)]
pub extern "C" fn rustos_driver_abi_version() -> u32 {
    driver_abi::DRIVER_MODULE_ABI_VERSION
}

#[cfg(feature = "module-image")]
#[unsafe(no_mangle)]
pub extern "C" fn rustos_driver_init(api: *const DriverKernelApiV1) -> i32 {
    // Safety: the module loader supplies an ABI table that remains mapped for
    // the lifetime of this loaded module.
    if unsafe { api::bind(api) }.is_err() {
        return -22;
    }

    api::log_info("amdgpu: module init");

    let Some(device) = probe::probe_phoenix3() else {
        api::log_info("amdgpu: Phoenix3 not present, skipping");
        return 0;
    };

    if let Err(message) = fw::load_required_firmware() {
        api::log_warn(message);
        return 0;
    }

    let registers = match mmio::map_registers(&device) {
        Ok(registers) => registers,
        Err(message) => {
            api::log_warn(message);
            return 0;
        }
    };

    let framebuffer = match scanout::take_over_inherited_framebuffer(&device, &registers) {
        Ok(framebuffer) => framebuffer,
        Err(message) => {
            api::log_warn(message);
            return 0;
        }
    };

    if api::register_display_framebuffer(&framebuffer).is_err() {
        api::log_warn("amdgpu: framebuffer registration failed");
        return 0;
    }

    api::log_info("amdgpu: inherited Phoenix3 framebuffer registered");
    0
}

#[cfg(feature = "module-image")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
