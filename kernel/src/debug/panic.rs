use crate::debug;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    debug::println!("");
    debug::println!("[PANIC]");
    debug::println!("message: {}", info.message());

    if let Some(location) = info.location() {
        debug::println!(
            "location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
    } else {
        debug::println!("location: <unknown>");
    }

    loop {
        core::hint::spin_loop();
    }
}
