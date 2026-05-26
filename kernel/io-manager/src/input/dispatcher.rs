#[cfg(not(test))]
use x86_64::instructions::interrupts;

use crate::input::keyboard::KeyboardEvent;

pub(crate) fn dispatch_keyboard_event(event: KeyboardEvent) {
    #[cfg(test)]
    {
        dispatch_keyboard_event_locked(event);
    }

    #[cfg(not(test))]
    interrupts::without_interrupts(|| {
        dispatch_keyboard_event_locked(event);
    });
}

fn dispatch_keyboard_event_locked(event: KeyboardEvent) {
    let _ = crate::input::event_queue::submit_keyboard_event(event);
}

#[cfg(test)]
mod tests {
    use super::dispatch_keyboard_event;
    use crate::input::keyboard::{KeyAction, KeyCode, KeyboardEvent, Modifiers};

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::exclusive_test()
    }

    fn key_event() -> KeyboardEvent {
        KeyboardEvent {
            code: KeyCode::A,
            action: KeyAction::Pressed,
            modifiers: Modifiers::empty(),
            text: Some(b'a'),
        }
    }

    #[test]
    fn keyboard_event_lands_in_event_queue() {
        let _guard = isolated();
        crate::input::event_queue::reset_for_tests();

        dispatch_keyboard_event(key_event());

        let mut ingress = [rustos_user_abi::syscall::InputIngressWire::default(); 1];
        assert_eq!(crate::input::event_queue::drain_ingress(&mut ingress), 1);
        assert_eq!(
            ingress[0].kind,
            rustos_user_abi::syscall::INPUTD_INGRESS_KIND_KEYBOARD
        );
        assert_eq!(ingress[0].keyboard.code, KeyCode::A as u32);
    }
}
