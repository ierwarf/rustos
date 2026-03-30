use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::input::keyboard::{KeyAction, KeyCode, KeyboardEvent};
use crate::io::console;
use crate::io::session::ConsoleSessionHandle;
use crate::user::linux as linux_abi;
use crate::util::ring::RingBuffer;

const INPUT_BUFFER_CAPACITY: usize = 1024;
const EDIT_BUFFER_CAPACITY: usize = 256;
const CURSOR_MOVE_SEQUENCE_MAX_LEN: usize = 16;
const TTY_DEBUG_LOG_LIMIT: usize = 32;

static TTY: Mutex<TtyCollection> = Mutex::new(TtyCollection::new());
static TTY_COMMIT_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);
static TTY_WAKE_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);

pub fn init() {}

pub fn on_key_event(event: KeyboardEvent) {
    on_key_event_for_session(ConsoleSessionHandle::SYSTEM, event);
}

pub fn on_key_event_for_session(session: ConsoleSessionHandle, event: KeyboardEvent) {
    if !session_accepts_user_input(session) {
        return;
    }

    interrupts::without_interrupts(|| TTY.lock().session_mut(session).on_key_event(session, event))
}

#[allow(dead_code)]
pub fn read_input(dest: &mut [u8]) -> usize {
    read_input_for_session(ConsoleSessionHandle::SYSTEM, dest)
}

pub fn read_input_for_session(session: ConsoleSessionHandle, dest: &mut [u8]) -> usize {
    interrupts::without_interrupts(|| TTY.lock().session_mut(session).read_input(dest))
}

pub fn has_pending_input_for_session(session: ConsoleSessionHandle) -> bool {
    interrupts::without_interrupts(|| TTY.lock().session_mut(session).has_pending_input())
}

pub fn pending_input_len_for_session(session: ConsoleSessionHandle) -> usize {
    interrupts::without_interrupts(|| TTY.lock().session_mut(session).pending_input_len())
}

pub fn termios_for_session(session: ConsoleSessionHandle) -> linux_abi::LinuxTermios {
    interrupts::without_interrupts(|| TTY.lock().session_mut(session).termios())
}

pub fn set_termios_for_session(
    session: ConsoleSessionHandle,
    termios: linux_abi::LinuxTermios,
    flush_input: bool,
) {
    interrupts::without_interrupts(|| {
        TTY.lock()
            .session_mut(session)
            .set_termios(termios, flush_input)
    });
}

pub fn read_input_blocking(dest: &mut [u8]) -> usize {
    read_input_blocking_for_session(ConsoleSessionHandle::SYSTEM, dest)
}

pub fn read_input_blocking_for_session(session: ConsoleSessionHandle, dest: &mut [u8]) -> usize {
    if dest.is_empty() {
        return 0;
    }

    let current_task_id = crate::multitask::current_user_id();

    loop {
        enum ReadDisposition {
            Ready(usize),
            Blocked,
        }

        let disposition = interrupts::without_interrupts(|| {
            let mut tty = TTY.lock();
            let session_state = tty.session_mut(session);
            let read = session_state.read_input(dest);
            if read != 0 {
                return ReadDisposition::Ready(read);
            }

            let Some(task_id) = current_task_id else {
                return ReadDisposition::Ready(0);
            };
            if !crate::multitask::block_current_user_task() {
                return ReadDisposition::Ready(0);
            }

            session_state.input_waiter = Some(task_id);
            ReadDisposition::Blocked
        });

        match disposition {
            ReadDisposition::Ready(read) => return read,
            ReadDisposition::Blocked => crate::multitask::yield_now(),
        }
    }
}

pub fn write(bytes: &[u8]) -> usize {
    write_to_session(ConsoleSessionHandle::SYSTEM, bytes)
}

pub fn write_to_session(session: ConsoleSessionHandle, bytes: &[u8]) -> usize {
    interrupts::without_interrupts(|| TTY.lock().session_mut(session).write(session, bytes))
}

pub(crate) fn reset_session(session: ConsoleSessionHandle) {
    interrupts::without_interrupts(|| TTY.lock().reset_session(session));
}

fn session_accepts_user_input(session: ConsoleSessionHandle) -> bool {
    #[cfg(test)]
    {
        let _ = session;
        true
    }

    #[cfg(not(test))]
    {
        // The boot emergency console is a write-only status surface. Dropping keyboard input
        // here avoids accumulating unread TTY state before the userspace desktop takes over.
        !session.is_system() && crate::io::gui::is_userspace_display_active()
    }
}

