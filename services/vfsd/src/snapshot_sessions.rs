//! Bounded, owner-bound leases for asynchronous immutable snapshot work.
//! No reply capability survives OPEN/POLL. Payloads returned by these methods
//! must be destroyed only AFTER releasing the table lock.
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct SnapshotControl {
    cancelled: AtomicBool,
    completed: AtomicU64,
    total: AtomicU64,
}
impl SnapshotControl {
    fn new() -> Self { Self { cancelled: AtomicBool::new(false), completed: AtomicU64::new(0), total: AtomicU64::new(0) } }
    pub fn cancelled(&self) -> bool { self.cancelled.load(Ordering::Acquire) }
    pub fn completed_bytes(&self) -> u64 { self.completed.load(Ordering::Acquire) }
    pub fn total_bytes(&self) -> u64 { self.total.load(Ordering::Acquire) }
    pub fn set_total(&self, bytes: u64) { self.total.store(bytes, Ordering::Release); }
    pub fn progress(&self, bytes: u64) { self.completed.store(bytes, Ordering::Release); }
    fn cancel(&self) { self.cancelled.store(true, Ordering::Release); }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionError { Full, Exhausted, Unknown, Owner, Lease }
pub enum SessionPoll<T> {
    Pending { completed: u64, total: u64 },
    Ready(Result<T, i32>),
}
enum State<T> { Active, Ready(Result<T, i32>) }
struct Session<T> {
    id: u64,
    owner: (u64, u64),
    end_ns: u64,
    control: Arc<SnapshotControl>,
    state: State<T>,
}
pub struct SnapshotSessions<T, const N: usize> {
    slots: [Option<Session<T>>; N],
    next_id: u64,
}
impl<T, const N: usize> SnapshotSessions<T, N> {
    pub const fn new() -> Self { Self { slots: [const { None }; N], next_id: 1 } }
    pub fn begin(&mut self, owner: (u64, u64), end_ns: u64, now: u64) -> Result<(u64, Arc<SnapshotControl>), SessionError> {
        if end_ns <= now { return Err(SessionError::Lease); }
        let index = self.slots.iter().position(Option::is_none).ok_or(SessionError::Full)?;
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(SessionError::Exhausted)?;
        let control = Arc::new(SnapshotControl::new());
        self.slots[index] = Some(Session { id, owner, end_ns, control: control.clone(), state: State::Active });
        Ok((id, control))
    }
    fn index(&self, id: u64, owner: (u64, u64), end_ns: u64) -> Result<usize, SessionError> {
        let index = self.slots.iter().position(|slot| slot.as_ref().is_some_and(|s| s.id == id)).ok_or(SessionError::Unknown)?;
        let session = self.slots[index].as_ref().unwrap();
        if session.owner != owner { return Err(SessionError::Owner); }
        if session.end_ns != end_ns { return Err(SessionError::Lease); }
        Ok(index)
    }
    pub fn poll(&mut self, id: u64, owner: (u64, u64), end_ns: u64, now: u64) -> Result<SessionPoll<T>, SessionError> {
        let index = self.index(id, owner, end_ns)?;
        let s = self.slots[index].as_ref().unwrap();
        if now >= s.end_ns || s.control.cancelled() { return Err(SessionError::Lease); }
        if let State::Active = s.state {
            return Ok(SessionPoll::Pending { completed: s.control.completed.load(Ordering::Acquire), total: s.control.total.load(Ordering::Acquire) });
        }
        let session = self.slots[index].take().unwrap();
        match session.state { State::Ready(result) => Ok(SessionPoll::Ready(result)), State::Active => unreachable!() }
    }
    /// Active cancellation retains the slot until its worker acknowledges it;
    /// an old worker can never update the progress of a reused generation.
    pub fn cancel(&mut self, id: u64, owner: (u64, u64), end_ns: u64) -> Result<(), SessionError> {
        let index = self.index(id, owner, end_ns)?;
        self.slots[index].as_ref().unwrap().control.cancel();
        // Ready payloads are reaped by a worker, never the namespace loop.
        Ok(())
    }
    /// Returns a rejected payload to its caller; never drops it under a lock.
    pub fn finish(&mut self, id: u64, result: Result<T, i32>, now: u64) -> Option<Result<T, i32>> {
        let Some(index) = self.slots.iter().position(|s| s.as_ref().is_some_and(|s| s.id == id)) else { return Some(result); };
        let s = self.slots[index].as_ref().unwrap();
        if !matches!(s.state, State::Active) { return Some(result); }
        if now >= s.end_ns || s.control.cancelled() {
            self.slots[index].take(); // Active: owns no payload.
            return Some(result);
        }
        self.slots[index].as_mut().unwrap().state = State::Ready(result);
        None
    }
    /// Called periodically even without client traffic. Expired active work is
    /// cancelled; completed orphan payloads are returned for out-of-lock drop.
    pub fn expire_one(&mut self, now: u64) -> Option<Result<T, i32>> {
        for index in 0..N {
            let Some(s) = self.slots[index].as_ref() else { continue; };
            if now < s.end_ns && !s.control.cancelled() { continue; }
            s.control.cancel();
            if matches!(s.state, State::Active) { continue; }
            let s = self.slots[index].take().unwrap();
            if let State::Ready(result) = s.state { return Some(result); }
        }
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_consumption_progress_and_no_lease_renewal() {
        let mut table = SnapshotSessions::<u32, 2>::new();
        let (id, ctl) = table.begin((1, 2), 100, 0).unwrap();
        ctl.set_total(30); ctl.progress(7);
        assert!(matches!(table.poll(id, (1, 3), 100, 1), Err(SessionError::Owner)));
        assert!(matches!(table.poll(id, (1, 2), 101, 1), Err(SessionError::Lease)));
        assert!(matches!(table.poll(id, (1, 2), 100, 1), Ok(SessionPoll::Pending { completed: 7, total: 30 })));
        assert!(table.finish(id, Ok(9), 2).is_none());
        assert!(matches!(table.poll(id, (1, 2), 100, 3), Ok(SessionPoll::Ready(Ok(9)))));
        assert!(matches!(table.poll(id, (1, 2), 100, 4), Err(SessionError::Unknown)));
    }
    #[test]
    fn cancellation_holds_generation_until_worker_acknowledges() {
        let mut table = SnapshotSessions::<u32, 1>::new();
        let (id, ctl) = table.begin((1, 2), 100, 0).unwrap();
        table.cancel(id, (1, 2), 100).unwrap();
        assert!(ctl.cancelled());
        assert!(matches!(table.begin((1, 2), 200, 1), Err(SessionError::Full)));
        assert_eq!(table.finish(id, Ok(99), 2), Some(Ok(99)));
        let (next, _) = table.begin((1, 2), 200, 3).unwrap();
        assert_ne!(next, id);
        assert_eq!(table.finish(id, Ok(98), 4), Some(Ok(98)));
        assert!(matches!(table.poll(next, (1, 2), 200, 4), Ok(SessionPoll::Pending { .. })));
    }
    #[test]
    fn cancellation_of_ready_payload_is_reaped_without_dropping_under_a_lock() {
        struct Payload(Arc<AtomicBool>, Arc<AtomicU64>);
        impl Drop for Payload {
            fn drop(&mut self) { assert!(!self.0.load(Ordering::Relaxed)); self.1.fetch_add(1, Ordering::Relaxed); }
        }
        let locked = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicU64::new(0));
        let mut table = SnapshotSessions::<Payload, 1>::new();
        let (id, _) = table.begin((1, 2), 100, 0).unwrap();
        locked.store(true, Ordering::Relaxed);
        assert!(table.finish(id, Ok(Payload(locked.clone(), drops.clone())), 1).is_none());
        table.cancel(id, (1, 2), 100).unwrap();
        let retired = table.expire_one(2);
        assert!(retired.is_some());
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        locked.store(false, Ordering::Relaxed);
        drop(retired);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn expiry_reclaims_success_error_and_active_orphans() {
        let mut table = SnapshotSessions::<u32, 2>::new();
        let (a, _) = table.begin((1, 2), 10, 0).unwrap();
        let (b, ctl) = table.begin((1, 2), 10, 0).unwrap();
        table.finish(a, Ok(4), 1);
        assert_eq!(table.expire_one(10), Some(Ok(4)));
        assert_eq!(table.expire_one(10), None);
        assert!(ctl.cancelled());
        assert_eq!(table.finish(b, Err(5), 11), Some(Err(5)));
        let (c, _) = table.begin((1, 2), 20, 12).unwrap();
        table.finish(c, Err(6), 13);
        assert_eq!(table.expire_one(20), Some(Err(6)));
        table.next_id = u64::MAX;
        assert!(matches!(table.begin((1, 2), 30, 21), Err(SessionError::Exhausted)));
    }
}
