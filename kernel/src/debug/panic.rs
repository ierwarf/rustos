use crate::debug;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    debug::println("");
    debug::println("[PANIC]");
    debug::print_fmt(format_args!("message: {}\r\n", info.message()));

    if let Some(location) = info.location() {
        debug::print_fmt(format_args!(
            "location: {}:{}:{}\r\n",
            location.file(),
            location.line(),
            location.column()
        ));
    } else {
        debug::println("location: <unknown>");
    }

    loop {
        core::hint::spin_loop();
    }
}
