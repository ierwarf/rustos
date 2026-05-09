#![no_std]

#[cfg(feature = "module-image")]
mod api;

#[cfg(feature = "module-image")]
use driver_abi::{
    DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER, DriverBus, DriverClass, DriverKernelApiV1,
    DriverModuleHeader,
};

pub const BOOTFB_DRIVER_NAME: &str = "bootfb";
pub const BOOTFB_DRIVER_MODULE_PATH: &str = "system/drivers/display/bootfb.ko";

#[cfg(feature = "module-image")]
#[unsafe(no_mangle)]
pub static RUSTOS_DRIVER_HEADER: DriverModuleHeader = DriverModuleHeader::new(
    DriverClass::Display,
    DriverBus::Platform,
    BOOTFB_DRIVER_MODULE_PATH,
    BOOTFB_DRIVER_NAME,
);

#[cfg(feature = "module-image")]
#[unsafe(no_mangle)]
pub extern "C" fn rustos_driver_abi_version() -> u32 {
    driver_abi::DRIVER_MODULE_ABI_VERSION
}

#[cfg(feature = "module-image")]
#[unsafe(no_mangle)]
pub extern "C" fn rustos_driver_init(api: *const DriverKernelApiV1) -> i32 {
    if api::bind(api).is_err() {
        return -22;
    }

    let mut framebuffer = match api::query_boot_framebuffer() {
        Ok(framebuffer) => framebuffer,
        Err(status) => {
            api::log_warn("bootfb: boot framebuffer query failed");
            return status;
        }
    };
    framebuffer.flags |= DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER;

    if let Err(status) = api::register_display_framebuffer(&framebuffer) {
        api::log_warn("bootfb: framebuffer registration failed");
        return status;
    }

    api::log_info("bootfb: boot framebuffer registered");
    0
}

#[cfg(feature = "module-image")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
