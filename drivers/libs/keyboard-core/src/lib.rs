#![no_std]

use bitflags::bitflags;

const EVENT_QUEUE_CAPACITY: usize = 128;

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

pub struct KeyboardDriver {
    pressed: [bool; KEY_CODE_COUNT],
    caps_lock: bool,
    num_lock: bool,
    scroll_lock: bool,
    events: RingBuffer<KeyboardEvent, EVENT_QUEUE_CAPACITY>,
}

impl KeyboardDriver {
    pub const fn new() -> Self {
        Self {
            pressed: [false; KEY_CODE_COUNT],
            caps_lock: false,
            num_lock: false,
            scroll_lock: false,
            events: RingBuffer::new(),
        }
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
    }

    /// End one untrusted input-provider session without leaving its pressed
    /// keys or modifier state visible to the next provider session.  Pending
    /// events are discarded before the synthetic releases so consumers never
    /// observe stale text or a key press after its provider was revoked.
    pub fn reset_provider_state(&mut self) {
        self.events = RingBuffer::new();
        self.caps_lock = false;
        self.num_lock = false;
        self.scroll_lock = false;
        for index in 0..KEY_CODE_COUNT {
            if !self.pressed[index] {
                continue;
            }
            let code = KeyCode::from_u32(index as u32).expect("contiguous keyboard key code");
            self.inject_key_transition(code, true);
        }
    }

    pub fn pop_event(&mut self) -> Option<KeyboardEvent> {
        self.events.pop()
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
    use super::{KeyAction, KeyCode, KeyboardDriver, KeyboardEvent, Modifiers};

    #[test]
    fn emits_text_for_basic_typing() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.inject_key_transition(KeyCode::A, false);
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
    fn navigation_keys_carry_no_text() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.inject_key_transition(KeyCode::ArrowUp, false);
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
        keyboard.inject_key_transition(KeyCode::CapsLock, false);
        assert!(keyboard.modifiers().contains(Modifiers::CAPS_LOCK));
        keyboard.inject_key_transition(KeyCode::A, false);
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

    #[test]
    fn provider_reset_emits_release_for_every_pressed_key_and_clears_modifiers() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.inject_key_transition(KeyCode::LeftCtrl, false);
        keyboard.inject_key_transition(KeyCode::A, false);
        assert!(keyboard.modifiers().contains(Modifiers::CTRL));

        keyboard.reset_provider_state();

        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::LeftCtrl,
                action: KeyAction::Released,
                modifiers: Modifiers::empty(),
                text: None,
            })
        );
        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::A,
                action: KeyAction::Released,
                modifiers: Modifiers::empty(),
                text: None,
            })
        );
        assert!(keyboard.pop_event().is_none());
        assert_eq!(keyboard.modifiers(), Modifiers::empty());
    }
}
