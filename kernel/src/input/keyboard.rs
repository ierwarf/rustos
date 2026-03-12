use bitflags::bitflags;
use spin::Mutex;
use x86_64::instructions::{interrupts, port::Port};

use crate::ring::RingBuffer;

const KEYBOARD_IRQ: u8 = 1;
const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const STATUS_OUTPUT_READY: u8 = 1 << 0;
const MAX_SCANCODES_PER_INTERRUPT: usize = 32;
const EVENT_QUEUE_CAPACITY: usize = 128;
const TEXT_QUEUE_CAPACITY: usize = 512;

static KEYBOARD: Mutex<KeyboardDriver> = Mutex::new(KeyboardDriver::new());

bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const CTRL = 1 << 1;
        const ALT = 1 << 2;
        const CAPS_LOCK = 1 << 3;
        const NUM_LOCK = 1 << 4;
        const SCROLL_LOCK = 1 << 5;
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
}

const KEY_CODE_COUNT: usize = KeyCode::ArrowRight as usize + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardEvent {
    pub code: KeyCode,
    pub action: KeyAction,
    pub modifiers: Modifiers,
    pub text: Option<u8>,
}

pub fn init() {
    interrupts::without_interrupts(|| {
        drain_output_buffer();
    });
    crate::pic::enable_irq(KEYBOARD_IRQ);
}

pub fn on_interrupt() {
    interrupts::without_interrupts(|| {
        let mut keyboard = KEYBOARD.lock();
        for _ in 0..MAX_SCANCODES_PER_INTERRUPT {
            let Some(scancode) = read_scancode() else {
                break;
            };
            keyboard.on_scancode(scancode);
        }
    });
}

#[allow(dead_code)]
pub fn read_text(dest: &mut [u8]) -> usize {
    interrupts::without_interrupts(|| KEYBOARD.lock().read_text(dest))
}

#[allow(dead_code)]
pub fn pending_text_len() -> usize {
    interrupts::without_interrupts(|| KEYBOARD.lock().pending_text_len())
}

#[allow(dead_code)]
pub fn pop_event() -> Option<KeyboardEvent> {
    interrupts::without_interrupts(|| KEYBOARD.lock().pop_event())
}

#[allow(dead_code)]
pub fn modifiers() -> Modifiers {
    interrupts::without_interrupts(|| KEYBOARD.lock().modifiers())
}

fn drain_output_buffer() {
    while read_scancode().is_some() {}
}

fn read_scancode() -> Option<u8> {
    unsafe {
        let mut status_port = Port::<u8>::new(STATUS_PORT);
        if status_port.read() & STATUS_OUTPUT_READY == 0 {
            return None;
        }

        let mut data_port = Port::<u8>::new(DATA_PORT);
        Some(data_port.read())
    }
}

struct KeyboardDriver {
    parser: ScanCodeParser,
    pressed: [bool; KEY_CODE_COUNT],
    caps_lock: bool,
    num_lock: bool,
    scroll_lock: bool,
    events: RingBuffer<KeyboardEvent, EVENT_QUEUE_CAPACITY>,
    text: RingBuffer<u8, TEXT_QUEUE_CAPACITY>,
}

