use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use bitflags::bitflags;
use spin::Mutex;
use x86_64::instructions::{interrupts, port::Port};

use crate::ring::RingBuffer;

const KEYBOARD_IRQ: u8 = 1;
const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const STATUS_OUTPUT_READY: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;
const STATUS_AUX_OUTPUT: u8 = 1 << 5;
const I8042_IO_TIMEOUT_SPINS: usize = 100_000;
const I8042_READ_CONFIG: u8 = 0x20;
const I8042_WRITE_CONFIG: u8 = 0x60;
const I8042_SELF_TEST: u8 = 0xAA;
const I8042_DISABLE_SECOND_PORT: u8 = 0xA7;
const I8042_TEST_FIRST_PORT: u8 = 0xAB;
const I8042_DISABLE_FIRST_PORT: u8 = 0xAD;
const I8042_ENABLE_FIRST_PORT: u8 = 0xAE;
const I8042_CONFIG_IRQ1_ENABLE: u8 = 1 << 0;
const I8042_CONFIG_FIRST_PORT_CLOCK_DISABLE: u8 = 1 << 4;
const I8042_CONFIG_TRANSLATION: u8 = 1 << 6;
const I8042_SELF_TEST_PASSED: u8 = 0x55;
const I8042_FIRST_PORT_TEST_PASSED: u8 = 0x00;
const KEYBOARD_CMD_SET_SCANCODE_SET: u8 = 0xF0;
const KEYBOARD_CMD_ENABLE_SCANNING: u8 = 0xF4;
const KEYBOARD_CMD_RESET: u8 = 0xFF;
const KEYBOARD_CMD_RESET_DEFAULTS: u8 = 0xF6;
const KEYBOARD_SCANCODE_SET_2: u8 = 0x02;
const KEYBOARD_RESPONSE_SELF_TEST_PASSED: u8 = 0xAA;
const KEYBOARD_RESPONSE_ECHO: u8 = 0xEE;
const KEYBOARD_RESPONSE_ACK: u8 = 0xFA;
const KEYBOARD_RESPONSE_RESEND: u8 = 0xFE;
const KEYBOARD_RESPONSE_OVERRUN_0: u8 = 0x00;
const KEYBOARD_RESPONSE_OVERRUN_FF: u8 = 0xFF;
const KEYBOARD_SEND_RETRIES: usize = 3;
const KEYBOARD_RESPONSE_READ_RETRIES: usize = 8;
const FALLBACK_POLL_INTERVAL_TICKS: u8 = 1;
const MAX_SCANCODES_PER_INTERRUPT: usize = 32;
const EVENT_QUEUE_CAPACITY: usize = 128;
const TEXT_QUEUE_CAPACITY: usize = 512;

