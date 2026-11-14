//! Kernel-local `eventfd` counter object.
//!
//! - **Owner:** ring0 owns the 64-bit counter and descriptor lifetime; compat
//!   owns blocking, user copy and wakeups. No policy is involved (the object
//!   is anonymous and unnamed) and it holds no allocation, so no charge.
//! - **Boundary:** `initval` and written values are untrusted; `u64::MAX` is
//!   never a valid value and the counter never exceeds `u64::MAX - 1`.
//! - **Lifecycle:** one open description per `eventfd2` call; dup and fork
//!   retain it through compat's `ServiceHandleRef`; final release unregisters
//!   it from the readiness registry.
//! - **Concurrency:** one `LocalFdObject` leaf guards `(counter, generation)`;
//!   mutations return the `LocalWake` to publish after the guard is dropped.
//!   The wake generation covers every change; epoll edges use separate
//!   IN (write) and OUT (read) generations, so a read that leaves a nonzero
//!   counter is not a new `EPOLLIN` edge.
//! - **Failure:** an empty counter on read or an overflowing add reports
//!   `WouldBlock` with the generation observed under the guard.
//! - **Forbidden:** wrapping the counter, accepting `u64::MAX`, partial reads.
//! - **Evidence:** `userspace-wait-set`, `vfs-open-description`.
use alloc::sync::Arc;

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

use crate::user::linux as linux_abi;
use crate::user::local_wait::{
    self, LocalDescription, LocalReadiness, LocalRelease, LocalWaitable, LocalWake,
};

/// Largest counter value: `u64::MAX` is reserved by the ABI.
pub const EVENTFD_MAX_COUNTER: u64 = u64::MAX - 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventFdError {
    WouldBlock { generation: u64 },
    /// A write of `u64::MAX`.
    InvalidValue,
    TooManyObjects,
}

struct EventFdState {
    counter: u64,
    /// Advances on every change: what blocked readers and writers wait on.
    generation: u64,
    /// Advances when a write adds input (an `EPOLLIN` edge).
    in_generation: u64,
    /// Advances when a read makes room (an `EPOLLOUT` edge).
    out_generation: u64,
}

struct EventFdObject {
    semaphore: bool,
    description: LocalDescription,
    state: TrackedSpinLock<EventFdState, { LockClass::LocalFdObject as u8 }>,
}

impl EventFdObject {
    fn wake(&self, generation: u64) -> LocalWake {
        LocalWake {
            token: self.description.token(),
            generation,
        }
    }
}

impl Drop for EventFdObject {
    fn drop(&mut self) {
        // Safety net for a handle dropped without a final descriptor release.
        if !self.description.is_closed() {
            local_wait::unregister(self.description.token());
        }
    }
}

impl LocalWaitable for EventFdObject {
    fn readiness(&self) -> Option<LocalReadiness> {
        if self.description.is_closed() {
            return None;
        }
        let state = self.state.lock();
        let mut events = 0;
        if state.counter > 0 {
            events |= linux_abi::EPOLLIN;
        }
        if state.counter < EVENTFD_MAX_COUNTER {
            events |= linux_abi::EPOLLOUT;
        }
        Some(LocalReadiness {
            events,
            generation: state.generation,
            in_generation: state.in_generation,
            out_generation: state.out_generation,
        })
    }
}

/// One eventfd open description as held by descriptor tables.
#[derive(Clone)]
pub struct EventFdHandle {
    object: Arc<EventFdObject>,
}

impl core::fmt::Debug for EventFdHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EventFdHandle")
            .field("token", &self.token_id())
            .finish()
    }
}

impl EventFdHandle {
    pub fn new(initval: u32, semaphore: bool, nonblocking: bool) -> Result<Self, EventFdError> {
        let token = local_wait::allocate_token().ok_or(EventFdError::TooManyObjects)?;
        let object = Arc::new(EventFdObject {
            semaphore,
            description: LocalDescription::new(token, nonblocking),
            state: TrackedSpinLock::new(EventFdState {
                counter: u64::from(initval),
                generation: 1,
                in_generation: 1,
                out_generation: 1,
            }),
        });
        let waitable: Arc<dyn LocalWaitable> = object.clone();
        if !local_wait::register(token, Arc::downgrade(&waitable)) {
            return Err(EventFdError::TooManyObjects);
        }
        Ok(Self { object })
    }

    pub fn token_id(&self) -> u64 {
        self.object.description.token()
    }

    pub fn is_semaphore(&self) -> bool {
        self.object.semaphore
    }

    pub fn path(&self) -> alloc::string::String {
        alloc::string::String::from("anon_inode:[eventfd]")
    }

