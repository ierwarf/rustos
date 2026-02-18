use crate::debug;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    debug::print("\r\n[PANIC]\r\n");
    debug::print_fmt(format_args!("message: {}\r\n", info.message()));

    if let Some(location) = info.location() {
        debug::print_fmt(format_args!(
            "location: {}:{}:{}\r\n",
            location.file(),
            location.line(),
            location.column()
        ));
    } else {
        debug::print("location: <unknown>\r\n");
    }

    loop {
        core::hint::spin_loop();
    }
}
