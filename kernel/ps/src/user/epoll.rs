// RING3-MIGRATION-REFERENCE START: already migrated: vfsd/netd own epoll
// readiness policy. Ring0 keeps epoll token storage and fd-table handle
// substrate only.
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
        let mut bytes = [0_u8; 8];
        nucleus_core::util::random::Random::new().fill_bytes(&mut bytes);
        Self {
            token: u64::from_le_bytes(bytes).max(1),
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