struct TtyCollection {
    system: TtySessionState,
    sessions: Vec<Option<BoundTtySessionState>>,
}

impl TtyCollection {
    const fn new() -> Self {
        Self {
            system: TtySessionState::new(),
            sessions: Vec::new(),
        }
    }

    fn session_mut(&mut self, session: ConsoleSessionHandle) -> &mut TtySessionState {
        if session.is_system() {
            return &mut self.system;
        }

        let slot_index = session
            .slot_index()
            .expect("non-system TTY session must have a slot index");
        if self.sessions.len() <= slot_index {
            self.sessions.resize_with(slot_index + 1, || None);
        }

        let needs_reset = !matches!(
            self.sessions[slot_index].as_ref(),
            Some(bound) if bound.handle == session
        );
        if needs_reset {
            self.sessions[slot_index] = Some(BoundTtySessionState {
                handle: session,
                state: TtySessionState::new(),
            });
        }
        &mut self.sessions[slot_index]
            .as_mut()
            .expect("tty session state")
            .state
    }

    fn reset_session(&mut self, session: ConsoleSessionHandle) {
        if session.is_system() {
            self.system.reset();
            return;
        }
        let Some(slot_index) = session.slot_index() else {
            return;
        };
        let Some(entry) = self.sessions.get_mut(slot_index) else {
            return;
        };
        let Some(bound) = entry.as_ref() else {
            return;
        };
        if bound.handle == session {
            *entry = None;
        }
    }
}

struct BoundTtySessionState {
    handle: ConsoleSessionHandle,
    state: TtySessionState,
}

struct TtySessionState {
    input: RingBuffer<u8, INPUT_BUFFER_CAPACITY>,
    edit_buffer: [u8; EDIT_BUFFER_CAPACITY],
    edit_len: usize,
    edit_cursor: usize,
    termios: linux_abi::LinuxTermios,
    input_waiter: Option<u64>,
}

impl TtySessionState {
    const fn new() -> Self {
        Self {
            input: RingBuffer::new(),
            edit_buffer: [0; EDIT_BUFFER_CAPACITY],
            edit_len: 0,
            edit_cursor: 0,
            termios: linux_abi::LinuxTermios::default_console(),
            input_waiter: None,
        }
    }

    fn on_key_event(&mut self, session: ConsoleSessionHandle, event: KeyboardEvent) {
        if matches!(event.action, KeyAction::Released) {
            return;
        }

        if self.termios.is_canonical() {
            self.on_canonical_key_event(session, event);
        } else {
            self.on_noncanonical_key_event(session, event);
        }
    }

