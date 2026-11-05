//! Receive-owner-only staging leases. No borrowed cwd, retained reply cap or
//! mutable image parser crosses turns. Removing an entry returns its payload
//! for cancellation/descriptor cleanup outside the table operation.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LeaseError { Full, Exhausted, Unknown, Owner, Lease }
pub struct Entry<T> { pub id: u64, pub owner: (u64, u64), pub end_ns: u64, pub value: T }
pub struct LaunchLeases<T, const N: usize> { slots: [Option<Entry<T>>; N], next_id: u64 }
impl<T, const N: usize> LaunchLeases<T, N> {
    pub const fn new() -> Self { Self { slots: [const { None }; N], next_id: 1 } }
    pub fn available(&self) -> bool { self.next_id != u64::MAX && self.slots.iter().any(Option::is_none) }
    pub fn insert(&mut self, owner: (u64, u64), end_ns: u64, value: T) -> Result<u64, (LeaseError, T)> {
        let Some(next) = self.next_id.checked_add(1) else { return Err((LeaseError::Exhausted, value)); };
        let Some(index) = self.slots.iter().position(Option::is_none) else { return Err((LeaseError::Full, value)); };
        let id = self.next_id; self.next_id = next;
        self.slots[index] = Some(Entry { id, owner, end_ns, value });
        Ok(id)
    }
    fn index(&self, id: u64, owner: (u64, u64), end_ns: u64) -> Result<usize, LeaseError> {
        let index = self.slots.iter().position(|e| e.as_ref().is_some_and(|e| e.id == id)).ok_or(LeaseError::Unknown)?;
        let e = self.slots[index].as_ref().unwrap();
        if e.owner != owner { return Err(LeaseError::Owner); }
        if e.end_ns != end_ns { return Err(LeaseError::Lease); }
        Ok(index)
    }
    pub fn get_mut(&mut self, id: u64, owner: (u64, u64), end_ns: u64) -> Result<&mut T, LeaseError> {
        let i = self.index(id, owner, end_ns)?;
        Ok(&mut self.slots[i].as_mut().unwrap().value)
    }
    pub fn take(&mut self, id: u64, owner: (u64, u64), end_ns: u64) -> Result<Entry<T>, LeaseError> {
        let i = self.index(id, owner, end_ns)?;
        Ok(self.slots[i].take().unwrap())
    }
    pub fn expire_one(&mut self, now: u64) -> Option<Entry<T>> {
        let i = self.slots.iter().position(|e| e.as_ref().is_some_and(|e| e.end_ns <= now))?;
        self.slots[i].take()
    }
    pub fn cancel_owner(&mut self, id: u64, owner: (u64, u64)) -> Option<Entry<T>> {
        let i = self.slots.iter().position(|e| e.as_ref().is_some_and(|e| e.id == id && e.owner == owner))?;
        self.slots[i].take()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_owner_lease_and_non_reused_identity() {
        let mut leases = LaunchLeases::<u32, 1>::new();
        let id = leases.insert((1, 2), 50, 7).unwrap();
        assert_eq!(leases.get_mut(id, (1, 3), 50), Err(LeaseError::Owner));
        assert_eq!(leases.get_mut(id, (1, 2), 51), Err(LeaseError::Lease));
        assert!(!leases.available());
        assert!(leases.expire_one(49).is_none());
        assert_eq!(leases.expire_one(50).unwrap().value, 7);
        let next = leases.insert((1, 2), 100, 8).unwrap();
        assert_ne!(next, id);
        assert!(leases.cancel_owner(next, (1, 3)).is_none());
        assert_eq!(leases.take(next, (1, 2), 100).unwrap().value, 8);
        assert_eq!(leases.get_mut(id, (1, 2), 50), Err(LeaseError::Unknown));
    }
    #[test]
    fn admission_failure_returns_ownership_and_counter_never_wraps() {
        let mut leases = LaunchLeases::<u32, 1>::new();
        leases.insert((1, 2), 50, 7).unwrap();
        assert_eq!(leases.insert((1, 2), 50, 8), Err((LeaseError::Full, 8)));
        leases.next_id = u64::MAX;
        leases.expire_one(50);
        assert_eq!(leases.insert((1, 2), 100, 9), Err((LeaseError::Exhausted, 9)));
    }
}
