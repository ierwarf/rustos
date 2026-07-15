//! Shared evdev translation for the RustOS input pipeline.
//!
//! This crate owns the cold cpu-only logic that converts native
//! `rustos-user-abi::ui::UiInputEvent` records into Linux `evdev` events plus
//! the key/button code maps in either direction. The kernel uses it from the
//! `/dev/input/event0` read broker; `inputd` uses it to validate or rewrite
//! events once the queue moves out of ring0.
#![no_std]

use core::mem::size_of;

pub use keyboard_core::KeyCode;
pub use rustos_user_abi::ui::UiInputEvent as InputEvent;

pub const MAX_EVDEV_EVENTS_PER_INPUT_EVENT: usize = 3;
pub const MAX_INPUT_EVENTS_PER_READ: usize = 1024;
pub const MAX_NATIVE_READ_BYTES: usize = MAX_INPUT_EVENTS_PER_READ * size_of::<InputEvent>();
pub const MAX_EVDEV_EVENTS_PER_READ: usize =
    MAX_INPUT_EVENTS_PER_READ * MAX_EVDEV_EVENTS_PER_INPUT_EVENT;
pub const MAX_EVDEV_READ_BYTES: usize = MAX_EVDEV_EVENTS_PER_READ * size_of::<LinuxInputEvent>();

pub const INPUT_KIND_KEYBOARD: u16 = rustos_user_abi::ui::INPUT_KIND_KEYBOARD;
pub const INPUT_KIND_POINTER_MOTION: u16 = rustos_user_abi::ui::INPUT_KIND_POINTER_MOTION;
pub const INPUT_KIND_POINTER_BUTTON: u16 = rustos_user_abi::ui::INPUT_KIND_POINTER_BUTTON;
pub const INPUT_KIND_POINTER_SCROLL: u16 = rustos_user_abi::ui::INPUT_KIND_POINTER_SCROLL;
pub const INPUT_KIND_POINTER_POSITION: u16 = rustos_user_abi::ui::INPUT_KIND_POINTER_POSITION;

pub const INPUT_ACTION_NONE: u16 = rustos_user_abi::ui::INPUT_ACTION_NONE;
pub const INPUT_ACTION_PRESSED: u16 = rustos_user_abi::ui::INPUT_ACTION_PRESSED;
pub const INPUT_ACTION_RELEASED: u16 = rustos_user_abi::ui::INPUT_ACTION_RELEASED;
pub const INPUT_ACTION_REPEATED: u16 = rustos_user_abi::ui::INPUT_ACTION_REPEATED;

pub const POINTER_BUTTON_LEFT: u32 = rustos_user_abi::ui::POINTER_BUTTON_LEFT;
pub const POINTER_BUTTON_RIGHT: u32 = rustos_user_abi::ui::POINTER_BUTTON_RIGHT;
pub const POINTER_BUTTON_MIDDLE: u32 = rustos_user_abi::ui::POINTER_BUTTON_MIDDLE;
pub const POINTER_BUTTON_X1: u32 = rustos_user_abi::ui::POINTER_BUTTON_X1;
pub const POINTER_BUTTON_X2: u32 = rustos_user_abi::ui::POINTER_BUTTON_X2;

