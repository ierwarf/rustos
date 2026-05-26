use alloc::vec::Vec;
use nucleus_core::util::ring::RingBuffer;

use crate::input::keyboard::KeyboardEvent;
use crate::io::session::ConsoleSessionHandle;
use crate::sync::KernelWaitLock;
use crate::user::linux as linux_abi;

const INPUT_BUFFER_CAPACITY: usize = 1024;
const OUTPUT_BUFFER_CAPACITY: usize = 4096;

static TTY: KernelWaitLock<TtyCollection> = KernelWaitLock::new(TtyCollection::new());

pub fn init() {}

pub fn on_key_event_for_session(session: ConsoleSessionHandle, event: KeyboardEvent) {
    if !session_accepts_user_input(session) {
        return;
    }

    let mut tty = TTY.lock();
    let session_state = tty.session_mut(session);

    if let Some(byte) = event.text {
        let _ = session_state.input.push(byte);
        session_state.wake_input_waiter();
    }
}

pub fn read_input_for_session(session: ConsoleSessionHandle, dest: &mut [u8]) -> usize {
    TTY.lock().session_mut(session).input.pop_into(dest)
}

pub fn has_pending_input_for_session(session: ConsoleSessionHandle) -> bool {
    TTY.lock().session_mut(session).input.len() != 0
}

pub fn pending_input_len_for_session(session: ConsoleSessionHandle) -> usize {
    TTY.lock().session_mut(session).input.len()
}

pub fn termios_for_session(session: ConsoleSessionHandle) -> linux_abi::LinuxTermios {
    TTY.lock().session_mut(session).termios
}

pub fn set_termios_for_session(
    session: ConsoleSessionHandle,
    termios: linux_abi::LinuxTermios,
    flush_input: bool,
) {
    let mut tty = TTY.lock();
    let session_state = tty.session_mut(session);
    if flush_input {
        session_state.input = RingBuffer::new();
    }
    session_state.termios = termios;
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

        let disposition = {
            let mut tty = TTY.lock();
            let session_state = tty.session_mut(session);
            let read = session_state.input.pop_into(dest);
            if read != 0 {
                ReadDisposition::Ready(read)
            } else if let Some(task_id) = current_task_id {
                if crate::multitask::block_current_user_task() {
                    session_state.input_waiter = Some(task_id);
                    ReadDisposition::Blocked
                } else {
                    ReadDisposition::Ready(0)
                }
            } else {
                ReadDisposition::Ready(0)
            }
        };

        match disposition {
            ReadDisposition::Ready(read) => return read,
            ReadDisposition::Blocked => crate::multitask::yield_now(),
        }
    }
}

pub fn write_to_session(session: ConsoleSessionHandle, bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    TTY.lock().session_mut(session).output.extend_overwrite(bytes)
}

pub(crate) fn reset_session(session: ConsoleSessionHandle) {
    TTY.lock().reset_session(session);
}

fn session_accepts_user_input(session: ConsoleSessionHandle) -> bool {
    #[cfg(test)]
    {
        let _ = session;
        true
    }

    #[cfg(not(test))]
    {
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
    output: RingBuffer<u8, OUTPUT_BUFFER_CAPACITY>,
    termios: linux_abi::LinuxTermios,
    input_waiter: Option<u64>,
}

impl TtySessionState {
    const fn new() -> Self {
        Self {
            input: RingBuffer::new(),
            output: RingBuffer::new(),
            termios: linux_abi::LinuxTermios::default_console(),
            input_waiter: None,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn wake_input_waiter(&mut self) {
        let Some(task_id) = self.input_waiter.take() else {
            return;
        };
        let _ = crate::multitask::wake_user_task(task_id);
    }
}

#[cfg(test)]
mod tests {
    use super::TtySessionState;

    #[test]
    fn round_trip_input_and_output() {
        let mut tty = TtySessionState::new();
        assert!(tty.input.push(b'a'));
        assert!(tty.input.push(b'b'));

        let mut read = [0_u8; 4];
        assert_eq!(tty.input.pop_into(&mut read), 2);
        assert_eq!(&read[..2], b"ab");
    }
}
