//! Shared identity, descriptor-reference and readiness plumbing for the
//! kernel-local anonymous fd objects (`pipe`, `eventfd`).
//!
//! - **Owner:** `kernel-ps` owns each object; compat owns blocking, wakeups,
//!   user copy and the epoll/poll scan that reads [`LocalWaitable`].
//! - **Boundary:** Tokens are kernel-minted and only name an object inside
//!   provider `WAITSET_PROVIDER_KERNEL`; user space never supplies one.
//! - **Lifecycle:** An object registers its description once at creation and
//!   is removed on its final descriptor release. The registry holds `Weak`
//!   references only, so it never keeps an object (or its charged pages) alive.
//! - **Concurrency:** The registry is one `LocalFdObject` leaf: bounded probe,
//!   no allocation, no wake, no callback and no second lock inside it.
//!   Removed or upgraded references are dropped after the guard is released.
//! - **Failure:** A full registry refuses creation (`false`), never evicts.
//! - **Forbidden:** Resurrecting a zero-reference description, holding the
//!   registry while locking an object, or signalling from user space.
//! - **Evidence:** `userspace-wait-set`, `vfs-open-description`.
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

/// Provider-level readiness of one object at one generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalReadiness {
    /// `EPOLLIN | EPOLLOUT | EPOLLERR | EPOLLHUP` provider bits.
    pub events: u32,
    /// Strictly increasing per readiness object, starting at 1.
    pub generation: u64,
    /// Edge generation for `EPOLLIN` watchers: advances only when new
    /// input-side data arrives, not on every wake-worthy change.
    pub in_generation: u64,
    /// Edge generation for `EPOLLOUT` watchers.
    pub out_generation: u64,
}

impl LocalReadiness {
    /// Readiness whose edges all share the wake generation (pipes).
    pub const fn uniform(events: u32, generation: u64) -> Self {
        Self {
            events,
            generation,
            in_generation: generation,
            out_generation: generation,
        }
    }

    /// The generation an interest in `wanted` provider bits compares for edge
    /// and oneshot decisions. Monotonic for any fixed `wanted`.
    pub fn edge_generation(&self, wanted: u32) -> u64 {
        let input = wanted & crate::user::linux::EPOLLIN != 0;
        let output = wanted & crate::user::linux::EPOLLOUT != 0;
        match (input, output) {
            (true, false) => self.in_generation,
            (false, true) => self.out_generation,
            (true, true) => self.in_generation.saturating_add(self.out_generation),
            (false, false) => self.generation,
        }
    }
}

/// A waiter registered at `generation` must be woken once the object publishes
/// a strictly greater generation for `token`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalWake {
    pub token: u64,
    pub generation: u64,
}

pub trait LocalWaitable: Send + Sync {
    /// Current readiness, or `None` after the object's final descriptor close.
    fn readiness(&self) -> Option<LocalReadiness>;
}

/// Outcome of dropping one descriptor-table reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalRelease {
    /// This was the final reference: the open description is now closed.
    pub last: bool,
    /// The peer-visible state change the close caused, to be signalled by the
    /// caller after every lock is released.
    pub wake: Option<LocalWake>,
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// A fresh nonzero object token. Tokens never wrap and are never reused.
pub fn allocate_token() -> Option<u64> {
    NEXT_TOKEN
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0).then(|| current.checked_add(1)).flatten()
        })
        .ok()
}

/// Descriptor-table reference count plus the per-description status bits that
/// Linux keeps on the open file description (so `dup` shares them).
#[derive(Debug)]
pub struct LocalDescription {
    token: u64,
    refs: AtomicUsize,
    nonblocking: AtomicBool,
    epoll_registered: AtomicBool,
}

impl LocalDescription {
    pub fn new(token: u64, nonblocking: bool) -> Self {
        Self {
            token,
            refs: AtomicUsize::new(1),
            nonblocking: AtomicBool::new(nonblocking),
            epoll_registered: AtomicBool::new(false),
        }
    }

    pub const fn token(&self) -> u64 {
        self.token
    }

    /// Pins one more fd-table reference; fails once the description closed.
    pub fn try_acquire(&self) -> bool {
        // ORDERING: AcqRel linearizes publication against the final release;
        // Acquire on failure observes the zero terminal state.
        self.refs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
            .is_ok()
    }

    /// Drops one reference and reports whether it was the last. Underflow is a
    /// hard ownership bug, never wrapped.
    pub fn release(&self) -> bool {
        // ORDERING: AcqRel makes the zero transition the unique close authority.
        let previous = self
            .refs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(1)
            })
            .expect("local fd descriptor reference underflow");
        previous == 1
    }

    pub fn is_closed(&self) -> bool {
        // ORDERING: Acquire pairs with the final AcqRel release.
        self.refs.load(Ordering::Acquire) == 0
    }

    pub fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Relaxed)
    }

    pub fn set_nonblocking(&self, value: bool) {
        self.nonblocking.store(value, Ordering::Relaxed);
    }

    /// Remembers that an epoll interest was registered, so the final close
    /// only pays the vfsd purge round trip for objects that need it.
    pub fn mark_epoll_registered(&self) {
        self.epoll_registered.store(true, Ordering::Release);
    }

    pub fn epoll_registered(&self) -> bool {
        self.epoll_registered.load(Ordering::Acquire)
    }
}

