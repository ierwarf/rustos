use core::sync::atomic::{AtomicU8, Ordering};

pub(crate) const CONSOLE_SESSION_COUNT: usize = 2;

static FOCUSED_CONSOLE_SESSION: AtomicU8 = AtomicU8::new(ConsoleSessionId::PRIMARY.0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConsoleSessionId(u8);

impl ConsoleSessionId {
    pub(crate) const PRIMARY: Self = Self(0);
    pub(crate) const SECONDARY: Self = Self(1);

    pub(crate) const fn all() -> [Self; CONSOLE_SESSION_COUNT] {
        [Self::PRIMARY, Self::SECONDARY]
    }

    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }

    pub(crate) const fn name(self) -> &'static str {
        match self.0 {
            0 => "primary",
            1 => "secondary",
            _ => "unknown",
        }
    }

    pub(crate) fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::PRIMARY),
            1 => Some(Self::SECONDARY),
            _ => None,
        }
    }
}

pub(crate) fn focused_console_session() -> ConsoleSessionId {
    ConsoleSessionId::from_index(FOCUSED_CONSOLE_SESSION.load(Ordering::Acquire) as usize)
        .unwrap_or(ConsoleSessionId::PRIMARY)
}

pub(crate) fn set_focused_console_session(session: ConsoleSessionId) -> bool {
    FOCUSED_CONSOLE_SESSION.swap(session.0, Ordering::AcqRel) != session.0
}

#[cfg(test)]
pub(crate) fn reset_focus_for_tests() {
    FOCUSED_CONSOLE_SESSION.store(ConsoleSessionId::PRIMARY.0, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::{
        ConsoleSessionId, focused_console_session, reset_focus_for_tests,
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
        assert!(set_focused_console_session(ConsoleSessionId::SECONDARY));
        assert_eq!(focused_console_session(), ConsoleSessionId::SECONDARY);
        assert!(set_focused_console_session(ConsoleSessionId::PRIMARY));
        assert_eq!(focused_console_session(), ConsoleSessionId::PRIMARY);
    }
}