const HID_KEY_USAGE_MAP: &[(KeyCode, u8)] = &[
    (KeyCode::A, 0x04),
    (KeyCode::B, 0x05),
    (KeyCode::C, 0x06),
    (KeyCode::D, 0x07),
    (KeyCode::E, 0x08),
    (KeyCode::F, 0x09),
    (KeyCode::G, 0x0A),
    (KeyCode::H, 0x0B),
    (KeyCode::I, 0x0C),
    (KeyCode::J, 0x0D),
    (KeyCode::K, 0x0E),
    (KeyCode::L, 0x0F),
    (KeyCode::M, 0x10),
    (KeyCode::N, 0x11),
    (KeyCode::O, 0x12),
    (KeyCode::P, 0x13),
    (KeyCode::Q, 0x14),
    (KeyCode::R, 0x15),
    (KeyCode::S, 0x16),
    (KeyCode::T, 0x17),
    (KeyCode::U, 0x18),
    (KeyCode::V, 0x19),
    (KeyCode::W, 0x1A),
    (KeyCode::X, 0x1B),
    (KeyCode::Y, 0x1C),
    (KeyCode::Z, 0x1D),
    (KeyCode::Digit1, 0x1E),
    (KeyCode::Digit2, 0x1F),
    (KeyCode::Digit3, 0x20),
    (KeyCode::Digit4, 0x21),
    (KeyCode::Digit5, 0x22),
    (KeyCode::Digit6, 0x23),
    (KeyCode::Digit7, 0x24),
    (KeyCode::Digit8, 0x25),
    (KeyCode::Digit9, 0x26),
    (KeyCode::Digit0, 0x27),
    (KeyCode::Enter, 0x28),
    (KeyCode::Escape, 0x29),
    (KeyCode::Backspace, 0x2A),
    (KeyCode::Tab, 0x2B),
    (KeyCode::Space, 0x2C),
    (KeyCode::Minus, 0x2D),
    (KeyCode::Equal, 0x2E),
    (KeyCode::LeftBracket, 0x2F),
    (KeyCode::RightBracket, 0x30),
    (KeyCode::Backslash, 0x31),
    (KeyCode::Semicolon, 0x33),
    (KeyCode::Apostrophe, 0x34),
    (KeyCode::Grave, 0x35),
    (KeyCode::Comma, 0x36),
    (KeyCode::Dot, 0x37),
    (KeyCode::Slash, 0x38),
    (KeyCode::CapsLock, 0x39),
    (KeyCode::F1, 0x3A),
    (KeyCode::F2, 0x3B),
    (KeyCode::F3, 0x3C),
    (KeyCode::F4, 0x3D),
    (KeyCode::F5, 0x3E),
    (KeyCode::F6, 0x3F),
    (KeyCode::F7, 0x40),
    (KeyCode::F8, 0x41),
    (KeyCode::F9, 0x42),
    (KeyCode::F10, 0x43),
    (KeyCode::F11, 0x44),
    (KeyCode::F12, 0x45),
    (KeyCode::PrintScreen, 0x46),
    (KeyCode::ScrollLock, 0x47),
    (KeyCode::Pause, 0x48),
    (KeyCode::Insert, 0x49),
    (KeyCode::Home, 0x4A),
    (KeyCode::PageUp, 0x4B),
    (KeyCode::Delete, 0x4C),
    (KeyCode::End, 0x4D),
    (KeyCode::PageDown, 0x4E),
    (KeyCode::ArrowRight, 0x4F),
    (KeyCode::ArrowLeft, 0x50),
    (KeyCode::ArrowDown, 0x51),
    (KeyCode::ArrowUp, 0x52),
    (KeyCode::NumLock, 0x53),
    (KeyCode::NumpadSlash, 0x54),
    (KeyCode::NumpadStar, 0x55),
    (KeyCode::NumpadMinus, 0x56),
    (KeyCode::NumpadPlus, 0x57),
    (KeyCode::NumpadEnter, 0x58),
    (KeyCode::Numpad1, 0x59),
    (KeyCode::Numpad2, 0x5A),
    (KeyCode::Numpad3, 0x5B),
    (KeyCode::Numpad4, 0x5C),
    (KeyCode::Numpad5, 0x5D),
    (KeyCode::Numpad6, 0x5E),
    (KeyCode::Numpad7, 0x5F),
    (KeyCode::Numpad8, 0x60),
    (KeyCode::Numpad9, 0x61),
    (KeyCode::Numpad0, 0x62),
    (KeyCode::NumpadDot, 0x63),
    (KeyCode::Menu, 0x65),
    (KeyCode::LeftCtrl, 0xE0),
    (KeyCode::LeftShift, 0xE1),
    (KeyCode::LeftAlt, 0xE2),
    (KeyCode::LeftMeta, 0xE3),
    (KeyCode::RightCtrl, 0xE4),
    (KeyCode::RightShift, 0xE5),
    (KeyCode::RightAlt, 0xE6),
    (KeyCode::RightMeta, 0xE7),
];

