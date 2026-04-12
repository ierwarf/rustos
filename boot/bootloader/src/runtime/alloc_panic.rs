#![allow(unused_imports)]

#[cfg(rustos_boot_image)]
use core::panic::PanicInfo;

#[cfg(rustos_boot_image)]
use crate::debug;
#[cfg(rustos_boot_image)]
use uefi::allocator::Allocator;

#[cfg(all(not(test), rustos_boot_image))]
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

#[cfg(all(not(test), rustos_boot_image))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    uefi::println!("[PANIC] {info}");
    debug::println!("[PANIC] {info}");
    loop {
        core::hint::spin_loop();
    }
}
