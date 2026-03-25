#![no_std]

pub use keyboard_core::*;

#[cfg(feature = "module-image")]
use driver_abi::{DriverBus, DriverClass, DriverKernelApiV1, DriverModuleHeader};

pub const KEYBOARD_DRIVER_NAME: &str = "rustos-keyboard";
pub const KEYBOARD_DRIVER_MODULE_PATH: &str = "system/drivers/input/rustos-keyboard.ko";

#[cfg(feature = "module-image")]
#[unsafe(no_mangle)]
pub static RUSTOS_DRIVER_HEADER: DriverModuleHeader = DriverModuleHeader::new(
    DriverClass::Input,
    DriverBus::Serio,
    KEYBOARD_DRIVER_MODULE_PATH,
    KEYBOARD_DRIVER_NAME,
);

#[cfg(feature = "module-image")]
#[unsafe(no_mangle)]
pub extern "C" fn rustos_driver_abi_version() -> u32 {
    driver_abi::DRIVER_MODULE_ABI_VERSION
}

#[cfg(feature = "module-image")]
#[unsafe(no_mangle)]
pub extern "C" fn rustos_driver_init(_api: *const DriverKernelApiV1) -> i32 {
    0
}

#[cfg(feature = "module-image")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
