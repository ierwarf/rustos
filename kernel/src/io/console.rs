use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::ring::RingBuffer;
use crate::session::{ConsoleSessionId, MAX_CONSOLE_SESSIONS, active_console_sessions};

const OUTPUT_BUFFER_CAPACITY: usize = 4096;
const PENDING_BUFFER_CAPACITY: usize = 4096;
const FLUSH_CHUNK_CAPACITY: usize = 256;

static CONSOLE: Mutex<ConsoleState> = Mutex::new(ConsoleState::new());

pub fn init() {
    crate::gui::init_console();
}

#[allow(dead_code)]
pub fn write(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let written = with_console_state(|console| console.write_broadcast(bytes));
    while flush_pending_once() {}
    written
}

pub(crate) fn write_to_session(session: ConsoleSessionId, bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let written = with_console_state(|console| console.write_to_session(session, bytes));
    while flush_pending_once() {}
    written
}

pub(crate) fn write_from_tty(session: ConsoleSessionId, bytes: &[u8]) -> usize {
    write_to_session(session, bytes)
}

pub(crate) fn reset_session(session: ConsoleSessionId) {
    with_console_state(|console| console.reset_session(session));
}

pub(crate) fn snapshot_recent_output(session: ConsoleSessionId, dest: &mut [u8]) -> usize {
    with_console_state(|console| console.copy_recent_output(session, dest))
}

pub(crate) fn snapshot_output_generations() -> [u64; MAX_CONSOLE_SESSIONS] {
    with_console_state(|console| console.output_generations())
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *CONSOLE.lock() = ConsoleState::new();
}

#[cfg(test)]
pub(crate) fn copy_recent_output_for_tests(dest: &mut [u8]) -> usize {
    CONSOLE
        .lock()
        .copy_recent_output(ConsoleSessionId::PRIMARY, dest)
}

fn flush_pending_once() -> bool {
    let mut chunk = [0_u8; FLUSH_CHUNK_CAPACITY];
    let Some((session, len)) = with_console_state(|console| console.drain_pending(&mut chunk))
    else {
        return false;
    };

    crate::gui::write_console_session(session, &chunk[..len]);
    true
}

pub fn service() -> usize {
    let mut work = 0;
    while flush_pending_once() {
        work += 1;
    }
    crate::gui::tick_console_cursor();
    work
}

fn with_console_state<R>(f: impl FnOnce(&mut ConsoleState) -> R) -> R {
    #[cfg(test)]
    {
        f(&mut CONSOLE.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut CONSOLE.lock()))
    }
}

struct ConsoleState {
    sessions: [ConsoleSessionState; MAX_CONSOLE_SESSIONS],
    next_flush_session: usize,
}

impl ConsoleState {
    const fn new() -> Self {
        Self {
            sessions: [const { ConsoleSessionState::new() }; MAX_CONSOLE_SESSIONS],
            next_flush_session: 0,
        }
    }

    fn write_broadcast(&mut self, bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        for session in active_console_sessions().iter() {
            let _ = self.sessions[session.index()].write(bytes);
        }
        bytes.len()
    }

    fn write_to_session(&mut self, session: ConsoleSessionId, bytes: &[u8]) -> usize {
        self.sessions[session.index()].write(bytes)
    }

    fn drain_pending(&mut self, dest: &mut [u8]) -> Option<(ConsoleSessionId, usize)> {
        let active_sessions = active_console_sessions();
        for offset in 0..MAX_CONSOLE_SESSIONS {
            let index = (self.next_flush_session + offset) % MAX_CONSOLE_SESSIONS;
            if !active_sessions
                .iter()
                .any(|session| session.index() == index)
            {
                continue;
            }
            let len = self.sessions[index].drain_pending(dest);
            if len == 0 {
                continue;
            }

            self.next_flush_session = (index + 1) % MAX_CONSOLE_SESSIONS;
            return Some((
                ConsoleSessionId::from_index(index).expect("session index"),
                len,
            ));
        }

        None
    }

    fn copy_recent_output(&self, session: ConsoleSessionId, dest: &mut [u8]) -> usize {
        self.sessions[session.index()].copy_recent_output(dest)
    }

    fn output_generations(&self) -> [u64; MAX_CONSOLE_SESSIONS] {
        let mut generations = [0_u64; MAX_CONSOLE_SESSIONS];
        for (index, session) in self.sessions.iter().enumerate() {
            generations[index] = session.output_generation();
        }
        generations
    }

    fn reset_session(&mut self, session: ConsoleSessionId) {
        self.sessions[session.index()].reset();
        if self.next_flush_session == session.index() {
            self.next_flush_session = (self.next_flush_session + 1) % MAX_CONSOLE_SESSIONS;
        }
    }
}

struct ConsoleSessionState {
    output: RingBuffer<u8, OUTPUT_BUFFER_CAPACITY>,
    pending: RingBuffer<u8, PENDING_BUFFER_CAPACITY>,
    output_generation: u64,
}

impl ConsoleSessionState {
    const fn new() -> Self {
        Self {
            output: RingBuffer::new(),
            pending: RingBuffer::new(),
            output_generation: 0,
        }
    }

    fn write(&mut self, bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        let written = self.output.extend_overwrite(bytes);
        self.pending.extend_overwrite(bytes);
        self.output_generation = self.output_generation.wrapping_add(1);
        written
    }

    fn drain_pending(&mut self, dest: &mut [u8]) -> usize {
        self.pending.pop_into(dest)
    }

    fn copy_recent_output(&self, dest: &mut [u8]) -> usize {
        self.output.copy_into(dest)
    }

    fn output_generation(&self) -> u64 {
        self.output_generation
    }

    fn reset(&mut self) {
        self.output = RingBuffer::new();
        self.pending = RingBuffer::new();
        self.output_generation = self.output_generation.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::ConsoleState;
    use crate::session::ConsoleSessionId;

    #[test]
    fn stores_recent_output() {
        let mut console = ConsoleState::new();
        assert_eq!(
            console.write_to_session(ConsoleSessionId::PRIMARY, b"asdf\r\n"),
            6
        );

        let mut output = [0_u8; 8];
        assert_eq!(
            console.copy_recent_output(ConsoleSessionId::PRIMARY, &mut output),
            6
        );
        assert_eq!(&output[..6], b"asdf\r\n");
    }
}
