use kernel_compat::api::console_host;

use crate::{debug, io_services};

pub(super) fn fatal_init_bootstrap_load(err: console_host::ConsoleHostError) -> ! {
    err.log_debug_details();
    debug::println!();
    debug::println!(
        "[KERNEL FATAL] init bootstrap load failed: {} ({:?})",
        err.summary(),
        err,
    );
    io_services::console_write(b"\r\n[KERNEL FATAL]\r\nInit bootstrap failed.\r\n");
    x86_64::instructions::interrupts::disable();
    loop {
        x86_64::instructions::hlt();
    }
}

pub(super) fn fatal_init_bootstrap_spawn(err: console_host::ConsoleHostError) -> ! {
    err.log_debug_details();
    debug::println!();
    debug::println!(
        "[KERNEL FATAL] init bootstrap spawn failed: {} ({:?})",
        err.summary(),
        err,
    );
    io_services::console_write(b"\r\n[KERNEL FATAL]\r\nInit bootstrap failed.\r\n");
    x86_64::instructions::interrupts::disable();
    loop {
        x86_64::instructions::hlt();
    }
}
