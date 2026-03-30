use crate::debug;
use crate::io::console;
use crate::multitask;
use crate::user::runtime;

pub(crate) fn bootstrap_desktop_runtime(
    bootstrap: fn() -> Result<(), runtime::DesktopRuntimeError>,
) {
    console::write(b"Bootstrapping desktop runtime...\r\n");
    if let Err(err) = bootstrap() {
        fatal_desktop_runtime_bootstrap(err);
    }
    console::write(b"Desktop runtime ready.\r\n");
}

pub(crate) fn run_service_loop() -> ! {
    loop {
        let work = service_once();
        if work == 0 {
            x86_64::instructions::interrupts::enable_and_hlt();
        }
    }
}

pub(crate) fn service_once() -> usize {
    runtime::service_pending_requests()
        + crate::input::service_pending()
        + crate::usb::service_pending()
        + crate::driver::serio::service_pending()
        + multitask::service_deferred_work()
        + crate::driver::linux::workqueue::service_pending()
        + console::service()
}

fn fatal_desktop_runtime_bootstrap(err: runtime::DesktopRuntimeError) -> ! {
    err.log_debug_details();
    debug::println!();
    debug::println!(
        "[KERNEL FATAL] desktop runtime bootstrap failed: {} ({:?})",
        err.summary(),
        err,
    );
    console::write(b"\r\n[KERNEL FATAL]\r\nDesktop runtime bootstrap failed.\r\n");
    x86_64::instructions::interrupts::disable();
    loop {
        x86_64::instructions::hlt();
    }
}
