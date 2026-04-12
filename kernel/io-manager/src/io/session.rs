use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

pub(crate) const MAX_LIVE_CONSOLE_SESSIONS: usize = 32;
pub(crate) const CONSOLE_SESSION_TITLE_CAPACITY: usize = 48;
pub(crate) const CONSOLE_SESSION_PATH_CAPACITY: usize = 64;

static SESSION_MANAGER: Mutex<ConsoleSessionManager> = Mutex::new(ConsoleSessionManager::new());
static NEXT_SESSION_CREATED_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct ConsoleSessionHandle(u64);

impl ConsoleSessionHandle {
    pub const SYSTEM: Self = Self(0);

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn into_object_handle(self) -> kernel_object::api::session::ConsoleSessionHandle {
        kernel_object::api::session::ConsoleSessionHandle::from_raw(self.0)
    }

    pub const fn from_object_handle(
        handle: kernel_object::api::session::ConsoleSessionHandle,
    ) -> Self {
        Self(handle.raw())
    }

    pub const fn is_system(self) -> bool {
        self.0 == 0
    }

    pub(crate) const fn slot_index(self) -> Option<usize> {
        if self.0 == 0 {
            None
        } else {
            Some((self.0 as u32) as usize)
        }
    }

    #[allow(dead_code)]
    pub(crate) const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }

    const fn from_parts(slot_index: u32, generation: u32) -> Self {
        Self(((generation as u64) << 32) | slot_index as u64)
    }

    #[cfg(test)]
    pub(crate) const fn for_tests(slot_index: u32, generation: u32) -> Self {
        Self::from_parts(slot_index, generation)
    }
}

impl From<ConsoleSessionHandle> for kernel_object::api::session::ConsoleSessionHandle {
    fn from(value: ConsoleSessionHandle) -> Self {
        value.into_object_handle()
    }
}

impl From<kernel_object::api::session::ConsoleSessionHandle> for ConsoleSessionHandle {
    fn from(value: kernel_object::api::session::ConsoleSessionHandle) -> Self {
        Self::from_object_handle(value)
    }
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ConsoleSessionState {
    #[default]
    Queued = 1,
    LoadingImage = 2,
    Spawning = 3,
    Running = 4,
    Closing = 5,
}

// This is copied into the console device ABI and therefore intentionally keeps fields that the
// kernel itself does not read back.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConsoleSessionInfo {
    pub(crate) handle: ConsoleSessionHandle,
    pub(crate) program_id: u32,
    pub(crate) pid: Option<u64>,
    pub(crate) focused: bool,
    pub(crate) state: ConsoleSessionState,
    pub(crate) created_generation: u64,
    pub(crate) focus_order: u64,
    pub(crate) title: [u8; CONSOLE_SESSION_TITLE_CAPACITY],
    pub(crate) exec_path: [u8; CONSOLE_SESSION_PATH_CAPACITY],
}