const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const EV_SYN: u16 = 0x00;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinuxInputTimeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinuxInputEvent {
    pub time: LinuxInputTimeval,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvdevTranslateError {
    InvalidArgument,
}

pub fn hid_modifier_mask(usage: u8) -> Option<u8> {
    match usage {
        0xE0 => Some(1 << 0),
        0xE1 => Some(1 << 1),
        0xE2 => Some(1 << 2),
        0xE3 => Some(1 << 3),
        0xE4 => Some(1 << 4),
        0xE5 => Some(1 << 5),
        0xE6 => Some(1 << 6),
        0xE7 => Some(1 << 7),
        _ => None,
    }
}

pub fn mouse_buttons(buttons: u8) -> u8 {
    let mut value = 0u8;
    if buttons & rustos_user_abi::ui::POINTER_BUTTON_LEFT as u8 != 0 {
        value |= 1 << 0;
    }
    if buttons & rustos_user_abi::ui::POINTER_BUTTON_RIGHT as u8 != 0 {
        value |= 1 << 1;
    }
    if buttons & rustos_user_abi::ui::POINTER_BUTTON_MIDDLE as u8 != 0 {
        value |= 1 << 2;
    }
    value
}

pub fn pointer_buttons_from_report(buttons: u8) -> u8 {
    let mut value = 0u8;
    if buttons & (1 << 0) != 0 {
        value |= rustos_user_abi::ui::POINTER_BUTTON_LEFT as u8;
    }
    if buttons & (1 << 1) != 0 {
        value |= rustos_user_abi::ui::POINTER_BUTTON_RIGHT as u8;
    }
    if buttons & (1 << 2) != 0 {
        value |= rustos_user_abi::ui::POINTER_BUTTON_MIDDLE as u8;
    }
    value
}

pub fn clamp_i8(value: i32) -> i8 {
    value.clamp(i8::MIN as i32, i8::MAX as i32) as i8
}

pub fn keycode_to_hid_usage(code: KeyCode) -> Option<u8> {
    HID_KEY_USAGE_MAP
        .iter()
        .find_map(|(key_code, usage)| (*key_code == code).then_some(*usage))
}

pub fn hid_usage_to_keycode(usage: u8) -> Option<KeyCode> {
    HID_KEY_USAGE_MAP
        .iter()
        .find_map(|(key_code, hid_usage)| (*hid_usage == usage).then_some(*key_code))
}

pub fn translate_input_to_evdev(
    event: InputEvent,
    dest: &mut [LinuxInputEvent],
) -> Result<usize, EvdevTranslateError> {
    let mut count = 0usize;
    match event.kind {
        INPUT_KIND_KEYBOARD => {
            let code =
                rustos_key_code_to_linux(event.code).ok_or(EvdevTranslateError::InvalidArgument)?;
            push_evdev(
                dest,
                &mut count,
                EV_KEY,
                code,
                input_action_value(event.action),
            )?;
            push_syn(dest, &mut count)?;
        }
        INPUT_KIND_POINTER_MOTION => {
            if event.value0 != 0 {
                push_evdev(dest, &mut count, EV_REL, REL_X, event.value0)?;
            }
            if event.value1 != 0 {
                push_evdev(dest, &mut count, EV_REL, REL_Y, event.value1)?;
            }
            push_syn(dest, &mut count)?;
        }
        INPUT_KIND_POINTER_POSITION => {
            push_evdev(dest, &mut count, EV_ABS, ABS_X, event.value0)?;
            push_evdev(dest, &mut count, EV_ABS, ABS_Y, event.value1)?;
            push_syn(dest, &mut count)?;
        }
        INPUT_KIND_POINTER_BUTTON => {
            let code =
                pointer_button_to_linux(event.code).ok_or(EvdevTranslateError::InvalidArgument)?;
            push_evdev(
                dest,
                &mut count,
                EV_KEY,
                code,
                input_action_value(event.action),
            )?;
            push_syn(dest, &mut count)?;
        }
        INPUT_KIND_POINTER_SCROLL => {
            if event.value0 != 0 {
                push_evdev(dest, &mut count, EV_REL, REL_WHEEL, event.value0)?;
            }
            if event.value1 != 0 {
                push_evdev(dest, &mut count, EV_REL, REL_HWHEEL, event.value1)?;
            }
            push_syn(dest, &mut count)?;
        }
        _ => {}
    }
    Ok(count)
}

pub fn translate_input_events_to_evdev(
    input: &[InputEvent],
    dest: &mut [LinuxInputEvent],
) -> Result<usize, EvdevTranslateError> {
    let mut written = 0usize;
    for event in input {
        written += translate_input_to_evdev(*event, &mut dest[written..])?;
    }
    Ok(written)
}

pub fn linux_key_code_to_rustos(code: u32) -> Option<KeyCode> {
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

pub fn rustos_key_code_to_linux(code: u32) -> Option<u16> {
    let key_code = KeyCode::from_u32(code)?;
    for linux_code in 1..=127_u32 {
        if linux_key_code_to_rustos(linux_code) == Some(key_code) {
            return u16::try_from(linux_code).ok();
        }
    }
    None
}

pub fn pointer_button_to_linux(button: u32) -> Option<u16> {
    Some(match button {
        POINTER_BUTTON_LEFT => BTN_LEFT,
        POINTER_BUTTON_RIGHT => BTN_RIGHT,
        POINTER_BUTTON_MIDDLE => BTN_MIDDLE,
        POINTER_BUTTON_X1 => BTN_SIDE,
        POINTER_BUTTON_X2 => BTN_EXTRA,
        _ => return None,
    })
}

fn input_action_value(action: u16) -> i32 {
    match action {
        INPUT_ACTION_RELEASED => 0,
        INPUT_ACTION_REPEATED => 2,
        _ => 1,
    }
}

fn push_syn(dest: &mut [LinuxInputEvent], count: &mut usize) -> Result<(), EvdevTranslateError> {
    push_evdev(dest, count, EV_SYN, SYN_REPORT, 0)
}

fn push_evdev(
    dest: &mut [LinuxInputEvent],
    count: &mut usize,
    kind: u16,
    code: u16,
    value: i32,
) -> Result<(), EvdevTranslateError> {
    let slot = dest
        .get_mut(*count)
        .ok_or(EvdevTranslateError::InvalidArgument)?;
    *slot = LinuxInputEvent {
        time: LinuxInputTimeval::default(),
        kind,
        code,
        value,
    };
    *count += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_event_emits_key_and_syn() {
        let event = InputEvent {
            kind: INPUT_KIND_KEYBOARD,
            action: INPUT_ACTION_PRESSED,
            code: KeyCode::A as u32,
            value0: 0,
            value1: 0,
            modifiers: 0,
            text: b'a' as u32,
        };
        let mut dest = [LinuxInputEvent::default(); 3];
        let count = translate_input_to_evdev(event, &mut dest).expect("translate");
        assert_eq!(count, 2);
        assert_eq!(dest[0].kind, EV_KEY);
        assert_eq!(dest[1].kind, EV_SYN);
    }

    #[test]
    fn pointer_button_round_trip() {
        assert_eq!(pointer_button_to_linux(POINTER_BUTTON_LEFT), Some(BTN_LEFT));
        assert_eq!(pointer_button_to_linux(POINTER_BUTTON_X2), Some(BTN_EXTRA));
        assert_eq!(pointer_button_to_linux(0x9999), None);
    }

    #[test]
    fn linux_key_code_round_trip() {
        let rs = linux_key_code_to_rustos(30).expect("A");
        assert_eq!(rs, KeyCode::A);
        assert_eq!(rustos_key_code_to_linux(KeyCode::A as u32), Some(30));
    }
}
