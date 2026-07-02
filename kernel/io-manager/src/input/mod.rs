// RING3-MIGRATION-REFERENCE START: inputd/driverd should own input provider
// selection and legacy bus service policy. Ring0 keeps interrupt lower-half
// service hooks for native input substrate.
use crate::driver;
use driver_abi::{DriverBus, DriverClass};

pub mod event_queue;
pub(crate) mod i8042;
pub(crate) mod keyboard;

const KEYBOARD_DRIVER_NAME: &str = "rustos-keyboard";

pub fn init() {
    driver::register_kernel_builtin(KEYBOARD_DRIVER_NAME, DriverClass::Input, DriverBus::Serio);
    report_keyboard_transport(i8042::init_keyboard_port());
    initialize_aux_transport();
}

pub fn on_keyboard_interrupt() {
    i8042::on_keyboard_interrupt();
}

pub fn on_mouse_interrupt() {
    i8042::on_aux_interrupt();
}

pub fn service_pending() -> usize {
    serio_lower_half_service_pending()
}

fn report_keyboard_transport(result: i8042::KeyboardTransportInitResult) {
    match result {
        i8042::KeyboardTransportInitResult::Ready(info) => {
            keyboard::configure_scancode_transport(info.translated);
            crate::debug::println!(
                "PS/2 keyboard transport ready: translated={}, self_test={}, port_test={}",
                info.translated,
                info.controller_self_test_passed,
                info.first_port_test_passed,
            );
            crate::io::console::write(b"PS/2 keyboard transport ready.\r\n");
        }
        i8042::KeyboardTransportInitResult::Unavailable(_reason) => {
            crate::debug::println!("PS/2 keyboard transport unavailable: {}", _reason);
            crate::io::console::write(b"PS/2 keyboard transport unavailable.\r\n");
        }
    }
}

fn report_aux_transport(result: i8042::AuxTransportInitResult) {
    match result {
        i8042::AuxTransportInitResult::Ready(_info) => {
            crate::debug::println!(
                "PS/2 aux serio port ready: configured={}, port_test={}",
                _info.controller_configured,
                _info.second_port_test_passed,
            );
            if !crate::io::gui::is_userspace_display_active() {
                crate::io::console::write(b"PS/2 aux serio port ready.\r\n");
            }
        }
        i8042::AuxTransportInitResult::Unavailable(_reason) => {
            crate::debug::println!("PS/2 aux serio port unavailable: {}", _reason);
            if !crate::io::gui::is_userspace_display_active() {
                crate::io::console::write(b"PS/2 aux serio port unavailable.\r\n");
            }
        }
    }
}

pub(crate) fn initialize_aux_transport() -> bool {
    if crate::usb::has_runtime_pointer_device() {
        crate::debug::println!(
            "input: USB pointer present; keeping PS/2 aux fallback probe enabled"
        );
    }

    match i8042::init_aux_mouse_port() {
        i8042::AuxTransportInitResult::Ready(info) => {
            crate::debug::println!("input: PS/2 aux transport ready");
            report_aux_transport(i8042::AuxTransportInitResult::Ready(info));
            true
        }
        i8042::AuxTransportInitResult::Unavailable(reason) => {
            crate::debug::println!("input: PS/2 aux transport unavailable: {}", reason);
            report_aux_transport(i8042::AuxTransportInitResult::Unavailable(reason));
            false
        }
    }
}

pub(crate) fn serio_lower_half_service_pending() -> usize {
    i8042::service_pending() + crate::driver::serio::service_pending()
}
// RING3-MIGRATION-REFERENCE END: inputd/driverd-owned input provider policy.
