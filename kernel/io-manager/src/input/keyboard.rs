// RING3-MIGRATION-REFERENCE START: input-ingress exception: inputd owns
// keyboard layout and key transition policy. Ring0 keeps PS/2 scancode ingress
// and transport mode state.
use core::sync::atomic::{AtomicBool, Ordering};

static KEYBOARD_TRANSLATED: AtomicBool = AtomicBool::new(true);

pub(crate) fn configure_scancode_transport(translated: bool) {
    KEYBOARD_TRANSLATED.store(translated, Ordering::Release);
}

pub(crate) fn on_scancode(scancode: u8) {
    let translated = KEYBOARD_TRANSLATED.load(Ordering::Acquire);
    let _ = crate::input::event_queue::submit_ps2_scancode(scancode, translated);
}
// RING3-MIGRATION-REFERENCE END: inputd-owned keyboard ingress exception.