impl KeyboardDriver {
    const fn new() -> Self {
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

    fn on_scancode(&mut self, scancode: u8) {
        let Some(decoded) = self.parser.push(scancode) else {
            return;
        };
        let Some(code) = key_code_from_scancode(decoded.code, decoded.extended) else {
            return;
        };

        let action = self.update_key_state(code, decoded.released);
        let modifiers = self.modifiers();
        let text = key_to_text(code, action, modifiers);
        let event = KeyboardEvent {
            code,
            action,
            modifiers,
            text,
        };

        let _ = self.events.push(event);
        if let Some(byte) = text {
            let _ = self.text.push(byte);
        }
    }

    fn read_text(&mut self, dest: &mut [u8]) -> usize {
        self.text.pop_into(dest)
    }

    fn pending_text_len(&self) -> usize {
        self.text.len()
    }

    fn pop_event(&mut self) -> Option<KeyboardEvent> {
        self.events.pop()
    }

    fn modifiers(&self) -> Modifiers {
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

#[derive(Clone, Copy)]
struct DecodedScanCode {
    code: u8,
    released: bool,
    extended: bool,
}

struct ScanCodeParser {
    extended: bool,
    pause_bytes_remaining: u8,
}

impl ScanCodeParser {
    const fn new() -> Self {
        Self {
            extended: false,
            pause_bytes_remaining: 0,
        }
    }

    fn push(&mut self, byte: u8) -> Option<DecodedScanCode> {
        if self.pause_bytes_remaining != 0 {
            self.pause_bytes_remaining -= 1;
            return None;
        }

        match byte {
            0xE0 => {
                self.extended = true;
                None
            }
            0xE1 => {
                self.pause_bytes_remaining = 5;
                None
            }
            _ => {
                let extended = self.extended;
                self.extended = false;
                Some(DecodedScanCode {
                    code: byte & 0x7F,
                    released: (byte & 0x80) != 0,
                    extended,
                })
            }
        }
    }
}

fn key_code_from_scancode(code: u8, extended: bool) -> Option<KeyCode> {
    Some(if extended {
        match code {
            0x1C => KeyCode::NumpadEnter,
            0x1D => KeyCode::RightCtrl,
            0x35 => KeyCode::NumpadSlash,
            0x38 => KeyCode::RightAlt,
            0x47 => KeyCode::Home,
            0x48 => KeyCode::ArrowUp,
            0x49 => KeyCode::PageUp,
            0x4B => KeyCode::ArrowLeft,
            0x4D => KeyCode::ArrowRight,
            0x4F => KeyCode::End,
            0x50 => KeyCode::ArrowDown,
            0x51 => KeyCode::PageDown,
            0x52 => KeyCode::Insert,
            0x53 => KeyCode::Delete,
            _ => return None,
        }
    } else {
        match code {
            0x01 => KeyCode::Escape,
            0x02 => KeyCode::Digit1,
            0x03 => KeyCode::Digit2,
            0x04 => KeyCode::Digit3,
            0x05 => KeyCode::Digit4,
            0x06 => KeyCode::Digit5,
            0x07 => KeyCode::Digit6,
            0x08 => KeyCode::Digit7,
            0x09 => KeyCode::Digit8,
            0x0A => KeyCode::Digit9,
            0x0B => KeyCode::Digit0,
            0x0C => KeyCode::Minus,
            0x0D => KeyCode::Equal,
            0x0E => KeyCode::Backspace,
            0x0F => KeyCode::Tab,
            0x10 => KeyCode::Q,
            0x11 => KeyCode::W,
            0x12 => KeyCode::E,
            0x13 => KeyCode::R,
            0x14 => KeyCode::T,
            0x15 => KeyCode::Y,
            0x16 => KeyCode::U,
            0x17 => KeyCode::I,
            0x18 => KeyCode::O,
            0x19 => KeyCode::P,
            0x1A => KeyCode::LeftBracket,
            0x1B => KeyCode::RightBracket,
            0x1C => KeyCode::Enter,
            0x1D => KeyCode::LeftCtrl,
            0x1E => KeyCode::A,
            0x1F => KeyCode::S,
            0x20 => KeyCode::D,
            0x21 => KeyCode::F,
            0x22 => KeyCode::G,
            0x23 => KeyCode::H,
            0x24 => KeyCode::J,
            0x25 => KeyCode::K,
            0x26 => KeyCode::L,
            0x27 => KeyCode::Semicolon,
            0x28 => KeyCode::Apostrophe,
            0x29 => KeyCode::Grave,
            0x2A => KeyCode::LeftShift,
            0x2B => KeyCode::Backslash,
            0x2C => KeyCode::Z,
            0x2D => KeyCode::X,
            0x2E => KeyCode::C,
            0x2F => KeyCode::V,
            0x30 => KeyCode::B,
            0x31 => KeyCode::N,
            0x32 => KeyCode::M,
            0x33 => KeyCode::Comma,
            0x34 => KeyCode::Dot,
            0x35 => KeyCode::Slash,
            0x36 => KeyCode::RightShift,
            0x37 => KeyCode::NumpadStar,
            0x38 => KeyCode::LeftAlt,
            0x39 => KeyCode::Space,
            0x3A => KeyCode::CapsLock,
            0x3B => KeyCode::F1,
            0x3C => KeyCode::F2,
            0x3D => KeyCode::F3,
            0x3E => KeyCode::F4,
            0x3F => KeyCode::F5,
            0x40 => KeyCode::F6,
            0x41 => KeyCode::F7,
            0x42 => KeyCode::F8,
            0x43 => KeyCode::F9,
            0x44 => KeyCode::F10,
            0x45 => KeyCode::NumLock,
            0x46 => KeyCode::ScrollLock,
            0x47 => KeyCode::Numpad7,
            0x48 => KeyCode::Numpad8,
            0x49 => KeyCode::Numpad9,
            0x4A => KeyCode::NumpadMinus,
            0x4B => KeyCode::Numpad4,
            0x4C => KeyCode::Numpad5,
            0x4D => KeyCode::Numpad6,
            0x4E => KeyCode::NumpadPlus,
            0x4F => KeyCode::Numpad1,
            0x50 => KeyCode::Numpad2,
            0x51 => KeyCode::Numpad3,
            0x52 => KeyCode::Numpad0,
            0x53 => KeyCode::NumpadDot,
            0x57 => KeyCode::F11,
            0x58 => KeyCode::F12,
            _ => return None,
        }
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

const fn shifted(shift: bool, normal: u8, shifted: u8) -> u8 {
    if shift {
        shifted
    } else {
        normal
    }
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
    use super::*;

    #[test]
    fn emits_text_for_basic_typing() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.on_scancode(0x1E);
        keyboard.on_scancode(0x9E);
        keyboard.on_scancode(0x30);

        let mut out = [0_u8; 4];
        assert_eq!(keyboard.read_text(&mut out), 2);
        assert_eq!(&out[..2], b"ab");
    }

    #[test]
    fn tracks_press_repeat_and_release() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.on_scancode(0x1E);
        keyboard.on_scancode(0x1E);
        keyboard.on_scancode(0x9E);

        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::A,
                action: KeyAction::Pressed,
                modifiers: Modifiers::empty(),
                text: Some(b'a'),
            })
        );
        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::A,
                action: KeyAction::Repeated,
                modifiers: Modifiers::empty(),
                text: Some(b'a'),
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
    }

    #[test]
    fn updates_modifiers_and_shifted_text() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.on_scancode(0x2A);
        keyboard.on_scancode(0x02);
        keyboard.on_scancode(0x82);
        keyboard.on_scancode(0xAA);
        keyboard.on_scancode(0x3A);
        keyboard.on_scancode(0xBA);
        keyboard.on_scancode(0x1E);

        let mut out = [0_u8; 4];
        assert_eq!(keyboard.read_text(&mut out), 2);
        assert_eq!(&out[..2], b"!A");
        assert!(keyboard.modifiers().contains(Modifiers::CAPS_LOCK));
    }

    #[test]
    fn decodes_extended_keys_without_text() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.on_scancode(0xE0);
        keyboard.on_scancode(0x48);

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
}
