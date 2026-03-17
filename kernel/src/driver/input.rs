use core::sync::atomic::{AtomicU8, Ordering};

use driver_abi::{
    POINTER_BUTTON_LEFT, POINTER_BUTTON_MIDDLE, POINTER_BUTTON_RIGHT, POINTER_BUTTON_X1,
    POINTER_BUTTON_X2, PointerPacket,
};

static POINTER_BUTTON_STATE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn reset_pointer_state() {
    POINTER_BUTTON_STATE.store(0, Ordering::Release);
}

pub(crate) fn submit_pointer_packet(packet: PointerPacket) -> bool {
    let mut changed = false;

    if packet.dx != 0 || packet.dy != 0 {
        crate::input::event_queue::push_pointer_motion(packet.dx, packet.dy);
        changed = true;
    }
    if packet.wheel_vertical != 0 || packet.wheel_horizontal != 0 {
        crate::input::event_queue::push_pointer_scroll(
            packet.wheel_vertical,
            packet.wheel_horizontal,
        );
        changed = true;
    }

    let previous = POINTER_BUTTON_STATE.swap(packet.buttons, Ordering::AcqRel);
    changed |= emit_button_edges(previous, packet.buttons, POINTER_BUTTON_LEFT);
    changed |= emit_button_edges(previous, packet.buttons, POINTER_BUTTON_RIGHT);
    changed |= emit_button_edges(previous, packet.buttons, POINTER_BUTTON_MIDDLE);
    changed |= emit_button_edges(previous, packet.buttons, POINTER_BUTTON_X1);
    changed |= emit_button_edges(previous, packet.buttons, POINTER_BUTTON_X2);

    changed
}

pub(crate) unsafe extern "C" fn report_pointer_packet(packet: *const PointerPacket) -> i32 {
    if packet.is_null() {
        return -22;
    }

    let packet = unsafe { *packet };
    submit_pointer_packet(packet);
    0
}

fn emit_button_edges(previous: u8, current: u8, button_mask: u8) -> bool {
    let was_pressed = previous & button_mask != 0;
    let is_pressed = current & button_mask != 0;
    if was_pressed == is_pressed {
        return false;
    }

    crate::input::event_queue::push_pointer_button(button_code(button_mask), is_pressed);
    true
}

const fn button_code(mask: u8) -> u32 {
    mask as u32
}