impl Default for ConsoleSessionInfo {
    fn default() -> Self {
        Self {
            handle: ConsoleSessionHandle::SYSTEM,
            program_id: 0,
            pid: None,
            focused: false,
            state: ConsoleSessionState::Queued,
            created_generation: 0,
            focus_order: 0,
            title: [0; CONSOLE_SESSION_TITLE_CAPACITY],
            exec_path: [0; CONSOLE_SESSION_PATH_CAPACITY],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreateConsoleSessionError {
    NoCapacity,
}

struct ConsoleSessionRecord {
    handle: ConsoleSessionHandle,
    program_id: u32,
    pid: Option<u64>,
    focus_order: u64,
    created_generation: u64,
    state: ConsoleSessionState,
    title: [u8; CONSOLE_SESSION_TITLE_CAPACITY],
    exec_path: [u8; CONSOLE_SESSION_PATH_CAPACITY],
}

struct SessionSlot {
    generation: u32,
    record: Option<ConsoleSessionRecord>,
}

struct ConsoleSessionManager {
    slots: Vec<SessionSlot>,
    free_slots: Vec<u32>,
    next_focus_order: u64,
}

impl ConsoleSessionManager {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free_slots: Vec::new(),
            next_focus_order: 1,
        }
    }

    fn create_session(
        &mut self,
        program_id: u32,
        title: &str,
        exec_path: &str,
    ) -> Result<ConsoleSessionHandle, CreateConsoleSessionError> {
        let slot_index = if let Some(slot_index) = self.free_slots.pop() {
            slot_index
        } else {
            if self.slots.len() >= MAX_LIVE_CONSOLE_SESSIONS {
                return Err(CreateConsoleSessionError::NoCapacity);
            }
            self.slots.push(SessionSlot {
                generation: 1,
                record: None,
            });
            (self.slots.len() - 1) as u32
        };

        let created_generation = NEXT_SESSION_CREATED_GENERATION.fetch_add(1, Ordering::Relaxed);
        let focus_order = self.bump_focus_order();
        let slot = self
            .slots
            .get_mut(slot_index as usize)
            .expect("session slot index must exist");
        let generation = slot.generation.max(1);
        let handle = ConsoleSessionHandle::from_parts(slot_index, generation);
        slot.record = Some(ConsoleSessionRecord {
            handle,
            program_id,
            pid: None,
            focus_order,
            created_generation,
            state: ConsoleSessionState::Queued,
            title: ascii_array(title),
            exec_path: ascii_array(exec_path),
        });
        Ok(handle)
    }

    fn remove_session(&mut self, handle: ConsoleSessionHandle) -> bool {
        let Some(slot_index) = handle.slot_index() else {
            return false;
        };
        let Some(slot) = self.slots.get_mut(slot_index) else {
            return false;
        };
        let Some(record) = slot.record.as_ref() else {
            return false;
        };
        if record.handle != handle {
            return false;
        }

        slot.record = None;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        self.free_slots.push(slot_index as u32);
        true
    }

    fn contains(&self, handle: ConsoleSessionHandle) -> bool {
        self.record(handle).is_some()
    }

    fn record(&self, handle: ConsoleSessionHandle) -> Option<&ConsoleSessionRecord> {
        let slot_index = handle.slot_index()?;
        let slot = self.slots.get(slot_index)?;
        let record = slot.record.as_ref()?;
        (record.handle == handle).then_some(record)
    }

    fn record_mut(&mut self, handle: ConsoleSessionHandle) -> Option<&mut ConsoleSessionRecord> {
        let slot_index = handle.slot_index()?;
        let slot = self.slots.get_mut(slot_index)?;
        let record = slot.record.as_mut()?;
        (record.handle == handle).then_some(record)
    }

    fn bump_focus_order(&mut self) -> u64 {
        let focus_order = self.next_focus_order;
        self.next_focus_order = self.next_focus_order.wrapping_add(1).max(1);
        focus_order
    }

    fn focus_session(&mut self, handle: ConsoleSessionHandle) -> bool {
        let focus_order = self.bump_focus_order();
        let Some(record) = self.record_mut(handle) else {
            return false;
        };
        if record.state == ConsoleSessionState::Closing {
            return false;
        }
        record.focus_order = focus_order;
        true
    }

    fn focused_session(&self) -> Option<ConsoleSessionHandle> {
        self.slots
            .iter()
            .filter_map(|slot| slot.record.as_ref())
            .filter(|record| record.state != ConsoleSessionState::Closing)
            .max_by_key(|record| record.focus_order)
            .map(|record| record.handle)
    }

    fn transition_state(
        &mut self,
        handle: ConsoleSessionHandle,
        state: ConsoleSessionState,
    ) -> bool {
        let Some(record) = self.record_mut(handle) else {
            return false;
        };
        record.state = state;
        true
    }

    fn assign_pid(&mut self, handle: ConsoleSessionHandle, pid: Option<u64>) -> bool {
        let Some(record) = self.record_mut(handle) else {
            return false;
        };
        record.pid = pid;
        true
    }

    fn snapshot_infos(&self, dest: &mut [ConsoleSessionInfo]) -> usize {
        let focused = self.focused_session();
        let mut count = 0;
        for slot in &self.slots {
            let Some(record) = slot.record.as_ref() else {
                continue;
            };
            if count == dest.len() {
                break;
            }
            dest[count] = ConsoleSessionInfo {
                handle: record.handle,
                program_id: record.program_id,
                pid: record.pid,
                focused: Some(record.handle) == focused,
                state: record.state,
                created_generation: record.created_generation,
                focus_order: record.focus_order,
                title: record.title,
                exec_path: record.exec_path,
            };
            count += 1;
        }
        count
    }
}

pub fn create_console_session(
    program_id: u32,
    title: &str,
    exec_path: &str,
) -> Result<ConsoleSessionHandle, CreateConsoleSessionError> {
    with_manager(|manager| manager.create_session(program_id, title, exec_path))
}

pub(crate) fn remove_console_session(handle: ConsoleSessionHandle) -> bool {
    with_manager(|manager| manager.remove_session(handle))
}

pub fn is_console_session_active(handle: ConsoleSessionHandle) -> bool {
    if handle.is_system() {
        return false;
    }
    with_manager(|manager| manager.contains(handle))
}

pub fn focused_console_session() -> Option<ConsoleSessionHandle> {
    with_manager(|manager| manager.focused_session())
}

pub fn set_focused_console_session(handle: ConsoleSessionHandle) -> bool {
    if handle.is_system() {
        return false;
    }
    with_manager(|manager| manager.focus_session(handle))
}

pub(crate) fn transition_console_session_state(
    handle: ConsoleSessionHandle,
    state: ConsoleSessionState,
) -> bool {
    if handle.is_system() {
        return false;
    }
    with_manager(|manager| manager.transition_state(handle, state))
}

pub(crate) fn assign_console_session_pid(handle: ConsoleSessionHandle, pid: Option<u64>) -> bool {
    if handle.is_system() {
        return false;
    }
    with_manager(|manager| manager.assign_pid(handle, pid))
}

pub(crate) fn snapshot_console_sessions(dest: &mut [ConsoleSessionInfo]) -> usize {
    with_manager(|manager| manager.snapshot_infos(dest))
}

fn ascii_array<const N: usize>(value: &str) -> [u8; N] {
    let mut stored = [0_u8; N];
    let mut len = 0usize;
    for byte in value.bytes() {
        if len == stored.len() {
            break;
        }
        stored[len] = match byte {
            b' '..=b'~' => byte,
            _ => b'?',
        };
        len += 1;
    }
    stored
}

fn with_manager<R>(f: impl FnOnce(&mut ConsoleSessionManager) -> R) -> R {
    #[cfg(test)]
    {
        f(&mut SESSION_MANAGER.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut SESSION_MANAGER.lock()))
    }
}

