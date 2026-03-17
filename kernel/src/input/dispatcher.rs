use spin::Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

use crate::keyboard::KeyboardEvent;
use crate::ring::RingBuffer;
use crate::session::ConsoleSessionId;

const PENDING_TTY_KEYBOARD_EVENTS_CAPACITY: usize = 256;
const TTY_DISPATCH_BATCH_CAPACITY: usize = 32;

static PENDING_TTY_KEYBOARD_EVENTS: Mutex<
    RingBuffer<PendingTtyKeyboardEvent, PENDING_TTY_KEYBOARD_EVENTS_CAPACITY>,
> = Mutex::new(RingBuffer::new());

#[derive(Clone, Copy)]
struct PendingTtyKeyboardEvent {
    session: ConsoleSessionId,
    event: KeyboardEvent,
}

pub(crate) fn dispatch_keyboard_event(event: KeyboardEvent) {
    crate::input::event_queue::push_keyboard_event(event);
    let session = crate::session::focused_console_session();
    with_pending_tty_keyboard_events(|pending| {
        pending.push_overwrite(PendingTtyKeyboardEvent { session, event });
    });
}

pub(crate) fn service_pending() -> usize {
    let mut pending = [None; TTY_DISPATCH_BATCH_CAPACITY];
    let count = with_pending_tty_keyboard_events(|events| {
        let mut count = 0;
        for slot in pending.iter_mut() {
            let Some(event) = events.pop() else {
                break;
            };
            *slot = Some(event);
            count += 1;
        }
        count
    });

    for event in pending[..count].iter().flatten() {
        crate::tty::on_key_event_for_session(event.session, event.event);
    }

    count
}

fn with_pending_tty_keyboard_events<R>(
    f: impl FnOnce(
        &mut RingBuffer<PendingTtyKeyboardEvent, PENDING_TTY_KEYBOARD_EVENTS_CAPACITY>,
    ) -> R,
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
