//! Bounded one-shot activation permission transfer, not task/backing authority.
//!
//! Registry cleanup/bind and this transfer share the caller's short raw leaf.
//! Validate the WHOLE request before moving any original non-Copy record. PS
//! invokes this only after retaining Current/Cohort/frame loans, and retains the
//! returned grant through its last runnable publication. No raw leaf leaves the
//! callback; cleanup before transfer wins, cleanup after transfer cannot revoke
//! a capability already owned by that original in-flight activation.
use heapless::index_map::FnvIndexMap;

pub(super) fn take_exact<const N: usize, A, const CAP: usize>(
    registry: &mut FnvIndexMap<u64, A, CAP>,
    targets: &[u64],
    matches: impl Fn(u64, &A) -> bool,
) -> Option<[Option<A>; N]> {
    if targets.is_empty() || targets.len() > N { return None; }
    for (index, &pid) in targets.iter().enumerate() {
        if pid == 0 || targets[..index].contains(&pid)
            || registry.get(&pid).is_none_or(|authority| !matches(pid, authority)) {
            return None;
        }
    }
    let mut grant = [const { None }; N];
    for (index, pid) in targets.iter().enumerate() {
        grant[index] = Some(registry.remove(pid).expect("validated activation grant vanished"));
    }
    Some(grant)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    struct Original<'a> { generation: u64, dropped: &'a Cell<usize> }
    impl Drop for Original<'_> { fn drop(&mut self) { self.dropped.set(self.dropped.get() + 1); } }

    #[test]
    fn original_grants_move_once_and_whole_batch_refusals_preserve_every_record() {
        let dropped = Cell::new(0);
        let mut registry = FnvIndexMap::<u64, Original<'_>, 4>::new();
        assert!(registry.insert(11, Original { generation: 7, dropped: &dropped }).is_ok());
        assert!(registry.insert(13, Original { generation: 9, dropped: &dropped }).is_ok());
        for targets in [&[][..], &[11, 11], &[11, 15], &[11, 13, 15], &[0]] {
            assert!(take_exact::<2, _, 4>(&mut registry, targets, |_, _| true).is_none());
            assert_eq!(registry.len(), 2);
            assert_eq!(dropped.get(), 0);
        }
        assert!(take_exact::<2, _, 4>(&mut registry, &[11, 13], |pid, a| a.generation == pid).is_none());
        assert_eq!(registry.len(), 2);
        let grant = take_exact::<2, _, 4>(&mut registry, &[13, 11], |pid, a| a.generation == pid - 4).unwrap();
        assert_eq!(grant[0].as_ref().unwrap().generation, 9);
        assert_eq!(grant[1].as_ref().unwrap().generation, 7);
        assert_eq!(dropped.get(), 0, "permission must MOVE, not be discarded/copied");
        assert!(registry.is_empty());
        assert!(take_exact::<1, _, 4>(&mut registry, &[11], |_, _| true).is_none());
        drop(registry);
        assert_eq!(dropped.get(), 0, "original in-flight grant outlives registry cleanup");
        drop(grant);
        assert_eq!(dropped.get(), 2);
    }

    #[test]
    fn cleanup_winning_before_the_transfer_refuses_without_consuming_the_survivor() {
        let dropped = Cell::new(0);
        let mut registry = FnvIndexMap::<u64, Original<'_>, 4>::new();
        assert!(registry.insert(11, Original { generation: 7, dropped: &dropped }).is_ok());
        assert!(registry.insert(13, Original { generation: 9, dropped: &dropped }).is_ok());
        let revoked = registry.remove(&13).unwrap();
        assert!(take_exact::<2, _, 4>(&mut registry, &[11, 13], |_, _| true).is_none());
        assert!(registry.contains_key(&11));
        drop(revoked);
        assert_eq!(dropped.get(), 1);
        drop(registry);
        assert_eq!(dropped.get(), 2);
    }
}
