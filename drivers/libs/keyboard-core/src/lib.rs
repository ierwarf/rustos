#![no_std]

use bitflags::bitflags;
use pc_keyboard::{
    KeyCode as PcKeyCode, KeyEvent as PcKeyEvent, KeyState as PcKeyState,
    ScancodeSet as PcScancodeSet, ScancodeSet1, ScancodeSet2,
};

const EVENT_QUEUE_CAPACITY: usize = 128;
const TEXT_QUEUE_CAPACITY: usize = 512;

struct RingBuffer<T: Copy, const CAPACITY: usize> {
    data: [Option<T>; CAPACITY],
    head: usize,
    len: usize,
}

impl<T: Copy, const CAPACITY: usize> RingBuffer<T, CAPACITY> {
    const fn new() -> Self {
        Self {
            data: [None; CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn push_overwrite(&mut self, value: T) {
        self.normalize_head();
        if self.len == CAPACITY {
            self.data[self.head] = None;
            self.head = (self.head + 1) % CAPACITY;
            self.len -= 1;
            self.normalize_head();
        }

        let tail = (self.head + self.len) % CAPACITY;
        self.data[tail] = Some(value);
        self.len += 1;
    }

    fn push(&mut self, value: T) -> bool {
        self.normalize_head();
        if self.len == CAPACITY {
            return false;
        }

        let tail = (self.head + self.len) % CAPACITY;
        self.data[tail] = Some(value);
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<T> {
        self.normalize_head();
        if self.len == 0 {
            return None;
        }

        let value = self.data[self.head].take();
        self.head = (self.head + 1) % CAPACITY;
        self.len -= 1;
        value
    }

    fn pop_into(&mut self, dest: &mut [T]) -> usize {
        let mut count = 0;
        for slot in dest.iter_mut() {
            let Some(value) = self.pop() else {
                break;
            };
            *slot = value;
            count += 1;
        }
        count
    }

    fn normalize_head(&mut self) {
        while self.len != 0 && self.data[self.head].is_none() {
            self.head = (self.head + 1) % CAPACITY;
            self.len -= 1;
        }
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const CTRL = 1 << 1;
        const ALT = 1 << 2;
        const META = 1 << 3;
        const CAPS_LOCK = 1 << 4;
        const NUM_LOCK = 1 << 5;
        const SCROLL_LOCK = 1 << 6;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyAction {
    Pressed,
    Released,
    Repeated,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyCode {
    Escape,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Digit0,
    Minus,
    Equal,
    Backspace,
    Tab,
    Q,
    W,
    E,
    R,
    T,
    Y,
    U,
    I,
    O,
    P,
    LeftBracket,
    RightBracket,
    Enter,
    LeftCtrl,
    A,
    S,
    D,
    F,
    G,
    H,
    J,
    K,
    L,
    Semicolon,
    Apostrophe,
    Grave,
    LeftShift,
    Backslash,
    Z,
    X,
    C,
    V,
    B,
    N,
    M,
    Comma,
    Dot,
    Slash,
    RightShift,
    NumpadStar,
    LeftAlt,
    Space,
    CapsLock,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    NumLock,
    ScrollLock,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadMinus,
    Numpad4,
    Numpad5,
    Numpad6,
    NumpadPlus,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad0,
    NumpadDot,
    F11,
    F12,
    RightCtrl,
    RightAlt,
    LeftMeta,
    RightMeta,
    Menu,
    NumpadEnter,
    NumpadSlash,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    PrintScreen,
    Pause,
}

const KEY_CODE_COUNT: usize = KeyCode::Pause as usize + 1;

impl KeyCode {
    pub fn from_u32(value: u32) -> Option<Self> {
        if value > KeyCode::Pause as u32 {
            return None;
        }

        // `KeyCode` is a contiguous repr(u8) enum through `Pause`, checked
        // immediately above.
        Some(unsafe { core::mem::transmute::<u8, KeyCode>(value as u8) })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardEvent {
    pub code: KeyCode,
    pub action: KeyAction,
    pub modifiers: Modifiers,
    pub text: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanCodeSet {
    Set1,
    Set2,
}

impl ScanCodeSet {
    pub fn name(self) -> &'static str {
        match self {
            Self::Set1 => "set1",
            Self::Set2 => "set2",
        }
    }
}

#[derive(Clone, Copy)]
enum ParsedKey {
    Direct { code: KeyCode, released: bool },
}

pub struct KeyboardDriver {
    parser: ScanCodeParser,
    pressed: [bool; KEY_CODE_COUNT],
    caps_lock: bool,
    num_lock: bool,
    scroll_lock: bool,
    events: RingBuffer<KeyboardEvent, EVENT_QUEUE_CAPACITY>,
    text: RingBuffer<u8, TEXT_QUEUE_CAPACITY>,
}

impl KeyboardDriver {
    pub const fn new() -> Self {
        Self {
            parser: ScanCodeParser::new(),
            pressed: [false; KEY_CODE_COUNT],
            caps_lock: false,
            num_lock: false,
            scroll_lock: false,
            events: RingBuffer::new(),
            text: RingBuffer::new(),
        }
    }

    pub fn set_scan_code_set(&mut self, scan_set: ScanCodeSet) {
        self.parser.set_scan_set(scan_set);
    }

    pub fn feed_scancode(&mut self, scancode: u8) {
        if matches!(scancode, KEYBOARD_RESPONSE_ACK | KEYBOARD_RESPONSE_RESEND)
            || is_non_scancode_response(scancode)
        {
            return;
        }

        let Some(parsed) = self.parser.push(scancode) else {
            return;
        };
        let ParsedKey::Direct { code, released } = parsed;
        self.inject_key_transition(code, released);
    }

    pub fn inject_key_transition(&mut self, code: KeyCode, released: bool) {
        let action = self.update_key_state(code, released);
        let modifiers = self.modifiers();
        let text = key_to_text(code, action, modifiers);
        let event = KeyboardEvent {
            code,
            action,
            modifiers,
            text,
        };

        self.events.push_overwrite(event);
        if let Some(byte) = text {
            let _ = self.text.push(byte);
        }
    }

    pub fn read_text(&mut self, dest: &mut [u8]) -> usize {
        self.text.pop_into(dest)
    }

    pub fn pending_text_len(&self) -> usize {
        self.text.len()
    }

    pub fn pop_event(&mut self) -> Option<KeyboardEvent> {
        self.events.pop()
    }

    pub fn drain_events(&mut self, mut sink: impl FnMut(KeyboardEvent)) -> usize {
        let mut flushed = 0;
        while let Some(event) = self.pop_event() {
            sink(event);
            flushed += 1;
        }
        flushed
    }

    pub fn modifiers(&self) -> Modifiers {
        let mut modifiers = Modifiers::empty();
        if self.pressed[KeyCode::LeftShift as usize] || self.pressed[KeyCode::RightShift as usize] {
            modifiers.insert(Modifiers::SHIFT);
        }
        if self.pressed[KeyCode::LeftCtrl as usize] || self.pressed[KeyCode::RightCtrl as usize] {
            modifiers.insert(Modifiers::CTRL);
        }
        if self.pressed[KeyCode::LeftAlt as usize] || self.pressed[KeyCode::RightAlt as usize] {
            modifiers.insert(Modifiers::ALT);
        }
        if self.pressed[KeyCode::LeftMeta as usize] || self.pressed[KeyCode::RightMeta as usize] {
            modifiers.insert(Modifiers::META);
        }
        if self.caps_lock {
            modifiers.insert(Modifiers::CAPS_LOCK);
        }
        if self.num_lock {
            modifiers.insert(Modifiers::NUM_LOCK);
        }
        if self.scroll_lock {
            modifiers.insert(Modifiers::SCROLL_LOCK);
        }
        modifiers
    }

    fn update_key_state(&mut self, code: KeyCode, released: bool) -> KeyAction {
        if matches!(code, KeyCode::Pause) {
            return if released {
                KeyAction::Released
            } else {
                KeyAction::Pressed
            };
        }

        let index = code as usize;
        let was_pressed = self.pressed[index];

        if released {
            self.pressed[index] = false;
            return KeyAction::Released;
        }

        self.pressed[index] = true;
        match code {
            KeyCode::CapsLock if !was_pressed => self.caps_lock = !self.caps_lock,
            KeyCode::NumLock if !was_pressed => self.num_lock = !self.num_lock,
            KeyCode::ScrollLock if !was_pressed => self.scroll_lock = !self.scroll_lock,
            _ => {}
        }

        if was_pressed {
            KeyAction::Repeated
        } else {
            KeyAction::Pressed
        }
    }
}

impl Default for KeyboardDriver {
    fn default() -> Self {
        Self::new()
    }
}

struct ScanCodeParser {
    decoder: ScanCodeDecoder,
    hidden_pause_prefix: bool,
}

impl ScanCodeParser {
    const fn new() -> Self {
        Self {
            decoder: ScanCodeDecoder::Set1(ScancodeSet1::new()),
            hidden_pause_prefix: false,
        }
    }

    fn set_scan_set(&mut self, scan_set: ScanCodeSet) {
        self.decoder = match scan_set {
            ScanCodeSet::Set1 => ScanCodeDecoder::Set1(ScancodeSet1::new()),
            ScanCodeSet::Set2 => ScanCodeDecoder::Set2(ScancodeSet2::new()),
        };
        self.hidden_pause_prefix = false;
    }

    fn push(&mut self, byte: u8) -> Option<ParsedKey> {
        let event = match &mut self.decoder {
            ScanCodeDecoder::Set1(decoder) => decoder.advance_state(byte),
            ScanCodeDecoder::Set2(decoder) => decoder.advance_state(byte),
        }
        .ok()
        .flatten()?;
        self.translate_event(event)
    }

    fn translate_event(&mut self, event: PcKeyEvent) -> Option<ParsedKey> {
        let released = matches!(event.state, PcKeyState::Up);
        match event.code {
            PcKeyCode::RControl2 => {
                self.hidden_pause_prefix = !released;
                None
            }
            PcKeyCode::RAlt2 => None,
            PcKeyCode::NumpadLock if self.hidden_pause_prefix && !released => {
                self.hidden_pause_prefix = false;
                Some(ParsedKey::Direct {
                    code: KeyCode::Pause,
                    released: false,
                })
            }
            PcKeyCode::NumpadLock if self.hidden_pause_prefix => {
                self.hidden_pause_prefix = false;
                None
            }
            other => {
                self.hidden_pause_prefix = false;
                map_pc_key_code(other).map(|code| ParsedKey::Direct { code, released })
            }
        }
    }
}

enum ScanCodeDecoder {
    Set1(ScancodeSet1),
    Set2(ScancodeSet2),
}

fn map_pc_key_code(code: PcKeyCode) -> Option<KeyCode> {
    Some(match code {
        PcKeyCode::Escape => KeyCode::Escape,
        PcKeyCode::Key1 => KeyCode::Digit1,
        PcKeyCode::Key2 => KeyCode::Digit2,
        PcKeyCode::Key3 => KeyCode::Digit3,
        PcKeyCode::Key4 => KeyCode::Digit4,
        PcKeyCode::Key5 => KeyCode::Digit5,
        PcKeyCode::Key6 => KeyCode::Digit6,
        PcKeyCode::Key7 => KeyCode::Digit7,
        PcKeyCode::Key8 => KeyCode::Digit8,
        PcKeyCode::Key9 => KeyCode::Digit9,
        PcKeyCode::Key0 => KeyCode::Digit0,
        PcKeyCode::OemMinus => KeyCode::Minus,
        PcKeyCode::OemPlus => KeyCode::Equal,
        PcKeyCode::Backspace => KeyCode::Backspace,
        PcKeyCode::Tab => KeyCode::Tab,
        PcKeyCode::Q => KeyCode::Q,
        PcKeyCode::W => KeyCode::W,
        PcKeyCode::E => KeyCode::E,
        PcKeyCode::R => KeyCode::R,
        PcKeyCode::T => KeyCode::T,
        PcKeyCode::Y => KeyCode::Y,
        PcKeyCode::U => KeyCode::U,
        PcKeyCode::I => KeyCode::I,
        PcKeyCode::O => KeyCode::O,
        PcKeyCode::P => KeyCode::P,
        PcKeyCode::Oem4 => KeyCode::LeftBracket,
        PcKeyCode::Oem6 => KeyCode::RightBracket,
        PcKeyCode::Return => KeyCode::Enter,
        PcKeyCode::LControl => KeyCode::LeftCtrl,
        PcKeyCode::A => KeyCode::A,
        PcKeyCode::S => KeyCode::S,
        PcKeyCode::D => KeyCode::D,
        PcKeyCode::F => KeyCode::F,
        PcKeyCode::G => KeyCode::G,
        PcKeyCode::H => KeyCode::H,
        PcKeyCode::J => KeyCode::J,
        PcKeyCode::K => KeyCode::K,
        PcKeyCode::L => KeyCode::L,
        PcKeyCode::Oem1 => KeyCode::Semicolon,
        PcKeyCode::Oem3 => KeyCode::Apostrophe,
        PcKeyCode::Oem8 => KeyCode::Grave,
        PcKeyCode::LShift => KeyCode::LeftShift,
        PcKeyCode::Oem5 | PcKeyCode::Oem7 => KeyCode::Backslash,
        PcKeyCode::Z => KeyCode::Z,
        PcKeyCode::X => KeyCode::X,
        PcKeyCode::C => KeyCode::C,
        PcKeyCode::V => KeyCode::V,
        PcKeyCode::B => KeyCode::B,
        PcKeyCode::N => KeyCode::N,
        PcKeyCode::M => KeyCode::M,
        PcKeyCode::OemComma => KeyCode::Comma,
        PcKeyCode::OemPeriod => KeyCode::Dot,
        PcKeyCode::Oem2 => KeyCode::Slash,
        PcKeyCode::RShift => KeyCode::RightShift,
        PcKeyCode::NumpadMultiply => KeyCode::NumpadStar,
        PcKeyCode::LAlt => KeyCode::LeftAlt,
        PcKeyCode::Spacebar => KeyCode::Space,
        PcKeyCode::CapsLock => KeyCode::CapsLock,
        PcKeyCode::F1 => KeyCode::F1,
        PcKeyCode::F2 => KeyCode::F2,
        PcKeyCode::F3 => KeyCode::F3,
        PcKeyCode::F4 => KeyCode::F4,
        PcKeyCode::F5 => KeyCode::F5,
        PcKeyCode::F6 => KeyCode::F6,
        PcKeyCode::F7 => KeyCode::F7,
        PcKeyCode::F8 => KeyCode::F8,
        PcKeyCode::F9 => KeyCode::F9,
        PcKeyCode::F10 => KeyCode::F10,
        PcKeyCode::NumpadLock => KeyCode::NumLock,
        PcKeyCode::ScrollLock => KeyCode::ScrollLock,
        PcKeyCode::Numpad7 => KeyCode::Numpad7,
        PcKeyCode::Numpad8 => KeyCode::Numpad8,
        PcKeyCode::Numpad9 => KeyCode::Numpad9,
        PcKeyCode::NumpadSubtract => KeyCode::NumpadMinus,
        PcKeyCode::Numpad4 => KeyCode::Numpad4,
        PcKeyCode::Numpad5 => KeyCode::Numpad5,
        PcKeyCode::Numpad6 => KeyCode::Numpad6,
        PcKeyCode::NumpadAdd => KeyCode::NumpadPlus,
        PcKeyCode::Numpad1 => KeyCode::Numpad1,
        PcKeyCode::Numpad2 => KeyCode::Numpad2,
        PcKeyCode::Numpad3 => KeyCode::Numpad3,
        PcKeyCode::Numpad0 => KeyCode::Numpad0,
        PcKeyCode::NumpadPeriod => KeyCode::NumpadDot,
        PcKeyCode::F11 => KeyCode::F11,
        PcKeyCode::F12 => KeyCode::F12,
        PcKeyCode::RControl => KeyCode::RightCtrl,
        PcKeyCode::RAltGr => KeyCode::RightAlt,
        PcKeyCode::LWin => KeyCode::LeftMeta,
        PcKeyCode::RWin => KeyCode::RightMeta,
        PcKeyCode::Apps => KeyCode::Menu,
        PcKeyCode::NumpadEnter => KeyCode::NumpadEnter,
        PcKeyCode::NumpadDivide => KeyCode::NumpadSlash,
        PcKeyCode::Insert => KeyCode::Insert,
        PcKeyCode::Delete => KeyCode::Delete,
        PcKeyCode::Home => KeyCode::Home,
        PcKeyCode::End => KeyCode::End,
        PcKeyCode::PageUp => KeyCode::PageUp,
        PcKeyCode::PageDown => KeyCode::PageDown,
        PcKeyCode::ArrowUp => KeyCode::ArrowUp,
        PcKeyCode::ArrowDown => KeyCode::ArrowDown,
        PcKeyCode::ArrowLeft => KeyCode::ArrowLeft,
        PcKeyCode::ArrowRight => KeyCode::ArrowRight,
        PcKeyCode::PrintScreen => KeyCode::PrintScreen,
        PcKeyCode::PauseBreak => KeyCode::Pause,
        _ => return None,
    })
}

fn key_to_text(code: KeyCode, action: KeyAction, modifiers: Modifiers) -> Option<u8> {
    if matches!(action, KeyAction::Released) {
        return None;
    }

    let shift = modifiers.contains(Modifiers::SHIFT);
    let caps_lock = modifiers.contains(Modifiers::CAPS_LOCK);
    let num_lock = modifiers.contains(Modifiers::NUM_LOCK);

    Some(match code {
        KeyCode::Escape => 0x1B,
        KeyCode::Backspace => 0x08,
        KeyCode::Tab => b'\t',
        KeyCode::Enter | KeyCode::NumpadEnter => b'\n',
        KeyCode::Space => b' ',
        KeyCode::Digit1 => shifted(shift, b'1', b'!'),
        KeyCode::Digit2 => shifted(shift, b'2', b'@'),
        KeyCode::Digit3 => shifted(shift, b'3', b'#'),
        KeyCode::Digit4 => shifted(shift, b'4', b'$'),
        KeyCode::Digit5 => shifted(shift, b'5', b'%'),
        KeyCode::Digit6 => shifted(shift, b'6', b'^'),
        KeyCode::Digit7 => shifted(shift, b'7', b'&'),
        KeyCode::Digit8 => shifted(shift, b'8', b'*'),
        KeyCode::Digit9 => shifted(shift, b'9', b'('),
        KeyCode::Digit0 => shifted(shift, b'0', b')'),
        KeyCode::Minus => shifted(shift, b'-', b'_'),
        KeyCode::Equal => shifted(shift, b'=', b'+'),
        KeyCode::Q => letter(b'q', shift, caps_lock),
        KeyCode::W => letter(b'w', shift, caps_lock),
        KeyCode::E => letter(b'e', shift, caps_lock),
        KeyCode::R => letter(b'r', shift, caps_lock),
        KeyCode::T => letter(b't', shift, caps_lock),
        KeyCode::Y => letter(b'y', shift, caps_lock),
        KeyCode::U => letter(b'u', shift, caps_lock),
        KeyCode::I => letter(b'i', shift, caps_lock),
        KeyCode::O => letter(b'o', shift, caps_lock),
        KeyCode::P => letter(b'p', shift, caps_lock),
        KeyCode::LeftBracket => shifted(shift, b'[', b'{'),
        KeyCode::RightBracket => shifted(shift, b']', b'}'),
        KeyCode::A => letter(b'a', shift, caps_lock),
        KeyCode::S => letter(b's', shift, caps_lock),
        KeyCode::D => letter(b'd', shift, caps_lock),
        KeyCode::F => letter(b'f', shift, caps_lock),
        KeyCode::G => letter(b'g', shift, caps_lock),
        KeyCode::H => letter(b'h', shift, caps_lock),
        KeyCode::J => letter(b'j', shift, caps_lock),
        KeyCode::K => letter(b'k', shift, caps_lock),
        KeyCode::L => letter(b'l', shift, caps_lock),
        KeyCode::Semicolon => shifted(shift, b';', b':'),
        KeyCode::Apostrophe => shifted(shift, b'\'', b'"'),
        KeyCode::Grave => shifted(shift, b'`', b'~'),
        KeyCode::Backslash => shifted(shift, b'\\', b'|'),
        KeyCode::Z => letter(b'z', shift, caps_lock),
        KeyCode::X => letter(b'x', shift, caps_lock),
        KeyCode::C => letter(b'c', shift, caps_lock),
        KeyCode::V => letter(b'v', shift, caps_lock),
        KeyCode::B => letter(b'b', shift, caps_lock),
        KeyCode::N => letter(b'n', shift, caps_lock),
        KeyCode::M => letter(b'm', shift, caps_lock),
        KeyCode::Comma => shifted(shift, b',', b'<'),
        KeyCode::Dot => shifted(shift, b'.', b'>'),
        KeyCode::Slash => shifted(shift, b'/', b'?'),
        KeyCode::NumpadSlash => b'/',
        KeyCode::NumpadStar => b'*',
        KeyCode::NumpadMinus => b'-',
        KeyCode::NumpadPlus => b'+',
        KeyCode::Numpad0 if num_lock => b'0',
        KeyCode::Numpad1 if num_lock => b'1',
        KeyCode::Numpad2 if num_lock => b'2',
        KeyCode::Numpad3 if num_lock => b'3',
        KeyCode::Numpad4 if num_lock => b'4',
        KeyCode::Numpad5 if num_lock => b'5',
        KeyCode::Numpad6 if num_lock => b'6',
        KeyCode::Numpad7 if num_lock => b'7',
        KeyCode::Numpad8 if num_lock => b'8',
        KeyCode::Numpad9 if num_lock => b'9',
        KeyCode::NumpadDot if num_lock => b'.',
        _ => return None,
    })
}

const KEYBOARD_RESPONSE_ECHO: u8 = 0xEE;
const KEYBOARD_RESPONSE_ACK: u8 = 0xFA;
const KEYBOARD_RESPONSE_RESEND: u8 = 0xFE;
const KEYBOARD_RESPONSE_OVERRUN_0: u8 = 0x00;
const KEYBOARD_RESPONSE_OVERRUN_FF: u8 = 0xFF;

const fn is_non_scancode_response(byte: u8) -> bool {
    matches!(
        byte,
        KEYBOARD_RESPONSE_ECHO | KEYBOARD_RESPONSE_OVERRUN_0 | KEYBOARD_RESPONSE_OVERRUN_FF
    )
}

const fn shifted(shift: bool, normal: u8, shifted: u8) -> u8 {
    if shift { shifted } else { normal }
}

const fn letter(lowercase: u8, shift: bool, caps_lock: bool) -> u8 {
    if shift ^ caps_lock {
        lowercase - 32
    } else {
        lowercase
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyAction, KeyCode, KeyboardDriver, KeyboardEvent, Modifiers, ScanCodeSet};

    #[test]
    fn emits_text_for_basic_typing() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.feed_scancode(0x1e);
        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::A,
                action: KeyAction::Pressed,
                modifiers: Modifiers::empty(),
                text: Some(b'a'),
            })
        );
    }

    #[test]
    fn decodes_set2_extended_keys_without_text() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.set_scan_code_set(ScanCodeSet::Set2);
        keyboard.feed_scancode(0xe0);
        keyboard.feed_scancode(0x75);
        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::ArrowUp,
                action: KeyAction::Pressed,
                modifiers: Modifiers::empty(),
                text: None,
            })
        );
    }

    #[test]
    fn updates_modifiers_and_caps_lock() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.feed_scancode(0x3a);
        assert!(keyboard.modifiers().contains(Modifiers::CAPS_LOCK));
        keyboard.feed_scancode(0x1e);
        assert_eq!(keyboard.pop_event().unwrap().code, KeyCode::CapsLock);
        assert_eq!(keyboard.pop_event().unwrap().text, Some(b'A'));
    }

    #[test]
    fn direct_injected_transition_is_supported() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.inject_key_transition(KeyCode::A, false);
        keyboard.inject_key_transition(KeyCode::A, true);
        assert_eq!(keyboard.pop_event().unwrap().action, KeyAction::Pressed);
        assert_eq!(keyboard.pop_event().unwrap().action, KeyAction::Released);
    }
}
