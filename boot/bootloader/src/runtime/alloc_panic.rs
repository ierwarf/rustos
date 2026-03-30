use core::panic::PanicInfo;

use crate::debug;
use uefi::allocator::Allocator;

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    uefi::println!("[PANIC] {info}");
    debug::println!("[PANIC] {info}");
    loop {
        core::hint::spin_loop();
    }
}
