use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use driver_abi::PointerPacket;

static POINTER_BUTTON_STATE: AtomicU8 = AtomicU8::new(0);
static POINTER_DELIVERY_LOGGED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
static TEST_POINTER_EVENTS_READY: AtomicU8 = AtomicU8::new(0);
#[cfg(test)]
static TEST_POINTER_CAPTURE_RESULT: AtomicU8 = AtomicU8::new(0);

pub(crate) fn reset_pointer_state() {
    POINTER_BUTTON_STATE.store(0, Ordering::Release);
}

pub(crate) fn submit_pointer_packet(packet: PointerPacket) -> bool {
    if !pointer_events_ready() {
        reset_pointer_state();
        return false;
    }

    let _ = capture_pointer_packet(packet);

    let previous = POINTER_BUTTON_STATE.swap(packet.buttons, Ordering::AcqRel);
    let submitted = crate::input::event_queue::submit_pointer_packet(packet, previous);
    if submitted {
        log_pointer_delivery_once("relative");
    }
    submitted
}

pub(crate) fn submit_pointer_absolute(x: u32, y: u32, buttons: u8, wheel_vertical: i16) -> bool {
    if !pointer_events_ready() {
        let _ = (x, y, buttons, wheel_vertical);
        reset_pointer_state();
        return false;
    }

    let previous = POINTER_BUTTON_STATE.swap(buttons, Ordering::AcqRel);
    let submitted =
        crate::input::event_queue::submit_pointer_absolute(x, y, buttons, wheel_vertical, previous);
    if submitted {
        log_pointer_delivery_once("absolute");
    }
    submitted
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
    #[cfg(test)]
    match TEST_POINTER_EVENTS_READY.load(Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }

    crate::io::gui::is_userspace_display_active()
}

fn capture_pointer_packet(packet: PointerPacket) -> bool {
    #[cfg(test)]
    match TEST_POINTER_CAPTURE_RESULT.load(Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }

    if crate::usb::has_runtime_pointer_device() {
        return false;
    }

    crate::usb::capture_pointer_packet(packet)
}

fn log_pointer_delivery_once(kind: &'static str) {
    if !POINTER_DELIVERY_LOGGED.swap(true, Ordering::AcqRel) {
        crate::debug::info!(input, "input: pointer event delivered kind={}", kind);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        POINTER_BUTTON_STATE, TEST_POINTER_CAPTURE_RESULT, TEST_POINTER_EVENTS_READY,
        reset_pointer_state, submit_pointer_packet,
    };
    use crate::user::abi::device::{
        INPUT_ACTION_PRESSED, INPUT_KIND_POINTER_BUTTON, INPUT_KIND_POINTER_MOTION, InputEvent,
        POINTER_BUTTON_LEFT,
    };
    use core::sync::atomic::Ordering;
    use driver_abi::{POINTER_BUTTON_LEFT as POINTER_PACKET_LEFT, PointerPacket};

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::exclusive_test()
    }

    fn reset_for_tests() {
        reset_pointer_state();
        POINTER_BUTTON_STATE.store(0, Ordering::Relaxed);
        TEST_POINTER_EVENTS_READY.store(0, Ordering::Relaxed);
        TEST_POINTER_CAPTURE_RESULT.store(0, Ordering::Relaxed);
        crate::input::event_queue::reset_for_tests();
    }

    #[test]
    fn pointer_packet_still_reaches_event_queue_when_usb_capture_reports_true() {
        let _guard = isolated();
        reset_for_tests();
        TEST_POINTER_EVENTS_READY.store(2, Ordering::Relaxed);
        TEST_POINTER_CAPTURE_RESULT.store(2, Ordering::Relaxed);

        let changed = submit_pointer_packet(PointerPacket {
            buttons: POINTER_PACKET_LEFT,
            dx: 7,
            dy: -3,
            wheel_vertical: 0,
            wheel_horizontal: 0,
            reserved0: 0,
            reserved1: 0,
            reserved2: 0,
        });
        assert!(changed);

        let mut events = [InputEvent::default(); 4];
        let count = crate::input::event_queue::read_input_events(&mut events);
        assert_eq!(count, 2);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[0].value0, 7);
        assert_eq!(events[0].value1, -3);
        assert_eq!(events[1].kind, INPUT_KIND_POINTER_BUTTON);
        assert_eq!(events[1].action, INPUT_ACTION_PRESSED);
        assert_eq!(events[1].code, POINTER_BUTTON_LEFT);
    }
}