static KEYBOARD: Mutex<KeyboardDriver> = Mutex::new(KeyboardDriver::new());
static LEGACY_KEYBOARD_ACTIVE: AtomicBool = AtomicBool::new(false);
static FALLBACK_POLL_TICKS: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LegacyKeyboardInfo {
    pub scan_set: ScanCodeSet,
    pub translated: bool,
    pub controller_configured: bool,
    pub controller_self_test_passed: bool,
    pub first_port_test_passed: bool,
    pub keyboard_reset_acknowledged: bool,
    pub keyboard_bat_passed: bool,
    pub defaults_command_acknowledged: bool,
    pub scan_set_command_acknowledged: bool,
    pub scanning_command_acknowledged: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LegacyKeyboardInitResult {
    Ready(LegacyKeyboardInfo),
    Unavailable(&'static str),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardEvent {
    pub code: KeyCode,
    pub action: KeyAction,
    pub modifiers: Modifiers,
    pub text: Option<u8>,
}

pub fn init() -> LegacyKeyboardInitResult {
    let result = interrupts::without_interrupts(|| {
        drain_output_buffer();
        let init_info = init_controller();
        if let Ok(info) = init_info {
            KEYBOARD.lock().set_scan_code_set(info.scan_set);
            drain_output_buffer();
        }
        init_info
    });

    match result {
        Ok(info) => {
            LEGACY_KEYBOARD_ACTIVE.store(true, Ordering::Release);
            crate::pic::enable_irq(KEYBOARD_IRQ);
            LegacyKeyboardInitResult::Ready(info)
        }
        Err(reason) => {
            LEGACY_KEYBOARD_ACTIVE.store(false, Ordering::Release);
            LegacyKeyboardInitResult::Unavailable(reason)
        }
    }
}

pub fn on_interrupt() {
    if !LEGACY_KEYBOARD_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    interrupts::without_interrupts(|| {
        poll_controller();
    });
}

pub fn inject_key_transition(code: KeyCode, released: bool) {
    interrupts::without_interrupts(|| {
        KEYBOARD.lock().emit_key_transition(code, released);
    });
}

pub fn poll() {
    if !LEGACY_KEYBOARD_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    interrupts::without_interrupts(|| {
        poll_controller();
    });
}

pub fn poll_fallback() {
    let ticks = FALLBACK_POLL_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if ticks < FALLBACK_POLL_INTERVAL_TICKS {
        return;
    }

    FALLBACK_POLL_TICKS.store(0, Ordering::Relaxed);
    poll();
}

#[allow(dead_code)]
pub fn pending_text_len() -> usize {
    interrupts::without_interrupts(|| KEYBOARD.lock().pending_text_len())
}

#[allow(dead_code)]
pub fn pop_event() -> Option<KeyboardEvent> {
    interrupts::without_interrupts(|| KEYBOARD.lock().pop_event())
}

pub fn drain_events_to_tty() -> usize {
    let mut flushed = 0;
    while let Some(event) = pop_event() {
        crate::tty::on_key_event(event);
        flushed += 1;
    }
    flushed
}

#[allow(dead_code)]
pub fn read_text(dest: &mut [u8]) -> usize {
    interrupts::without_interrupts(|| KEYBOARD.lock().read_text(dest))
}

#[allow(dead_code)]
pub fn modifiers() -> Modifiers {
    interrupts::without_interrupts(|| KEYBOARD.lock().modifiers())
}

fn drain_output_buffer() {
    while read_controller_byte_nowait().is_some() {}
}

fn poll_controller() {
    let mut keyboard = KEYBOARD.lock();
    for _ in 0..MAX_SCANCODES_PER_INTERRUPT {
        let Some(scancode) = read_scancode() else {
            break;
        };
        keyboard.on_scancode(scancode);
    }
}

fn init_controller() -> Result<LegacyKeyboardInfo, &'static str> {
    let _ = write_command(I8042_DISABLE_SECOND_PORT);
    let _ = write_command(I8042_DISABLE_FIRST_PORT);
    drain_output_buffer();

    let controller_self_test_passed = controller_self_test();
    let config = read_controller_config().ok_or("i8042 config read timed out")?;
    let translated = config & I8042_CONFIG_TRANSLATION != 0;
    let next_config = (config | I8042_CONFIG_IRQ1_ENABLE) & !I8042_CONFIG_FIRST_PORT_CLOCK_DISABLE;
    let controller_configured = write_controller_config(next_config);
    let first_port_test_passed = first_port_test();
    if !write_command(I8042_ENABLE_FIRST_PORT) {
        return Err("i8042 enable-first-port command timed out");
    }
    drain_output_buffer();

    let (keyboard_reset_acknowledged, keyboard_bat_passed) = send_keyboard_reset();
    drain_output_buffer();
    let defaults_command_acknowledged = send_keyboard_command(KEYBOARD_CMD_RESET_DEFAULTS);
    drain_output_buffer();
    let scan_set_command_acknowledged =
        send_keyboard_command_with_data(KEYBOARD_CMD_SET_SCANCODE_SET, KEYBOARD_SCANCODE_SET_2);
    drain_output_buffer();
    let scan_set = if translated {
        ScanCodeSet::Set1
    } else {
        ScanCodeSet::Set2
    };
    drain_output_buffer();
    let scanning_command_acknowledged = send_keyboard_command(KEYBOARD_CMD_ENABLE_SCANNING);

    Ok(LegacyKeyboardInfo {
        scan_set,
        translated,
        controller_configured,
        controller_self_test_passed,
        first_port_test_passed,
        keyboard_reset_acknowledged,
        keyboard_bat_passed,
        defaults_command_acknowledged,
        scan_set_command_acknowledged,
        scanning_command_acknowledged,
    })
}

fn read_controller_config() -> Option<u8> {
    if !write_command(I8042_READ_CONFIG) {
        return None;
    }
    read_keyboard_data_byte_blocking()
}

fn write_controller_config(value: u8) -> bool {
    write_command(I8042_WRITE_CONFIG) && write_data_byte(value)
}

fn send_keyboard_command(command: u8) -> bool {
    send_keyboard_byte_and_expect_ack(command)
}

fn send_keyboard_command_with_data(command: u8, data: u8) -> bool {
    send_keyboard_byte_and_expect_ack(command) && send_keyboard_byte_and_expect_ack(data)
}

fn send_keyboard_reset() -> (bool, bool) {
    for _ in 0..KEYBOARD_SEND_RETRIES {
        if !write_data_byte(KEYBOARD_CMD_RESET) {
            return (false, false);
        }

        let mut acknowledged = false;
        for _ in 0..KEYBOARD_RESPONSE_READ_RETRIES {
            match read_keyboard_data_byte_blocking() {
                Some(KEYBOARD_RESPONSE_ACK) => {
                    acknowledged = true;
                    break;
                }
                Some(KEYBOARD_RESPONSE_RESEND) => continue,
                Some(byte) if is_ignorable_command_response(byte) => continue,
                _ => return (false, false),
            }
        }

        if !acknowledged {
            continue;
        }

        for _ in 0..I8042_IO_TIMEOUT_SPINS {
            match read_keyboard_data_byte_blocking() {
                Some(KEYBOARD_RESPONSE_SELF_TEST_PASSED) => return (true, true),
                Some(byte) if is_ignorable_command_response(byte) => continue,
                Some(_) => return (true, false),
                None => return (true, false),
            }
        }
    }

    (false, false)
}

fn controller_self_test() -> bool {
    if !write_command(I8042_SELF_TEST) {
        return false;
    }

    matches!(
        read_keyboard_data_byte_blocking(),
        Some(I8042_SELF_TEST_PASSED)
    )
}

fn first_port_test() -> bool {
    if !write_command(I8042_TEST_FIRST_PORT) {
        return false;
    }

    matches!(
        read_keyboard_data_byte_blocking(),
        Some(I8042_FIRST_PORT_TEST_PASSED)
    )
}

fn send_keyboard_byte_and_expect_ack(byte: u8) -> bool {
    for _ in 0..KEYBOARD_SEND_RETRIES {
        if !write_data_byte(byte) {
            return false;
        }

        for _ in 0..KEYBOARD_RESPONSE_READ_RETRIES {
            match read_keyboard_data_byte_blocking() {
                Some(KEYBOARD_RESPONSE_ACK) => return true,
                Some(KEYBOARD_RESPONSE_RESEND) => break,
                Some(byte) if is_ignorable_command_response(byte) => continue,
                _ => return false,
            }
        }
    }

    false
}

fn write_command(command: u8) -> bool {
    if !wait_for_input_empty() {
        return false;
    }

    unsafe {
        let mut status_port = Port::<u8>::new(STATUS_PORT);
        status_port.write(command);
    }
    true
}

fn write_data_byte(data: u8) -> bool {
    if !wait_for_input_empty() {
        return false;
    }

    unsafe {
        let mut data_port = Port::<u8>::new(DATA_PORT);
        data_port.write(data);
    }
    true
}

#[derive(Clone, Copy)]
struct ControllerByte {
    byte: u8,
    aux: bool,
}

fn read_keyboard_data_byte_blocking() -> Option<u8> {
    for _ in 0..I8042_IO_TIMEOUT_SPINS {
        let Some(data) = read_controller_byte_nowait() else {
            spin_loop();
            continue;
        };
        if !data.aux {
            return Some(data.byte);
        }
    }

    None
}

fn read_controller_byte_nowait() -> Option<ControllerByte> {
    let status = read_status();
    if status & STATUS_OUTPUT_READY == 0 {
        return None;
    }

    let byte = unsafe {
        let mut data_port = Port::<u8>::new(DATA_PORT);
        data_port.read()
    };
    Some(ControllerByte {
        byte,
        aux: status & STATUS_AUX_OUTPUT != 0,
    })
}

fn wait_for_input_empty() -> bool {
    wait_for_status_clear(STATUS_INPUT_FULL)
}

fn wait_for_status_clear(mask: u8) -> bool {
    for _ in 0..I8042_IO_TIMEOUT_SPINS {
        if read_status() & mask == 0 {
            return true;
        }
        spin_loop();
    }
    false
}

fn read_status() -> u8 {
    unsafe {
        let mut status_port = Port::<u8>::new(STATUS_PORT);
        status_port.read()
    }
}

fn read_scancode() -> Option<u8> {
    for _ in 0..MAX_SCANCODES_PER_INTERRUPT {
        let Some(data) = read_controller_byte_nowait() else {
            return None;
        };

        if !data.aux {
            return Some(data.byte);
        }
    }

    None
}

const fn is_non_scancode_response(byte: u8) -> bool {
    matches!(
        byte,
        KEYBOARD_RESPONSE_ECHO | KEYBOARD_RESPONSE_OVERRUN_0 | KEYBOARD_RESPONSE_OVERRUN_FF
    )
}

const fn is_ignorable_command_response(byte: u8) -> bool {
    is_non_scancode_response(byte) || byte == KEYBOARD_RESPONSE_SELF_TEST_PASSED
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

    fn set_scan_code_set(&mut self, scan_set: ScanCodeSet) {
        self.parser.set_scan_set(scan_set);
    }

    fn on_scancode(&mut self, scancode: u8) {
        if matches!(scancode, KEYBOARD_RESPONSE_ACK | KEYBOARD_RESPONSE_RESEND)
            || is_non_scancode_response(scancode)
        {
            return;
        }

        let Some(parsed) = self.parser.push(scancode) else {
            return;
        };
        let (code, released) = match parsed {
            ParsedKey::Scancode(decoded) => {
                let Some(code) =
                    key_code_from_scancode(decoded.code, decoded.extended, decoded.scan_set)
                else {
                    return;
                };
                (code, decoded.released)
            }
            ParsedKey::Direct { code, released } => (code, released),
        };

        self.emit_key_transition(code, released);
    }

    fn emit_key_transition(&mut self, code: KeyCode, released: bool) {
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

#[derive(Clone, Copy)]
struct DecodedScanCode {
    code: u8,
    released: bool,
    extended: bool,
    scan_set: ScanCodeSet,
}

#[derive(Clone, Copy)]
enum ParsedKey {
    Scancode(DecodedScanCode),
    Direct { code: KeyCode, released: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScanCodeSet {
    Set1,
    Set2,
}

impl ScanCodeSet {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Set1 => "set1",
            Self::Set2 => "set2",
        }
    }
}

struct ScanCodeParser {
    scan_set: ScanCodeSet,
    state: ScanCodeParserState,
}

impl ScanCodeParser {
    const fn new() -> Self {
        Self {
            scan_set: ScanCodeSet::Set1,
            state: ScanCodeParserState::Idle,
        }
    }

    fn set_scan_set(&mut self, scan_set: ScanCodeSet) {
        self.scan_set = scan_set;
        self.state = ScanCodeParserState::Idle;
    }

    fn push(&mut self, byte: u8) -> Option<ParsedKey> {
        match self.scan_set {
            ScanCodeSet::Set1 => self.push_set1(byte),
            ScanCodeSet::Set2 => self.push_set2(byte),
        }
    }

    fn push_set1(&mut self, byte: u8) -> Option<ParsedKey> {
        match self.state {
            ScanCodeParserState::Idle => match byte {
                0xE0 => {
                    self.state = ScanCodeParserState::Set1Extended;
                    None
                }
                0xE1 => {
                    self.state = ScanCodeParserState::Set1Pause(0);
                    None
                }
                _ => Some(ParsedKey::Scancode(DecodedScanCode {
                    code: byte & 0x7F,
                    released: (byte & 0x80) != 0,
                    extended: false,
                    scan_set: ScanCodeSet::Set1,
                })),
            },
            ScanCodeParserState::Set1Extended => match byte {
                0x2A => {
                    self.state = ScanCodeParserState::Set1PrintScreenPressE0_2A;
                    None
                }
                0xB7 => {
                    self.state = ScanCodeParserState::Set1PrintScreenReleaseE0B7;
                    None
                }
                _ => {
                    self.state = ScanCodeParserState::Idle;
                    Some(ParsedKey::Scancode(DecodedScanCode {
                        code: byte & 0x7F,
                        released: (byte & 0x80) != 0,
                        extended: true,
                        scan_set: ScanCodeSet::Set1,
                    }))
                }
            },
            ScanCodeParserState::Set1PrintScreenPressE0_2A => {
                self.state = if byte == 0xE0 {
                    ScanCodeParserState::Set1PrintScreenPressE0_2aE0
                } else {
                    ScanCodeParserState::Idle
                };
                None
            }
            ScanCodeParserState::Set1PrintScreenPressE0_2aE0 => {
                self.state = ScanCodeParserState::Idle;
                (byte == 0x37).then_some(ParsedKey::Direct {
                    code: KeyCode::PrintScreen,
                    released: false,
                })
            }
            ScanCodeParserState::Set1PrintScreenReleaseE0B7 => {
                self.state = if byte == 0xE0 {
                    ScanCodeParserState::Set1PrintScreenReleaseE0B7E0
                } else {
                    ScanCodeParserState::Idle
                };
                None
            }
            ScanCodeParserState::Set1PrintScreenReleaseE0B7E0 => {
                self.state = ScanCodeParserState::Idle;
                (byte == 0xAA).then_some(ParsedKey::Direct {
                    code: KeyCode::PrintScreen,
                    released: true,
                })
            }
            ScanCodeParserState::Set1Pause(index) => self.advance_sequence(
                byte,
                index,
                &SET1_PAUSE_SEQUENCE,
                ScanCodeParserState::Set1Pause,
                ParsedKey::Direct {
                    code: KeyCode::Pause,
                    released: false,
                },
            ),
            _ => {
                self.state = ScanCodeParserState::Idle;
                None
            }
        }
    }

    fn push_set2(&mut self, byte: u8) -> Option<ParsedKey> {
        match self.state {
            ScanCodeParserState::Idle => match byte {
                0xE0 => {
                    self.state = ScanCodeParserState::Set2Extended;
                    None
                }
                0xE1 => {
                    self.state = ScanCodeParserState::Set2Pause(0);
                    None
                }
                0xF0 => {
                    self.state = ScanCodeParserState::Set2Release;
                    None
                }
                _ => Some(ParsedKey::Scancode(DecodedScanCode {
                    code: byte,
                    released: false,
                    extended: false,
                    scan_set: ScanCodeSet::Set2,
                })),
            },
            ScanCodeParserState::Set2Release => {
                self.state = ScanCodeParserState::Idle;
                Some(ParsedKey::Scancode(DecodedScanCode {
                    code: byte,
                    released: true,
                    extended: false,
                    scan_set: ScanCodeSet::Set2,
                }))
            }
            ScanCodeParserState::Set2Extended => match byte {
                0x12 => {
                    self.state = ScanCodeParserState::Set2PrintScreenPressE0_12;
                    None
                }
                0xF0 => {
                    self.state = ScanCodeParserState::Set2ExtendedRelease;
                    None
                }
                _ => {
                    self.state = ScanCodeParserState::Idle;
                    Some(ParsedKey::Scancode(DecodedScanCode {
                        code: byte,
                        released: false,
                        extended: true,
                        scan_set: ScanCodeSet::Set2,
                    }))
                }
            },
            ScanCodeParserState::Set2ExtendedRelease => match byte {
                0x7C => {
                    self.state = ScanCodeParserState::Set2PrintScreenReleaseE0F07C;
                    None
                }
                _ => {
                    self.state = ScanCodeParserState::Idle;
                    Some(ParsedKey::Scancode(DecodedScanCode {
                        code: byte,
                        released: true,
                        extended: true,
                        scan_set: ScanCodeSet::Set2,
                    }))
                }
            },
            ScanCodeParserState::Set2PrintScreenPressE0_12 => {
                self.state = if byte == 0xE0 {
                    ScanCodeParserState::Set2PrintScreenPressE0_12E0
                } else {
                    ScanCodeParserState::Idle
                };
                None
            }
            ScanCodeParserState::Set2PrintScreenPressE0_12E0 => {
                self.state = ScanCodeParserState::Idle;
                (byte == 0x7C).then_some(ParsedKey::Direct {
                    code: KeyCode::PrintScreen,
                    released: false,
                })
            }
            ScanCodeParserState::Set2PrintScreenReleaseE0F07C => {
                self.state = if byte == 0xE0 {
                    ScanCodeParserState::Set2PrintScreenReleaseE0F0_7cE0
                } else {
                    ScanCodeParserState::Idle
                };
                None
            }
            ScanCodeParserState::Set2PrintScreenReleaseE0F0_7cE0 => {
                self.state = if byte == 0xF0 {
                    ScanCodeParserState::Set2PrintScreenReleaseE0F0_7cE0F0
                } else {
                    ScanCodeParserState::Idle
                };
                None
            }
            ScanCodeParserState::Set2PrintScreenReleaseE0F0_7cE0F0 => {
                self.state = ScanCodeParserState::Idle;
                (byte == 0x12).then_some(ParsedKey::Direct {
                    code: KeyCode::PrintScreen,
                    released: true,
                })
            }
            ScanCodeParserState::Set2Pause(index) => self.advance_sequence(
                byte,
                index,
                &SET2_PAUSE_SEQUENCE,
                ScanCodeParserState::Set2Pause,
                ParsedKey::Direct {
                    code: KeyCode::Pause,
                    released: false,
                },
            ),
            _ => {
                self.state = ScanCodeParserState::Idle;
                None
            }
        }
    }

    fn advance_sequence(
        &mut self,
        byte: u8,
        index: usize,
        expected: &[u8],
        next: fn(usize) -> ScanCodeParserState,
        event: ParsedKey,
    ) -> Option<ParsedKey> {
        if expected.get(index).copied() != Some(byte) {
            self.state = ScanCodeParserState::Idle;
            return None;
        }

        if index + 1 == expected.len() {
            self.state = ScanCodeParserState::Idle;
            Some(event)
        } else {
            self.state = next(index + 1);
            None
        }
    }
}

const SET1_PAUSE_SEQUENCE: [u8; 5] = [0x1D, 0x45, 0xE1, 0x9D, 0xC5];
const SET2_PAUSE_SEQUENCE: [u8; 7] = [0x14, 0x77, 0xE1, 0xF0, 0x14, 0xF0, 0x77];

#[derive(Clone, Copy)]
enum ScanCodeParserState {
    Idle,
    Set1Extended,
    Set1PrintScreenPressE0_2A,
    Set1PrintScreenPressE0_2aE0,
    Set1PrintScreenReleaseE0B7,
    Set1PrintScreenReleaseE0B7E0,
    Set1Pause(usize),
    Set2Release,
    Set2Extended,
    Set2ExtendedRelease,
    Set2PrintScreenPressE0_12,
    Set2PrintScreenPressE0_12E0,
    Set2PrintScreenReleaseE0F07C,
    Set2PrintScreenReleaseE0F0_7cE0,
    Set2PrintScreenReleaseE0F0_7cE0F0,
    Set2Pause(usize),
}

fn key_code_from_scancode(code: u8, extended: bool, scan_set: ScanCodeSet) -> Option<KeyCode> {
    match scan_set {
        ScanCodeSet::Set1 => key_code_from_set1_scancode(code, extended),
        ScanCodeSet::Set2 => key_code_from_set2_scancode(code, extended),
    }
}

fn key_code_from_set1_scancode(code: u8, extended: bool) -> Option<KeyCode> {
    Some(if extended {
        match code {
            0x1C => KeyCode::NumpadEnter,
            0x1D => KeyCode::RightCtrl,
            0x35 => KeyCode::NumpadSlash,
            0x38 => KeyCode::RightAlt,
            0x37 => KeyCode::PrintScreen,
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
            0x5B => KeyCode::LeftMeta,
            0x5C => KeyCode::RightMeta,
            0x5D => KeyCode::Menu,
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

fn key_code_from_set2_scancode(code: u8, extended: bool) -> Option<KeyCode> {
    Some(if extended {
        match code {
            0x11 => KeyCode::RightAlt,
            0x14 => KeyCode::RightCtrl,
            0x1F => KeyCode::LeftMeta,
            0x27 => KeyCode::RightMeta,
            0x2F => KeyCode::Menu,
            0x4A => KeyCode::NumpadSlash,
            0x5A => KeyCode::NumpadEnter,
            0x69 => KeyCode::End,
            0x6B => KeyCode::ArrowLeft,
            0x6C => KeyCode::Home,
            0x70 => KeyCode::Insert,
            0x71 => KeyCode::Delete,
            0x72 => KeyCode::ArrowDown,
            0x74 => KeyCode::ArrowRight,
            0x75 => KeyCode::ArrowUp,
            0x7C => KeyCode::PrintScreen,
            0x7A => KeyCode::PageDown,
            0x7D => KeyCode::PageUp,
            _ => return None,
        }
    } else {
        match code {
            0x01 => KeyCode::F9,
            0x03 => KeyCode::F5,
            0x04 => KeyCode::F3,
            0x05 => KeyCode::F1,
            0x06 => KeyCode::F2,
            0x07 => KeyCode::F12,
            0x09 => KeyCode::F10,
            0x0A => KeyCode::F8,
            0x0B => KeyCode::F6,
            0x0C => KeyCode::F4,
            0x0D => KeyCode::Tab,
            0x0E => KeyCode::Grave,
            0x11 => KeyCode::LeftAlt,
            0x12 => KeyCode::LeftShift,
            0x14 => KeyCode::LeftCtrl,
            0x15 => KeyCode::Q,
            0x16 => KeyCode::Digit1,
            0x1A => KeyCode::Z,
            0x1B => KeyCode::S,
            0x1C => KeyCode::A,
            0x1D => KeyCode::W,
            0x1E => KeyCode::Digit2,
            0x21 => KeyCode::C,
            0x22 => KeyCode::X,
            0x23 => KeyCode::D,
            0x24 => KeyCode::E,
            0x25 => KeyCode::Digit4,
            0x26 => KeyCode::Digit3,
            0x29 => KeyCode::Space,
            0x2A => KeyCode::V,
            0x2B => KeyCode::F,
            0x2C => KeyCode::T,
            0x2D => KeyCode::R,
            0x2E => KeyCode::Digit5,
            0x31 => KeyCode::N,
            0x32 => KeyCode::B,
            0x33 => KeyCode::H,
            0x34 => KeyCode::G,
            0x35 => KeyCode::Y,
            0x36 => KeyCode::Digit6,
            0x3A => KeyCode::M,
            0x3B => KeyCode::J,
            0x3C => KeyCode::U,
            0x3D => KeyCode::Digit7,
            0x3E => KeyCode::Digit8,
            0x41 => KeyCode::Comma,
            0x42 => KeyCode::K,
            0x43 => KeyCode::I,
            0x44 => KeyCode::O,
            0x45 => KeyCode::Digit0,
            0x46 => KeyCode::Digit9,
            0x49 => KeyCode::Dot,
            0x4A => KeyCode::Slash,
            0x4B => KeyCode::L,
            0x4C => KeyCode::Semicolon,
            0x4D => KeyCode::P,
            0x4E => KeyCode::Minus,
            0x52 => KeyCode::Apostrophe,
            0x54 => KeyCode::LeftBracket,
            0x55 => KeyCode::Equal,
            0x58 => KeyCode::CapsLock,
            0x59 => KeyCode::RightShift,
            0x5A => KeyCode::Enter,
            0x5B => KeyCode::RightBracket,
            0x5D => KeyCode::Backslash,
            0x66 => KeyCode::Backspace,
            0x69 => KeyCode::Numpad1,
            0x6B => KeyCode::Numpad4,
            0x6C => KeyCode::Numpad7,
            0x70 => KeyCode::Numpad0,
            0x71 => KeyCode::NumpadDot,
            0x72 => KeyCode::Numpad2,
            0x73 => KeyCode::Numpad5,
            0x74 => KeyCode::Numpad6,
            0x75 => KeyCode::Numpad8,
            0x76 => KeyCode::Escape,
            0x77 => KeyCode::NumLock,
            0x78 => KeyCode::F11,
            0x79 => KeyCode::NumpadPlus,
            0x7A => KeyCode::Numpad3,
            0x7B => KeyCode::NumpadMinus,
            0x7C => KeyCode::NumpadStar,
            0x7D => KeyCode::Numpad9,
            0x7E => KeyCode::ScrollLock,
            0x83 => KeyCode::F7,
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

    #[test]
    fn emits_text_for_set2_basic_typing() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.set_scan_code_set(ScanCodeSet::Set2);
        keyboard.on_scancode(0x1C);
        keyboard.on_scancode(0xF0);
        keyboard.on_scancode(0x1C);
        keyboard.on_scancode(0x32);

        let mut out = [0_u8; 4];
        assert_eq!(keyboard.read_text(&mut out), 2);
        assert_eq!(&out[..2], b"ab");
    }

    #[test]
    fn decodes_set2_extended_keys_without_text() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.set_scan_code_set(ScanCodeSet::Set2);
        keyboard.on_scancode(0xE0);
        keyboard.on_scancode(0x75);

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
    fn decodes_set1_print_screen_sequence() {
        let mut keyboard = KeyboardDriver::new();
        for byte in [0xE0, 0x2A, 0xE0, 0x37, 0xE0, 0xB7, 0xE0, 0xAA] {
            keyboard.on_scancode(byte);
        }

        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::PrintScreen,
                action: KeyAction::Pressed,
                modifiers: Modifiers::empty(),
                text: None,
            })
        );
        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::PrintScreen,
                action: KeyAction::Released,
                modifiers: Modifiers::empty(),
                text: None,
            })
        );
    }

    #[test]
    fn decodes_set2_print_screen_sequence() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.set_scan_code_set(ScanCodeSet::Set2);
        for byte in [0xE0, 0x12, 0xE0, 0x7C, 0xE0, 0xF0, 0x7C, 0xE0, 0xF0, 0x12] {
            keyboard.on_scancode(byte);
        }

        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::PrintScreen,
                action: KeyAction::Pressed,
                modifiers: Modifiers::empty(),
                text: None,
            })
        );
        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::PrintScreen,
                action: KeyAction::Released,
                modifiers: Modifiers::empty(),
                text: None,
            })
        );
    }

    #[test]
    fn decodes_pause_without_latching_pressed_state() {
        let mut keyboard = KeyboardDriver::new();
        for byte in [0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5] {
            keyboard.on_scancode(byte);
        }
        for byte in [0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5] {
            keyboard.on_scancode(byte);
        }

        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::Pause,
                action: KeyAction::Pressed,
                modifiers: Modifiers::empty(),
                text: None,
            })
        );
        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::Pause,
                action: KeyAction::Pressed,
                modifiers: Modifiers::empty(),
                text: None,
            })
        );
    }

    #[test]
    fn decodes_meta_keys() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.on_scancode(0xE0);
        keyboard.on_scancode(0x5B);

        assert_eq!(
            keyboard.pop_event(),
            Some(KeyboardEvent {
                code: KeyCode::LeftMeta,
                action: KeyAction::Pressed,
                modifiers: Modifiers::META,
                text: None,
            })
        );
    }
}
