//! Kernel-local `timerfd` interval/one-shot timer object.
//!
//! - **Owner:** ring0 owns timer deadlines, interval recurrence, expiration
//!   counts and descriptor lifetime; compat owns flag/timespec validation,
//!   blocking/epoll wait integration and user copy.
//! - **Boundary:** Clock ids, flags, and `itimerspec` values are untrusted;
//!   negative values or nanoseconds `>= 1_000_000_000` are rejected.
//! - **Lifecycle:** One open description per `timerfd_create` call; dup and
//!   fork retain it through compat's `ServiceHandleRef`; final release
//!   unregisters it from `local_wait`.
//! - **Concurrency:** One `LocalFdObject` leaf guards timer state; readiness
//!   checks advance expired periods lazily against `arch::clock::monotonic_nanos`
//!   so no background timer thread is needed. Setting a timer publishes
//!   `LocalWake` outside the guard so blocked readers/pollers recalculate their
//!   sleep deadline immediately.

use alloc::sync::Arc;

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

use crate::user::linux as linux_abi;
use crate::user::local_wait::{
    self, LocalDescription, LocalReadiness, LocalRelease, LocalWaitable, LocalWake,
};
use crate::user::page_lease::PageLease;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerFdError {
    WouldBlock {
        generation: u64,
        deadline_ns: Option<u64>,
    },
    TooManyObjects,
    Retry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimerFdRead {
    pub count: u64,
    epoch: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TimerFdSpec {
    pub interval_ns: u64,
    pub value_ns: u64,
}

struct TimerFdState {
    clock_id: u32,
    /// Monotonic-domain instant for the next expiration, or `0` when disarmed.
    next_expiry_ns: u64,
    interval_ns: u64,
    expirations: u64,
    /// Changes on settime or successful consumption, not on expiration.
    read_epoch: u64,
    generation: u64,
    in_generation: u64,
}

impl TimerFdState {
    fn advance(&mut self, now_ns: u64) -> bool {
        if self.next_expiry_ns == 0 || now_ns < self.next_expiry_ns {
            return false;
        }
        if self.interval_ns == 0 {
            self.expirations = self.expirations.saturating_add(1);
            self.next_expiry_ns = 0;
        } else {
            let elapsed = now_ns - self.next_expiry_ns;
            let count = 1_u64.saturating_add(elapsed / self.interval_ns);
            self.expirations = self.expirations.saturating_add(count);
            let remaining = self.interval_ns - elapsed % self.interval_ns;
            self.next_expiry_ns = now_ns.checked_add(remaining).unwrap_or(0);
        }
        self.generation = self.generation.saturating_add(1);
        self.in_generation = self.in_generation.saturating_add(1);
        true
    }

    fn snapshot_remaining(&mut self, now_ns: u64) -> TimerFdSpec {
        let _ = self.advance(now_ns);
        let value_ns = if self.next_expiry_ns > now_ns {
            self.next_expiry_ns - now_ns
        } else {
            0
        };
        TimerFdSpec {
            interval_ns: self.interval_ns,
            value_ns,
        }
    }
}

struct TimerFdObject {
    description: LocalDescription,
    _lease: PageLease,
    state: TrackedSpinLock<TimerFdState, { LockClass::LocalFdObject as u8 }>,
}

impl TimerFdObject {
    fn wake(&self, generation: u64) -> LocalWake {
        LocalWake {
            token: self.description.token(),
            generation,
        }
    }
}

impl Drop for TimerFdObject {
    fn drop(&mut self) {
        if !self.description.is_closed() {
            local_wait::unregister(self.description.token());
        }
    }
}

impl LocalWaitable for TimerFdObject {
    fn readiness(&self) -> Option<LocalReadiness> {
        if self.description.is_closed() {
            return None;
        }
        let now_ns = crate::arch::clock::monotonic_nanos();
        let mut state = self.state.lock();
        let _ = state.advance(now_ns);
        let events = if state.expirations > 0 {
            linux_abi::EPOLLIN
        } else {
            0
        };
        Some(LocalReadiness {
            events,
            generation: state.generation,
            in_generation: state.in_generation,
            out_generation: 1,
        })
    }

    fn deadline_ns(&self) -> Option<u64> {
        if self.description.is_closed() {
            return None;
        }
        let state = self.state.lock();
        (state.expirations == 0 && state.next_expiry_ns != 0).then_some(state.next_expiry_ns)
    }
}

/// One timerfd open description held by descriptor tables.
#[derive(Clone)]
pub struct TimerFdHandle {
    object: Arc<TimerFdObject>,
}

impl core::fmt::Debug for TimerFdHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TimerFdHandle")
            .field("token", &self.token_id())
            .finish()
    }
}

impl TimerFdHandle {
    pub fn new_in_group(
        clock_id: u32,
        nonblocking: bool,
        charge_group: Option<usize>,
    ) -> Result<Self, TimerFdError> {
        let lease = PageLease::acquire(charge_group, 1).ok_or(TimerFdError::TooManyObjects)?;
        let token = local_wait::allocate_token().ok_or(TimerFdError::TooManyObjects)?;
        let object = Arc::new(TimerFdObject {
            description: LocalDescription::new(token, nonblocking),
            _lease: lease,
            state: TrackedSpinLock::new(TimerFdState {
                clock_id,
                next_expiry_ns: 0,
                interval_ns: 0,
                expirations: 0,
                read_epoch: 0,
                generation: 1,
                in_generation: 1,
            }),
        });
        let waitable: Arc<dyn LocalWaitable> = object.clone();
        if !local_wait::register(token, Arc::downgrade(&waitable)) {
            return Err(TimerFdError::TooManyObjects);
        }
        Ok(Self { object })
    }

