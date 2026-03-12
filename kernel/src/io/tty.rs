use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::console;
use crate::keyboard::{KeyAction, KeyboardEvent};
use crate::ring::RingBuffer;

const INPUT_BUFFER_CAPACITY: usize = 1024;
const EDIT_BUFFER_CAPACITY: usize = 256;

static TTY: Mutex<TtyState> = Mutex::new(TtyState::new());

pub fn init() {}

pub fn on_key_event(event: KeyboardEvent) {
    interrupts::without_interrupts(|| TTY.lock().on_key_event(event))
}

#[allow(dead_code)]
pub fn read_input(dest: &mut [u8]) -> usize {
    interrupts::without_interrupts(|| TTY.lock().read_input(dest))
}

pub fn read_input_blocking(dest: &mut [u8]) -> usize {
    if dest.is_empty() {
        return 0;
    }

    loop {
        let read = interrupts::without_interrupts(|| TTY.lock().read_input(dest));
        if read != 0 {
            return read;
        }

        interrupts::enable_and_hlt();
    }
}

pub fn pending_input_len() -> usize {
    interrupts::without_interrupts(|| TTY.lock().pending_input_len())
}

pub fn write(bytes: &[u8]) -> usize {
    interrupts::without_interrupts(|| TTY.lock().write(bytes))
}

struct TtyState {
    input: RingBuffer<u8, INPUT_BUFFER_CAPACITY>,
    edit_buffer: [u8; EDIT_BUFFER_CAPACITY],
    edit_len: usize,
}

impl TtyState {
    const fn new() -> Self {
        Self {
            input: RingBuffer::new(),
            edit_buffer: [0; EDIT_BUFFER_CAPACITY],
            edit_len: 0,
        }
    }

    fn on_key_event(&mut self, event: KeyboardEvent) {
        if matches!(event.action, KeyAction::Released) {
            return;
        }

        let Some(byte) = event.text else {
            return;
        };

        match byte {
            0x08 => self.handle_backspace(),
            b'\n' => self.commit_line(),
            b'\t' => self.push_edit_byte(byte),
            0x20..=0x7E => self.push_edit_byte(byte),
            _ => {}
        }
    }

    fn read_input(&mut self, dest: &mut [u8]) -> usize {
        self.input.pop_into(dest)
    }

    fn pending_input_len(&self) -> usize {
        self.input.len()
    }

    fn write(&mut self, bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        let redraw_edit_buffer = self.edit_len != 0;
        if redraw_edit_buffer {
            console::write_from_tty(b"\r\n");
        }

        let mut chunk_start = 0;
        let mut previous = None;

        for (index, &byte) in bytes.iter().enumerate() {
            if byte != b'\n' || previous == Some(b'\r') {
                previous = Some(byte);
                continue;
            }

            if chunk_start < index {
                console::write_from_tty(&bytes[chunk_start..index]);
            }
            console::write_from_tty(b"\r\n");
            chunk_start = index + 1;
            previous = Some(byte);
        }

        if chunk_start < bytes.len() {
            console::write_from_tty(&bytes[chunk_start..]);
        }

        if redraw_edit_buffer {
            if !ends_at_fresh_line(bytes) {
                console::write_from_tty(b"\r\n");
            }
            console::write_from_tty(&self.edit_buffer[..self.edit_len]);
        }

        bytes.len()
    }

    fn push_edit_byte(&mut self, byte: u8) {
        if self.edit_len == EDIT_BUFFER_CAPACITY {
            return;
        }

        self.edit_buffer[self.edit_len] = byte;
        self.edit_len += 1;
        console::write_from_tty(&[byte]);
    }

    fn handle_backspace(&mut self) {
        if self.edit_len == 0 {
            return;
        }

        self.edit_len -= 1;
        console::write_from_tty(b"\x08 \x08");
    }

    fn commit_line(&mut self) {
        let required = self.edit_len + 1;
        if self.input.remaining_capacity() < required {
            return;
        }

        console::write_from_tty(b"\r\n");
        for &byte in self.edit_buffer[..self.edit_len].iter() {
            let _ = self.input.push(byte);
        }
        let _ = self.input.push(b'\n');
        self.edit_len = 0;
    }
}

fn ends_at_fresh_line(bytes: &[u8]) -> bool {
    matches!(bytes.last(), Some(b'\n' | b'\r'))
}

#[cfg(test)]
mod tests {
    use super::TtyState;
    use crate::console;
    use crate::keyboard::{KeyAction, KeyCode, KeyboardEvent, Modifiers};

    fn text_event(code: KeyCode, byte: u8) -> KeyboardEvent {
        KeyboardEvent {
            code,
            action: KeyAction::Pressed,
            modifiers: Modifiers::empty(),
            text: Some(byte),
        }
    }

    #[test]
    fn canonical_input_echoes_and_commits_on_enter() {
        console::reset_for_tests();
        let mut tty = TtyState::new();
        tty.on_key_event(text_event(KeyCode::A, b'a'));
        tty.on_key_event(text_event(KeyCode::S, b's'));
        tty.on_key_event(text_event(KeyCode::D, b'd'));
        tty.on_key_event(text_event(KeyCode::F, b'f'));
        tty.on_key_event(text_event(KeyCode::Enter, b'\n'));

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
        let mut tty = TtyState::new();
        tty.on_key_event(text_event(KeyCode::A, b'a'));
        tty.on_key_event(text_event(KeyCode::B, b'b'));
        tty.on_key_event(text_event(KeyCode::Backspace, 0x08));
        tty.on_key_event(text_event(KeyCode::C, b'c'));
        tty.on_key_event(text_event(KeyCode::Enter, b'\n'));

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 3);
        assert_eq!(&input[..3], b"ac\n");
    }

    #[test]
    fn output_redraws_pending_input_on_clean_line() {
        console::reset_for_tests();
        let mut tty = TtyState::new();
        tty.on_key_event(text_event(KeyCode::A, b'a'));
        tty.on_key_event(text_event(KeyCode::S, b's'));
        assert_eq!(tty.write(b"out\n"), 4);

        let mut output = [0_u8; 16];
        assert_eq!(console::copy_recent_output_for_tests(&mut output), 11);
        assert_eq!(&output[..11], b"as\r\nout\r\nas");
    }
}
