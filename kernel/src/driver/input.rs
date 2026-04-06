use core::sync::atomic::{AtomicU8, Ordering};

use driver_abi::PointerPacket;

static POINTER_BUTTON_STATE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn reset_pointer_state() {
    POINTER_BUTTON_STATE.store(0, Ordering::Release);
}

pub(crate) fn submit_pointer_packet(packet: PointerPacket) -> bool {
    if !pointer_events_ready() {
        reset_pointer_state();
        return false;
    }

    if crate::usb::capture_pointer_packet(packet) {
        return packet.dx != 0
            || packet.dy != 0
            || packet.wheel_vertical != 0
            || packet.wheel_horizontal != 0
            || packet.buttons != 0;
    }

    let previous = POINTER_BUTTON_STATE.swap(packet.buttons, Ordering::AcqRel);
    crate::input::event_queue::submit_pointer_packet(packet, previous)
}

pub(crate) fn submit_pointer_absolute(x: u32, y: u32, buttons: u8, wheel_vertical: i16) -> bool {
    if !pointer_events_ready() {
        let _ = (x, y, buttons, wheel_vertical);
        reset_pointer_state();
        return false;
    }

    let previous = POINTER_BUTTON_STATE.swap(buttons, Ordering::AcqRel);
    crate::input::event_queue::submit_pointer_absolute(x, y, buttons, wheel_vertical, previous)
}

pub(crate) unsafe extern "C" fn report_pointer_packet(packet: *const PointerPacket) -> i32 {
    if packet.is_null() {
        return -22;
    }

    let packet = unsafe { *packet };
    submit_pointer_packet(packet);
    0
}

fn pointer_events_ready() -> bool {
    crate::io::gui::is_userspace_display_active()
}
