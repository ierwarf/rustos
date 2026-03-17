use spin::Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

use super::keyboard::{KeyAction, KeyboardEvent};
use crate::ring::RingBuffer;
use crate::user::abi::device::{
    INPUT_ACTION_NONE, INPUT_ACTION_PRESSED, INPUT_ACTION_RELEASED, INPUT_ACTION_REPEATED,
    INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON, INPUT_KIND_POINTER_MOTION,
    INPUT_KIND_POINTER_SCROLL, InputEvent, POINTER_BUTTON_LEFT,
};

const INPUT_EVENT_QUEUE_CAPACITY: usize = 256;

static INPUT_EVENTS: Mutex<RingBuffer<InputEvent, INPUT_EVENT_QUEUE_CAPACITY>> =
    Mutex::new(RingBuffer::new());

pub(crate) fn push_keyboard_event(event: KeyboardEvent) {
    with_event_queue(|events| {
        events.push_overwrite(InputEvent {
            kind: INPUT_KIND_KEYBOARD,
            action: map_key_action(event.action),
            code: event.code as u32,
            value0: 0,
            value1: 0,
            modifiers: event.modifiers.bits() as u32,
            text: event.text.unwrap_or(0) as u32,
        });
    });
}

pub(crate) fn push_pointer_motion(dx: i16, dy: i16) {
    if dx == 0 && dy == 0 {
        return;
    }

    with_event_queue(|events| {
        events.push_overwrite(InputEvent {
            kind: INPUT_KIND_POINTER_MOTION,
            action: INPUT_ACTION_NONE,
            code: 0,
            value0: dx as i32,
            value1: dy as i32,
            modifiers: 0,
            text: 0,
        });
    });
}

pub(crate) fn push_pointer_button(code: u32, pressed: bool) {
    with_event_queue(|events| {
        events.push_overwrite(InputEvent {
            kind: INPUT_KIND_POINTER_BUTTON,
            action: if pressed {
                INPUT_ACTION_PRESSED
            } else {
                INPUT_ACTION_RELEASED
            },
            code,
            value0: 0,
            value1: 0,
            modifiers: 0,
            text: 0,
        });
    });
}

pub(crate) fn push_pointer_scroll(vertical: i16, horizontal: i16) {
    if vertical == 0 && horizontal == 0 {
        return;
    }

    with_event_queue(|events| {
        events.push_overwrite(InputEvent {
            kind: INPUT_KIND_POINTER_SCROLL,
            action: INPUT_ACTION_NONE,
            code: 0,
            value0: vertical as i32,
            value1: horizontal as i32,
            modifiers: 0,
            text: 0,
        });
    });
}

pub(crate) fn push_pointer_button_left(pressed: bool) {
    push_pointer_button(POINTER_BUTTON_LEFT, pressed);
}

pub(crate) fn read_input_events(dest: &mut [InputEvent]) -> usize {
    with_event_queue(|events| events.pop_into(dest))
}

pub(crate) fn has_pending_input_events() -> bool {
    with_event_queue(|events| events.len() != 0)
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *INPUT_EVENTS.lock() = RingBuffer::new();
}

fn map_key_action(action: KeyAction) -> u16 {
    match action {
        KeyAction::Pressed => INPUT_ACTION_PRESSED,
        KeyAction::Released => INPUT_ACTION_RELEASED,
        KeyAction::Repeated => INPUT_ACTION_REPEATED,
    }
}

fn with_event_queue<R>(
    f: impl FnOnce(&mut RingBuffer<InputEvent, INPUT_EVENT_QUEUE_CAPACITY>) -> R,
) -> R {
    #[cfg(test)]
    {
        f(&mut INPUT_EVENTS.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut INPUT_EVENTS.lock()))
    }
}
