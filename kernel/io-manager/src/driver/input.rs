#[cfg(not(test))]
use core::sync::atomic::AtomicBool;
#[cfg(test)]
use core::sync::atomic::AtomicU8;
use core::sync::atomic::Ordering;

use driver_abi::PointerPacket;

#[cfg(not(test))]
static POINTER_DELIVERY_LOGGED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
static TEST_POINTER_CAPTURE_RESULT: AtomicU8 = AtomicU8::new(0);

pub(crate) fn reset_pointer_state() {}

pub(crate) fn submit_pointer_packet(packet: PointerPacket) -> bool {
    let _ = capture_pointer_packet(packet);

    let submitted = crate::input::event_queue::submit_pointer_packet(packet);
    if submitted {
        log_pointer_delivery_once("relative");
    }
    submitted
}

pub(crate) fn submit_pointer_absolute(x: u32, y: u32, buttons: u8, wheel_vertical: i16) -> bool {
    let submitted =
        crate::input::event_queue::submit_pointer_absolute(x, y, buttons, wheel_vertical);
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

// RING3-MIGRATION-REFERENCE START: inputd should own pointer capture policy.
// Ring0 driver callbacks should forward validated packets to the
// service-owned ingress queue.
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
// RING3-MIGRATION-REFERENCE END: inputd-owned pointer capture policy.

fn log_pointer_delivery_once(kind: &'static str) {
    #[cfg(test)]
    {
        let _ = kind;
        return;
    }

    #[cfg(not(test))]
    if !POINTER_DELIVERY_LOGGED.swap(true, Ordering::AcqRel) {
        crate::debug::info!(input, "input: pointer event delivered kind={}", kind);
    }
}

#[cfg(test)]
mod tests {
    use super::{TEST_POINTER_CAPTURE_RESULT, reset_pointer_state, submit_pointer_packet};
    use core::sync::atomic::Ordering;
    use driver_abi::{POINTER_BUTTON_LEFT as POINTER_PACKET_LEFT, PointerPacket};
    use rustos_user_abi::syscall::{INPUTD_INGRESS_KIND_POINTER_PACKET, InputIngressWire};

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::exclusive_test()
    }

    fn reset_for_tests() {
        reset_pointer_state();
        TEST_POINTER_CAPTURE_RESULT.store(0, Ordering::Relaxed);
        crate::input::event_queue::reset_for_tests();
    }

    #[test]
    fn pointer_packet_still_reaches_event_queue_when_usb_capture_reports_true() {
        let _guard = isolated();
        reset_for_tests();
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

        let mut ingress = [InputIngressWire::default(); 4];
        let count = crate::input::event_queue::drain_ingress(&mut ingress);
        assert_eq!(count, 1);
        assert_eq!(ingress[0].kind, INPUTD_INGRESS_KIND_POINTER_PACKET);
        assert_eq!(ingress[0].pointer_packet.buttons, POINTER_PACKET_LEFT);
        assert_eq!(ingress[0].pointer_packet.dx, 7);
        assert_eq!(ingress[0].pointer_packet.dy, -3);
    }
}
