use spin::Mutex;

pub use rustos_keyboard_driver::{KeyAction, KeyCode, KeyboardEvent, Modifiers};
use rustos_keyboard_driver::{KeyboardDriver, ScanCodeSet};

static KEYBOARD_DRIVER: Mutex<KeyboardDriver> = Mutex::new(KeyboardDriver::new());

pub(crate) fn configure_legacy_transport(translated: bool) {
    let scan_set = if translated {
        ScanCodeSet::Set1
    } else {
        ScanCodeSet::Set2
    };
    KEYBOARD_DRIVER.lock().set_scan_code_set(scan_set);
}

pub(crate) fn on_scancode(scancode: u8) {
    let mut first = true;
    loop {
        let maybe_event = {
            let mut driver = KEYBOARD_DRIVER.lock();
            if first {
                driver.feed_scancode(scancode);
                first = false;
            }
            driver.pop_event()
        };
        let Some(event) = maybe_event else {
            break;
        };
        crate::input::dispatcher::dispatch_keyboard_event(event);
    }
}