    pub fn token_id(&self) -> u64 {
        self.object.description.token()
    }

    pub fn clock_id(&self) -> u32 {
        self.object.state.lock().clock_id
    }

    pub fn status_flags(&self) -> u64 {
        linux_abi::O_RDONLY
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
        let mut state = self.object.state.lock();
        state.generation = state.generation.saturating_add(1);
        state.in_generation = state.in_generation.saturating_add(1);
        LocalRelease {
            last: true,
            wake: Some(self.object.wake(state.generation)),
        }
    }

    pub fn gettime_at(&self, now_ns: u64) -> TimerFdSpec {
        self.object.state.lock().snapshot_remaining(now_ns)
    }

    /// Programs the next deadline (zero means no more deadlines) and any
    /// expirations already due in the caller's clock domain.
    pub fn settime_at(
        &self,
        now_ns: u64,
        next_expiry_ns: u64,
        interval_ns: u64,
        expirations: u64,
    ) -> (TimerFdSpec, LocalWake) {
        let mut state = self.object.state.lock();
        let old = state.snapshot_remaining(now_ns);
        state.read_epoch = state.read_epoch.wrapping_add(1);
        state.expirations = expirations;
        state.next_expiry_ns = next_expiry_ns;
        state.interval_ns = if next_expiry_ns == 0 && expirations == 0 {
            0
        } else {
            interval_ns
        };
        let advanced = state.advance(now_ns);
        state.generation = state.generation.saturating_add(1);
        if !advanced && state.expirations > 0 {
            state.in_generation = state.in_generation.saturating_add(1);
        }
        let wake = self.object.wake(state.generation);
        (old, wake)
    }

    /// Peeks accumulated expirations at `now_ns`. When none have elapsed yet,
    /// returns `WouldBlock` with the active monotonic deadline (if armed).
    pub fn peek_read_at(&self, now_ns: u64) -> Result<TimerFdRead, TimerFdError> {
        let mut state = self.object.state.lock();
        let _ = state.advance(now_ns);
        if state.expirations == 0 {
            return Err(TimerFdError::WouldBlock {
                generation: state.generation,
                deadline_ns: (state.next_expiry_ns != 0).then_some(state.next_expiry_ns),
            });
        }
        Ok(TimerFdRead {
            count: state.expirations,
            epoch: state.read_epoch,
        })
    }

    /// Consumes exactly the expirations copied out, preserving later expirations.
    pub fn commit_read(&self, read: TimerFdRead) -> Result<LocalWake, TimerFdError> {
        let mut state = self.object.state.lock();
        if state.read_epoch != read.epoch || state.expirations < read.count {
            return Err(TimerFdError::Retry);
        }
        state.read_epoch = state.read_epoch.wrapping_add(1);
        state.expirations -= read.count;
        state.generation = state.generation.saturating_add(1);
        Ok(self.object.wake(state.generation))
    }
}

impl PartialEq for TimerFdHandle {
    fn eq(&self, other: &Self) -> bool {
        self.token_id() == other.token_id()
    }
}

impl Eq for TimerFdHandle {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_reads_and_rearm_invalidate_a_stale_observation() {
        let tfd = TimerFdHandle::new_in_group(linux_abi::CLOCK_MONOTONIC as u32, false, None)
            .unwrap();
        tfd.settime_at(100, 200, 100, 0);
        let read = tfd.peek_read_at(200).unwrap();
        assert_eq!(tfd.peek_read_at(300).unwrap().count, 2);
        tfd.commit_read(read).unwrap();
        assert_eq!(tfd.commit_read(read), Err(TimerFdError::Retry));
        let remaining = tfd.peek_read_at(300).unwrap();
        assert_eq!(remaining.count, 1);
        tfd.settime_at(300, 300, 0, 0);
        assert_eq!(tfd.commit_read(remaining), Err(TimerFdError::Retry));
        assert_eq!(tfd.peek_read_at(300).unwrap().count, 1);
    }

    #[test]
    fn one_shot_and_periodic_expirations_advance_and_reset_cleanly() {
        let tfd =
            TimerFdHandle::new_in_group(linux_abi::CLOCK_MONOTONIC as u32, false, None).unwrap();
        assert_eq!(tfd.gettime_at(100), TimerFdSpec::default());

        let (old, _) = tfd.settime_at(1_000, 1_500, 0, 0);
        assert_eq!(old, TimerFdSpec::default());
        assert!(matches!(
            tfd.peek_read_at(1_200),
            Err(TimerFdError::WouldBlock {
                deadline_ns: Some(1_500),
                ..
            })
        ));
        let read = tfd.peek_read_at(1_500).unwrap();
        assert_eq!(read.count, 1);
        assert!(tfd.commit_read(read).is_ok());
        assert_eq!(
            tfd.gettime_at(1_600),
            TimerFdSpec::default(),
            "one-shot disarms after firing"
        );

        let _ = tfd.settime_at(2_000, 2_100, 100, 0);
        let read = tfd.peek_read_at(2_350).unwrap();
        assert_eq!(read.count, 3);
        assert!(tfd.commit_read(read).is_ok());
        assert_eq!(
            tfd.gettime_at(2_350),
            TimerFdSpec {
                interval_ns: 100,
                value_ns: 50,
            }
        );

        let (prev, _) = tfd.settime_at(2_350, 0, 0, 0);
        assert_eq!(
            prev,
            TimerFdSpec {
                interval_ns: 100,
                value_ns: 50,
            }
        );
        assert!(matches!(
            tfd.peek_read_at(5_000),
            Err(TimerFdError::WouldBlock {
                deadline_ns: None,
                ..
            })
        ));
    }
}
