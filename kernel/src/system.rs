use crate::debug;
use crate::io::console;
use crate::multitask;
use crate::user::runtime;
use core::sync::atomic::{AtomicUsize, Ordering};

const SERVICE_TRACE_BUDGET: usize = 0;

static SERVICE_TRACE_COUNT: AtomicUsize = AtomicUsize::new(0);

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
    let service_thread = multitask::Thread::new(service_loop_task, 50);
    service_thread.start();
    core::mem::forget(service_thread);

    debug::println!("service loop: spawned dedicated kernel task");
    loop {
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

fn service_loop_task(_id: u64) {
    debug::println!("service loop: kernel task entered");
    loop {
        x86_64::instructions::interrupts::enable();
        let work = service_once();
        if work == 0 {
            multitask::yield_now();
            x86_64::instructions::interrupts::enable_and_hlt();
        }
    }
}

pub(crate) fn service_once() -> usize {
    let mut work = 0;

    trace_service_phase("runtime");
    work += runtime::service_pending_requests();

    trace_service_phase("input");
    work += crate::input::service_pending();

    trace_service_phase("usb");
    work += crate::usb::service_pending();

    trace_service_phase("serio");
    work += crate::driver::serio::service_pending();

    trace_service_phase("reap");
    work += multitask::service_deferred_work();

    trace_service_phase("workqueue");
    work += crate::driver::linux::workqueue::service_pending();

    trace_service_phase("console");
    work += console::service();

    work
}

fn trace_service_phase(phase: &'static str) {
    let index = SERVICE_TRACE_COUNT.fetch_add(1, Ordering::Relaxed);
    if index < SERVICE_TRACE_BUDGET {
        debug::println!("service loop phase[{}]: {}", index, phase);
    }
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