    pub fn status_flags(&self) -> u64 {
        linux_abi::O_RDWR
            | if self.object.description.nonblocking() {
                linux_abi::O_NONBLOCK
            } else {
                0
            }
    }

    pub fn nonblocking(&self) -> bool {
        self.object.description.nonblocking()
    }

    pub fn set_nonblocking(&self, value: bool) {
        self.object.description.set_nonblocking(value);
    }

    pub fn mark_epoll_registered(&self) {
        self.object.description.mark_epoll_registered();
    }

    pub fn epoll_registered(&self) -> bool {
        self.object.description.epoll_registered()
    }

    /// Peeks the value a `read` would return without consuming the counter.
    pub fn peek_read(&self) -> Result<u64, EventFdError> {
        let state = self.object.state.lock();
        if state.counter == 0 {
            return Err(EventFdError::WouldBlock {
                generation: state.generation,
            });
        }
        Ok(if self.object.semaphore { 1 } else { state.counter })
    }

    /// Commits a previously peeked read after copy-out succeeds, waking writers.
    pub fn commit_read(&self, value: u64) -> Result<LocalWake, EventFdError> {
        let mut state = self.object.state.lock();
        let consumed = if self.object.semaphore {
            if state.counter == 0 {
                return Err(EventFdError::WouldBlock {
                    generation: state.generation,
                });
            }
            state.counter -= 1;
            1
        } else {
            if state.counter < value {
                return Err(EventFdError::WouldBlock {
                    generation: state.generation,
                });
            }
            state.counter -= value;
            value
        };
        let _ = consumed;
        state.generation += 1;
        state.out_generation += 1;
        Ok(self.object.wake(state.generation))
    }

    /// `read`: returns the counter (or 1 in semaphore mode) and zeroes it (or
    /// decrements it). A zero counter is `WouldBlock`.
    pub fn try_read(&self) -> Result<(u64, LocalWake), EventFdError> {
        let mut state = self.object.state.lock();
        if state.counter == 0 {
            return Err(EventFdError::WouldBlock {
                generation: state.generation,
            });
        }
        let value = if self.object.semaphore {
            state.counter -= 1;
            1
        } else {
            core::mem::take(&mut state.counter)
        };
        state.generation += 1;
        state.out_generation += 1;
        Ok((value, self.object.wake(state.generation)))
    }

    /// `write`: adds `value`. `WouldBlock` if the sum would pass
    /// [`EVENTFD_MAX_COUNTER`].
    pub fn try_write(&self, value: u64) -> Result<LocalWake, EventFdError> {
        if value == u64::MAX {
            return Err(EventFdError::InvalidValue);
        }
        let mut state = self.object.state.lock();
        match state.counter.checked_add(value) {
            Some(sum) if sum <= EVENTFD_MAX_COUNTER => state.counter = sum,
            _ => {
                return Err(EventFdError::WouldBlock {
                    generation: state.generation,
                });
            }
        }
        state.generation += 1;
        state.in_generation += 1;
        Ok(self.object.wake(state.generation))
    }

    pub fn counter(&self) -> u64 {
        self.object.state.lock().counter
    }

    pub fn try_acquire_descriptor_reference(&self) -> bool {
        self.object.description.try_acquire()
    }

    pub fn release_descriptor_reference(&self) -> LocalRelease {
        if !self.object.description.release() {
            return LocalRelease {
                last: false,
                wake: None,
            };
        }
        local_wait::unregister(self.object.description.token());
        LocalRelease {
            last: true,
            wake: None,
        }
    }
}

impl PartialEq for EventFdHandle {
    fn eq(&self, other: &Self) -> bool {
        self.token_id() == other.token_id()
    }
}

impl Eq for EventFdHandle {}

#[cfg(test)]
mod tests {
    use super::*;

    fn readiness(handle: &EventFdHandle) -> LocalReadiness {
        handle.object.readiness().unwrap()
    }

    #[test]
    fn plain_read_returns_and_zeroes_the_counter() {
        let efd = EventFdHandle::new(5, false, false).unwrap();
        assert_eq!(efd.try_read().unwrap().0, 5);
        assert!(matches!(efd.try_read(), Err(EventFdError::WouldBlock { .. })));
        efd.try_write(3).unwrap();
        efd.try_write(4).unwrap();
        assert_eq!(efd.try_read().unwrap().0, 7, "writes accumulate");
        assert_eq!(efd.counter(), 0);
    }

