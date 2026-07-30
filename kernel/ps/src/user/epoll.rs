//! Kernel-local epoll open-description identity and descriptor ownership.
//!
//! - **Owner:** `kernel-ps` owns fd-table reference lifetime; vfsd owns interests.
//! - **Boundary:** Tokens are kernel-minted; only published descriptors count.
//! - **Lifecycle:** Create with one ref, dup/fork/transfer retain, final close
//!   reaches zero once and authorizes one durable vfsd retire mutation.
//! - **Concurrency:** Transient `Arc` snapshots never affect descriptor refs;
//!   atomic retain rejects zero and release panics on ownership underflow.
//! - **Failure:** Provider delay cannot fail ordinary descriptor duplication.
//! - **Forbidden:** No vfsd call per dup, zero resurrection, or token aliasing.
//! - **Evidence:** `userspace-wait-set`, requirements REQ-VFS-016/017.
// RING3-MIGRATION-REFERENCE START: already migrated: vfsd/netd own epoll
// readiness policy. Ring0 keeps the epoll open-description identity and exact
// descriptor ownership because dup/fork/SCM_RIGHTS are fd-table operations.
// Provider mutation is therefore create/final-retire only; ordinary descriptor
// duplication must never synchronously call vfsd.
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EpollError {
    Busy,
    InvalidArgument,
    NotFound,
}

#[derive(Debug)]
struct EpollOpenDescription {
    token: u64,
    descriptor_refs: AtomicUsize,
}

#[derive(Debug, Clone)]
pub struct EpollHandle {
    description: Arc<EpollOpenDescription>,
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
            description: Arc::new(EpollOpenDescription {
                token: u64::from_le_bytes(bytes).max(1),
                descriptor_refs: AtomicUsize::new(1),
            }),
        }
    }

    pub fn from_token(token: u64) -> Self {
        assert!(token != 0, "epoll open-description token must be nonzero");
        Self {
            description: Arc::new(EpollOpenDescription {
                token,
                descriptor_refs: AtomicUsize::new(1),
            }),
        }
    }

    pub fn path(&self) -> &'static str {
        "anon_inode:[eventpoll]"
    }

    pub fn token_id(&self) -> u64 {
        self.description.token
    }

    /// Pins one live fd-table reference without crossing the vfsd boundary.
    ///
    /// `Arc` clones are transient kernel snapshots and deliberately do not
    /// affect this count. Only a descriptor-table publication may call this.
    pub fn try_acquire_descriptor_reference(&self) -> bool {
        // ORDERING: AcqRel linearizes descriptor publication against final
        // release; Acquire on failure observes the zero terminal state.
        self.description
            .descriptor_refs
            // ORDERING: see the retain linearization contract above.
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
            .is_ok()
    }

    /// Drops one published fd-table reference and reports final retirement.
    ///
    /// A zero-to-live transition would resurrect a vfsd object after its
    /// durable tombstone, so underflow and resurrection are hard invariants.
    pub fn release_descriptor_reference(&self) -> bool {
        // ORDERING: AcqRel makes the final zero transition the unique retire
        // authority after all earlier descriptor publications.
        let previous = self
            .description
            .descriptor_refs
            // ORDERING: see the final-retire linearization contract above.
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(1)
            })
            .expect("epoll descriptor reference count underflow");
        previous == 1
    }

    pub fn is_last_reference(&self) -> bool {
        // ORDERING: Acquire observes the final AcqRel release before cleanup
        // derives service retirement from this snapshot.
        self.description.descriptor_refs.load(Ordering::Acquire) == 0
    }
}

impl PartialEq for EpollHandle {
    fn eq(&self, other: &Self) -> bool {
        self.token_id() == other.token_id()
    }
}

impl Eq for EpollHandle {}

impl Default for EpollHandle {
    fn default() -> Self {
        Self::new()
    }
}
// RING3-MIGRATION-REFERENCE END: vfsd/netd-owned epoll policy token substrate.

#[cfg(test)]
mod tests {
    use super::EpollHandle;

    #[test]
    fn descriptor_references_are_explicit_and_transient_clones_do_not_count() {
        let epoll = EpollHandle::new();
        let snapshot = epoll.clone();
        assert!(epoll.try_acquire_descriptor_reference());
        assert!(!snapshot.release_descriptor_reference());
        assert!(epoll.release_descriptor_reference());
        assert!(epoll.is_last_reference());
        assert!(!snapshot.try_acquire_descriptor_reference());
    }
}
