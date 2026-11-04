//! Linux evdev and RustOS input reads and event translation.

use super::*;

pub(crate) fn read_input(fds: &[OwnedFd], events: &mut [InputEvent]) -> Result<usize, i32> {
    if fds.is_empty() || events.is_empty() {
        return Ok(0);
    }
    if running_on_rustos() {
        let fd = fds[0].as_raw_fd();
        // A direct inputd READ is a stateful authorize/read transaction with
        // the generic service IPC deadline.  Never start it merely to discover
        // that the queue is empty: the 0-time poll path performs the existing
        // 16 ms-bounded, non-consuming STATS recheck first.  This keeps an idle
        // or wedged input service from pinning uiserver's reader until its
        // process-wide watchdog fires and withdraws the display compositor.
        if !input_fd_ready(fd)? {
            return Ok(0);
        }
        let bytes = match read(
            fd,
            events.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of_val(events),
        ) {
            Ok(bytes) => bytes,
            Err(EAGAIN) => return Ok(0),
            Err(err) => return Err(err),
        };
        if bytes == 0 {
            return Ok(0);
        }
        if bytes % size_of::<InputEvent>() != 0 {
            return Err(5);
        }
        return Ok(bytes / size_of::<InputEvent>());
    }

    let mut raw_events = [LinuxInputEvent::default(); MAX_EVDEV_EVENTS_PER_READ];
    let mut written = 0usize;
    let mut state = input_translation_state().lock().unwrap();
    for fd in fds {
        if written >= events.len() {
            break;
        }

        let bytes = match read(
            fd.as_raw_fd(),
            raw_events.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of_val(&raw_events),
        ) {
            Ok(bytes) => bytes,
            Err(EAGAIN) => continue,
            Err(err) => return Err(err),
        };
        if bytes == 0 {
            continue;
        }
        if bytes % size_of::<LinuxInputEvent>() != 0 {
            return Err(5);
        }
        let count = bytes / size_of::<LinuxInputEvent>();
        for raw in &raw_events[..count] {
            if written >= events.len() {
                break;
            }
            if let Some(event) = translate_linux_input_event(&mut state, *raw) {
                events[written] = event;
                written += 1;
            }
        }
    }
    Ok(written)
}

pub(super) fn input_fd_ready(fd: RawFd) -> Result<bool, i32> {
    let mut poll_fd = LinuxPollFd {
        fd,
        events: POLLIN,
        revents: 0,
    };
    let ready = unsafe { syscall3(SYS_POLL, (&mut poll_fd as *mut LinuxPollFd) as usize, 1, 0) };
    let ready = syscall_usize(ready)?;
    if ready == 0 {
        return Ok(false);
    }
    input_poll_revents(poll_fd.revents)
}

pub(super) fn input_poll_revents(revents: i16) -> Result<bool, i32> {
    if revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
        return Err(5);
    }
    Ok(revents & POLLIN != 0)
}

pub(super) fn translate_linux_input_event(
    state: &mut InputTranslationState,
    raw: LinuxInputEvent,
) -> Option<InputEvent> {
    match raw.kind {
        EV_KEY => {
            if raw.code >= BTN_LEFT {
                let code = linux_pointer_button_to_rustos(raw.code)?;
                return Some(InputEvent {
                    kind: INPUT_KIND_POINTER_BUTTON,
                    action: linux_key_value_to_action(raw.value)?,
                    code,
                    value0: 0,
                    value1: 0,
                    modifiers: 0,
                    text: 0,
                });
            }

            let key_code = linux_key_code_to_rustos(u32::from(raw.code))?;
            state
                .keyboard
                .inject_key_transition(key_code, linux_key_value_is_release(raw.value)?);
            let keyboard_event = state.keyboard.pop_event()?;
            Some(InputEvent {
                kind: INPUT_KIND_KEYBOARD,
                action: keyboard_action_to_input(keyboard_event.action),
                code: keyboard_event.code as u32,
                value0: 0,
                value1: 0,
                modifiers: keyboard_event.modifiers.bits() as u32,
                text: keyboard_event.text.unwrap_or(0) as u32,
            })
        }
        EV_REL => match raw.code {
            REL_X => Some(InputEvent {
                kind: INPUT_KIND_POINTER_MOTION,
                action: INPUT_ACTION_NONE,
                code: 0,
                value0: raw.value,
                value1: 0,
                modifiers: 0,
                text: 0,
            }),
            REL_Y => Some(InputEvent {
                kind: INPUT_KIND_POINTER_MOTION,
                action: INPUT_ACTION_NONE,
                code: 0,
                value0: 0,
                value1: raw.value,
                modifiers: 0,
                text: 0,
            }),
            REL_WHEEL => Some(InputEvent {
                kind: INPUT_KIND_POINTER_SCROLL,
                action: INPUT_ACTION_NONE,
                code: 0,
                value0: raw.value,
                value1: 0,
                modifiers: 0,
                text: 0,
            }),
            REL_HWHEEL => Some(InputEvent {
                kind: INPUT_KIND_POINTER_SCROLL,
                action: INPUT_ACTION_NONE,
                code: 0,
                value0: 0,
                value1: raw.value,
                modifiers: 0,
                text: 0,
            }),
            _ => None,
        },
        EV_ABS => match raw.code {
            ABS_X => {
                state.abs_x = raw.value;
                Some(InputEvent {
                    kind: INPUT_KIND_POINTER_POSITION,
                    action: INPUT_ACTION_NONE,
                    code: 0,
                    value0: state.abs_x,
                    value1: state.abs_y,
                    modifiers: 0,
                    text: 0,
                })
            }
            ABS_Y => {
                state.abs_y = raw.value;
                Some(InputEvent {
                    kind: INPUT_KIND_POINTER_POSITION,
                    action: INPUT_ACTION_NONE,
                    code: 0,
                    value0: state.abs_x,
                    value1: state.abs_y,
                    modifiers: 0,
                    text: 0,
                })
            }
            _ => None,
        },
        EV_SYN if raw.code == SYN_REPORT => None,
        _ => None,
    }
}

