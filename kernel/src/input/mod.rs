use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

pub(crate) mod keyboard;
pub(crate) mod usb;

const INPUT_THREAD_WEIGHT_MICROS: u64 = 10;

static INPUT_THREAD: Mutex<Option<crate::multitask::Thread>> = Mutex::new(None);
static INPUT_THREAD_STARTED: AtomicBool = AtomicBool::new(false);

pub fn init() {
    let legacy_ready = report_legacy_keyboard(keyboard::init());
    let usb_ready = report_usb_keyboard(usb::init());

    if !legacy_ready && !usb_ready {
        crate::debug::println!("No keyboard backend became ready.");
        crate::gui::write_console(b"No keyboard backend ready.\r\n");
    }
}

pub fn on_legacy_keyboard_interrupt() {
    keyboard::on_interrupt();
}

pub fn start_worker() {
    interrupts::without_interrupts(|| {
        if INPUT_THREAD_STARTED.load(Ordering::Acquire) {
            return;
        }

        let thread = crate::multitask::Thread::new(input_thread_main, INPUT_THREAD_WEIGHT_MICROS);
        thread.start();
        *INPUT_THREAD.lock() = Some(thread);
        INPUT_THREAD_STARTED.store(true, Ordering::Release);
    });
}

pub fn poll_fallback() {
    if INPUT_THREAD_STARTED.load(Ordering::Acquire) {
        return;
    }

    let _ = service_once();
}

fn input_thread_main(_id: u64) {
    loop {
        if service_once() == 0 {
            interrupts::enable_and_hlt();
        }
    }
}

fn service_once() -> usize {
    keyboard::poll_fallback();
    usb::poll_fallback();
    keyboard::drain_events_to_tty()
}

fn report_legacy_keyboard(result: keyboard::LegacyKeyboardInitResult) -> bool {
    match result {
        keyboard::LegacyKeyboardInitResult::Ready(info) => {
            crate::debug::println!(
                "Legacy keyboard ready: scan_set={}, translated={}",
                info.scan_set.name(),
                info.translated,
            );
            crate::gui::write_console(b"Legacy keyboard ready.\r\n");
            true
        }
        keyboard::LegacyKeyboardInitResult::Unavailable(reason) => {
            crate::debug::println!("Legacy keyboard unavailable: {}", reason);
            crate::gui::write_console(b"Legacy keyboard unavailable.\r\n");
            false
        }
    }
}

fn report_usb_keyboard(result: usb::UsbKeyboardInitResult) -> bool {
    match result {
        usb::UsbKeyboardInitResult::Ready(info) => {
            crate::debug::println!(
                "USB keyboard ready via xHCI {:04x}:{:04x} on {:04x}:{:02x}:{:02x}.{} port {}",
                info.controller.vendor_id(),
                info.controller.device_id(),
                info.controller.segment,
                info.controller.bus,
                info.controller.device,
                info.controller.function,
                info.port_id,
            );
            crate::gui::write_console(b"USB keyboard ready.\r\n");
            true
        }
        usb::UsbKeyboardInitResult::Unavailable(reason) => {
            crate::debug::println!("USB keyboard unavailable: {}", reason);
            crate::gui::write_console(b"USB keyboard unavailable.\r\n");
            false
        }
    }
}
