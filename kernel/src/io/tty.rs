use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::console;
use crate::keyboard::{KeyAction, KeyCode, KeyboardEvent};
use crate::ring::RingBuffer;
use crate::session::{CONSOLE_SESSION_COUNT, ConsoleSessionId};

const INPUT_BUFFER_CAPACITY: usize = 1024;
const EDIT_BUFFER_CAPACITY: usize = 256;
const CURSOR_MOVE_SEQUENCE_MAX_LEN: usize = 16;

static TTY: Mutex<TtyCollection> = Mutex::new(TtyCollection::new());

pub fn init() {}

pub fn on_key_event(event: KeyboardEvent) {
    on_key_event_for_session(ConsoleSessionId::PRIMARY, event);
}

pub fn on_key_event_for_session(session: ConsoleSessionId, event: KeyboardEvent) {
    interrupts::without_interrupts(|| TTY.lock().session_mut(session).on_key_event(session, event))
}

#[allow(dead_code)]
pub fn read_input(dest: &mut [u8]) -> usize {
    read_input_for_session(ConsoleSessionId::PRIMARY, dest)
}

pub fn read_input_for_session(session: ConsoleSessionId, dest: &mut [u8]) -> usize {
    interrupts::without_interrupts(|| TTY.lock().session_mut(session).read_input(dest))
}

pub fn read_input_blocking(dest: &mut [u8]) -> usize {
    read_input_blocking_for_session(ConsoleSessionId::PRIMARY, dest)
}

pub fn read_input_blocking_for_session(session: ConsoleSessionId, dest: &mut [u8]) -> usize {
    if dest.is_empty() {
        return 0;
    }

    loop {
        let read =
            interrupts::without_interrupts(|| TTY.lock().session_mut(session).read_input(dest));
        if read != 0 {
            return read;
        }

        interrupts::enable_and_hlt();
    }
}

pub fn write(bytes: &[u8]) -> usize {
    write_to_session(ConsoleSessionId::PRIMARY, bytes)
}

pub fn write_to_session(session: ConsoleSessionId, bytes: &[u8]) -> usize {
    interrupts::without_interrupts(|| TTY.lock().session_mut(session).write(session, bytes))
}

struct TtyCollection {
    sessions: [TtySessionState; CONSOLE_SESSION_COUNT],
}

impl TtyCollection {
    const fn new() -> Self {
        Self {
            sessions: [TtySessionState::new(), TtySessionState::new()],
        }
    }

    fn session_mut(&mut self, session: ConsoleSessionId) -> &mut TtySessionState {
        &mut self.sessions[session.index()]
    }
}

struct TtySessionState {
    input: RingBuffer<u8, INPUT_BUFFER_CAPACITY>,
    edit_buffer: [u8; EDIT_BUFFER_CAPACITY],
    edit_len: usize,
    edit_cursor: usize,
}

impl TtySessionState {
    const fn new() -> Self {
        Self {
            input: RingBuffer::new(),
            edit_buffer: [0; EDIT_BUFFER_CAPACITY],
            edit_len: 0,
            edit_cursor: 0,
        }
    }

    fn on_key_event(&mut self, session: ConsoleSessionId, event: KeyboardEvent) {
        if matches!(event.action, KeyAction::Released) {
            return;
        }

        match event.code {
            KeyCode::ArrowLeft => self.move_cursor_left(session),
            KeyCode::ArrowRight => self.move_cursor_right(session),
            KeyCode::Backspace => self.handle_backspace(session),
            KeyCode::Enter => self.commit_line(session),
            _ => {
                let Some(byte) = event.text else {
                    return;
                };

                match byte {
                    0x08 => self.handle_backspace(session),
                    b'\n' => self.commit_line(session),
                    b'\t' | 0x20..=0x7E => self.insert_edit_byte(session, byte),
                    _ => {}
                }
            }
        }
    }

    fn read_input(&mut self, dest: &mut [u8]) -> usize {
        self.input.pop_into(dest)
    }