#[cfg(test)]
pub fn reset_for_tests() {
    *SESSION_MANAGER.lock() = ConsoleSessionManager::new();
    NEXT_SESSION_CREATED_GENERATION.store(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use spin::Mutex;

    use super::{
        ConsoleSessionHandle, ConsoleSessionState, create_console_session, focused_console_session,
        remove_console_session, reset_for_tests, set_focused_console_session,
        snapshot_console_sessions, transition_console_session_state,
    };

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_isolated_session_test(f: impl FnOnce()) {
        let _guard = TEST_LOCK.lock();
        reset_for_tests();
        f();
    }

    #[test]
    fn stale_handle_is_not_reused() {
        with_isolated_session_test(|| {
            let handle = create_console_session(1, "printf demo", "/bin/printf").expect("session");
            assert!(remove_console_session(handle));
            let replacement =
                create_console_session(2, "printf demo 2", "/bin/printf2").expect("replacement");
            assert_ne!(handle, replacement);
        });
    }

    #[test]
    fn focus_falls_back_to_latest_live_session() {
        with_isolated_session_test(|| {
            let first = create_console_session(1, "one", "/one").expect("first");
            let second = create_console_session(2, "two", "/two").expect("second");
            assert_eq!(focused_console_session(), Some(second));
            assert!(set_focused_console_session(first));
            assert_eq!(focused_console_session(), Some(first));
            assert!(remove_console_session(first));
            assert_eq!(focused_console_session(), Some(second));
        });
    }

    #[test]
    fn snapshots_include_state_and_focus() {
        with_isolated_session_test(|| {
            let handle = create_console_session(7, "demo", "/demo").expect("session");
            assert!(transition_console_session_state(
                handle,
                ConsoleSessionState::Running
            ));
            let mut sessions = [super::ConsoleSessionInfo::default(); 4];
            let count = snapshot_console_sessions(&mut sessions);
            assert_eq!(count, 1);
            assert_eq!(sessions[0].handle, handle);
            assert_eq!(sessions[0].program_id, 7);
            assert_eq!(sessions[0].state, ConsoleSessionState::Running);
            assert!(sessions[0].focused);
            assert_eq!(sessions[0].title[0], b'd');
        });
    }

    #[test]
    fn system_handle_is_not_a_live_session() {
        with_isolated_session_test(|| {
            assert_eq!(focused_console_session(), None);
            assert!(!set_focused_console_session(ConsoleSessionHandle::SYSTEM));
            assert!(!remove_console_session(ConsoleSessionHandle::SYSTEM));
        });
    }
}