pub(super) fn linux_key_value_to_action(value: i32) -> Option<u16> {
    match value {
        0 => Some(INPUT_ACTION_RELEASED),
        1 => Some(INPUT_ACTION_PRESSED),
        2 => Some(INPUT_ACTION_REPEATED),
        _ => None,
    }
}

pub(super) fn linux_key_value_is_release(value: i32) -> Option<bool> {
    match value {
        0 => Some(true),
        1 | 2 => Some(false),
        _ => None,
    }
}

pub(super) fn keyboard_action_to_input(action: KeyAction) -> u16 {
    match action {
        KeyAction::Pressed => INPUT_ACTION_PRESSED,
        KeyAction::Released => INPUT_ACTION_RELEASED,
        KeyAction::Repeated => INPUT_ACTION_REPEATED,
    }
}

pub(super) fn linux_pointer_button_to_rustos(code: u16) -> Option<u32> {
    Some(match code {
        BTN_LEFT => POINTER_BUTTON_LEFT,
        BTN_RIGHT => POINTER_BUTTON_RIGHT,
        BTN_MIDDLE => POINTER_BUTTON_MIDDLE,
        BTN_SIDE => POINTER_BUTTON_X1,
        BTN_EXTRA => POINTER_BUTTON_X2,
        _ => return None,
    })
}

pub(super) fn linux_key_code_to_rustos(code: u32) -> Option<KeyCode> {
    Some(match code {
        1 => KeyCode::Escape,
        2 => KeyCode::Digit1,
        3 => KeyCode::Digit2,
        4 => KeyCode::Digit3,
        5 => KeyCode::Digit4,
        6 => KeyCode::Digit5,
        7 => KeyCode::Digit6,
        8 => KeyCode::Digit7,
        9 => KeyCode::Digit8,
        10 => KeyCode::Digit9,
        11 => KeyCode::Digit0,
        12 => KeyCode::Minus,
        13 => KeyCode::Equal,
        14 => KeyCode::Backspace,
        15 => KeyCode::Tab,
        16 => KeyCode::Q,
        17 => KeyCode::W,
        18 => KeyCode::E,
        19 => KeyCode::R,
        20 => KeyCode::T,
        21 => KeyCode::Y,
        22 => KeyCode::U,
        23 => KeyCode::I,
        24 => KeyCode::O,
        25 => KeyCode::P,
        26 => KeyCode::LeftBracket,
        27 => KeyCode::RightBracket,
        28 => KeyCode::Enter,
        29 => KeyCode::LeftCtrl,
        30 => KeyCode::A,
        31 => KeyCode::S,
        32 => KeyCode::D,
        33 => KeyCode::F,
        34 => KeyCode::G,
        35 => KeyCode::H,
        36 => KeyCode::J,
        37 => KeyCode::K,
        38 => KeyCode::L,
        39 => KeyCode::Semicolon,
        40 => KeyCode::Apostrophe,
        41 => KeyCode::Grave,
        42 => KeyCode::LeftShift,
        43 => KeyCode::Backslash,
        44 => KeyCode::Z,
        45 => KeyCode::X,
        46 => KeyCode::C,
        47 => KeyCode::V,
        48 => KeyCode::B,
        49 => KeyCode::N,
        50 => KeyCode::M,
        51 => KeyCode::Comma,
        52 => KeyCode::Dot,
        53 => KeyCode::Slash,
        54 => KeyCode::RightShift,
        55 => KeyCode::NumpadStar,
        56 => KeyCode::LeftAlt,
        57 => KeyCode::Space,
        58 => KeyCode::CapsLock,
        59 => KeyCode::F1,
        60 => KeyCode::F2,
        61 => KeyCode::F3,
        62 => KeyCode::F4,
        63 => KeyCode::F5,
        64 => KeyCode::F6,
        65 => KeyCode::F7,
        66 => KeyCode::F8,
        67 => KeyCode::F9,
        68 => KeyCode::F10,
        69 => KeyCode::NumLock,
        70 => KeyCode::ScrollLock,
        71 => KeyCode::Numpad7,
        72 => KeyCode::Numpad8,
        73 => KeyCode::Numpad9,
        74 => KeyCode::NumpadMinus,
        75 => KeyCode::Numpad4,
        76 => KeyCode::Numpad5,
        77 => KeyCode::Numpad6,
        78 => KeyCode::NumpadPlus,
        79 => KeyCode::Numpad1,
        80 => KeyCode::Numpad2,
        81 => KeyCode::Numpad3,
        82 => KeyCode::Numpad0,
        83 => KeyCode::NumpadDot,
        87 => KeyCode::F11,
        88 => KeyCode::F12,
        96 => KeyCode::NumpadEnter,
        97 => KeyCode::RightCtrl,
        98 => KeyCode::NumpadSlash,
        100 => KeyCode::RightAlt,
        102 => KeyCode::Home,
        103 => KeyCode::ArrowUp,
        104 => KeyCode::PageUp,
        105 => KeyCode::ArrowLeft,
        106 => KeyCode::ArrowRight,
        107 => KeyCode::End,
        108 => KeyCode::ArrowDown,
        109 => KeyCode::PageDown,
        110 => KeyCode::Insert,
        111 => KeyCode::Delete,
        125 => KeyCode::LeftMeta,
        126 => KeyCode::RightMeta,
        127 => KeyCode::Menu,
        _ => return None,
    })
}