// Four thousand live objects system wide. A pipe is two objects; this bounds
// registry memory (128 KiB of `.bss`) independently of the per-process fd cap.
const REGISTRY_CAPACITY: usize = 4096;
const _: () = assert!(REGISTRY_CAPACITY.is_power_of_two());

enum Slot {
    Empty,
    Tombstone,
    Live {
        token: u64,
        object: Weak<dyn LocalWaitable>,
    },
}

struct Registry {
    slots: [Slot; REGISTRY_CAPACITY],
    live: usize,
}

impl Registry {
    const fn new() -> Self {
        Self {
            slots: [const { Slot::Empty }; REGISTRY_CAPACITY],
            live: 0,
        }
    }

    fn probe_start(token: u64) -> usize {
        let mut value = token;
        value ^= value >> 33;
        value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
        value ^= value >> 33;
        value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        value ^= value >> 33;
        value as usize & (REGISTRY_CAPACITY - 1)
    }

    fn find(&self, token: u64) -> Option<usize> {
        let start = Self::probe_start(token);
        for offset in 0..REGISTRY_CAPACITY {
            let index = (start + offset) & (REGISTRY_CAPACITY - 1);
            match &self.slots[index] {
                Slot::Empty => return None,
                Slot::Live { token: held, .. } if *held == token => return Some(index),
                _ => {}
            }
        }
        None
    }

    /// Installs `object`; on refusal hands it back so the caller drops it
    /// outside the guard.
    fn insert(&mut self, token: u64, object: Weak<dyn LocalWaitable>) -> Result<(), Weak<dyn LocalWaitable>> {
        if self.live == REGISTRY_CAPACITY || self.find(token).is_some() {
            return Err(object);
        }
        let start = Self::probe_start(token);
        for offset in 0..REGISTRY_CAPACITY {
            let index = (start + offset) & (REGISTRY_CAPACITY - 1);
            if matches!(self.slots[index], Slot::Empty | Slot::Tombstone) {
                self.slots[index] = Slot::Live { token, object };
                self.live += 1;
                return Ok(());
            }
        }
        Err(object)
    }

    fn remove(&mut self, token: u64) -> Option<Weak<dyn LocalWaitable>> {
        let index = self.find(token)?;
        let Slot::Live { object, .. } = core::mem::replace(&mut self.slots[index], Slot::Tombstone)
        else {
            return None;
        };
        self.live -= 1;
        Some(object)
    }
}

static REGISTRY: TrackedSpinLock<Registry, { LockClass::LocalFdObject as u8 }> =
    TrackedSpinLock::new(Registry::new());

/// Publishes `object` under `token`. False when the registry is full.
pub fn register(token: u64, object: Weak<dyn LocalWaitable>) -> bool {
    let refused = REGISTRY.lock().insert(token, object);
    // The refused `Weak` (if any) is released here, outside the guard.
    refused.is_ok()
}

/// Removes `token`. Idempotent.
pub fn unregister(token: u64) {
    let removed = REGISTRY.lock().remove(token);
    drop(removed);
}

/// Resolves `token` to its live object, if any. The returned strong reference
/// is a transient snapshot: it does not count as a descriptor reference.
pub fn lookup(token: u64) -> Option<Arc<dyn LocalWaitable>> {
    let upgraded = {
        let registry = REGISTRY.lock();
        let index = registry.find(token)?;
        match &registry.slots[index] {
            Slot::Live { object, .. } => object.upgrade(),
            _ => None,
        }
    };
    upgraded
}

/// Readiness of `token`, or `None` if it is unknown or closed.
pub fn readiness_of(token: u64) -> Option<LocalReadiness> {
    let object = lookup(token)?;
    object.readiness()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(LocalReadiness);
    impl LocalWaitable for Fixed {
        fn readiness(&self) -> Option<LocalReadiness> {
            Some(self.0)
        }
    }

    #[test]
    fn description_references_never_resurrect_and_report_the_last_release() {
        let description = LocalDescription::new(9, false);
        assert!(description.try_acquire());
        assert!(!description.release());
        assert!(!description.is_closed());
        assert!(description.release());
        assert!(description.is_closed());
        assert!(!description.try_acquire(), "a closed description is never revived");
    }

    #[test]
    fn registry_resolves_live_objects_and_forgets_dropped_ones() {
        let token = allocate_token().unwrap();
        let object: Arc<dyn LocalWaitable> = Arc::new(Fixed(LocalReadiness::uniform(1, 7)));
        assert!(register(token, Arc::downgrade(&object)));
        assert!(!register(token, Arc::downgrade(&object)), "a token is registered once");
        assert_eq!(readiness_of(token).unwrap().generation, 7);
        drop(object);
        assert!(readiness_of(token).is_none(), "the registry holds no strong reference");
        unregister(token);
        unregister(token);
    }

    #[test]
    fn tokens_are_distinct_and_nonzero() {
        let first = allocate_token().unwrap();
        let second = allocate_token().unwrap();
        assert!(first != 0 && second != 0 && first != second);
    }
}