    fn on_canonical_key_event(&mut self, session: ConsoleSessionHandle, event: KeyboardEvent) {
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

    fn on_noncanonical_key_event(&mut self, session: ConsoleSessionHandle, event: KeyboardEvent) {
        let Some((bytes, len)) = self.noncanonical_input_bytes(event) else {
            return;
        };
        let bytes = &bytes[..len];
        if !self.push_input_bytes_exact(bytes) {
            return;
        }

        if self.termios.echo_enabled() {
            self.echo_noncanonical_input(session, bytes);
        }
    }

    fn read_input(&mut self, dest: &mut [u8]) -> usize {
        self.input.pop_into(dest)
    }

    fn has_pending_input(&self) -> bool {
        self.input.len() != 0
    }

    fn pending_input_len(&self) -> usize {
        self.input.len()
    }

    fn termios(&self) -> linux_abi::LinuxTermios {
        self.termios
    }

    fn set_termios(&mut self, termios: linux_abi::LinuxTermios, flush_input: bool) {
        if flush_input {
            self.clear_pending_input();
        } else if self.termios.is_canonical() && !termios.is_canonical() {
            self.release_edit_buffer_to_input();
        }

        self.termios = termios;
    }

    fn write(&mut self, session: ConsoleSessionHandle, bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        let redraw_edit_buffer = self.should_redraw_edit_buffer();
        if redraw_edit_buffer {
            console::write_from_tty(session, b"\r\n");
        }

        if self.termios.maps_output_newline_to_crlf() {
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
        } else {
            console::write_from_tty(session, bytes);
        }

        if redraw_edit_buffer {
            if !ends_at_fresh_line(bytes) {
                console::write_from_tty(session, b"\r\n");
            }
            self.redraw_edit_buffer_on_fresh_line(session);
        }

        bytes.len()
    }

    fn insert_edit_byte(&mut self, session: ConsoleSessionHandle, byte: u8) {
        if self.edit_len == EDIT_BUFFER_CAPACITY {
            return;
        }

        if self.edit_cursor == self.edit_len {
            self.edit_buffer[self.edit_len] = byte;
            self.edit_len += 1;
            self.edit_cursor += 1;
            if self.should_echo_canonical_input() {
                console::write_from_tty(session, &[byte]);
            }
            return;
        }

        self.edit_buffer
            .copy_within(self.edit_cursor..self.edit_len, self.edit_cursor + 1);
        self.edit_buffer[self.edit_cursor] = byte;
        self.edit_len += 1;
        self.edit_cursor += 1;
        if self.should_echo_canonical_input() {
            console::write_from_tty(
                session,
                &self.edit_buffer[self.edit_cursor - 1..self.edit_len],
            );
            self.move_visual_cursor_left(session, self.edit_len - self.edit_cursor);
        }
    }

    fn handle_backspace(&mut self, session: ConsoleSessionHandle) {
        if self.edit_cursor == 0 {
            return;
        }

        if self.edit_cursor == self.edit_len {
            self.edit_len -= 1;
            self.edit_cursor -= 1;
            if self.should_echo_canonical_input() {
                console::write_from_tty(session, b"\x08 \x08");
            }
            return;
        }

        let delete_at = self.edit_cursor - 1;
        self.edit_buffer
            .copy_within(self.edit_cursor..self.edit_len, delete_at);
        self.edit_len -= 1;
        self.edit_cursor -= 1;
        if self.should_echo_canonical_input() {
            self.move_visual_cursor_left(session, 1);
            console::write_from_tty(session, &self.edit_buffer[delete_at..self.edit_len]);
            console::write_from_tty(session, b" ");
            self.move_visual_cursor_left(session, self.edit_len - delete_at + 1);
        }
    }

    fn move_cursor_left(&mut self, session: ConsoleSessionHandle) {
        if self.edit_cursor == 0 {
            return;
        }

        self.edit_cursor -= 1;
        if self.should_echo_canonical_input() {
            self.move_visual_cursor_left(session, 1);
        }
    }

    fn move_cursor_right(&mut self, session: ConsoleSessionHandle) {
        if self.edit_cursor >= self.edit_len {
            return;
        }

        self.edit_cursor += 1;
        if self.should_echo_canonical_input() {
            self.move_visual_cursor_right(session, 1);
        }
    }

    fn commit_line(&mut self, session: ConsoleSessionHandle) {
        let required = self.edit_len + 1;
        if self.input.remaining_capacity() < required {
            return;
        }

        if self.should_echo_canonical_input() {
            console::write_from_tty(session, b"\r\n");
        }
        for &byte in self.edit_buffer[..self.edit_len].iter() {
            let _ = self.input.push(byte);
        }
        let _ = self.input.push(b'\n');
        self.edit_len = 0;
        self.edit_cursor = 0;
        if !session.is_system()
            && TTY_COMMIT_DEBUG_LOGS.fetch_add(1, Ordering::Relaxed) < TTY_DEBUG_LOG_LIMIT
        {
            crate::debug::println!(
                "tty commit: session={} queued_len={}",
                session.raw(),
                self.input.len(),
            );
        }
        self.wake_input_waiter();
    }

    fn redraw_edit_buffer_on_fresh_line(&self, session: ConsoleSessionHandle) {
        if self.edit_len == 0 {
            return;
        }

        console::write_from_tty(session, &self.edit_buffer[..self.edit_len]);
        self.move_visual_cursor_left(session, self.edit_len - self.edit_cursor);
    }

    fn move_visual_cursor_left(&self, session: ConsoleSessionHandle, count: usize) {
        self.write_cursor_move_sequence(session, count, b'D');
    }

    fn move_visual_cursor_right(&self, session: ConsoleSessionHandle, count: usize) {
        self.write_cursor_move_sequence(session, count, b'C');
    }

    fn write_cursor_move_sequence(
        &self,
        session: ConsoleSessionHandle,
        count: usize,
        direction: u8,
    ) {
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

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn should_echo_canonical_input(&self) -> bool {
        self.termios.is_canonical() && self.termios.echo_enabled()
    }

    fn should_redraw_edit_buffer(&self) -> bool {
        self.should_echo_canonical_input() && self.edit_len != 0
    }

    fn noncanonical_input_bytes(&self, event: KeyboardEvent) -> Option<([u8; 4], usize)> {
        let mut bytes = [0_u8; 4];
        let len = match event.code {
            KeyCode::Backspace => {
                bytes[0] = self.termios.erase_byte();
                1
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                bytes[0] = b'\n';
                1
            }
            KeyCode::ArrowUp => copy_bytes(&mut bytes, b"\x1b[A"),
            KeyCode::ArrowDown => copy_bytes(&mut bytes, b"\x1b[B"),
            KeyCode::ArrowRight => copy_bytes(&mut bytes, b"\x1b[C"),
            KeyCode::ArrowLeft => copy_bytes(&mut bytes, b"\x1b[D"),
            KeyCode::Home => copy_bytes(&mut bytes, b"\x1b[H"),
            KeyCode::End => copy_bytes(&mut bytes, b"\x1b[F"),
            KeyCode::Insert => copy_bytes(&mut bytes, b"\x1b[2~"),
            KeyCode::Delete => copy_bytes(&mut bytes, b"\x1b[3~"),
            KeyCode::PageUp => copy_bytes(&mut bytes, b"\x1b[5~"),
            KeyCode::PageDown => copy_bytes(&mut bytes, b"\x1b[6~"),
            KeyCode::Escape => {
                bytes[0] = 0x1b;
                1
            }
            _ => {
                bytes[0] = event.text?;
                1
            }
        };
        Some((bytes, len))
    }

    fn echo_noncanonical_input(&self, session: ConsoleSessionHandle, bytes: &[u8]) {
        if bytes.len() != 1 {
            return;
        }

        match bytes[0] {
            b'\n' => {
                console::write_from_tty(session, b"\r\n");
            }
            0x08 | 0x7f => {
                console::write_from_tty(session, b"\x08 \x08");
            }
            b'\t' | 0x20..=0x7e => {
                console::write_from_tty(session, bytes);
            }
            byte if self.termios.echoes_control_chars() => {
                let mut echoed = [b'^', b'?'];
                echoed[1] = if byte == 0x7f {
                    b'?'
                } else {
                    byte.saturating_add(64)
                };
                console::write_from_tty(session, &echoed);
            }
            _ => {}
        }
    }

    fn push_input_bytes_exact(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() || self.input.remaining_capacity() < bytes.len() {
            return false;
        }

        for &byte in bytes {
            let _ = self.input.push(byte);
        }
        self.wake_input_waiter();
        true
    }

    fn release_edit_buffer_to_input(&mut self) {
        let mut pushed = false;
        for &byte in self.edit_buffer[..self.edit_len].iter() {
            if !self.input.push(byte) {
                break;
            }
            pushed = true;
        }
        self.edit_len = 0;
        self.edit_cursor = 0;
        if pushed {
            self.wake_input_waiter();
        }
    }

    fn clear_pending_input(&mut self) {
        self.input = RingBuffer::new();
        self.edit_len = 0;
        self.edit_cursor = 0;
    }

    fn wake_input_waiter(&mut self) {
        let Some(task_id) = self.input_waiter.take() else {
            return;
        };
        let woke = crate::multitask::wake_user_task(task_id);
        if TTY_WAKE_DEBUG_LOGS.fetch_add(1, Ordering::Relaxed) < TTY_DEBUG_LOG_LIMIT {
            crate::debug::println!("tty wake: task_id={} woke={}", task_id, woke);
        }
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

fn copy_bytes(dest: &mut [u8], source: &[u8]) -> usize {
    let len = source.len().min(dest.len());
    dest[..len].copy_from_slice(&source[..len]);
    len
}

#[cfg(test)]
mod tests {
    use super::TtySessionState;
    use crate::input::keyboard::{KeyAction, KeyCode, KeyboardEvent, Modifiers};
    use crate::io::console;
    use crate::io::session::ConsoleSessionHandle;
    use crate::user::linux as linux_abi;

    const TEST_SESSION: ConsoleSessionHandle = ConsoleSessionHandle::for_tests(1, 1);

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::exclusive_test()
    }

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
        let _guard = isolated();
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::A, b'a'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::S, b's'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::D, b'd'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::F, b'f'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::Enter, b'\n'));

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 5);
        assert_eq!(&input[..5], b"asdf\n");