    fn write(&mut self, session: ConsoleSessionId, bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        let redraw_edit_buffer = self.edit_len != 0;
        if redraw_edit_buffer {
            console::write_from_tty(session, b"\r\n");
        }

        let mut chunk_start = 0;
        let mut previous = None;

        for (index, &byte) in bytes.iter().enumerate() {
            if byte != b'\n' || previous == Some(b'\r') {
                previous = Some(byte);
                continue;
            }

            if chunk_start < index {
                console::write_from_tty(session, &bytes[chunk_start..index]);
            }
            console::write_from_tty(session, b"\r\n");
            chunk_start = index + 1;
            previous = Some(byte);
        }

        if chunk_start < bytes.len() {
            console::write_from_tty(session, &bytes[chunk_start..]);
        }

        if redraw_edit_buffer {
            if !ends_at_fresh_line(bytes) {
                console::write_from_tty(session, b"\r\n");
            }
            self.redraw_edit_buffer_on_fresh_line(session);
        }

        bytes.len()
    }

    fn insert_edit_byte(&mut self, session: ConsoleSessionId, byte: u8) {
        if self.edit_len == EDIT_BUFFER_CAPACITY {
            return;
        }

        if self.edit_cursor == self.edit_len {
            self.edit_buffer[self.edit_len] = byte;
            self.edit_len += 1;
            self.edit_cursor += 1;
            console::write_from_tty(session, &[byte]);
            return;
        }

        self.edit_buffer
            .copy_within(self.edit_cursor..self.edit_len, self.edit_cursor + 1);
        self.edit_buffer[self.edit_cursor] = byte;
        self.edit_len += 1;
        console::write_from_tty(session, &self.edit_buffer[self.edit_cursor..self.edit_len]);
        self.edit_cursor += 1;
        self.move_visual_cursor_left(session, self.edit_len - self.edit_cursor);
    }

    fn handle_backspace(&mut self, session: ConsoleSessionId) {
        if self.edit_cursor == 0 {
            return;
        }

        if self.edit_cursor == self.edit_len {
            self.edit_len -= 1;
            self.edit_cursor -= 1;
            console::write_from_tty(session, b"\x08 \x08");
            return;
        }

        let delete_at = self.edit_cursor - 1;
        self.edit_buffer
            .copy_within(self.edit_cursor..self.edit_len, delete_at);
        self.edit_len -= 1;
        self.edit_cursor -= 1;
        self.move_visual_cursor_left(session, 1);
        console::write_from_tty(session, &self.edit_buffer[delete_at..self.edit_len]);
        console::write_from_tty(session, b" ");
        self.move_visual_cursor_left(session, self.edit_len - delete_at + 1);
    }

    fn move_cursor_left(&mut self, session: ConsoleSessionId) {
        if self.edit_cursor == 0 {
            return;
        }

        self.edit_cursor -= 1;
        self.move_visual_cursor_left(session, 1);
    }

    fn move_cursor_right(&mut self, session: ConsoleSessionId) {
        if self.edit_cursor >= self.edit_len {
            return;
        }

        self.edit_cursor += 1;
        self.move_visual_cursor_right(session, 1);
    }

    fn commit_line(&mut self, session: ConsoleSessionId) {
        let required = self.edit_len + 1;
        if self.input.remaining_capacity() < required {
            return;
        }

        console::write_from_tty(session, b"\r\n");
        for &byte in self.edit_buffer[..self.edit_len].iter() {
            let _ = self.input.push(byte);
        }
        let _ = self.input.push(b'\n');
        self.edit_len = 0;
        self.edit_cursor = 0;
    }

    fn redraw_edit_buffer_on_fresh_line(&self, session: ConsoleSessionId) {
        if self.edit_len == 0 {
            return;
        }

        console::write_from_tty(session, &self.edit_buffer[..self.edit_len]);
        self.move_visual_cursor_left(session, self.edit_len - self.edit_cursor);
    }

    fn move_visual_cursor_left(&self, session: ConsoleSessionId, count: usize) {
        self.write_cursor_move_sequence(session, count, b'D');
    }

    fn move_visual_cursor_right(&self, session: ConsoleSessionId, count: usize) {
        self.write_cursor_move_sequence(session, count, b'C');
    }

    fn write_cursor_move_sequence(&self, session: ConsoleSessionId, count: usize, direction: u8) {
        if count == 0 {
            return;
        }

        let mut sequence = [0_u8; CURSOR_MOVE_SEQUENCE_MAX_LEN];
        sequence[0] = 0x1b;
        sequence[1] = b'[';
        let mut len = 2;

        if count != 1 {
            len += write_decimal_ascii(&mut sequence[len..], count);
        }

        sequence[len] = direction;
        console::write_from_tty(session, &sequence[..=len]);
    }
}

