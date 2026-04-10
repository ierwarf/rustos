#![allow(unused_imports)]

use core::panic::PanicInfo;

use crate::debug;
use uefi::allocator::Allocator;

#[cfg(not(test))]
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    uefi::println!("[PANIC] {info}");
    debug::println!("[PANIC] {info}");
    loop {
        core::hint::spin_loop();
    }
}