        let mut output = [0_u8; 8];
        assert_eq!(
            console::snapshot_recent_output(TEST_SESSION, &mut output),
            6
        );
        assert_eq!(&output[..6], b"asdf\r\n");
    }

    #[test]
    fn backspace_edits_current_line_before_commit() {
        let _guard = isolated();
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::A, b'a'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::B, b'b'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::Backspace, 0x08));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::C, b'c'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::Enter, b'\n'));

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 3);
        assert_eq!(&input[..3], b"ac\n");
    }

    #[test]
    fn output_redraws_pending_input_on_clean_line() {
        let _guard = isolated();
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::A, b'a'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::S, b's'));
        assert_eq!(tty.write(TEST_SESSION, b"out\n"), 4);

        let mut output = [0_u8; 16];
        assert_eq!(
            console::snapshot_recent_output(TEST_SESSION, &mut output),
            11
        );
        assert_eq!(&output[..11], b"as\r\nout\r\nas");
    }

    #[test]
    fn arrow_keys_move_cursor_for_middle_insertion() {
        let _guard = isolated();
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::A, b'a'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::C, b'c'));
        tty.on_key_event(TEST_SESSION, key_event(KeyCode::ArrowLeft));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::B, b'b'));
        tty.on_key_event(TEST_SESSION, key_event(KeyCode::Enter));

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 4);
        assert_eq!(&input[..4], b"abc\n");

        let mut output = [0_u8; 32];
        let len = console::snapshot_recent_output(TEST_SESSION, &mut output);
        assert_eq!(&output[..len], b"ac\x1b[Dbc\x1b[D\r\n");
    }

    #[test]
    fn backspace_updates_middle_of_line() {
        let _guard = isolated();
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::A, b'a'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::B, b'b'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::C, b'c'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::D, b'd'));
        tty.on_key_event(TEST_SESSION, key_event(KeyCode::ArrowLeft));
        tty.on_key_event(TEST_SESSION, key_event(KeyCode::ArrowLeft));
        tty.on_key_event(TEST_SESSION, key_event(KeyCode::Backspace));
        tty.on_key_event(TEST_SESSION, key_event(KeyCode::Enter));

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 4);
        assert_eq!(&input[..4], b"acd\n");

        let mut output = [0_u8; 48];
        let len = console::snapshot_recent_output(TEST_SESSION, &mut output);
        assert_eq!(&output[..len], b"abcd\x1b[D\x1b[D\x1b[Dcd \x1b[3D\r\n");
    }

    #[test]
    fn redraw_preserves_cursor_after_output() {
        let _guard = isolated();
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::A, b'a'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::C, b'c'));
        tty.on_key_event(TEST_SESSION, key_event(KeyCode::ArrowLeft));

        assert_eq!(tty.write(TEST_SESSION, b"out"), 3);

        let mut output = [0_u8; 32];
        let len = console::snapshot_recent_output(TEST_SESSION, &mut output);
        assert_eq!(&output[..len], b"ac\x1b[D\r\nout\r\nac\x1b[D");
    }

    #[test]
    fn noncanonical_mode_queues_input_immediately_without_local_echo() {
        let _guard = isolated();
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        let mut raw = linux_abi::LinuxTermios::default_console();
        raw.c_lflag &= !(linux_abi::ICANON | linux_abi::ECHO);
        raw.c_oflag = 0;
        tty.set_termios(raw, false);

        tty.on_key_event(TEST_SESSION, text_event(KeyCode::A, b'a'));
        tty.on_key_event(TEST_SESSION, key_event(KeyCode::ArrowLeft));
        tty.on_key_event(TEST_SESSION, key_event(KeyCode::Enter));

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 5);
        assert_eq!(&input[..5], b"a\x1b[D\n");

        let mut output = [0_u8; 8];
        assert_eq!(
            console::snapshot_recent_output(TEST_SESSION, &mut output),
            0
        );
    }

    #[test]
    fn tcsetsf_style_flush_discards_pending_canonical_input() {
        let _guard = isolated();
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::A, b'a'));
        tty.on_key_event(TEST_SESSION, text_event(KeyCode::B, b'b'));

        let raw = linux_abi::LinuxTermios {
            c_lflag: linux_abi::ISIG,
            ..linux_abi::LinuxTermios::default_console()
        };
        tty.set_termios(raw, true);

        let mut input = [0_u8; 8];
        assert_eq!(tty.read_input(&mut input), 0);
    }

    #[test]
    fn output_honors_disabled_onlcr_translation() {
        let _guard = isolated();
        console::reset_for_tests();
        let mut tty = TtySessionState::new();
        let mut termios = tty.termios();
        termios.c_oflag = 0;
        tty.set_termios(termios, false);

        assert_eq!(tty.write(TEST_SESSION, b"out\nnext"), 8);

        let mut output = [0_u8; 16];
        let len = console::snapshot_recent_output(TEST_SESSION, &mut output);
        assert_eq!(&output[..len], b"out\nnext");
    }
}
