#[cfg(not(test))]
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;

use driver_abi::PointerPacket;

#[cfg(not(test))]
static POINTER_DELIVERY_LOGGED: AtomicBool = AtomicBool::new(false);

pub(crate) fn reset_pointer_state() {}

pub(crate) fn submit_pointer_packet(packet: PointerPacket) -> bool {
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
    use super::{reset_pointer_state, submit_pointer_packet};
    use driver_abi::{POINTER_BUTTON_LEFT as POINTER_PACKET_LEFT, PointerPacket};
    use rustos_user_abi::syscall::{INPUTD_INGRESS_KIND_POINTER_PACKET, InputIngressWire};

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::exclusive_test()
    }

    fn reset_for_tests() {
        reset_pointer_state();
        crate::input::event_queue::reset_for_tests();
    }

    #[test]
    fn pointer_packet_reaches_inputd_ingress_queue() {
        let _guard = isolated();
        reset_for_tests();

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