fn ends_at_fresh_line(bytes: &[u8]) -> bool {
    matches!(bytes.last(), Some(b'\n' | b'\r'))
}

fn write_decimal_ascii(dest: &mut [u8], mut value: usize) -> usize {
    if value == 0 {
        dest[0] = b'0';
        return 1;
    }

    let mut digits = [0_u8; 20];
    let mut len = 0;
    while value != 0 {
        digits[len] = b'0' + (value % 10) as u8;
        value /= 10;
        len += 1;
    }

    for index in 0..len {
        dest[index] = digits[len - index - 1];
    }
    len
}

#[cfg(test)]
mod tests {
    use super::TtySessionState;
    use crate::console;
    use crate::keyboard::{KeyAction, KeyCode, KeyboardEvent, Modifiers};
    use crate::session::ConsoleSessionId;

    fn text_event(code: KeyCode, byte: u8) -> KeyboardEvent {
        KeyboardEvent {
            code,
            action: KeyAction::Pressed,
            modifiers: Modifiers::empty(),
            text: Some(byte),
        }
    }

    fn key_event(code: KeyCode) -> KeyboardEvent {
        KeyboardEvent {
            code,
            action: KeyAction::Pressed,
            modifiers: Modifiers::empty(),
            text: None,
        }
    }

    #[test]
    fn canonical_input_echoes_and_commits_on_enter() {
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::A, b'a'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::S, b's'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::D, b'd'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::F, b'f'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::Enter, b'\n'));

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 5);
        assert_eq!(&input[..5], b"asdf\n");

        let mut output = [0_u8; 8];
        assert_eq!(console::copy_recent_output_for_tests(&mut output), 6);
        assert_eq!(&output[..6], b"asdf\r\n");
    }

    #[test]
    fn backspace_edits_current_line_before_commit() {
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::A, b'a'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::B, b'b'));
        tty.on_key_event(
            ConsoleSessionId::PRIMARY,
            text_event(KeyCode::Backspace, 0x08),
        );
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::C, b'c'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::Enter, b'\n'));

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 3);
        assert_eq!(&input[..3], b"ac\n");
    }

    #[test]
    fn output_redraws_pending_input_on_clean_line() {
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::A, b'a'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::S, b's'));
        assert_eq!(tty.write(ConsoleSessionId::PRIMARY, b"out\n"), 4);

        let mut output = [0_u8; 16];
        assert_eq!(console::copy_recent_output_for_tests(&mut output), 11);
        assert_eq!(&output[..11], b"as\r\nout\r\nas");
    }

    #[test]
    fn arrow_keys_move_cursor_for_middle_insertion() {
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::A, b'a'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::C, b'c'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, key_event(KeyCode::ArrowLeft));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::B, b'b'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, key_event(KeyCode::Enter));

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 4);
        assert_eq!(&input[..4], b"abc\n");

        let mut output = [0_u8; 32];
        let len = console::copy_recent_output_for_tests(&mut output);
        assert_eq!(&output[..len], b"ac\x1b[Dbc\x1b[D\r\n");
    }

    #[test]
    fn backspace_updates_middle_of_line() {
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::A, b'a'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::B, b'b'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::C, b'c'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::D, b'd'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, key_event(KeyCode::ArrowLeft));
        tty.on_key_event(ConsoleSessionId::PRIMARY, key_event(KeyCode::ArrowLeft));
        tty.on_key_event(ConsoleSessionId::PRIMARY, key_event(KeyCode::Backspace));
        tty.on_key_event(ConsoleSessionId::PRIMARY, key_event(KeyCode::Enter));

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 4);
        assert_eq!(&input[..4], b"acd\n");

        let mut output = [0_u8; 48];
        let len = console::copy_recent_output_for_tests(&mut output);
        assert_eq!(&output[..len], b"abcd\x1b[D\x1b[D\x1b[Dcd \x1b[3D\r\n");
    }

    #[test]
    fn redraw_preserves_cursor_after_output() {
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::A, b'a'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, text_event(KeyCode::C, b'c'));
        tty.on_key_event(ConsoleSessionId::PRIMARY, key_event(KeyCode::ArrowLeft));

        assert_eq!(tty.write(ConsoleSessionId::PRIMARY, b"out"), 3);

        let mut output = [0_u8; 32];
        let len = console::copy_recent_output_for_tests(&mut output);
        assert_eq!(&output[..len], b"ac\x1b[D\r\nout\r\nac\x1b[D");
    }
}
