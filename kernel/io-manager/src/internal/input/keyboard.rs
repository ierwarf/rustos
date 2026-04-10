use spin::Mutex;

pub use keyboard_core::{KeyAction, KeyCode, KeyboardEvent, Modifiers};
use keyboard_core::{KeyboardDriver, ScanCodeSet};

static KEYBOARD_DRIVER: Mutex<KeyboardDriver> = Mutex::new(KeyboardDriver::new());

pub(crate) fn configure_scancode_transport(translated: bool) {
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

pub(crate) fn inject_key_transition(code: KeyCode, released: bool) {
    let mut driver = KEYBOARD_DRIVER.lock();
    driver.inject_key_transition(code, released);
    while let Some(event) = driver.pop_event() {
        crate::input::dispatcher::dispatch_keyboard_event(event);
    }
}
