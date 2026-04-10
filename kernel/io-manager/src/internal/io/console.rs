use alloc::vec::Vec;

use spin::Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

use crate::io::session::ConsoleSessionHandle;
use crate::util::ring::RingBuffer;

const OUTPUT_BUFFER_CAPACITY: usize = 4096;
const PENDING_BUFFER_CAPACITY: usize = 4096;
const FLUSH_CHUNK_CAPACITY: usize = 256;

static CONSOLE: Mutex<ConsoleState> = Mutex::new(ConsoleState::new());

pub fn init() {
    crate::io::gui::init_console();
}

#[allow(dead_code)]
pub fn write(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    with_console_state(|console| console.system.write(bytes))
}

pub(crate) fn write_to_session(session: ConsoleSessionHandle, bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let written = with_console_state(|console| console.write_to_session(session, bytes));
    while flush_pending_once() {}
    written
}

pub(crate) fn write_from_tty(session: ConsoleSessionHandle, bytes: &[u8]) -> usize {
    write_to_session(session, bytes)
}

pub(crate) fn reset_session(session: ConsoleSessionHandle) {
    with_console_state(|console| console.reset_session(session));
}

pub(crate) fn snapshot_recent_output(session: ConsoleSessionHandle, dest: &mut [u8]) -> usize {
    with_console_state(|console| console.copy_recent_output(session, dest))
}

pub(crate) fn session_output_generation(session: ConsoleSessionHandle) -> u64 {
    with_console_state(|console| console.output_generation(session))
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *CONSOLE.lock() = ConsoleState::new();
}

#[cfg(test)]
pub(crate) fn copy_recent_output_for_tests(dest: &mut [u8]) -> usize {
    CONSOLE
        .lock()
        .copy_recent_output(ConsoleSessionHandle::SYSTEM, dest)
}

fn flush_pending_once() -> bool {
    let mut chunk = [0_u8; FLUSH_CHUNK_CAPACITY];
    let Some((session, len)) = with_console_state(|console| console.drain_pending(&mut chunk))
    else {
        return false;
    };

    crate::io::gui::write_console_session(session, &chunk[..len]);
    true
}

pub fn service() -> usize {
    let mut work = 0;
    while flush_pending_once() {
        work += 1;
    }
    crate::io::gui::tick_console_cursor();
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
    system: ConsoleSessionState,
    sessions: Vec<Option<BoundConsoleSessionState>>,
    next_flush_slot: usize,
}

impl ConsoleState {
    const fn new() -> Self {
        Self {
            system: ConsoleSessionState::new(),
            sessions: Vec::new(),
            next_flush_slot: 0,
        }
    }

    fn write_to_session(&mut self, session: ConsoleSessionHandle, bytes: &[u8]) -> usize {
        if session.is_system() {
            return self.system.write(bytes);
        }
        self.ensure_session(session).write(bytes)
    }

    fn drain_pending(&mut self, dest: &mut [u8]) -> Option<(ConsoleSessionHandle, usize)> {
        if self.sessions.is_empty() {
            return None;
        }

        let session_count = self.sessions.len();

        for offset in 0..session_count {
            let index = (self.next_flush_slot + offset) % session_count;
            let Some(handle) = self
                .sessions
                .get(index)
                .and_then(|entry| entry.as_ref())
                .map(|bound| bound.handle)
            else {
                continue;
            };
            if !crate::io::session::is_console_session_active(handle) {
                continue;
            }

            let Some(bound) = self.sessions[index].as_mut() else {
                continue;
            };

            let len = bound.state.drain_pending(dest);
            if len == 0 {
                continue;
            }

            self.next_flush_slot = (index + 1) % session_count;
            return Some((handle, len));
        }

        None
    }

    fn copy_recent_output(&self, session: ConsoleSessionHandle, dest: &mut [u8]) -> usize {
        if session.is_system() {
            return self.system.copy_recent_output(dest);
        }
        let Some(slot_index) = session.slot_index() else {
            return 0;
        };
        let Some(Some(bound)) = self.sessions.get(slot_index) else {
            return 0;
        };
        if bound.handle != session {
            return 0;
        }
        bound.state.copy_recent_output(dest)
    }

    fn output_generation(&self, session: ConsoleSessionHandle) -> u64 {
        if session.is_system() {
            return self.system.output_generation();
        }
        let Some(slot_index) = session.slot_index() else {
            return 0;
        };
        let Some(Some(bound)) = self.sessions.get(slot_index) else {
            return 0;
        };
        if bound.handle != session {
            return 0;
        }
        bound.state.output_generation()
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
        if bound.handle != session {
            return;
        }
        *entry = None;
        if self.next_flush_slot >= self.sessions.len().saturating_sub(1) {
            self.next_flush_slot = 0;
        }
    }

    fn ensure_session(&mut self, session: ConsoleSessionHandle) -> &mut ConsoleSessionState {
        let slot_index = session
            .slot_index()
            .expect("non-system console session must have a slot index");
        if self.sessions.len() <= slot_index {
            self.sessions.resize_with(slot_index + 1, || None);
        }

        let needs_reset = !matches!(
            self.sessions[slot_index].as_ref(),
            Some(bound) if bound.handle == session
        );
        if needs_reset {
            self.sessions[slot_index] = Some(BoundConsoleSessionState {
                handle: session,
                state: ConsoleSessionState::new(),
            });
        }
        &mut self.sessions[slot_index]
            .as_mut()
            .expect("console session state")
            .state
    }
}

struct BoundConsoleSessionState {
    handle: ConsoleSessionHandle,
    state: ConsoleSessionState,
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
        *self = Self {
            output_generation: self.output_generation.wrapping_add(1),
            ..Self::new()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{ConsoleSessionHandle, ConsoleState};

    #[test]
    fn stores_recent_output() {
        let mut console = ConsoleState::new();
        let session = ConsoleSessionHandle::for_tests(1, 1);
        assert_eq!(console.write_to_session(session, b"asdf\r\n"), 6);

        let mut output = [0_u8; 8];
        assert_eq!(console.copy_recent_output(session, &mut output), 6);
        assert_eq!(&output[..6], b"asdf\r\n");
    }
}
