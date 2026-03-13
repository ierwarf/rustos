use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

pub(crate) mod dispatcher;
pub(crate) mod keyboard;
pub(crate) mod mouse;
pub(crate) mod usb;

const INPUT_THREAD_WEIGHT_MICROS: u64 = 10;

static INPUT_THREAD: Mutex<Option<crate::multitask::Thread>> = Mutex::new(None);
static INPUT_THREAD_STARTED: AtomicBool = AtomicBool::new(false);

pub fn init() {
    let _ = crate::gui::show_mouse_cursor();
    let legacy_ready = report_legacy_keyboard(keyboard::init());
    let usb = usb::init();
    let usb_keyboard_ready = report_usb_keyboard(usb.keyboard);
    let usb_mouse_ready = report_usb_mouse(usb.mouse, usb.error);
    mouse::set_available(usb_mouse_ready);

    if !legacy_ready && !usb_keyboard_ready && !usb_mouse_ready {
        crate::debug::println!("No keyboard or mouse backend became ready.");
        crate::console::write(b"No keyboard or mouse backend ready.\r\n");
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

#[allow(dead_code)]
pub fn poll_fallback() {
    if INPUT_THREAD_STARTED.load(Ordering::Acquire) {
        return;
    }

    let _ = service_once();
}

fn input_thread_main(_id: u64) {
    loop {
        let work = service_once() + crate::multitask::service_deferred_work();
        if work == 0 {
            interrupts::enable_and_hlt();
        }
    }
}

fn service_once() -> usize {
    keyboard::poll_fallback();
    let usb_work = usb::poll_fallback();
    let keyboard_work = keyboard::drain_events(dispatcher::dispatch_keyboard_event);
    usb_work + keyboard_work
}

fn report_legacy_keyboard(result: keyboard::LegacyKeyboardInitResult) -> bool {
    match result {
        keyboard::LegacyKeyboardInitResult::Ready(info) => {
            crate::debug::println!(
                "Legacy keyboard ready: scan_set={}, translated={}",
                info.scan_set.name(),
                info.translated,
            );
            crate::console::write(b"Legacy keyboard ready.\r\n");
            true
        }
        keyboard::LegacyKeyboardInitResult::Unavailable(reason) => {
            crate::debug::println!("Legacy keyboard unavailable: {}", reason);
            crate::console::write(b"Legacy keyboard unavailable.\r\n");
            false
        }
    }
}

fn report_usb_keyboard(info: Option<usb::UsbKeyboardInfo>) -> bool {
    let Some(info) = info else {
        crate::debug::println!("USB keyboard unavailable.");
        crate::console::write(b"USB keyboard unavailable.\r\n");
        return false;
    };

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
    crate::console::write(b"USB keyboard ready.\r\n");
    true
}

fn report_usb_mouse(info: Option<usb::UsbMouseInfo>, error: Option<&'static str>) -> bool {
    let Some(info) = info else {
        if let Some(reason) = error {
            crate::debug::println!("USB mouse unavailable: {}", reason);
        } else {
            crate::debug::println!("USB mouse unavailable.");
        }
        crate::console::write(b"USB mouse unavailable.\r\n");
        return false;
    };

    crate::debug::println!(
        "USB mouse ready via xHCI {:04x}:{:04x} on {:04x}:{:02x}:{:02x}.{} port {}",
        info.controller.vendor_id(),
        info.controller.device_id(),
        info.controller.segment,
        info.controller.bus,
        info.controller.device,
        info.controller.function,
        info.port_id,
    );
    crate::console::write(b"USB mouse ready.\r\n");
    true
}

#[cfg(test)]
mod tests {
    use super::report_usb_mouse;

    #[test]
    fn usb_mouse_error_without_device_is_not_ready() {
        assert!(!report_usb_mouse(None, Some("missing")));
    }
}
