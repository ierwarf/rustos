// RING3-MIGRATION-REFERENCE START: already migrated: vfsd/netd own epoll
// readiness policy. Ring0 keeps epoll token storage and fd-table handle
// substrate only.
use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EpollError {
    Busy,
    InvalidArgument,
    NotFound,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct EpollHandle {
    token: u64,
}

#[derive(Debug, Clone)]
pub struct EpollInterestSnapshot {
    pub events: u32,
    pub data: u64,
}

impl EpollHandle {
    pub fn new() -> Self {
        static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
        Self {
            token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn from_token(token: u64) -> Self {
        Self { token }
    }

    pub fn path(&self) -> &'static str {
        "anon_inode:[eventpoll]"
    }

    pub fn token_id(&self) -> u64 {
        self.token
    }
}

impl Default for EpollHandle {
    fn default() -> Self {
        Self::new()
    }
}
// RING3-MIGRATION-REFERENCE END: vfsd/netd-owned epoll policy token substrate.
