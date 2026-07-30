// RING3-MIGRATION-REFERENCE START: bootstrap exception: sessiond/runtimed own
// normal TTY line discipline, session routing, and console read/write policy.
// Ring0 keeps the system console bootstrap buffer substrate.
use nucleus_core::util::ring::RingBuffer;
use rustos_user_abi::console::MAX_CONSOLE_SESSIONS;

use crate::io::session::ConsoleSessionHandle;
use crate::sync::KernelWaitLock;
use crate::user::linux as linux_abi;

const INPUT_BUFFER_CAPACITY: usize = 1024;

static TTY: KernelWaitLock<
    TtyCollection,
    { nucleus_core::util::lockdep::LockClass::TtyWait as u8 },
> = KernelWaitLock::new(TtyCollection::new());

pub fn init() {}

pub fn read_input_for_session(session: ConsoleSessionHandle, dest: &mut [u8]) -> usize {
    TTY.lock()
        .session_mut(session)
        .map_or(0, |state| state.input.pop_into(dest))
}

pub fn has_pending_input_for_session(session: ConsoleSessionHandle) -> bool {
    TTY.lock()
        .session_mut(session)
        .is_some_and(|state| !state.input.is_empty())
}

pub fn pending_input_len_for_session(session: ConsoleSessionHandle) -> usize {
    TTY.lock()
        .session_mut(session)
        .map_or(0, |state| state.input.len())
}

pub fn disarm_input_waiter(task_id: u64) -> bool {
    TTY.lock().remove_input_waiter(task_id)
}

pub fn termios_for_session(session: ConsoleSessionHandle) -> linux_abi::LinuxTermios {
    TTY.lock()
        .session_mut(session)
        .map_or_else(linux_abi::LinuxTermios::default_console, |state| {
            state.termios
        })
}

pub fn set_termios_for_session(
    session: ConsoleSessionHandle,
    termios: linux_abi::LinuxTermios,
    flush_input: bool,
) {
    let mut tty = TTY.lock();
    let Some(session_state) = tty.session_mut(session) else {
        return;
    };
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
            Armed,
        }

        if current_task_id.is_some() && !crate::multitask::arm_block_current_task() {
            return 0;
        }

        let disposition = {
            let mut tty = TTY.lock();
            let Some(session_state) = tty.session_mut(session) else {
                if current_task_id.is_some() {
                    let _ = crate::multitask::cancel_block_current_task();
                }
                return 0;
            };
            let read = session_state.input.pop_into(dest);
            if read != 0 {
                if current_task_id.is_some() {
                    let _ = crate::multitask::cancel_block_current_task();
                }
                ReadDisposition::Ready(read)
            } else if let Some(task_id) = current_task_id {
                session_state.input_waiter = Some(task_id);
                ReadDisposition::Armed
            } else {
                ReadDisposition::Ready(0)
            }
        };

        match disposition {
            ReadDisposition::Ready(read) => return read,
            ReadDisposition::Armed => {
                match crate::multitask::commit_block_current_task_and_yield() {
                    Some(true) => {}
                    Some(false) => {
                        // A non-TTY wake raced the arm. It must not leave a
                        // stale waiter that consumes a later input wake.
                        if let Some(task_id) = current_task_id {
                            disarm_input_waiter(task_id);
                        }
                    }
                    None => {
                        if let Some(task_id) = current_task_id {
                            disarm_input_waiter(task_id);
                        }
                        return 0;
                    }
                }
            }
        }
    }
}

pub fn write_to_session(_session: ConsoleSessionHandle, bytes: &[u8]) -> usize {
    bytes.len()
}

struct TtyCollection {
    system: TtySessionState,
    sessions: [Option<BoundTtySessionState>; MAX_CONSOLE_SESSIONS],
}

impl TtyCollection {
    const fn new() -> Self {
        Self {
            system: TtySessionState::new(),
            sessions: [const { None }; MAX_CONSOLE_SESSIONS],
        }
    }

    fn session_mut(&mut self, session: ConsoleSessionHandle) -> Option<&mut TtySessionState> {
        if session.is_system() {
            return Some(&mut self.system);
        }

        let slot_index = session.slot_index()?;
        let slot = self.sessions.get_mut(slot_index)?;

        let needs_reset = !matches!(
            slot.as_ref(),
            Some(bound) if bound.handle == session
        );
        if needs_reset {
            *slot = Some(BoundTtySessionState {
                handle: session,
                state: TtySessionState::new(),
            });
        }
        Some(&mut slot.as_mut().expect("tty session state").state)
    }

    fn remove_input_waiter(&mut self, task_id: u64) -> bool {
        let mut removed = false;
        if self.system.input_waiter == Some(task_id) {
            self.system.input_waiter = None;
            removed = true;
        }
        for bound in self.sessions.iter_mut().flatten() {
            if bound.state.input_waiter == Some(task_id) {
                bound.state.input_waiter = None;
                removed = true;
            }
        }
        removed
    }
}

struct BoundTtySessionState {
    handle: ConsoleSessionHandle,
    state: TtySessionState,
}

struct TtySessionState {
    input: RingBuffer<u8, INPUT_BUFFER_CAPACITY>,
    termios: linux_abi::LinuxTermios,
    input_waiter: Option<u64>,
}

impl TtySessionState {
    const fn new() -> Self {
        Self {
            input: RingBuffer::new(),
            termios: linux_abi::LinuxTermios::default_console(),
            input_waiter: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundTtySessionState, TtyCollection, TtySessionState};
    use crate::io::session::ConsoleSessionHandle;

    #[test]
    fn round_trip_input_and_output() {
        let mut tty = TtySessionState::new();
        assert!(tty.input.push(b'a'));
        assert!(tty.input.push(b'b'));

        let mut read = [0_u8; 4];
        assert_eq!(tty.input.pop_into(&mut read), 2);
        assert_eq!(&read[..2], b"ab");
    }

    #[test]
    fn retired_task_cleanup_removes_all_tty_waiter_authority() {
        let task_id = 71;
        let mut tty = TtyCollection::new();
        tty.system.input_waiter = Some(task_id);
        tty.sessions[0] = Some(BoundTtySessionState {
            handle: ConsoleSessionHandle::from_raw(1),
            state: TtySessionState {
                input_waiter: Some(task_id),
                ..TtySessionState::new()
            },
        });

        assert!(tty.remove_input_waiter(task_id));
        assert!(!tty.remove_input_waiter(task_id));
    }

    #[test]
    fn out_of_range_session_handle_cannot_expand_ring0_state() {
        let mut tty = TtyCollection::new();
        let invalid = ConsoleSessionHandle::from_parts(
            rustos_user_abi::console::MAX_CONSOLE_SESSIONS as u32,
            1,
        );
        assert!(tty.session_mut(invalid).is_none());
        assert!(tty.sessions.iter().all(Option::is_none));
    }
}
// RING3-MIGRATION-REFERENCE END: sessiond/runtimed-owned bootstrap TTY substrate exception.