    #[test]
    fn semaphore_read_returns_one_and_decrements() {
        let efd = EventFdHandle::new(2, true, false).unwrap();
        assert_eq!(efd.try_read().unwrap().0, 1);
        assert_eq!(efd.try_read().unwrap().0, 1);
        assert!(matches!(efd.try_read(), Err(EventFdError::WouldBlock { .. })));
    }

    #[test]
    fn counter_stops_at_u64_max_minus_one() {
        let efd = EventFdHandle::new(0, false, false).unwrap();
        assert_eq!(efd.try_write(u64::MAX), Err(EventFdError::InvalidValue));
        efd.try_write(EVENTFD_MAX_COUNTER - 1).unwrap();
        let observed = match efd.try_write(2) {
            Err(EventFdError::WouldBlock { generation }) => generation,
            other => panic!("overflowing add must block: {other:?}"),
        };
        efd.try_write(1).unwrap();
        assert_eq!(efd.counter(), EVENTFD_MAX_COUNTER);
        assert_eq!(readiness(&efd).events, linux_abi::EPOLLIN, "no EPOLLOUT at the maximum");
        let (value, wake) = efd.try_read().unwrap();
        assert_eq!(value, EVENTFD_MAX_COUNTER);
        assert!(wake.generation > observed, "a blocked writer registered at {observed} is woken");
        assert_eq!(efd.try_write(0).unwrap().token, efd.token_id(), "adding zero is legal");
    }

    #[test]
    fn readiness_follows_the_counter() {
        let efd = EventFdHandle::new(0, false, false).unwrap();
        let idle = readiness(&efd);
        assert_eq!(idle.events, linux_abi::EPOLLOUT);
        let wake = efd.try_write(1).unwrap();
        let ready = readiness(&efd);
        assert_eq!(ready.events, linux_abi::EPOLLIN | linux_abi::EPOLLOUT);
        assert_eq!(ready.generation, wake.generation);
        assert!(ready.generation > idle.generation);
        efd.try_read().unwrap();
        assert_eq!(readiness(&efd).events, linux_abi::EPOLLOUT);
    }

    #[test]
    fn semaphore_read_leaving_a_count_is_not_a_new_input_edge() {
        let efd = EventFdHandle::new(0, true, false).unwrap();
        efd.try_write(2).unwrap();
        let before = readiness(&efd);
        efd.try_read().unwrap();
        let after = readiness(&efd);
        assert_eq!(after.events & linux_abi::EPOLLIN, linux_abi::EPOLLIN, "still readable");
        assert!(after.generation > before.generation, "blocked writers are still woken");
        assert_eq!(
            after.edge_generation(linux_abi::EPOLLIN),
            before.edge_generation(linux_abi::EPOLLIN),
            "no spurious EPOLLIN edge"
        );
        assert!(
            after.edge_generation(linux_abi::EPOLLOUT) > before.edge_generation(linux_abi::EPOLLOUT),
            "the read is an EPOLLOUT edge"
        );
        efd.try_write(1).unwrap();
        assert!(
            readiness(&efd).edge_generation(linux_abi::EPOLLIN)
                > after.edge_generation(linux_abi::EPOLLIN),
            "a write is an EPOLLIN edge"
        );
    }

    #[test]
    fn initial_value_may_be_the_full_u32_range() {
        let efd = EventFdHandle::new(u32::MAX, false, false).unwrap();
        assert_eq!(efd.try_read().unwrap().0, u64::from(u32::MAX));
    }

    #[test]
    fn dup_retains_the_description_and_the_last_release_unregisters_it() {
        let efd = EventFdHandle::new(1, false, true).unwrap();
        assert!(efd.nonblocking());
        assert!(efd.try_acquire_descriptor_reference());
        assert!(!efd.release_descriptor_reference().last);
        assert!(local_wait::readiness_of(efd.token_id()).is_some());
        assert!(efd.release_descriptor_reference().last);
        assert!(local_wait::readiness_of(efd.token_id()).is_none());
        assert!(!efd.try_acquire_descriptor_reference());
    }

    #[test]
    fn peek_read_does_not_consume_counter_until_commit_read() {
        let efd = EventFdHandle::new(5, false, false).unwrap();
        assert_eq!(efd.peek_read().unwrap(), 5);
        assert_eq!(efd.counter(), 5);
        assert!(efd.commit_read(5).is_ok());
        assert_eq!(efd.counter(), 0);

        let sem = EventFdHandle::new(2, true, false).unwrap();
        assert_eq!(sem.peek_read().unwrap(), 1);
        assert_eq!(sem.counter(), 2);
        assert!(sem.commit_read(1).is_ok());
        assert_eq!(sem.counter(), 1);
    }
}
