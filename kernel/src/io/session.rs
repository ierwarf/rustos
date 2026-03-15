use core::sync::atomic::{AtomicU8, Ordering};

use spin::Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

pub(crate) const MAX_CONSOLE_SESSIONS: usize = 8;

static SESSION_REGISTRY: Mutex<ConsoleSessionRegistry> = Mutex::new(ConsoleSessionRegistry::new());
static FOCUSED_CONSOLE_SESSION: AtomicU8 = AtomicU8::new(ConsoleSessionId::PRIMARY.0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConsoleSessionId(u8);

impl ConsoleSessionId {
    pub(crate) const PRIMARY: Self = Self(0);
    pub(crate) const SECONDARY: Self = Self(1);

    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }

    pub(crate) const fn name(self) -> &'static str {
        match self.0 {
            0 => "primary",
            1 => "secondary",
            _ => "console",
        }
    }

    pub(crate) fn from_index(index: usize) -> Option<Self> {
        if index < MAX_CONSOLE_SESSIONS {
            Some(Self(index as u8))
        } else {
            None
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ActiveConsoleSessions {
    sessions: [Option<ConsoleSessionId>; MAX_CONSOLE_SESSIONS],
}

impl ActiveConsoleSessions {
    const fn empty() -> Self {
        Self {
            sessions: [None; MAX_CONSOLE_SESSIONS],
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = ConsoleSessionId> + '_ {
        self.sessions.iter().copied().flatten()
    }

    pub(crate) fn count(&self) -> usize {
        self.iter().count()
    }
}

pub(crate) fn active_console_sessions() -> ActiveConsoleSessions {
    with_registry(|registry| registry.snapshot())
}

pub(crate) fn active_console_session_count() -> usize {
    with_registry(|registry| registry.active_count)
}

pub(crate) fn is_console_session_active(session: ConsoleSessionId) -> bool {
    with_registry(|registry| registry.is_active(session))
}

pub(crate) fn ensure_console_session(session: ConsoleSessionId) -> bool {
    with_registry(|registry| registry.ensure_registered(session))
}

pub(crate) fn allocate_console_session() -> Option<ConsoleSessionId> {
    with_registry(|registry| registry.allocate())
}

pub(crate) fn release_console_session(session: ConsoleSessionId) -> bool {
    let released = with_registry(|registry| registry.release(session));
    if !released {
        return false;
    }

    let focused = focused_console_session();
    if focused == session {
        let replacement = active_console_sessions()
            .iter()
            .next()
            .unwrap_or(ConsoleSessionId::PRIMARY);
        FOCUSED_CONSOLE_SESSION.store(replacement.0, Ordering::Release);
    }

    true
}

pub(crate) fn focused_console_session() -> ConsoleSessionId {
    let focused =
        ConsoleSessionId::from_index(FOCUSED_CONSOLE_SESSION.load(Ordering::Acquire) as usize)
            .unwrap_or(ConsoleSessionId::PRIMARY);
    if is_console_session_active(focused) {
        return focused;
    }

    active_console_sessions()
        .iter()
        .next()
        .unwrap_or(ConsoleSessionId::PRIMARY)
}

pub(crate) fn set_focused_console_session(session: ConsoleSessionId) -> bool {
    if !is_console_session_active(session) {
        return false;
    }

    FOCUSED_CONSOLE_SESSION.swap(session.0, Ordering::AcqRel) != session.0
}

#[cfg(test)]
pub(crate) fn reset_focus_for_tests() {
    with_registry(|registry| registry.reset_for_tests());
    FOCUSED_CONSOLE_SESSION.store(ConsoleSessionId::PRIMARY.0, Ordering::Release);
}

struct ConsoleSessionRegistry {
    active: [bool; MAX_CONSOLE_SESSIONS],
    active_count: usize,
}

impl ConsoleSessionRegistry {
    const fn new() -> Self {
        let mut active = [false; MAX_CONSOLE_SESSIONS];
        active[ConsoleSessionId::PRIMARY.index()] = true;
        Self {
            active,
            active_count: 1,
        }
    }

    fn snapshot(&self) -> ActiveConsoleSessions {
        let mut snapshot = ActiveConsoleSessions::empty();
        let mut slot = 0;
        let mut index = 0;
        while index < MAX_CONSOLE_SESSIONS {
            if self.active[index] {
                snapshot.sessions[slot] =
                    Some(ConsoleSessionId::from_index(index).expect("session index"));
                slot += 1;
            }
            index += 1;
        }
        snapshot
    }

    fn is_active(&self, session: ConsoleSessionId) -> bool {
        self.active.get(session.index()).copied().unwrap_or(false)
    }

    fn ensure_registered(&mut self, session: ConsoleSessionId) -> bool {
        let Some(active) = self.active.get_mut(session.index()) else {
            return false;
        };
        if *active {
            return true;
        }

        *active = true;
        self.active_count += 1;
        true
    }

    fn allocate(&mut self) -> Option<ConsoleSessionId> {
        for index in 0..MAX_CONSOLE_SESSIONS {
            if self.active[index] {
                continue;
            }

            self.active[index] = true;
            self.active_count += 1;
            return ConsoleSessionId::from_index(index);
        }

        None
    }

    fn release(&mut self, session: ConsoleSessionId) -> bool {
        if session == ConsoleSessionId::PRIMARY {
            return false;
        }

        let Some(active) = self.active.get_mut(session.index()) else {
            return false;
        };
        if !*active {
            return false;
        }

        *active = false;
        self.active_count = self.active_count.saturating_sub(1);
        true
    }

    #[cfg(test)]
    fn reset_for_tests(&mut self) {
        *self = Self::new();
    }
}

fn with_registry<R>(f: impl FnOnce(&mut ConsoleSessionRegistry) -> R) -> R {
    #[cfg(test)]
    {
        f(&mut SESSION_REGISTRY.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut SESSION_REGISTRY.lock()))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConsoleSessionId, active_console_sessions, allocate_console_session,
        focused_console_session, release_console_session, reset_focus_for_tests,
        set_focused_console_session,
    };

    #[test]
    fn focus_defaults_to_primary() {
        reset_focus_for_tests();
        assert_eq!(focused_console_session(), ConsoleSessionId::PRIMARY);
    }

    #[test]
    fn focus_can_switch_between_sessions() {
        reset_focus_for_tests();
        let secondary = allocate_console_session().expect("secondary session");
        assert_eq!(secondary, ConsoleSessionId::SECONDARY);
        assert!(set_focused_console_session(ConsoleSessionId::SECONDARY));
        assert_eq!(focused_console_session(), ConsoleSessionId::SECONDARY);
        assert!(set_focused_console_session(ConsoleSessionId::PRIMARY));
        assert_eq!(focused_console_session(), ConsoleSessionId::PRIMARY);
    }

    #[test]
    fn releasing_focused_session_falls_back_to_primary() {
        reset_focus_for_tests();
        let secondary = allocate_console_session().expect("secondary session");
        assert!(set_focused_console_session(secondary));
        assert!(release_console_session(secondary));
        assert_eq!(focused_console_session(), ConsoleSessionId::PRIMARY);
    }

    #[test]
    fn active_sessions_expand_dynamically() {
        reset_focus_for_tests();
        let secondary = allocate_console_session().expect("secondary session");
        let tertiary = allocate_console_session().expect("tertiary session");
        let sessions = active_console_sessions();
        let mut iter = sessions.iter();
        assert_eq!(iter.next(), Some(ConsoleSessionId::PRIMARY));
        assert_eq!(iter.next(), Some(secondary));
        assert_eq!(iter.next(), Some(tertiary));
        assert_eq!(iter.next(), None);
    }
}
