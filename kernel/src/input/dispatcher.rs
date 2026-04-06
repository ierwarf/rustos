use spin::Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

use crate::input::keyboard::KeyboardEvent;
use crate::io::session::ConsoleSessionHandle;
use crate::util::ring::RingBuffer;

const PENDING_TTY_KEYBOARD_EVENTS_CAPACITY: usize = 1024;
const TTY_DISPATCH_BATCH_CAPACITY: usize = 128;
const TTY_KEYBOARD_DROP_LOG_INTERVAL: u64 = 128;

static PENDING_TTY_KEYBOARD_EVENTS: Mutex<PendingTtyKeyboardEventState> =
    Mutex::new(PendingTtyKeyboardEventState::new());

#[derive(Clone, Copy)]
struct PendingTtyKeyboardEvent {
    session: ConsoleSessionHandle,
    event: KeyboardEvent,
}

struct PendingTtyKeyboardEventState {
    queued: RingBuffer<PendingTtyKeyboardEvent, PENDING_TTY_KEYBOARD_EVENTS_CAPACITY>,
    dropped_events: u64,
}

impl PendingTtyKeyboardEventState {
    const fn new() -> Self {
        Self {
            queued: RingBuffer::new(),
            dropped_events: 0,
        }
    }

    fn can_push_event(&self) -> bool {
        self.queued.remaining_capacity() != 0
    }

    fn push_event(&mut self, event: PendingTtyKeyboardEvent) {
        debug_assert!(self.queued.push(event));
    }

    fn record_drop(&mut self) {
        self.dropped_events = self.dropped_events.saturating_add(1);
        if self.dropped_events % TTY_KEYBOARD_DROP_LOG_INTERVAL == 0 {
            crate::debug::println!(
                "tty keyboard queue overload: dropped={} queued={}",
                self.dropped_events,
                self.queued.len()
            );
        }
    }
}

pub(crate) fn dispatch_keyboard_event(event: KeyboardEvent) {
    if crate::usb::capture_keyboard_event(event) {
        return;
    }

    #[cfg(test)]
    {
        dispatch_keyboard_event_locked(event);
    }

    #[cfg(not(test))]
    interrupts::without_interrupts(|| {
        dispatch_keyboard_event_locked(event);
    });
}

pub(crate) fn service_pending() -> usize {
    let mut pending = [None; TTY_DISPATCH_BATCH_CAPACITY];
    let count = with_pending_tty_keyboard_events(|events| {
        let mut count = 0;
        for slot in pending.iter_mut() {
            let Some(event) = events.queued.pop() else {
                break;
            };
            *slot = Some(event);
            count += 1;
        }
        count
    });

    for event in pending[..count].iter().flatten() {
        crate::io::tty::on_key_event_for_session(event.session, event.event);
    }

    count
}

fn with_pending_tty_keyboard_events<R>(
    f: impl FnOnce(&mut PendingTtyKeyboardEventState) -> R,
) -> R {
    #[cfg(test)]
    {
        f(&mut PENDING_TTY_KEYBOARD_EVENTS.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut PENDING_TTY_KEYBOARD_EVENTS.lock()))
    }
}

fn dispatch_keyboard_event_locked(event: KeyboardEvent) {
    let session = if crate::io::gui::is_userspace_display_active() {
        None
    } else {
        crate::io::session::focused_console_session()
    };
    let mut input_events = crate::input::event_queue::lock_event_queue();
    let input_ok = input_events.can_accept_keyboard_event(event);
    let Some(session) = session else {
        if input_ok {
            let pushed = input_events.push_keyboard_event(event);
            debug_assert!(pushed);
        } else {
            input_events.note_keyboard_drop(event);
        }
        return;
    };

    let mut pending = PENDING_TTY_KEYBOARD_EVENTS.lock();
    let tty_ok = pending.can_push_event();
    if input_ok && tty_ok {
        let pushed = input_events.push_keyboard_event(event);
        debug_assert!(pushed);
        pending.push_event(PendingTtyKeyboardEvent { session, event });
        return;
    }

    input_events.note_keyboard_drop(event);
    pending.record_drop();
}

#[cfg(test)]
mod tests {
    use super::{
        PENDING_TTY_KEYBOARD_EVENTS, PENDING_TTY_KEYBOARD_EVENTS_CAPACITY, PendingTtyKeyboardEvent,
        PendingTtyKeyboardEventState, dispatch_keyboard_event, dispatch_keyboard_event_locked,
    };
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

    fn reset_for_tests() {
        *PENDING_TTY_KEYBOARD_EVENTS.lock() = PendingTtyKeyboardEventState::new();
        crate::input::event_queue::reset_for_tests();
        crate::io::session::reset_for_tests();
    }

    #[test]
    fn tty_overflow_drops_keyboard_event_for_both_sinks() {
        let _guard = isolated();
        reset_for_tests();
        let session = crate::io::session::create_console_session(1, "test", "/test")
            .expect("test console session");
        assert!(crate::io::session::set_focused_console_session(session));
        {
            let mut pending = PENDING_TTY_KEYBOARD_EVENTS.lock();
            for _ in 0..PENDING_TTY_KEYBOARD_EVENTS_CAPACITY {
                pending.push_event(PendingTtyKeyboardEvent {
                    session,
                    event: key_event(),
                });
            }
        }

        dispatch_keyboard_event_locked(key_event());

        let mut events = [crate::user::abi::device::InputEvent::default(); 1];
        assert_eq!(crate::input::event_queue::read_input_events(&mut events), 0);
        assert_eq!(
            PENDING_TTY_KEYBOARD_EVENTS.lock().queued.len(),
            PENDING_TTY_KEYBOARD_EVENTS_CAPACITY
        );
    }

    #[test]
    fn input_overflow_drops_keyboard_event_for_both_sinks() {
        let _guard = isolated();
        reset_for_tests();
        for code in 0..4096_u32 {
            crate::input::event_queue::push_pointer_button(code, true);
        }

        dispatch_keyboard_event(key_event());

        assert_eq!(PENDING_TTY_KEYBOARD_EVENTS.lock().queued.len(), 0);
    }
}
